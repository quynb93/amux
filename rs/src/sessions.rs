//! Reads each CLI's session files for listing, and builds cross-CLI handoffs.

use crate::providers::ProviderSpec;
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

/// The first 256 KB of a transcript: enough for the metadata and first prompt.
fn read_head(file: &Path, buf: &mut Vec<u8>) -> String {
    buf.clear();
    if let Ok(f) = fs::File::open(file) {
        let _ = f.take(HEAD_BYTES).read_to_end(buf);
    }
    String::from_utf8_lossy(buf).into_owned()
}

fn json_lines(text: &str) -> impl Iterator<Item = Value> + '_ {
    // Skips blank lines, a partial last line from a head read, and corrupt lines.
    text.split('\n')
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
}

/// Collects matching files under `dir`, descending at most `max_depth` directory levels.
fn walk(dir: &Path, matches: &dyn Fn(&str) -> bool, max_depth: usize, out: &mut Vec<PathBuf>) {
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

fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) | Some(Value::Bool(false)) => false,
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0),
        _ => true,
    }
}

fn mtime_ms(file: &Path) -> i64 {
    fs::metadata(file)
        .and_then(|m| m.modified())
        .map(time::to_ms)
        .unwrap_or(0)
}

fn first_user(turns: &[Turn]) -> Option<&Turn> {
    turns.iter().find(|t| t.role == Role::User)
}

// ---- Claude Code: <home>/projects/<cwd-slug>/<session-id>.jsonl

fn claude_turns(text: &str) -> (Option<String>, Vec<Turn>) {
    let mut cwd: Option<String> = None;
    let mut turns = vec![];
    for rec in json_lines(text) {
        if cwd.is_none() {
            cwd = rec.get("cwd").and_then(Value::as_str).map(str::to_string);
        }
        if truthy(rec.get("isSidechain")) || truthy(rec.get("isMeta")) {
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

fn list_claude(spec: &ProviderSpec) -> Vec<SessionInfo> {
    // Sessions sit at projects/<cwd-slug>/<id>.jsonl; deeper dirs hold subagent transcripts and tool output.
    let mut files = vec![];
    walk(
        &spec.shared_home.join("projects"),
        &|n| n.ends_with(".jsonl"),
        1,
        &mut files,
    );
    let mut buf = Vec::with_capacity(HEAD_BYTES as usize);
    files
        .into_iter()
        .filter_map(|file| {
            let (cwd, turns) = claude_turns(&read_head(&file, &mut buf));
            let first = first_user(&turns)?;
            Some(SessionInfo {
                provider: spec.id.clone(),
                id: file.file_stem()?.to_string_lossy().into_owned(),
                cwd,
                title: one_line(&first.text, 80),
                updated_at: mtime_ms(&file),
                file,
            })
        })
        .collect()
}

// ---- Codex: <home>/sessions/YYYY/MM/DD/rollout-*.jsonl

fn codex_turns(text: &str) -> (Option<Value>, Option<String>, Vec<Turn>) {
    let mut id: Option<Value> = None;
    let mut cwd: Option<String> = None;
    let mut turns: Vec<Turn> = vec![];
    let empty = Value::Object(Default::default());
    for rec in json_lines(text) {
        let p = rec
            .get("payload")
            .filter(|v| !v.is_null())
            .unwrap_or(&empty);
        let kind = rec.get("type").and_then(Value::as_str);
        if kind == Some("session_meta") {
            if id.as_ref().is_none_or(Value::is_null) {
                id = p
                    .get("id")
                    .filter(|v| !v.is_null())
                    .or_else(|| p.get("session_id"))
                    .cloned();
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
            let (id, cwd, turns) = codex_turns(&read_head(&file, &mut buf));
            let id = id
                .as_ref()
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())?
                .to_string();
            let first = first_user(&turns)?;
            Some(SessionInfo {
                provider: spec.id.clone(),
                id,
                cwd,
                title: one_line(&first.text, 80),
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

pub fn list_sessions(spec: &ProviderSpec) -> Vec<SessionInfo> {
    match spec.sessions.as_deref() {
        Some("claude") => list_claude(spec),
        Some("codex") => list_codex(spec),
        Some("gemini") => list_gemini(spec),
        _ => vec![],
    }
}

pub fn read_turns(s: &SessionInfo, format: Option<&str>) -> anyhow::Result<Vec<Turn>> {
    let text = fs::read_to_string(&s.file)?;
    Ok(match format {
        Some("gemini") => gemini_turns(&serde_json::from_str(&text)?),
        Some("codex") => codex_turns(&text).2,
        _ => claude_turns(&text).1,
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
