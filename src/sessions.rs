//! Reads each CLI's session files for listing, and builds cross-CLI handoffs.

use crate::providers::ProviderSpec;
use crate::store::falsy;
use crate::time;
use crate::util::one_line;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct SessionInfo {
    pub provider: String,
    pub id: String,
    pub cwd: Option<String>,
    pub title: String,
    /// Milliseconds since the epoch.
    pub updated_at: i64,
    pub file: PathBuf,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Role {
    User,
    Assistant,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Turn {
    pub role: Role,
    pub text: String,
}

const HEAD_BYTES: u64 = 256 * 1024;
/// Most transcripts have their metadata and first prompt in the first few KB.
const FIRST_READ_BYTES: u64 = 32 * 1024;

/// Runs `parse` on the start of a transcript: the first 32 KB, then the first 256 KB if
/// that wasn't enough. A record cut off at the end is skipped, and records are in order,
/// so whatever is found in the shorter read is what the longer one would find first.
fn scan_head<T>(file: &Path, buf: &mut Vec<u8>, parse: impl Fn(&str) -> Option<T>) -> Option<T> {
    for limit in [FIRST_READ_BYTES, HEAD_BYTES] {
        buf.clear();
        let f = fs::File::open(file).ok()?;
        let _ = f.take(limit).read_to_end(buf);
        if let Some(found) = parse(&String::from_utf8_lossy(buf)) {
            return Some(found);
        }
        if (buf.len() as u64) < limit {
            break;
        }
    }
    None
}

fn json_lines(text: &str) -> impl Iterator<Item = Value> + '_ {
    // Skips blank lines, a partial last line from a head read, and corrupt lines.
    text.split('\n')
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
}

/// Collects matching files under `dir`, descending at most `max_depth` directory levels.
pub(crate) fn walk(
    dir: &Path,
    matches: &dyn Fn(&str) -> bool,
    max_depth: usize,
    out: &mut Vec<PathBuf>,
) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if is_dir {
            if max_depth > 0 {
                walk(&p, matches, max_depth - 1, out);
            }
        } else if matches(&e.file_name().to_string_lossy()) {
            out.push(p);
        }
    }
}

/// Skips injected context (AGENTS.md, environment blocks, command wrappers) when picking a title.
fn is_real_prompt(t: &str) -> bool {
    let s = t.trim_start();
    !t.trim().is_empty()
        && !(s.starts_with('<') || s.starts_with("# AGENTS.md") || s.starts_with("Caveat:"))
}

fn text_of(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|c| match c {
                Value::String(s) => Some(s.clone()),
                Value::Object(o)
                    if matches!(
                        o.get("type").and_then(Value::as_str),
                        Some("text" | "input_text" | "output_text")
                    ) =>
                {
                    o.get("text").and_then(Value::as_str).map(str::to_string)
                }
                _ => None,
            })
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

pub(crate) fn mtime_ms(file: &Path) -> i64 {
    fs::metadata(file)
        .and_then(|m| m.modified())
        .map(time::to_ms)
        .unwrap_or(0)
}

fn first_user(turns: &[Turn]) -> Option<&Turn> {
    turns.iter().find(|t| t.role == Role::User)
}

// ---- Claude Code: <home>/projects/<cwd-slug>/<session-id>.jsonl

/// With `first_only`, stops once the cwd and the first user prompt are known (for listing).
fn claude_turns(text: &str, first_only: bool) -> (Option<String>, Vec<Turn>) {
    let mut cwd: Option<String> = None;
    let mut turns = vec![];
    for rec in json_lines(text) {
        if first_only && cwd.is_some() && first_user(&turns).is_some() {
            break;
        }
        if cwd.is_none() {
            cwd = rec.get("cwd").and_then(Value::as_str).map(str::to_string);
        }
        if !falsy(rec.get("isSidechain")) || !falsy(rec.get("isMeta")) {
            continue;
        }
        let kind = rec.get("type").and_then(Value::as_str);
        let msg = rec.get("message");
        if kind == Some("user")
            && msg.and_then(|m| m.get("role")).and_then(Value::as_str) == Some("user")
        {
            let t = text_of(msg.and_then(|m| m.get("content")));
            if is_real_prompt(&t) {
                turns.push(Turn {
                    role: Role::User,
                    text: t,
                });
            }
        } else if kind == Some("assistant") {
            let t = text_of(msg.and_then(|m| m.get("content")));
            if !t.is_empty() {
                turns.push(Turn {
                    role: Role::Assistant,
                    text: t,
                });
            }
        }
    }
    (cwd, turns)
}

fn list_claude(spec: &ProviderSpec, id_prefix: Option<&str>) -> Vec<SessionInfo> {
    // Sessions sit at projects/<cwd-slug>/<id>.jsonl; deeper dirs hold subagent transcripts and tool output.
    let mut files = vec![];
    walk(
        &spec.shared_home.join("projects"),
        // The id is the file name, so a prefix lookup can skip other files unread.
        &|n| n.ends_with(".jsonl") && id_prefix.is_none_or(|p| n.starts_with(p)),
        1,
        &mut files,
    );
    let mut buf = Vec::with_capacity(HEAD_BYTES as usize);
    files
        .into_iter()
        .filter_map(|file| {
            let (cwd, title) = scan_head(&file, &mut buf, |text| {
                let (cwd, turns) = claude_turns(text, true);
                Some((cwd, one_line(&first_user(&turns)?.text, 80)))
            })?;
            Some(SessionInfo {
                provider: spec.id.clone(),
                id: file.file_stem()?.to_string_lossy().into_owned(),
                cwd,
                title,
                updated_at: mtime_ms(&file),
                file,
            })
        })
        .collect()
}

// ---- Codex: <home>/sessions/YYYY/MM/DD/rollout-*.jsonl

/// With `first_only`, stops once the session id and the first user prompt are known (for listing).
fn codex_turns(text: &str, first_only: bool) -> (Option<String>, Option<String>, Vec<Turn>) {
    let mut id: Option<String> = None;
    let mut cwd: Option<String> = None;
    let mut turns: Vec<Turn> = vec![];
    let empty = Value::Object(Default::default());
    for rec in json_lines(text) {
        if first_only && id.is_some() && first_user(&turns).is_some() {
            break;
        }
        let p = rec
            .get("payload")
            .filter(|v| !v.is_null())
            .unwrap_or(&empty);
        let kind = rec.get("type").and_then(Value::as_str);
        if kind == Some("session_meta") {
            if id.is_none() {
                id = ["id", "session_id"]
                    .iter()
                    .find_map(|k| p.get(*k).and_then(Value::as_str).filter(|s| !s.is_empty()))
                    .map(str::to_string);
            }
            if cwd.is_none() {
                cwd = p.get("cwd").and_then(Value::as_str).map(str::to_string);
            }
        } else if kind == Some("response_item")
            && p.get("type").and_then(Value::as_str) == Some("message")
        {
            let role = match p.get("role").and_then(Value::as_str) {
                Some("user") => Role::User,
                Some("assistant") => Role::Assistant,
                _ => continue,
            };
            let t = text_of(p.get("content"));
            // Codex can record the same prompt twice in a row (raw and after context injection).
            if turns
                .last()
                .is_some_and(|prev| prev.role == role && prev.text == t)
            {
                continue;
            }
            let keep = if role == Role::Assistant {
                !t.is_empty()
            } else {
                is_real_prompt(&t)
            };
            if keep {
                turns.push(Turn { role, text: t });
            }
        }
    }
    (id, cwd, turns)
}

fn list_codex(spec: &ProviderSpec) -> Vec<SessionInfo> {
    let mut files = vec![];
    walk(
        &spec.shared_home.join("sessions"),
        &|n| n.starts_with("rollout-") && n.ends_with(".jsonl"),
        usize::MAX,
        &mut files,
    );
    let mut buf = Vec::with_capacity(HEAD_BYTES as usize);
    files
        .into_iter()
        .filter_map(|file| {
            let (id, cwd, title) = scan_head(&file, &mut buf, |text| {
                let (id, cwd, turns) = codex_turns(text, true);
                Some((id?, cwd, one_line(&first_user(&turns)?.text, 80)))
            })?;
            Some(SessionInfo {
                provider: spec.id.clone(),
                id,
                cwd,
                title,
                updated_at: mtime_ms(&file),
                file,
            })
        })
        .collect()
}

// ---- Gemini: <home>/tmp/<project-hash-or-name>/chats/session-*.json

fn gemini_project_dirs(spec: &ProviderSpec) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let Ok(bytes) = fs::read(spec.shared_home.join("projects.json")) else {
        return map;
    };
    let Ok(data) = serde_json::from_slice::<Value>(&bytes) else {
        return map;
    };
    if let Some(projects) = data.get("projects").and_then(Value::as_object) {
        for (cwd, short) in projects {
            if let Some(short) = short.as_str() {
                map.insert(short.to_string(), cwd.clone());
            }
            map.insert(format!("{:x}", Sha256::digest(cwd.as_bytes())), cwd.clone());
        }
    }
    map
}

fn gemini_turns(data: &Value) -> Vec<Turn> {
    let Some(messages) = data.get("messages").and_then(Value::as_array) else {
        return vec![];
    };
    messages
        .iter()
        .filter_map(|m| {
            let t = text_of(m.get("content"));
            match m.get("type").and_then(Value::as_str) {
                Some("user") if is_real_prompt(&t) => Some(Turn {
                    role: Role::User,
                    text: t,
                }),
                Some("gemini") if !t.is_empty() => Some(Turn {
                    role: Role::Assistant,
                    text: t,
                }),
                _ => None,
            }
        })
        .collect()
}

fn list_gemini(spec: &ProviderSpec) -> Vec<SessionInfo> {
    let dirs = gemini_project_dirs(spec);
    let mut files = vec![];
    walk(
        &spec.shared_home.join("tmp"),
        &|n| n.starts_with("session-") && n.ends_with(".json"),
        usize::MAX,
        &mut files,
    );
    files
        .into_iter()
        .filter_map(|file| {
            let data: Value = serde_json::from_slice(&fs::read(&file).ok()?).ok()?;
            let id = data
                .get("sessionId")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())?
                .to_string();
            let turns = gemini_turns(&data);
            let first = first_user(&turns)?;
            let project_key = file
                .parent()?
                .parent()?
                .file_name()?
                .to_string_lossy()
                .into_owned();
            let updated_at = data
                .get("lastUpdated")
                .and_then(Value::as_str)
                .and_then(time::parse_iso)
                .unwrap_or_else(|| mtime_ms(&file));
            Some(SessionInfo {
                provider: spec.id.clone(),
                id,
                cwd: dirs.get(&project_key).cloned(),
                title: one_line(&first.text, 80),
                updated_at,
                file,
            })
        })
        .collect()
}

/// Lists a provider's sessions. With `id_prefix`, providers that name files by session id
/// skip the others without reading them; callers still filter the result by id.
/// A session id amux will pass to a CLI (`--resume {id}`): a letter or digit, then
/// letters, digits, `_` and `-`. Anything else (an id starting with `-` would read as an
/// option) comes from a file someone else could have written, and is left out.
pub fn valid_id(id: &str) -> bool {
    let mut c = id.chars();
    c.next().is_some_and(|f| f.is_ascii_alphanumeric())
        && c.all(|x| x.is_ascii_alphanumeric() || x == '_' || x == '-')
}

/// The CLI's sessions; those whose id isn't `valid_id` are left out.
pub fn list_sessions(spec: &ProviderSpec, id_prefix: Option<&str>) -> Vec<SessionInfo> {
    let all = match spec.sessions.as_deref() {
        Some("claude") => list_claude(spec, id_prefix),
        Some("codex") => list_codex(spec),
        Some("gemini") => list_gemini(spec),
        _ => vec![],
    };
    all.into_iter().filter(|s| valid_id(&s.id)).collect()
}

pub fn read_turns(s: &SessionInfo, format: Option<&str>) -> anyhow::Result<Vec<Turn>> {
    // Lossy, so one bad byte in a long transcript doesn't make the whole session unreadable.
    let text = String::from_utf8_lossy(&fs::read(&s.file)?).into_owned();
    Ok(match format {
        Some("gemini") => gemini_turns(&serde_json::from_str(&text)?),
        Some("codex") => codex_turns(&text, false).2,
        _ => claude_turns(&text, false).1,
    })
}

fn heading(role: Role) -> &'static str {
    match role {
        Role::User => "User",
        Role::Assistant => "Assistant",
    }
}

/// Builds a Markdown handoff for continuing a session in another provider:
/// the original request, then the most recent turns that fit in the budget.
pub fn build_handoff(s: &SessionInfo, turns: &[Turn], budget: usize) -> String {
    let mut out = vec![
        format!("# Handoff from {} session {}", s.provider, s.id),
        String::new(),
        format!(
            "- Working directory: {}",
            s.cwd.as_deref().unwrap_or("unknown")
        ),
        format!("- Last active: {}", time::iso(s.updated_at)),
        format!("- Source transcript: {}", s.file.display()),
        String::new(),
        "You are continuing work started in another agent session. Read the transcript below,"
            .into(),
        "check the current state of the working directory, then continue where it left off.".into(),
        String::new(),
    ];
    let fmt = |t: &Turn| format!("## {}\n\n{}\n", heading(t.role), t.text.trim());
    let first = turns.iter().position(|t| t.role == Role::User);
    let mut body: Vec<String> = vec![];
    let mut used = 0usize;
    for i in (0..turns.len()).rev() {
        if Some(i) == first {
            break;
        }
        let block = fmt(&turns[i]);
        let len = block.chars().count();
        if used + len > budget {
            // Keep the tail of an oversized turn rather than dropping it entirely.
            let room = budget - used;
            if room > 1000 {
                let text = turns[i].text.trim();
                let n = text.chars().count();
                let tail: String = text.chars().skip(n.saturating_sub(room)).collect();
                body.insert(0, format!("## {}\n\n…{tail}\n", heading(turns[i].role)));
            }
            body.insert(0, "_…earlier turns omitted…_\n".into());
            break;
        }
        body.insert(0, block);
        used += len;
    }
    out.push("# Transcript".into());
    out.push(String::new());
    if let Some(i) = first {
        out.push(fmt(&turns[i]));
    }
    out.extend(body);
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{jsonl, spec, write, TempDir};
    use serde_json::json;

    fn session_spec(format: &str, home: &Path) -> ProviderSpec {
        let mut s = spec(format, home);
        s.sessions = Some(format.into());
        s
    }

    fn turn(role: Role, text: &str) -> Turn {
        Turn {
            role,
            text: text.into(),
        }
    }

    #[test]
    fn lists_claude_sessions_skipping_wrappers_meta_and_subagents() {
        let home = TempDir::new();
        write(
            &home.path().join("projects/-work-app/11111111-aaaa.jsonl"),
            &jsonl(&[
                json!({"type": "user", "cwd": "/work/app", "message": {"role": "user", "content": "<command-name>/clear</command-name>"}}),
                json!({"type": "user", "cwd": "/work/app", "message": {"role": "user", "content": "Fix the login bug"}}),
                json!({"type": "assistant", "message": {"content": [{"type": "text", "text": "Looking at auth.ts"}, {"type": "tool_use", "name": "Read"}]}}),
                json!({"type": "user", "isMeta": true, "message": {"role": "user", "content": "meta"}}),
            ]),
        );
        write(
            &home.path().join("projects/-work-app/subagents/x.jsonl"),
            &jsonl(&[json!({"type": "user", "message": {"role": "user", "content": "sub"}})]),
        );
        let list = list_sessions(&session_spec("claude", home.path()), None);
        assert_eq!(list.len(), 1);
        let s = &list[0];
        assert_eq!(s.id, "11111111-aaaa");
        assert_eq!(s.cwd.as_deref(), Some("/work/app"));
        assert_eq!(s.title, "Fix the login bug");
        assert_eq!(
            read_turns(s, Some("claude")).unwrap(),
            [
                turn(Role::User, "Fix the login bug"),
                turn(Role::Assistant, "Looking at auth.ts")
            ]
        );
    }

    #[test]
    fn finds_a_first_prompt_past_the_first_read() {
        let home = TempDir::new();
        let filler = "x".repeat(40 * 1024);
        write(
            &home.path().join("projects/-p/late.jsonl"),
            &jsonl(&[
                json!({"type": "user", "cwd": "/p", "message": {"role": "user", "content": format!("<context>{filler}</context>")}}),
                json!({"type": "user", "cwd": "/p", "message": {"role": "user", "content": "The real ask"}}),
            ]),
        );
        let list = list_sessions(&session_spec("claude", home.path()), None);
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].title, "The real ask");
    }

    #[test]
    fn lists_codex_sessions_dropping_injected_context_and_duplicates() {
        let home = TempDir::new();
        let prompt = json!({"type": "response_item", "payload": {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Add tests"}]}});
        write(
            &home.path().join("sessions/2026/01/02/rollout-x.jsonl"),
            &jsonl(&[
                json!({"type": "session_meta", "payload": {"id": "codex-1", "cwd": "/work/api"}}),
                json!({"type": "response_item", "payload": {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "# AGENTS.md\nrules"}]}}),
                prompt.clone(),
                prompt,
                json!({"type": "response_item", "payload": {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "Done"}]}}),
            ]),
        );
        // A session whose id is not a string is skipped instead of breaking the list.
        write(
            &home.path().join("sessions/2026/01/03/rollout-y.jsonl"),
            &jsonl(&[
                json!({"type": "session_meta", "payload": {"id": 123}}),
                json!({"type": "response_item", "payload": {"type": "message", "role": "user", "content": "hi"}}),
            ]),
        );
        let list = list_sessions(&session_spec("codex", home.path()), None);
        assert_eq!(list.len(), 1);
        let s = &list[0];
        assert_eq!(
            (s.id.as_str(), s.cwd.as_deref(), s.title.as_str()),
            ("codex-1", Some("/work/api"), "Add tests")
        );
        assert_eq!(
            read_turns(s, Some("codex")).unwrap(),
            [turn(Role::User, "Add tests"), turn(Role::Assistant, "Done")]
        );
    }

    #[test]
    fn lists_gemini_sessions_with_their_project_dir() {
        let home = TempDir::new();
        write(
            &home.path().join("projects.json"),
            &json!({"projects": {"/work/web": "web"}}).to_string(),
        );
        write(
            &home.path().join("tmp/web/chats/session-1.json"),
            &json!({
                "sessionId": "gem-1",
                "lastUpdated": "2026-03-04T05:06:07.000Z",
                "messages": [{"type": "user", "content": "Style the page"}, {"type": "gemini", "content": "Sure"}]
            })
            .to_string(),
        );
        let list = list_sessions(&session_spec("gemini", home.path()), None);
        assert_eq!(list.len(), 1);
        let s = &list[0];
        assert_eq!(
            (s.id.as_str(), s.cwd.as_deref(), s.title.as_str()),
            ("gem-1", Some("/work/web"), "Style the page")
        );
        assert_eq!(time::iso(s.updated_at), "2026-03-04T05:06:07.000Z");
    }

    #[test]
    fn accepts_only_plain_session_ids() {
        for ok in ["0199a2b3-4c5d-7e8f-9a0b-1c2d3e4f5a6b", "abc_DEF-1", "7"] {
            assert!(valid_id(ok), "{ok}");
        }
        for bad in [
            "",
            "-c",
            "--dangerously-skip-permissions",
            "a b",
            "a;b",
            "../x",
            "a.b",
            "é",
        ] {
            assert!(!valid_id(bad), "{bad}");
        }
    }

    fn info() -> SessionInfo {
        SessionInfo {
            provider: "codex".into(),
            id: "codex-1".into(),
            cwd: Some("/work/api".into()),
            title: "t".into(),
            updated_at: 0,
            file: "/f.jsonl".into(),
        }
    }

    #[test]
    fn handoff_keeps_the_first_prompt_and_recent_turns() {
        let mut turns = vec![turn(Role::User, "ORIGINAL REQUEST")];
        for i in 0..50 {
            turns.push(turn(
                if i % 2 == 1 {
                    Role::User
                } else {
                    Role::Assistant
                },
                &format!("turn {i} {}", "x".repeat(200)),
            ));
        }
        let out = build_handoff(&info(), &turns, 2_000);
        assert!(out.contains("ORIGINAL REQUEST"));
        assert!(out.contains("earlier turns omitted"));
        assert!(out.contains("turn 49 "));
        assert!(!out.contains("turn 0 "));
    }

    #[test]
    fn handoff_keeps_the_tail_of_an_oversized_turn() {
        let out = build_handoff(
            &info(),
            &[
                turn(Role::User, "ask"),
                turn(Role::Assistant, &("a".repeat(10_000) + "THE END")),
            ],
            3_000,
        );
        assert!(out.contains("THE END"));
    }
}
