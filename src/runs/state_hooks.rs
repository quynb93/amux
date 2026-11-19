//! State hooks: exact run state from CLIs that don't report it with OSC 7501.
//!
//! `amux state-hooks install` adds entries to the hooks config in a CLI's shared home
//! (Claude Code's and Gemini CLI's `settings.json`). Each runs `amux __hook <cli> <event>`,
//! which reads the hook's JSON on stdin and, inside a run (AMUX_RUN set), sends the state it
//! means to the run's host in one `STATE` frame. Outside a run it exits at once.
//!
//! A hook must never change what the CLI does: `__hook` always exits 0 and prints nothing,
//! on stdout or stderr (Gemini CLI reads stderr as the hook's answer when stdout is empty).
//! It gives up after 2 seconds whatever happens, and the entries carry a 5-second timeout.
//!
//! Entries are recognized by their command: it runs amux with `__hook`. `remove` takes out
//! exactly those and leaves everything else in the file as it was.

use super::{client, load, write_frame, STATE};
use crate::{fsx, providers::ProviderSpec};
use anyhow::{bail, Context, Result};
use serde_json::{json, Map, Value};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// A CLI that state hooks support, and the hooks it gets.
pub struct Cli {
    /// The provider id: claude or gemini.
    pub id: &'static str,
    /// The config file, relative to the shared home.
    pub file: &'static str,
    /// (event, matcher)
    pub events: &'static [(&'static str, Option<&'static str>)],
    /// What the hook entry's timeout is in: seconds (Claude Code) or ms (Gemini CLI).
    pub timeout: u64,
}

/// Claude Code: https://code.claude.com/docs/en/hooks
/// Gemini CLI: https://geminicli.com/docs/hooks/reference/ (hooks are on by default,
/// `hooksConfig.enabled`).
/// Codex isn't here: it reports its state with OSC 7501 on any terminal, its hooks must be
/// trusted one by one in `/hooks`, and `notify` has room for one program only.
pub const CLIS: &[Cli] = &[
    Cli {
        id: "claude",
        file: "settings.json",
        events: &[
            ("UserPromptSubmit", None),
            ("PreToolUse", None),
            ("PostToolUse", None),
            ("PostToolUseFailure", None),
            ("PermissionRequest", None),
            ("Notification", Some("permission_prompt|elicitation_dialog")),
            ("Stop", None),
            ("StopFailure", None),
        ],
        timeout: 5,
    },
    Cli {
        id: "gemini",
        file: "settings.json",
        events: &[
            ("BeforeAgent", None),
            ("BeforeTool", None),
            ("AfterTool", None),
            ("Notification", None),
            ("AfterAgent", None),
        ],
        timeout: 5000,
    },
];

pub fn cli(id: &str) -> Option<&'static Cli> {
    CLIS.iter().find(|c| c.id == id)
}

// ---------------------------------------------------------------------------------------
// `amux __hook <cli> <event>`

/// Runs as a CLI's hook. Always 0, never any output.
pub fn hook_main(args: &[String]) -> i32 {
    let Some(run) = std::env::var("AMUX_RUN").ok().filter(|r| !r.is_empty()) else {
        return 0;
    };
    // Never hold up the CLI, whatever stalls below.
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(2));
        std::process::exit(0);
    });
    let (Some(cli), Some(event)) = (args.first(), args.get(1)) else {
        return 0;
    };
    let mut input = Vec::new();
    let _ = std::io::stdin().take(1 << 20).read_to_end(&mut input);
    let input: Value = serde_json::from_slice(&input).unwrap_or(Value::Null);
    let Some(report) = map(cli, event, &input) else {
        return 0;
    };
    if from_the_run(&run) {
        let _ = send(&run, &report);
    }
    0
}

/// Whether this hook comes from the run's own CLI, not from another CLI the agent started
/// inside the run (which inherits AMUX_RUN). The CLI is at most a shell and a wrapper
/// script up from the hook.
fn from_the_run(run: &str) -> bool {
    let Some(pid) = load(run).ok().and_then(|r| r.pid) else {
        return false;
    };
    let mut p = std::os::unix::process::parent_id();
    for _ in 0..3 {
        if p == pid {
            return true;
        }
        match super::peer::parent_of(p) {
            Some(pp) if pp > 1 => p = pp,
            // Can't tell (or not on this system): the host would refuse it anyway.
            _ => return false,
        }
    }
    false
}

fn send(run: &str, report: &Value) -> std::io::Result<()> {
    let mut s = client::connect(run)?;
    s.set_write_timeout(Some(Duration::from_millis(500)))?;
    write_frame(&mut s, STATE, report.to_string().as_bytes())?;
    // Stay until the host has read it: it checks who sent it (runs/peer.rs), which it
    // can't once this process is gone.
    let _ = s.shutdown(std::net::Shutdown::Write);
    s.set_read_timeout(Some(Duration::from_millis(500)))?;
    let _ = std::io::Read::read(&mut s, &mut [0u8; 1]);
    Ok(())
}

/// The state a hook event means, as a `STATE` payload. None: the event says nothing
/// about the state (or isn't one amux installed).
///
/// Claude Code's tool events carry `"tool"` (see `tool_key`), so a block on one tool call
/// isn't ended by another one (a parallel call) starting or finishing. Tool events from a
/// subagent (their input has `agent_id`) say nothing about the main agent's state: they are
/// ignored unless they block.
pub fn map(cli: &str, event: &str, input: &Value) -> Option<Value> {
    let s = |k: &str| input.get(k).and_then(Value::as_str).unwrap_or("");
    let working = || json!({ "state": "working" });
    let blocked =
        |kind: &str, msg: String| json!({ "state": "blocked", "kind": kind, "message": msg });
    let subagent = !s("agent_id").is_empty();
    let with_tool = |mut v: Value| {
        if let Some(t) = tool_key(input) {
            v["tool"] = json!(t);
        }
        v
    };
    match (cli, event) {
        ("claude", "UserPromptSubmit") => Some(working()),
        ("claude", "PostToolUse" | "PostToolUseFailure") => {
            (!subagent).then(|| with_tool(working()))
        }
        ("claude", "PreToolUse") => match s("tool_name") {
            // These tools ask you something themselves, with no permission prompt.
            "AskUserQuestion" => {
                let q = input
                    .pointer("/tool_input/questions/0/question")
                    .and_then(Value::as_str)
                    .unwrap_or("a question");
                Some(with_tool(blocked("question", q.to_string())))
            }
            "ExitPlanMode" => Some(with_tool(blocked("permission", "approve the plan".into()))),
            _ => (!subagent).then(|| with_tool(working())),
        },
        ("claude", "PermissionRequest") => {
            let mut v = with_tool(blocked("permission", permission(input)));
            if let Some(ask) = exact_ask(input) {
                v["ask"] = ask;
            }
            Some(v)
        }
        ("claude", "Notification") => match s("notification_type") {
            // Follows the PermissionRequest hook, some seconds after the prompt showed.
            "permission_prompt" => {
                let mut v = blocked("permission", s("message").into());
                v["echo"] = json!(true);
                Some(v)
            }
            "elicitation_dialog" => Some(blocked("question", s("message").into())),
            _ => None,
        },
        ("claude", "Stop") => Some(json!({ "state": "idle" })),
        ("claude", "StopFailure") => {
            let msg = ["error", "error_message", "message"]
                .iter()
                .find_map(|k| input.get(*k).and_then(Value::as_str))
                .unwrap_or("the turn ended with an error");
            Some(json!({ "state": "error", "message": first_line(msg) }))
        }
        ("gemini", "BeforeAgent" | "BeforeTool" | "AfterTool") => Some(working()),
        ("gemini", "Notification") => (s("notification_type") == "ToolPermission")
            .then(|| blocked("permission", s("message").into())),
        ("gemini", "AfterAgent") => Some(json!({ "state": "idle" })),
        _ => None,
    }
}

/// Names a tool call the same way in every event about it: "Bash:<hash of its input>".
/// PermissionRequest has no `tool_use_id`, so the input is what ties it to the
/// PostToolUse that follows.
fn tool_key(input: &Value) -> Option<String> {
    let name = input.get("tool_name").and_then(Value::as_str)?;
    let args = input
        .get("tool_input")
        .map(Value::to_string)
        .unwrap_or_default();
    // FNV-1a: the same in every amux process, unlike std's hasher.
    let hash = args.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    });
    Some(format!("{name}:{hash:016x}"))
}

fn first_line(s: &str) -> String {
    let l = s
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    if l.chars().count() > 200 {
        format!("{}…", l.chars().take(199).collect::<String>())
    } else {
        l.to_string()
    }
}

/// What a PermissionRequest asks, exactly as the CLI will run it, for auto-approve to check
/// the prompt on screen against: `{"kind": "bash", "subject": <the command>}`, for a file
/// tool `{"kind": "edit", "subject": <the path>}`, and for any other tool (a web fetch, an
/// MCP tool) `{"kind": "other", "subject": <the tool>}`, so a prompt for it on screen isn't
/// taken for one the hook didn't report. The screen can show text the agent wrote; this
/// can't.
fn exact_ask(input: &Value) -> Option<Value> {
    let tool = input.get("tool_name").and_then(Value::as_str)?;
    let ti = |k: &str| {
        input
            .pointer(&format!("/tool_input/{k}"))
            .and_then(Value::as_str)
    };
    let (kind, subject) = match tool {
        "Bash" => ("bash", ti("command")?),
        "Write" | "Edit" | "MultiEdit" => ("edit", ti("file_path")?),
        "NotebookEdit" => ("edit", ti("notebook_path")?),
        _ => ("other", tool),
    };
    // A command too long to send whole is one auto-approve won't match: it then waits.
    (subject.len() <= 16 * 1024).then(|| json!({ "kind": kind, "subject": subject }))
}

/// "approve Bash: cargo test", like Claude Code's own OSC 7501 messages.
fn permission(input: &Value) -> String {
    let tool = input.get("tool_name").and_then(Value::as_str).unwrap_or("");
    if tool.is_empty() {
        return "permission request".into();
    }
    let ti = |k: &str| {
        input
            .pointer(&format!("/tool_input/{k}"))
            .and_then(Value::as_str)
    };
    let detail = ti("command")
        .or_else(|| ti("file_path"))
        .or_else(|| ti("notebook_path"))
        .or_else(|| ti("url"))
        .or_else(|| ti("pattern"))
        .map(|d| {
            let cwd = input.get("cwd").and_then(Value::as_str).unwrap_or("");
            let rel = Path::new(d)
                .strip_prefix(cwd)
                .ok()
                .filter(|_| !cwd.is_empty())
                .map(|p| p.to_string_lossy().into_owned());
            first_line(&rel.unwrap_or_else(|| d.to_string()))
        });
    match detail {
        Some(d) if !d.is_empty() => format!("approve {tool}: {d}"),
        _ => format!("approve {tool}"),
    }
}

// ---------------------------------------------------------------------------------------
// Installing

/// Whether a hook command is one of ours: it runs amux with `__hook <cli>`. The program
/// may be any path (amux renamed, or installed somewhere without "amux" in it).
pub fn is_ours(command: &str) -> bool {
    CLIS.iter()
        .any(|c| command.contains(&format!(" __hook {} ", c.id)))
}

/// Quotes a path for the shell that runs hook commands, when it needs quoting.
pub fn sh_quote(s: &str) -> String {
    if s.bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"/._-+@".contains(&b))
    {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

pub fn command(program: &str, cli: &Cli, event: &str) -> String {
    format!("{program} __hook {} {event}", cli.id)
}

/// Takes our entries out of a settings object. Groups and events left empty by that go
/// too, and so does `hooks` if it is left empty. Returns how many entries it removed.
pub fn strip(settings: &mut Value) -> usize {
    let Some(hooks) = settings.get_mut("hooks").and_then(Value::as_object_mut) else {
        return 0;
    };
    let mut removed = 0;
    let mut emptied = vec![];
    for (event, groups) in hooks.iter_mut() {
        let Some(groups) = groups.as_array_mut() else {
            continue;
        };
        let before = removed;
        groups.retain_mut(|g| {
            let Some(list) = g.get_mut("hooks").and_then(Value::as_array_mut) else {
                return true;
            };
            let n = list.len();
            list.retain(|h| {
                !h.get("command")
                    .and_then(Value::as_str)
                    .is_some_and(is_ours)
            });
            removed += n - list.len();
            !(n > list.len() && list.is_empty())
        });
        if removed > before && groups.is_empty() {
            emptied.push(event.clone());
        }
    }
    for e in emptied {
        hooks.remove(&e);
    }
    if removed > 0 && hooks.is_empty() {
        if let Some(o) = settings.as_object_mut() {
            o.remove("hooks");
        }
    }
    removed
}

/// Adds our entries for `cli` to a settings object (after taking out old ones, so an
/// install twice leaves one set).
pub fn add(settings: &mut Value, cli: &Cli, program: &str) -> Result<()> {
    strip(settings);
    let Some(obj) = settings.as_object_mut() else {
        bail!("it isn't a JSON object");
    };
    let hooks = obj
        .entry("hooks")
        .or_insert_with(|| Value::Object(Map::new()));
    let Some(hooks) = hooks.as_object_mut() else {
        bail!("its \"hooks\" isn't a JSON object");
    };
    for (event, matcher) in cli.events {
        let mut handler = json!({
            "type": "command",
            "command": command(program, cli, event),
            "timeout": cli.timeout,
        });
        if cli.id == "gemini" {
            handler["name"] = json!("amux-state");
        }
        let mut group = Map::new();
        if let Some(m) = matcher {
            group.insert("matcher".into(), json!(m));
        }
        group.insert("hooks".into(), json!([handler]));
        let list = hooks
            .entry(event.to_string())
            .or_insert_with(|| Value::Array(vec![]));
        let Some(list) = list.as_array_mut() else {
            bail!("its \"hooks\".\"{event}\" isn't a list");
        };
        list.push(Value::Object(group));
    }
    Ok(())
}

/// Which of `cli`'s events have our entry in a settings object.
pub fn installed_events(settings: &Value, cli: &Cli) -> Vec<&'static str> {
    cli.events
        .iter()
        .map(|(e, _)| *e)
        .filter(|e| {
            settings
                .pointer(&format!("/hooks/{e}"))
                .and_then(Value::as_array)
                .is_some_and(|groups| {
                    groups.iter().any(|g| {
                        g.get("hooks").and_then(Value::as_array).is_some_and(|l| {
                            l.iter().any(|h| {
                                h.get("command")
                                    .and_then(Value::as_str)
                                    .is_some_and(is_ours)
                            })
                        })
                    })
                })
        })
        .collect()
}

/// Where `cli`'s hooks config lives for this provider.
pub fn config_path(spec: &ProviderSpec, cli: &Cli) -> PathBuf {
    spec.shared_home.join(cli.file)
}

/// The settings file, parsed. A missing file is an empty object.
pub fn read_settings(path: &Path) -> Result<Value> {
    match std::fs::read_to_string(path) {
        Ok(t) if t.trim().is_empty() => Ok(json!({})),
        Ok(t) => serde_json::from_str(&t).with_context(|| {
            format!(
                "{} isn't plain JSON (comments or trailing commas?), so amux won't rewrite it",
                crate::paths::tildify(path)
            )
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(json!({})),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// Writes the settings back atomically, through a symlink to where it points (a dotfiles
/// repo, say), keeping the file's mode.
pub fn write_settings(path: &Path, settings: &Value) -> Result<()> {
    let mut text = serde_json::to_string_pretty(settings)?;
    text.push('\n');
    write_config_text(path, &text)
}

/// Writes a config file atomically, through a symlink to its target, keeping its mode.
pub fn write_config_text(path: &Path, text: &str) -> Result<()> {
    let target = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let mode = std::fs::metadata(&target).ok().map(|m| m.permissions());
    fsx::write_atomic(&target, text.as_bytes())
        .with_context(|| format!("writing {}", target.display()))?;
    if let Some(mode) = mode {
        let _ = std::fs::set_permissions(&target, mode);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claude() -> &'static Cli {
        cli("claude").unwrap()
    }

    #[test]
    fn maps_claude_events() {
        let st = |e: &str, v: Value| map("claude", e, &v).map(|r| r["state"].clone());
        assert_eq!(st("UserPromptSubmit", json!({})), Some(json!("working")));
        assert_eq!(st("Stop", json!({})), Some(json!("idle")));
        let perm = map(
            "claude",
            "PermissionRequest",
            &json!({"cwd": "/w", "tool_name": "Write", "tool_input": {"file_path": "/w/src/a.rs"}}),
        )
        .unwrap();
        assert_eq!(perm["state"], "blocked");
        assert_eq!(perm["message"], "approve Write: src/a.rs");
        let bash = map(
            "claude",
            "PermissionRequest",
            &json!({"tool_name": "Bash", "tool_input": {"command": "cargo test\necho"}}),
        )
        .unwrap();
        assert_eq!(bash["message"], "approve Bash: cargo test");
        // The exact request, for auto-approve to check the screen against.
        assert_eq!(
            bash["ask"],
            json!({"kind": "bash", "subject": "cargo test\necho"})
        );
        assert_eq!(
            perm["ask"],
            json!({"kind": "edit", "subject": "/w/src/a.rs"})
        );
        assert_eq!(
            crate::runs::program::ask_from_hook(bash.to_string().as_bytes()),
            Some(("bash".into(), "cargo test\necho".into()))
        );
        // Any other tool is reported by name, so its prompt can be told from a forged one.
        let fetch = map(
            "claude",
            "PermissionRequest",
            &json!({"tool_name": "WebFetch", "tool_input": {"url": "https://x.example"}}),
        )
        .unwrap();
        assert_eq!(
            fetch["ask"],
            json!({"kind": "other", "subject": "WebFetch"})
        );
        let q = map(
            "claude",
            "PreToolUse",
            &json!({"tool_name": "AskUserQuestion", "tool_input": {"questions": [{"question": "Which db?"}]}}),
        )
        .unwrap();
        assert_eq!(
            (q["kind"].as_str(), q["message"].as_str()),
            (Some("question"), Some("Which db?"))
        );
        let n = |t: &str| {
            map(
                "claude",
                "Notification",
                &json!({"notification_type": t, "message": "Claude needs your permission"}),
            )
        };
        assert_eq!(n("permission_prompt").unwrap()["state"], "blocked");
        assert!(n("idle_prompt").is_none());
        assert_eq!(
            map(
                "claude",
                "StopFailure",
                &json!({"error": "rate limited\nmore"})
            )
            .unwrap()["message"],
            "rate limited"
        );
        assert!(map("claude", "SessionStart", &json!({})).is_none());
        // A permission notification repeats the PermissionRequest block.
        assert_eq!(n("permission_prompt").unwrap()["echo"], true);
        assert!(perm.get("echo").is_none());
    }

    #[test]
    fn ties_tool_events_together_and_ignores_subagents() {
        let call = json!({"tool_name": "Bash", "tool_input": {"command": "cargo test"}});
        let other = json!({"tool_name": "Bash", "tool_input": {"command": "ls"}});
        let tool = |e: &str, v: &Value| map("claude", e, v).unwrap()["tool"].clone();
        assert!(tool("PermissionRequest", &call).is_string());
        assert_eq!(tool("PermissionRequest", &call), tool("PostToolUse", &call));
        assert_eq!(tool("PreToolUse", &call), tool("PostToolUseFailure", &call));
        assert_ne!(
            tool("PermissionRequest", &call),
            tool("PostToolUse", &other)
        );
        assert!(map("claude", "UserPromptSubmit", &json!({}))
            .unwrap()
            .get("tool")
            .is_none());
        // A subagent's tool calls say nothing about the main agent's state...
        let mut sub = call.clone();
        sub["agent_id"] = json!("a4d2c8f1");
        sub["agent_type"] = json!("Explore");
        assert!(map("claude", "PreToolUse", &sub).is_none());
        assert!(map("claude", "PostToolUse", &sub).is_none());
        assert!(map("claude", "PostToolUseFailure", &sub).is_none());
        // ...unless it waits for you.
        assert_eq!(
            map("claude", "PermissionRequest", &sub).unwrap()["state"],
            "blocked"
        );
        // `--agent` sets agent_type alone: that is the main agent.
        let mut main = call.clone();
        main["agent_type"] = json!("reviewer");
        assert_eq!(
            map("claude", "PostToolUse", &main).unwrap()["state"],
            "working"
        );
        assert!(map("codex", "Stop", &json!({})).is_none());
    }

    #[test]
    fn maps_gemini_events() {
        let n = map(
            "gemini",
            "Notification",
            &json!({"notification_type": "ToolPermission", "message": "Allow shell: ls"}),
        )
        .unwrap();
        assert_eq!(
            (n["state"].as_str(), n["message"].as_str()),
            (Some("blocked"), Some("Allow shell: ls"))
        );
        assert_eq!(
            map("gemini", "AfterAgent", &json!({})).unwrap()["state"],
            "idle"
        );
        assert_eq!(
            map("gemini", "BeforeTool", &json!({})).unwrap()["state"],
            "working"
        );
    }

    #[test]
    fn install_and_remove_keep_everything_else() {
        let original = json!({
            "model": "opus",
            "hooks": {
                "PreToolUse": [
                    {"matcher": "Bash", "hooks": [{"type": "command", "command": "amux __slot-hook"}]}
                ],
                "SessionStart": [{"hooks": [{"type": "command", "command": "echo hi"}]}]
            },
            "permissions": {"allow": ["Bash(git:*)"]}
        });
        let mut s = original.clone();
        add(&mut s, claude(), "amux").unwrap();
        assert_eq!(installed_events(&s, claude()).len(), claude().events.len());
        // The user's PreToolUse hook is still first; ours follows it.
        assert_eq!(
            s["hooks"]["PreToolUse"][0],
            original["hooks"]["PreToolUse"][0]
        );
        assert_eq!(
            s["hooks"]["PreToolUse"][1]["hooks"][0]["command"],
            "amux __hook claude PreToolUse"
        );
        assert_eq!(
            s["hooks"]["Notification"][0]["matcher"],
            "permission_prompt|elicitation_dialog"
        );
        // Installing again replaces ours rather than adding a second set.
        add(&mut s, claude(), "/opt/amux").unwrap();
        assert_eq!(s["hooks"]["PreToolUse"].as_array().unwrap().len(), 2);
        assert_eq!(strip(&mut s), claude().events.len());
        assert_eq!(s, original);
        assert_eq!(strip(&mut s), 0);

        // From nothing and back to nothing.
        let mut empty = json!({});
        add(&mut empty, cli("gemini").unwrap(), "amux").unwrap();
        assert_eq!(empty["hooks"]["AfterAgent"][0]["hooks"][0]["timeout"], 5000);
        strip(&mut empty);
        assert_eq!(empty, json!({}));
        assert!(add(&mut json!([]), claude(), "amux").is_err());
        assert!(add(&mut json!({"hooks": []}), claude(), "amux").is_err());
    }

    #[test]
    fn recognizes_its_own_commands() {
        assert!(is_ours("amux __hook claude Stop"));
        assert!(is_ours("'/Users/me/my bin/amux' __hook gemini AfterAgent"));
        assert!(!is_ours("amux __slot-hook"));
        assert!(!is_ours("my-script __hook x"));
        assert!(is_ours("/opt/bin/am __hook claude Stop"));
        assert_eq!(sh_quote("/usr/local/bin/amux"), "/usr/local/bin/amux");
        assert_eq!(sh_quote("/a b/amux"), "'/a b/amux'");
    }

    #[test]
    fn writes_through_symlinks_and_keeps_the_mode() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("amux-sh-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let real = dir.join("real.json");
        std::fs::write(&real, "{\"a\": 1}").unwrap();
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o644)).unwrap();
        let link = dir.join("settings.json");
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let mut s = read_settings(&link).unwrap();
        add(&mut s, claude(), "amux").unwrap();
        write_settings(&link, &s).unwrap();
        assert!(std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(
            std::fs::metadata(&real).unwrap().permissions().mode() & 0o777,
            0o644
        );
        assert_eq!(read_settings(&real).unwrap()["a"], 1);
        assert!(read_settings(&dir.join("missing.json")).unwrap() == json!({}));
        std::fs::write(&real, "{ // comment\n}").unwrap();
        assert!(read_settings(&real).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
