//! `amux mcp`: a Model Context Protocol server over stdio, so an agent that prefers tools
//! to shell commands can run an amux team: spawn workers, wait for them, peek, send,
//! review and land their work.
//!
//! - Framing: one JSON-RPC 2.0 message per line on stdin and stdout. Nothing else ever
//!   goes to stdout: at start the server keeps private copies of stdin and stdout, points
//!   fd 0 at /dev/null and fd 1 at stderr, and each tool that runs a CLI command runs it
//!   as its own `amux` process, whose output becomes the tool's text.
//! - Versions: dual-era. A client that opens with `initialize` gets the newest legacy
//!   revision it asks for (2025-11-25 and earlier). A request carrying
//!   `_meta["io.modelcontextprotocol/protocolVersion"]` is served statelessly as
//!   2026-07-28, which also answers `server/discover`.
//! - Tools are in tools.rs; `--install`/`--remove`/`--status` (the CLIs' MCP config) in
//!   install.rs.
//! - Inside a run (AMUX_RUN, inherited from the CLI that started the server), spawns are
//!   that run's workers, exactly as `amux spawn` from its shell.

pub mod install;
pub mod tools;

use anyhow::{bail, Result};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::FromRawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// The stateless revision, served to requests that carry their version in `_meta`.
pub const MODERN: &str = "2026-07-28";
/// Handshake revisions a client may pick with `initialize`, newest first.
pub const LEGACY: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

const META_VERSION: &str = "io.modelcontextprotocol/protocolVersion";
const META_CAPS: &str = "io.modelcontextprotocol/clientCapabilities";
const META_SERVER: &str = "io.modelcontextprotocol/serverInfo";

// JSON-RPC 2.0 error codes, and MCP's own.
pub const PARSE_ERROR: i64 = -32700;
pub const INVALID_REQUEST: i64 = -32600;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;
pub const UNSUPPORTED_VERSION: i64 = -32022;

const USAGE: &str =
    "amux mcp [--install [claude|codex|gemini] | --remove [claude|codex|gemini] | --status]";

const INSTRUCTIONS: &str = "amux runs coding agents (Claude Code, Codex, Gemini CLI) in the \
background, each in its own terminal and, with worktree: true, its own git worktree and branch. \
To delegate: spawn a worker per independent part with a self-contained prompt, then call wait \
(not list in a loop): it returns when a worker finishes, needs an answer or ends, with its \
report or the end of its screen. Review with diff and verify, merge with overlap and land, \
answer or redirect with send. Inside an amux run, spawns become this run's workers (within its \
worker limit), and a worker finishes by calling report.";

pub fn mcp_cmd(args: &[String]) -> Result<i32> {
    let only = args.get(1).map(String::as_str);
    match args.first().map(String::as_str) {
        None => serve(),
        Some("--install" | "install") => install::install_cmd(only),
        Some("--remove" | "remove" | "--uninstall") => install::remove_cmd(only),
        Some("--status" | "status") => install::status_cmd(),
        Some("-h" | "--help") => {
            println!("Usage: {USAGE}\n\nWith no option, serves MCP on stdin and stdout (an MCP client starts it).");
            Ok(0)
        }
        Some(_) => bail!("Usage: {USAGE}"),
    }
}

fn server_info() -> Value {
    json!({ "name": "amux", "title": "amux", "version": crate::update::version() })
}

/// A JSON-RPC error response. `id` is null when the request's id couldn't be read.
pub fn error(id: Value, code: i64, message: &str, data: Option<Value>) -> Value {
    let mut e = json!({ "code": code, "message": message });
    if let Some(d) = data {
        e["data"] = d;
    }
    json!({ "jsonrpc": "2.0", "id": id, "error": e })
}

fn reply(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

/// What the server does with one incoming message.
#[derive(Debug, PartialEq)]
pub enum Outcome {
    /// Send this.
    Reply(Value),
    /// Run a tool on another thread, then reply to `id` (adding the modern result fields
    /// when `modern`).
    Call {
        id: Value,
        name: String,
        args: Map<String, Value>,
        modern: bool,
    },
    /// The client cancelled the request with this id.
    Cancel(Value),
    /// A notification, or a response from the client: nothing to send.
    Nothing,
}

/// Adds what a 2026-07-28 result carries.
fn modern_result(mut result: Value) -> Value {
    if let Some(o) = result.as_object_mut() {
        o.insert("resultType".into(), json!("complete"));
        let meta = o.entry("_meta").or_insert_with(|| json!({}));
        meta[META_SERVER] = server_info();
    }
    result
}

/// Handles one parsed message. Pure apart from reading the tool list, so it can be tested
/// without a client.
pub fn handle(msg: &Value) -> Outcome {
    let Some(obj) = msg.as_object() else {
        let why = if msg.is_array() {
            "Batches aren't supported: send one message per line."
        } else {
            "A message is a JSON object."
        };
        return Outcome::Reply(error(Value::Null, INVALID_REQUEST, why, None));
    };
    let id = obj.get("id").cloned();
    let valid_id = matches!(&id, Some(Value::String(_)))
        || id.as_ref().is_some_and(Value::is_i64)
        || id.as_ref().is_some_and(Value::is_u64);
    if obj.get("jsonrpc") != Some(&json!("2.0")) {
        let id = if valid_id { id.unwrap() } else { Value::Null };
        return Outcome::Reply(error(id, INVALID_REQUEST, "jsonrpc must be \"2.0\".", None));
    }
    let Some(method) = obj.get("method") else {
        // A response to a request of ours (we send none), or an error about one.
        if obj.contains_key("result") || obj.contains_key("error") {
            return Outcome::Nothing;
        }
        let id = if valid_id { id.unwrap() } else { Value::Null };
        return Outcome::Reply(error(id, INVALID_REQUEST, "No method.", None));
    };
    let Some(method) = method.as_str() else {
        let id = if valid_id { id.unwrap() } else { Value::Null };
        return Outcome::Reply(error(id, INVALID_REQUEST, "method must be a string.", None));
    };
    let params = obj.get("params").cloned().unwrap_or_else(|| json!({}));
    let Some(id) = id else {
        // A notification: never answered.
        if method == "notifications/cancelled" {
            if let Some(r) = params.get("requestId") {
                return Outcome::Cancel(r.clone());
            }
        }
        return Outcome::Nothing;
    };
    if !valid_id {
        return Outcome::Reply(error(
            Value::Null,
            INVALID_REQUEST,
            "A request's id is a string or an integer.",
            None,
        ));
    }
    let Some(params) = params.as_object() else {
        return Outcome::Reply(error(id, INVALID_PARAMS, "params must be an object.", None));
    };
    // Per-request metadata marks a modern (2026-07-28) client.
    let meta = params.get("_meta").and_then(Value::as_object);
    let modern = match meta.and_then(|m| m.get(META_VERSION)) {
        None => false,
        Some(v) => {
            if v.as_str() != Some(MODERN) {
                return Outcome::Reply(error(
                    id,
                    UNSUPPORTED_VERSION,
                    "Unsupported protocol version",
                    Some(json!({ "supported": [MODERN], "requested": v })),
                ));
            }
            if !meta.is_some_and(|m| m.get(META_CAPS).is_some_and(Value::is_object)) {
                return Outcome::Reply(error(
                    id,
                    INVALID_PARAMS,
                    &format!("_meta needs \"{META_CAPS}\"."),
                    None,
                ));
            }
            true
        }
    };
    let done = |result: Value| {
        Outcome::Reply(reply(
            id.clone(),
            if modern {
                modern_result(result)
            } else {
                result
            },
        ))
    };
    match method {
        "initialize" if !modern => {
            let Some(asked) = params.get("protocolVersion").and_then(Value::as_str) else {
                return Outcome::Reply(error(
                    id,
                    INVALID_PARAMS,
                    "initialize needs protocolVersion.",
                    None,
                ));
            };
            // The version asked for if we speak it, else our newest: the client decides
            // whether it can go on with that.
            let version = LEGACY.iter().find(|v| **v == asked).unwrap_or(&LEGACY[0]);
            done(json!({
                "protocolVersion": version,
                "capabilities": { "tools": { "listChanged": false } },
                "serverInfo": server_info(),
                "instructions": INSTRUCTIONS,
            }))
        }
        "server/discover" => Outcome::Reply(reply(
            id,
            modern_result(json!({
                "supportedVersions": [MODERN],
                "capabilities": { "tools": { "listChanged": false } },
                "instructions": INSTRUCTIONS,
            })),
        )),
        "ping" => done(json!({})),
        "tools/list" => done(json!({ "tools": tools::list() })),
        "tools/call" => {
            let Some(name) = params.get("name").and_then(Value::as_str) else {
                return Outcome::Reply(error(id, INVALID_PARAMS, "tools/call needs a name.", None));
            };
            if !tools::exists(name) {
                return Outcome::Reply(error(
                    id,
                    INVALID_PARAMS,
                    &format!("Unknown tool: {name}"),
                    None,
                ));
            }
            let args = match params.get("arguments") {
                None | Some(Value::Null) => Map::new(),
                Some(Value::Object(m)) => m.clone(),
                Some(_) => {
                    return Outcome::Reply(error(
                        id,
                        INVALID_PARAMS,
                        "arguments must be an object.",
                        None,
                    ))
                }
            };
            Outcome::Call {
                id,
                name: name.to_string(),
                args,
                modern,
            }
        }
        _ => Outcome::Reply(error(
            id,
            METHOD_NOT_FOUND,
            &format!("Method not found: {method}"),
            None,
        )),
    }
}

/// Parses one line from the client and handles it.
pub fn handle_line(line: &str) -> Outcome {
    match serde_json::from_str::<Value>(line) {
        Ok(v) => handle(&v),
        Err(e) => Outcome::Reply(error(
            Value::Null,
            PARSE_ERROR,
            &format!("Parse error: {e}"),
            None,
        )),
    }
}

/// Writes messages to the client, one line each, from any thread.
struct Wire {
    out: Mutex<File>,
}

impl Wire {
    fn send(&self, v: &Value) {
        // Compact JSON has no raw newlines: strings escape theirs.
        let mut line = v.to_string();
        line.push('\n');
        let mut out = self.out.lock().unwrap_or_else(|e| e.into_inner());
        let _ = out.write_all(line.as_bytes());
        let _ = out.flush();
    }
}

/// Keeps private copies of stdin and stdout for the protocol (closed on exec, so no child
/// holds the client's pipes), then points fd 0 at /dev/null and fd 1 at stderr, so no
/// code that reads stdin or prints can touch the protocol.
fn take_stdio() -> Result<(File, File)> {
    unsafe {
        let input = libc::fcntl(0, libc::F_DUPFD_CLOEXEC, 3);
        let output = libc::fcntl(1, libc::F_DUPFD_CLOEXEC, 3);
        if input < 0 || output < 0 {
            bail!("amux mcp: can't use stdin and stdout");
        }
        let null = libc::open(c"/dev/null".as_ptr(), libc::O_RDWR | libc::O_CLOEXEC);
        if null >= 0 {
            libc::dup2(null, 0);
            libc::close(null);
        }
        libc::dup2(2, 1);
        Ok((File::from_raw_fd(input), File::from_raw_fd(output)))
    }
}

fn serve() -> Result<i32> {
    let (input, output) = take_stdio()?;
    let wire = Arc::new(Wire {
        out: Mutex::new(output),
    });
    // Requests in progress, by id: set when the client cancels one.
    let inflight: Arc<Mutex<HashMap<String, Arc<AtomicBool>>>> = Arc::default();
    // Set when stdin closes: waits give up, so the server can exit soon.
    let closing = Arc::new(AtomicBool::new(false));
    let mut calls = vec![];
    let mut input = BufReader::new(input);
    let mut buf = vec![];
    loop {
        buf.clear();
        match input.read_until(b'\n', &mut buf) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        // A line that isn't UTF-8 is a parse error for the client, not the end of the
        // server.
        let line = String::from_utf8_lossy(&buf);
        if line.trim().is_empty() {
            continue;
        }
        match handle_line(&line) {
            Outcome::Reply(v) => wire.send(&v),
            Outcome::Nothing => {}
            Outcome::Cancel(id) => {
                if let Some(flag) = inflight.lock().unwrap().get(&id.to_string()) {
                    flag.store(true, Ordering::SeqCst);
                }
            }
            Outcome::Call {
                id,
                name,
                args,
                modern,
            } => {
                let key = id.to_string();
                let flag = Arc::new(AtomicBool::new(false));
                inflight.lock().unwrap().insert(key.clone(), flag.clone());
                let (wire, inflight, closing) = (wire.clone(), inflight.clone(), closing.clone());
                calls.push(std::thread::spawn(move || {
                    let cancelled = || flag.load(Ordering::SeqCst);
                    let stop = || cancelled() || closing.load(Ordering::SeqCst);
                    // A panic in a tool that runs in this process (wait, list,
                    // report) still answers, so the client isn't left waiting.
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        tools::call(&name, &args, &stop)
                    }))
                    .unwrap_or_else(|_| {
                        json!({
                            "content": [{ "type": "text", "text": "amux hit a bug running this tool." }],
                            "isError": true,
                        })
                    });
                    inflight.lock().unwrap_or_else(|e| e.into_inner()).remove(&key);
                    // A cancelled request gets no answer.
                    if !cancelled() {
                        let result = if modern {
                            modern_result(result)
                        } else {
                            result
                        };
                        wire.send(&reply(id, result));
                    }
                }));
                calls.retain(|c| !c.is_finished());
            }
        }
    }
    // stdin closed: the client is done. Let the calls it made finish (a wait ends at
    // once) and answer, if anyone still reads.
    closing.store(true, Ordering::SeqCst);
    for c in calls {
        let _ = c.join();
    }
    Ok(0)
}

/// Runs `amux <args…>` as its own process and returns its exit code and everything it
/// printed (stdout and stderr together, in order). Each tool call gets its own process,
/// so calls run side by side and one call's output never lands in another's result. The
/// child inherits this server's environment, so `AMUX_RUN` makes its spawns workers.
pub fn run_amux(args: &[String]) -> (Result<i32>, String) {
    use std::process::{Command, Stdio};
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(e) => {
            return (
                Err(anyhow::anyhow!("can't find amux itself: {e}")),
                String::new(),
            )
        }
    };
    let (mut reader, writer) = match std::io::pipe() {
        Ok(p) => p,
        Err(e) => {
            return (
                Err(anyhow::anyhow!("can't capture output: {e}")),
                String::new(),
            )
        }
    };
    let writer2 = match writer.try_clone() {
        Ok(w) => w,
        Err(e) => {
            return (
                Err(anyhow::anyhow!("can't capture output: {e}")),
                String::new(),
            )
        }
    };
    let child = Command::new(exe)
        .args(args)
        .env("AMUX_NO_UPDATE_CHECK", "1")
        .stdin(Stdio::null())
        .stdout(writer)
        .stderr(writer2)
        .spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(e) => return (Err(anyhow::anyhow!("can't run amux: {e}")), String::new()),
    };
    // The child's copies of the pipe's write end are the only ones left now that the
    // Command (which held ours) is gone, so reading ends when the child and anything it
    // left running in the foreground close them.
    let mut bytes = vec![];
    let _ = reader.read_to_end(&mut bytes);
    let text = String::from_utf8_lossy(&bytes).into_owned();
    match child.wait() {
        Ok(st) => (Ok(st.code().unwrap_or(1)), text),
        Err(e) => (Err(anyhow::anyhow!("amux didn't finish: {e}")), text),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply_of(o: Outcome) -> Value {
        match o {
            Outcome::Reply(v) => v,
            other => panic!("expected a reply, got {other:?}"),
        }
    }

    #[test]
    fn negotiates_a_legacy_version_on_initialize() {
        let r = reply_of(handle_line(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}"#,
        ));
        assert_eq!(r["id"], 1);
        assert_eq!(r["result"]["protocolVersion"], "2025-06-18");
        assert_eq!(r["result"]["serverInfo"]["name"], "amux");
        assert!(r["result"]["capabilities"]["tools"].is_object());
        assert!(r["result"].get("resultType").is_none());
        // A version we don't know: our newest legacy one.
        let r = reply_of(handle_line(
            r#"{"jsonrpc":"2.0","id":"a","method":"initialize","params":{"protocolVersion":"2099-01-01"}}"#,
        ));
        assert_eq!(r["result"]["protocolVersion"], LEGACY[0]);
        let r = reply_of(handle_line(
            r#"{"jsonrpc":"2.0","id":2,"method":"initialize","params":{}}"#,
        ));
        assert_eq!(r["error"]["code"], INVALID_PARAMS);
    }

    #[test]
    fn serves_modern_requests_statelessly() {
        let meta = json!({ META_VERSION: MODERN, META_CAPS: {} });
        let d = reply_of(handle(&json!({
            "jsonrpc": "2.0", "id": "d", "method": "server/discover", "params": { "_meta": meta }
        })));
        assert_eq!(d["result"]["supportedVersions"], json!([MODERN]));
        assert_eq!(d["result"]["resultType"], "complete");
        assert_eq!(d["result"]["_meta"][META_SERVER]["name"], "amux");
        let l = reply_of(handle(&json!({
            "jsonrpc": "2.0", "id": 3, "method": "tools/list", "params": { "_meta": meta }
        })));
        assert_eq!(l["result"]["resultType"], "complete");
        assert!(l["result"]["tools"].as_array().unwrap().len() >= 10);
        let old = reply_of(handle(&json!({
            "jsonrpc": "2.0", "id": 4, "method": "tools/list",
            "params": { "_meta": { META_VERSION: "1900-01-01", META_CAPS: {} } }
        })));
        assert_eq!(old["error"]["code"], UNSUPPORTED_VERSION);
        assert_eq!(old["error"]["data"]["supported"], json!([MODERN]));
        assert_eq!(old["error"]["data"]["requested"], "1900-01-01");
        let no_caps = reply_of(handle(&json!({
            "jsonrpc": "2.0", "id": 5, "method": "ping", "params": { "_meta": { META_VERSION: MODERN } }
        })));
        assert_eq!(no_caps["error"]["code"], INVALID_PARAMS);
    }

    #[test]
    fn answers_framing_errors_the_json_rpc_way() {
        let r = reply_of(handle_line("{not json"));
        assert_eq!(
            (r["id"].clone(), r["error"]["code"].clone()),
            (Value::Null, json!(PARSE_ERROR))
        );
        let r = reply_of(handle_line("[]"));
        assert_eq!(r["error"]["code"], INVALID_REQUEST);
        let r = reply_of(handle_line(r#"{"id":1,"method":"ping"}"#));
        assert_eq!(
            (r["id"].clone(), r["error"]["code"].clone()),
            (json!(1), json!(INVALID_REQUEST))
        );
        let r = reply_of(handle_line(
            r#"{"jsonrpc":"2.0","id":null,"method":"ping"}"#,
        ));
        assert_eq!(r["error"]["code"], INVALID_REQUEST);
        let r = reply_of(handle_line(r#"{"jsonrpc":"2.0","id":7,"method":"nope"}"#));
        assert_eq!(
            (r["id"].clone(), r["error"]["code"].clone()),
            (json!(7), json!(METHOD_NOT_FOUND))
        );
        let r = reply_of(handle_line(
            r#"{"jsonrpc":"2.0","id":8,"method":"ping","params":[1]}"#,
        ));
        assert_eq!(r["error"]["code"], INVALID_PARAMS);
        let r = reply_of(handle_line(r#"{"jsonrpc":"2.0","id":9,"method":"ping"}"#));
        assert_eq!(r["result"], json!({}));
    }

    #[test]
    fn notifications_get_no_answer() {
        assert_eq!(
            handle_line(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#),
            Outcome::Nothing
        );
        assert_eq!(
            handle_line(r#"{"jsonrpc":"2.0","method":"nope"}"#),
            Outcome::Nothing
        );
        assert_eq!(
            handle_line(
                r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":4}}"#
            ),
            Outcome::Cancel(json!(4))
        );
        // A client's response to a request of ours.
        assert_eq!(
            handle_line(r#"{"jsonrpc":"2.0","id":1,"result":{}}"#),
            Outcome::Nothing
        );
    }

    #[test]
    fn dispatches_tool_calls() {
        let r = reply_of(handle_line(
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"nope","arguments":{}}}"#,
        ));
        assert_eq!(r["error"]["code"], INVALID_PARAMS);
        assert!(r["error"]["message"].as_str().unwrap().contains("nope"));
        let r = reply_of(handle_line(
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"peek","arguments":"x"}}"#,
        ));
        assert_eq!(r["error"]["code"], INVALID_PARAMS);
        match handle_line(
            r#"{"jsonrpc":"2.0","id":"c","method":"tools/call","params":{"name":"peek","arguments":{"run":"r1"}}}"#,
        ) {
            Outcome::Call {
                id,
                name,
                args,
                modern,
            } => {
                assert_eq!((id, name.as_str(), modern), (json!("c"), "peek", false));
                assert_eq!(args["run"], "r1");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn runs_amux_and_returns_what_it_printed() {
        // current_exe() here is the test binary: --help prints and exits 0.
        let (r, out) = run_amux(&["--help".to_string()]);
        assert!(r.is_ok(), "{out}");
    }
}
