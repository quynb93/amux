//! The MCP server's tools. Each is a thin wrapper over the CLI command of the same job:
//! it builds that command's arguments and runs it as its own `amux` process
//! (`super::run_amux`), so the rules and limits are the CLI's own (a lead's worker cap,
//! no workers of workers, slots, guards). `wait`, `list` and `report` return their text
//! directly and run alongside the others, so a long wait never holds up a peek.

use crate::commands;
use crate::runs;
use serde_json::{json, Map, Value};

/// Most text a tool returns; the rest is cut, with a note saying so.
const MAX_TEXT: usize = 60_000;
/// Longest `wait`, in seconds. Codex and Gemini CLI get a tool timeout above it.
pub const MAX_WAIT: u64 = 600;
const DEFAULT_WAIT: u64 = 300;

fn run_prop() -> Value {
    json!({ "type": "string", "minLength": 1, "description": "Run name, as in list (machine:run for another machine)" })
}

fn schema(props: Value, required: &[&str]) -> Value {
    json!({
        "type": "object",
        "properties": props,
        "required": required,
        "additionalProperties": false,
    })
}

/// name, title, description, input schema, read-only.
fn defs() -> Vec<(&'static str, &'static str, &'static str, Value, bool)> {
    vec![
        (
            "spawn",
            "Start an agent",
            "Start a coding agent in the background (a worker when called from inside an amux run). Give it a self-contained prompt; worktree: true gives it its own git worktree and branch. Returns the run name.",
            schema(
                json!({
                    "account": { "type": "string", "minLength": 1, "pattern": "^[^-]", "description": "provider or provider@name, e.g. claude, codex@work" },
                    "prompt": { "type": "string", "description": "The task: context, files, how to know it's done" },
                    "name": { "type": "string", "pattern": "^[A-Za-z0-9._-]+$", "description": "Run name (default: generated)" },
                    "worktree": { "type": "boolean", "description": "Own git worktree and branch (default false)" },
                    "branch": { "type": "string", "description": "Branch for the worktree (default amux/<run>)" },
                    "base": { "type": "string", "description": "Ref the worktree starts from (default HEAD)" },
                    "dir": { "type": "string", "description": "Folder to start in (default: the server's)" },
                    "issue": { "type": "string", "description": "GitHub issue number, linear:KEY or jira:KEY to work on" },
                    "autopilot": { "type": "string", "enum": ["on", "merge"], "description": "Open the PR when it commits and feed back CI and reviews; merge: also merge when ready" },
                    "args": { "type": "array", "items": { "type": "string" }, "description": "Extra arguments for the agent's CLI, e.g. [\"--permission-mode\", \"acceptEdits\"]" }
                }),
                &["account"],
            ),
            false,
        ),
        (
            "list",
            "List runs",
            "Every run: state (working, waiting, idle, done…), account, branch, what it waits for, last verify.",
            schema(
                json!({ "lead": { "type": "string", "description": "Only this lead's workers" } }),
                &[],
            ),
            true,
        ),
        (
            "wait",
            "Wait for workers",
            "Block until a worker finishes its turn, needs an answer or ends, and return its report (or its question and the end of its screen). Use instead of polling. Without runs: this run's team (or the only team). With runs: until all of them stop, or one needs an answer.",
            schema(
                json!({
                    "lead": { "type": "string", "description": "The team's lead (default: this run, or the only team)" },
                    "runs": { "type": "array", "items": { "type": "string" }, "minItems": 1, "description": "Wait for these runs instead of a team" },
                    "timeout": { "type": "integer", "minimum": 1, "maximum": MAX_WAIT, "description": "Seconds (default 300); on timeout, call again" }
                }),
                &[],
            ),
            true,
        ),
        (
            "peek",
            "Read a run's screen",
            "The run's terminal screen as text, now (or as it ended).",
            schema(
                json!({
                    "run": run_prop(),
                    "lines": { "type": "integer", "minimum": 1, "description": "Only the last n non-empty lines" }
                }),
                &["run"],
            ),
            true,
        ),
        (
            "send",
            "Type into a run",
            "Type text into a run's agent and press Enter (enter: false to only type), or press keys (e.g. down, enter, esc, 1). Read a menu with peek first.",
            schema(
                json!({
                    "run": run_prop(),
                    "text": { "type": "string" },
                    "enter": { "type": "boolean", "description": "Press Enter after the text (default true)" },
                    "keys": { "type": "array", "items": { "type": "string" }, "minItems": 1, "description": "Keys to press after the text: enter, esc, tab, up, down, left, right, space, backspace, ctrl-c, or one character" }
                }),
                &["run"],
            ),
            false,
        ),
        (
            "report",
            "Report to the lead",
            "For a worker: what you did, how you checked it, and what's left. Your lead sees it in wait. Commit first.",
            schema(
                json!({
                    "text": { "type": "string", "minLength": 1 },
                    "run": { "type": "string", "description": "The reporting run (default: this one)" }
                }),
                &["text"],
            ),
            false,
        ),
        (
            "diff",
            "A run's changes",
            "What a run changed since its worktree started: committed, uncommitted and new files.",
            schema(
                json!({
                    "run": run_prop(),
                    "stat": { "type": "boolean", "description": "Only files and line counts" }
                }),
                &["run"],
            ),
            true,
        ),
        (
            "verify",
            "Check a run's work",
            "test: run its tests here (waits for a test slot); push: push its branch for CI; send-failures: hand the last failing output to its agent; plan (default): which is better now.",
            schema(
                json!({
                    "run": run_prop(),
                    "action": { "type": "string", "enum": ["plan", "test", "push", "send-failures"] },
                    "command": { "type": "string", "description": "Test command (default: the repo's)" }
                }),
                &["run"],
            ),
            false,
        ),
        (
            "overlap",
            "Find overlapping runs",
            "Which runs touch the same files, and which would conflict when landed.",
            schema(
                json!({
                    "runs": { "type": "array", "items": { "type": "string" }, "description": "Only these runs (default: all on this repo)" },
                    "into": { "type": "string", "description": "Target branch (default: the repo's current branch)" }
                }),
                &[],
            ),
            true,
        ),
        (
            "land",
            "Merge runs",
            "Merge queue: rebase each run onto the branch, test it, fast-forward. A run that conflicts or fails is skipped and its agent told what to fix.",
            schema(
                json!({
                    "runs": { "type": "array", "items": { "type": "string" }, "minItems": 1 },
                    "into": { "type": "string", "description": "Target branch" },
                    "command": { "type": "string", "description": "Test command" },
                    "test": { "type": "boolean", "description": "Test before landing (default true)" },
                    "tell": { "type": "boolean", "description": "Tell a run's agent what to fix when it can't land (default true)" },
                    "dryRun": { "type": "boolean", "description": "Only say what would happen" }
                }),
                &["runs"],
            ),
            false,
        ),
        (
            "stop",
            "Stop a run",
            "End a run's agent (its worktree and branch stay).",
            schema(
                json!({
                    "run": run_prop(),
                    "force": { "type": "boolean", "description": "SIGKILL instead of SIGTERM" }
                }),
                &["run"],
            ),
            false,
        ),
    ]
}

/// The tools, for tools/list, in a fixed order.
pub fn list() -> Vec<Value> {
    defs()
        .into_iter()
        .map(|(name, title, description, input, read_only)| {
            json!({
                "name": name,
                "title": title,
                "description": description,
                "inputSchema": input,
                "annotations": {
                    "readOnlyHint": read_only,
                    "destructiveHint": name == "stop" || name == "land",
                    "openWorldHint": false,
                },
            })
        })
        .collect()
}

pub fn exists(name: &str) -> bool {
    defs().iter().any(|d| d.0 == name)
}

/// Checks `args` against the subset of JSON Schema the tools use: object properties,
/// required, additionalProperties: false, type, enum, minLength, pattern (a leading ^
/// and simple classes only), minimum, maximum, minItems and string items.
pub fn validate(schema: &Value, args: &Map<String, Value>) -> Result<(), String> {
    let props = schema["properties"]
        .as_object()
        .cloned()
        .unwrap_or_default();
    for r in schema["required"].as_array().into_iter().flatten() {
        let r = r.as_str().unwrap_or_default();
        if !args.contains_key(r) {
            return Err(format!("{r} is required."));
        }
    }
    for (k, v) in args {
        let Some(p) = props.get(k) else {
            return Err(format!(
                "Unknown argument {k}. Arguments: {}.",
                props.keys().cloned().collect::<Vec<_>>().join(", ")
            ));
        };
        check(k, p, v)?;
    }
    Ok(())
}

fn check(k: &str, p: &Value, v: &Value) -> Result<(), String> {
    let ty = p["type"].as_str().unwrap_or_default();
    let ok = match ty {
        "string" => v.is_string(),
        "boolean" => v.is_boolean(),
        "integer" => v.is_i64() || v.is_u64(),
        "array" => v.is_array(),
        _ => true,
    };
    if !ok {
        return Err(format!("{k} must be a {ty}."));
    }
    if let Some(e) = p["enum"].as_array() {
        if !e.contains(v) {
            let opts: Vec<String> = e.iter().map(|x| x.to_string()).collect();
            return Err(format!("{k} is one of {}.", opts.join(", ")));
        }
    }
    if let Some(s) = v.as_str() {
        if p["minLength"]
            .as_u64()
            .is_some_and(|m| (s.chars().count() as u64) < m)
        {
            return Err(format!("{k} can't be empty."));
        }
        if let Some(pat) = p["pattern"].as_str() {
            if !matches_pattern(pat, s) {
                return Err(format!("{k} doesn't match {pat}."));
            }
        }
    }
    if let Some(n) = v.as_i64() {
        if p["minimum"].as_i64().is_some_and(|m| n < m)
            || p["maximum"].as_i64().is_some_and(|m| n > m)
        {
            return Err(format!(
                "{k} is from {} to {}.",
                p["minimum"].as_i64().unwrap_or(i64::MIN),
                p["maximum"]
                    .as_i64()
                    .map_or("any".into(), |m| m.to_string())
            ));
        }
    }
    if let Some(a) = v.as_array() {
        if p["minItems"].as_u64().is_some_and(|m| (a.len() as u64) < m) {
            return Err(format!("{k} can't be empty."));
        }
        if p["items"]["type"] == "string" && !a.iter().all(Value::is_string) {
            return Err(format!("{k} is a list of strings."));
        }
    }
    Ok(())
}

/// The two patterns the tools use: `^[^-]` (doesn't start with -) and `^[A-Za-z0-9._-]+$`.
fn matches_pattern(pat: &str, s: &str) -> bool {
    match pat {
        "^[^-]" => !s.is_empty() && !s.starts_with('-'),
        "^[A-Za-z0-9._-]+$" => {
            !s.is_empty()
                && s.chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        }
        _ => true,
    }
}

fn s<'a>(args: &'a Map<String, Value>, k: &str) -> Option<&'a str> {
    args.get(k).and_then(Value::as_str)
}

fn b(args: &Map<String, Value>, k: &str) -> Option<bool> {
    args.get(k).and_then(Value::as_bool)
}

fn strings(args: &Map<String, Value>, k: &str) -> Vec<String> {
    args.get(k)
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// Cuts text past MAX_TEXT, saying so.
fn cap(text: &str) -> String {
    if text.len() <= MAX_TEXT {
        return text.to_string();
    }
    let mut end = MAX_TEXT;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}\n… cut: {} more bytes. Ask for less (diff stat: true, peek lines).",
        &text[..end],
        text.len() - end
    )
}

fn text_result(text: &str, is_error: bool, structured: Option<Value>) -> Value {
    let text = if text.trim().is_empty() {
        if is_error { "Failed." } else { "Done." }.to_string()
    } else {
        cap(text.trim_end())
    };
    let mut r = json!({ "content": [{ "type": "text", "text": text }], "isError": is_error });
    if let Some(s) = structured {
        r["structuredContent"] = s;
    }
    r
}

/// A CLI command, run as its own `amux` process. On another machine (`gpu:run`) it goes
/// there over SSH, as `amux <cmd> gpu:run` does.
fn cli(cmd: &str, argv: Vec<String>) -> (anyhow::Result<i32>, String) {
    let mut all = vec![cmd.to_string()];
    all.extend(argv);
    super::run_amux(&all)
}

/// A command's outcome as a tool result: its output, and isError when it failed or
/// exited non-zero.
fn outcome(r: anyhow::Result<i32>, out: String, structured: Option<Value>) -> Value {
    match r {
        Ok(0) => text_result(&out, false, structured),
        Ok(code) => text_result(
            &then(&out, &format!("(exit code {code})")),
            true,
            structured,
        ),
        Err(e) => text_result(&then(&out, &format!("{e:#}")), true, None),
    }
}

/// `out`, then `more` on a line of its own.
fn then(out: &str, more: &str) -> String {
    match out.trim_end() {
        "" => more.to_string(),
        o => format!("{o}\n{more}"),
    }
}

/// Runs a tool. Always a CallToolResult: failures are `isError`, so the agent can see
/// them and correct itself. `stop` turns true when the client cancels the call.
pub fn call(name: &str, args: &Map<String, Value>, stop: &dyn Fn() -> bool) -> Value {
    let Some(def) = defs().into_iter().find(|d| d.0 == name) else {
        return text_result(&format!("Unknown tool {name}."), true, None);
    };
    if let Err(e) = validate(&def.3, args) {
        return text_result(&e, true, None);
    }
    match name {
        "spawn" => spawn(args),
        "list" => list_runs(args),
        "wait" => wait(args, stop),
        "peek" => peek(args),
        "send" => send(args),
        "report" => report(args),
        "diff" => {
            let mut argv = vec![s(args, "run").unwrap_or_default().to_string()];
            if b(args, "stat") == Some(true) {
                argv.push("--stat".into());
            }
            let (r, out) = cli("diff", argv);
            let out = if r.as_ref().is_ok_and(|c| *c == 0) && out.trim().is_empty() {
                "No changes.".to_string()
            } else {
                out
            };
            outcome(r, out, None)
        }
        "verify" => {
            let mut argv = vec![s(args, "run").unwrap_or_default().to_string()];
            match s(args, "action").unwrap_or("plan") {
                "test" => argv.push("--test".into()),
                "push" => argv.push("--push".into()),
                "send-failures" => argv.push("--send-failures".into()),
                _ => {}
            }
            if let Some(c) = s(args, "command") {
                argv.extend(["--command".into(), c.into()]);
            }
            let (r, out) = cli("verify", argv);
            let r = match r {
                Ok(crate::work::slots::EX_TEMPFAIL) => {
                    return text_result(
                        &format!("{out}\nNo test slot came free in time. Try again later, or push for CI (action: push)."),
                        true,
                        None,
                    )
                }
                r => r,
            };
            outcome(r, out, None)
        }
        "overlap" => {
            let mut argv = strings(args, "runs");
            if let Some(i) = s(args, "into") {
                argv.extend(["--into".into(), i.into()]);
            }
            let (r, out) = cli("overlap", argv);
            outcome(r, out, None)
        }
        "land" => {
            let mut argv = strings(args, "runs");
            if let Some(i) = s(args, "into") {
                argv.extend(["--into".into(), i.into()]);
            }
            if let Some(c) = s(args, "command") {
                argv.extend(["--command".into(), c.into()]);
            }
            if b(args, "test") == Some(false) {
                argv.push("--no-test".into());
            }
            if b(args, "tell") == Some(false) {
                argv.push("--no-tell".into());
            }
            if b(args, "dryRun") == Some(true) {
                argv.push("--dry-run".into());
            }
            let (r, out) = cli("land", argv);
            outcome(r, out, None)
        }
        "stop" => {
            let mut argv = vec![s(args, "run").unwrap_or_default().to_string()];
            if b(args, "force") == Some(true) {
                argv.push("--force".into());
            }
            let (r, out) = cli("stop", argv);
            outcome(r, out, None)
        }
        _ => text_result(&format!("Unknown tool {name}."), true, None),
    }
}

/// `amux spawn`'s arguments for these tool arguments.
pub fn spawn_argv(args: &Map<String, Value>) -> Vec<String> {
    let mut argv: Vec<String> = vec![];
    for (k, flag) in [
        ("name", "--name"),
        ("dir", "--dir"),
        ("branch", "--branch"),
        ("base", "--base"),
        ("issue", "--issue"),
        ("prompt", "--prompt"),
    ] {
        if let Some(v) = s(args, k) {
            argv.push(flag.into());
            // The CLI refuses values that read as options, and a prompt that starts with
            // a list ("- do x") is a fine prompt.
            if k == "prompt" && v.trim_start().starts_with('-') {
                argv.push(format!("Task:\n{v}"));
            } else {
                argv.push(v.into());
            }
        }
    }
    if b(args, "worktree") == Some(true) {
        argv.push("--worktree".into());
    }
    match s(args, "autopilot") {
        Some("merge") => argv.push("--autopilot=merge".into()),
        Some(_) => argv.push("--autopilot".into()),
        None => {}
    }
    argv.push(s(args, "account").unwrap_or_default().into());
    argv.extend(strings(args, "args"));
    argv
}

fn spawn(args: &Map<String, Value>) -> Value {
    // The new run: amux says "started <run> as <account>" on success.
    let (r, out) = cli("spawn", spawn_argv(args));
    let started = out
        .lines()
        .find_map(|l| l.split_once("started ")?.1.split_once(" as ").map(|x| x.0))
        .and_then(|id| runs::load(id).ok());
    let structured = match (&r, &started) {
        (Ok(0), Some(run)) => {
            let mut v = commands::runs::ps_values(std::slice::from_ref(run))
                .pop()
                .unwrap_or_default();
            if let Some(o) = v.as_object_mut() {
                o.remove("run");
            }
            Some(json!({ "run": run.id, "status": v }))
        }
        _ => None,
    };
    let out = match &started {
        Some(run) if r.as_ref().is_ok_and(|c| *c == 0) => format!(
            "{}\nRun: {}. Next: wait (not list in a loop), peek, send.",
            out.trim_end(),
            run.id
        ),
        _ => out,
    };
    outcome(r, out, structured)
}

fn list_runs(args: &Map<String, Value>) -> Value {
    let mut all = runs::list();
    let lead = s(args, "lead");
    if let Some(l) = lead {
        match runs::find(l) {
            Ok(l) => all.retain(|r| r.lead.as_deref() == Some(l.id.as_str())),
            Err(e) => return text_result(&format!("{e:#}"), true, None),
        }
    }
    let mut values = commands::runs::ps_values(&all);
    for (v, r) in values.iter_mut().zip(&all) {
        v["lead"] = json!(r.lead);
    }
    if lead.is_none() {
        values.extend(crate::remote::runs(true));
    }
    for v in &mut values {
        if let Some(o) = v.as_object_mut() {
            // The raw run.json and the CLI's arguments (with the whole prompt): too much.
            o.remove("run");
            o.remove("args");
        }
    }
    let text = if values.is_empty() {
        "No runs. Start one with spawn.".to_string()
    } else {
        serde_json::to_string_pretty(&values).unwrap_or_default()
    };
    text_result(&text, false, Some(json!({ "runs": values })))
}

fn wait(args: &Map<String, Value>, stop: &dyn Fn() -> bool) -> Value {
    let timeout = args
        .get("timeout")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_WAIT)
        .min(MAX_WAIT);
    let ids = strings(args, "runs");
    let mut out = String::new();
    let r = if ids.is_empty() {
        commands::team::wait_team(s(args, "lead"), timeout, &mut out, stop)
    } else {
        commands::team::wait_runs(&ids, timeout, &mut out, stop)
    };
    match r {
        Ok(124) => text_result(
            &then(&out, "Nothing new yet. Call wait again."),
            false,
            Some(json!({ "timedOut": true })),
        ),
        Ok(0) => text_result(&out, false, Some(json!({ "timedOut": false }))),
        // 3: no workers yet, which the text says.
        Ok(_) => text_result(&out, true, None),
        Err(e) => {
            let mut msg = format!("{e:#}");
            if std::env::var_os("AMUX_RUN").is_none() && ids.is_empty() {
                msg.push_str(
                    "\nThis server isn't inside an amux run, so the runs it spawns aren't a team's \
                     workers: pass runs to wait for them.",
                );
            }
            text_result(&msg, true, None)
        }
    }
}

fn peek(args: &Map<String, Value>) -> Value {
    let run = s(args, "run").unwrap_or_default().to_string();
    let (r, out) = cli("peek", vec![run]);
    let lines: Vec<&str> = out.lines().map(str::trim_end).collect();
    let text = match args.get("lines").and_then(Value::as_u64) {
        Some(n) => {
            let shown: Vec<&str> = lines.into_iter().filter(|l| !l.is_empty()).collect();
            shown[shown.len().saturating_sub(n as usize)..].join("\n")
        }
        None => {
            let end = lines
                .iter()
                .rposition(|l| !l.is_empty())
                .map_or(0, |i| i + 1);
            lines[..end].join("\n")
        }
    };
    let text = if text.is_empty() && r.as_ref().is_ok_and(|c| *c == 0) {
        "(the screen is empty)".to_string()
    } else {
        text
    };
    outcome(r, text, None)
}

fn send(args: &Map<String, Value>) -> Value {
    let run = s(args, "run").unwrap_or_default().to_string();
    let keys = strings(args, "keys");
    let text = s(args, "text");
    if text.is_none() && keys.is_empty() {
        return text_result("Give text, keys, or both.", true, None);
    }
    let mut out = String::new();
    if let Some(t) = text {
        let mut argv = vec![run.clone()];
        if b(args, "enter") == Some(false) {
            argv.push("--no-enter".into());
        }
        // After `--`, text that starts with - is still text.
        argv.extend(["--".into(), t.into()]);
        let (r, o) = cli("send", argv);
        out.push_str(&o);
        if !matches!(r, Ok(0)) {
            return outcome(r, out, None);
        }
    }
    if !keys.is_empty() {
        let mut argv = vec![run];
        for k in keys {
            argv.extend(["--key".into(), k]);
        }
        let (r, o) = cli("send", argv);
        out.push_str(&o);
        return outcome(r, out, None);
    }
    outcome(Ok(0), out, None)
}

fn report(args: &Map<String, Value>) -> Value {
    let id = match s(args, "run") {
        Some(r) => r.to_string(),
        None => match std::env::var("AMUX_RUN").ok().filter(|v| !v.is_empty()) {
            Some(r) => r,
            None => {
                return text_result(
                    "This server isn't inside an amux run: say which run is reporting (run).",
                    true,
                    None,
                )
            }
        },
    };
    let r = runs::find(&id)
        .and_then(|run| commands::team::save_report(&run, s(args, "text").unwrap_or_default()));
    match r {
        Ok(msg) => text_result(&msg, false, None),
        Err(e) => text_result(&format!("{e:#}"), true, None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn every_tool_has_a_valid_schema() {
        let tools = list();
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(
            names,
            [
                "spawn", "list", "wait", "peek", "send", "report", "diff", "verify", "overlap",
                "land", "stop"
            ]
        );
        for t in &tools {
            let s = &t["inputSchema"];
            assert_eq!(s["type"], "object", "{t}");
            assert_eq!(s["additionalProperties"], false);
            let props = s["properties"].as_object().unwrap();
            for r in s["required"].as_array().unwrap() {
                assert!(props.contains_key(r.as_str().unwrap()), "{t}");
            }
            for (k, p) in props {
                assert!(
                    ["string", "boolean", "integer", "array"]
                        .contains(&p["type"].as_str().unwrap()),
                    "{k}"
                );
            }
            let d = t["description"].as_str().unwrap();
            assert!(!d.is_empty() && d.len() < 400, "{d}");
            let n = t["name"].as_str().unwrap();
            assert!(n.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'));
        }
    }

    #[test]
    fn validates_arguments_against_the_schema() {
        let spawn = &defs()[0].3;
        assert!(validate(
            spawn,
            &obj(json!({ "account": "claude", "worktree": true }))
        )
        .is_ok());
        assert!(validate(spawn, &obj(json!({})))
            .unwrap_err()
            .contains("account is required"));
        assert!(validate(spawn, &obj(json!({ "account": "-x" }))).is_err());
        assert!(
            validate(spawn, &obj(json!({ "account": "c", "worktree": "yes" })))
                .unwrap_err()
                .contains("boolean")
        );
        assert!(validate(spawn, &obj(json!({ "account": "c", "autopilot": "off" }))).is_err());
        assert!(validate(spawn, &obj(json!({ "account": "c", "name": "a b" }))).is_err());
        assert!(validate(spawn, &obj(json!({ "account": "c", "args": ["a", 1] }))).is_err());
        assert!(validate(spawn, &obj(json!({ "account": "c", "bogus": 1 })))
            .unwrap_err()
            .contains("Unknown argument bogus"));
        let wait = &defs()[2].3;
        assert!(validate(wait, &obj(json!({ "timeout": 600 }))).is_ok());
        assert!(validate(wait, &obj(json!({ "timeout": 601 }))).is_err());
        assert!(validate(wait, &obj(json!({ "timeout": 1.5 }))).is_err());
        assert!(validate(wait, &obj(json!({ "runs": [] }))).is_err());
        // A failed check is a tool error the agent sees, not a protocol error.
        let r = call("peek", &obj(json!({})), &|| false);
        assert_eq!(r["isError"], true);
        assert!(r["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("run is required"));
    }

    #[test]
    fn builds_spawn_arguments_like_the_cli() {
        let argv = spawn_argv(&obj(json!({
            "account": "codex@me", "name": "w1", "worktree": true, "prompt": "-fix it",
            "autopilot": "merge", "args": ["--model", "o3"], "base": "main"
        })));
        assert_eq!(
            argv,
            [
                "--name",
                "w1",
                "--base",
                "main",
                "--prompt",
                "Task:\n-fix it",
                "--worktree",
                "--autopilot=merge",
                "codex@me",
                "--model",
                "o3"
            ]
        );
    }

    #[test]
    fn cuts_long_output() {
        let long = "é".repeat(MAX_TEXT);
        let r = text_result(&long, false, None);
        let t = r["content"][0]["text"].as_str().unwrap();
        assert!(t.len() < MAX_TEXT + 200 && t.contains("more bytes"));
        assert_eq!(text_result("", true, None)["content"][0]["text"], "Failed.");
    }
}
