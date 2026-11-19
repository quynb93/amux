//! `amux mcp --install | --remove | --status`: the amux MCP server in each CLI's own MCP
//! config, so every account's agent has the tools.
//!
//! - Claude Code keeps user-scope servers in `.claude.json` (`mcpServers`), which is
//!   per account: amux writes it in each Claude account's overlay, and in the file new
//!   accounts are seeded from (`providers::seed_overlay`).
//! - Codex: a `[mcp_servers.amux]` table in the shared `config.toml`, between marker
//!   comments, forwarding AMUX_RUN and AMUX_HOME (Codex passes servers only the
//!   variables it is told to) and with a tool timeout above the longest `wait`.
//! - Gemini CLI: `mcpServers.amux` in the shared `settings.json`.
//!
//! An entry is amux's when it runs `amux mcp` (args exactly ["mcp"], a command named
//! amux). `--remove` takes out only that, and another server called "amux" is left alone.
//! Files are rewritten atomically, through a symlink to its target, keeping their mode.

use crate::paths::tildify;
use crate::providers::ProviderSpec;
use crate::runs::state_hooks::{read_settings, write_settings};
use crate::store::{load_accounts, load_providers, overlay_home};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

pub const CLIS: &[&str] = &["claude", "codex", "gemini"];
const NAME: &str = "amux";
const BEGIN: &str =
    "# amux mcp: begin (added by `amux mcp --install`; `amux mcp --remove` takes it out)";
const END: &str = "# amux mcp: end";
/// Tool timeout for the CLIs that have one: above the longest `wait`.
const TOOL_TIMEOUT_SECS: u64 = super::tools::MAX_WAIT + 60;

#[derive(Debug, PartialEq)]
pub enum State {
    Installed,
    Missing,
    /// Another server is called "amux" there.
    Other,
}

/// The program an entry runs: `amux` when that is this amux on PATH, else this binary.
fn program() -> String {
    let me = std::env::current_exe().ok();
    let on_path = crate::commands::maintenance::which("amux");
    let same = match (&me, &on_path) {
        (Some(a), Some(b)) => a.canonicalize().ok() == b.canonicalize().ok(),
        _ => false,
    };
    match me {
        Some(p) if !same => p.to_string_lossy().into_owned(),
        _ => "amux".into(),
    }
}

fn is_amux_command(c: &str) -> bool {
    Path::new(c).file_name().is_some_and(|n| n == "amux")
}

/// Whether a JSON server entry is the one `--install` writes.
pub fn is_ours(entry: &Value) -> bool {
    entry["args"] == json!(["mcp"]) && entry["command"].as_str().is_some_and(is_amux_command)
}

fn json_entry(cli: &str, program: &str) -> Value {
    match cli {
        "claude" => json!({ "type": "stdio", "command": program, "args": ["mcp"], "env": {} }),
        // Gemini CLI's timeout is in milliseconds.
        _ => json!({ "command": program, "args": ["mcp"], "timeout": TOOL_TIMEOUT_SECS * 1000 }),
    }
}

pub fn json_state(v: &Value) -> State {
    match v.get("mcpServers").and_then(|m| m.get(NAME)) {
        None => State::Missing,
        Some(e) if is_ours(e) => State::Installed,
        Some(_) => State::Other,
    }
}

/// Adds amux's entry (or refreshes it). Err when another server is called amux.
pub fn json_add(v: &mut Value, cli: &str, program: &str) -> Result<()> {
    if json_state(v) == State::Other {
        bail!("another MCP server is already called \"{NAME}\" there; left it alone");
    }
    let Some(obj) = v.as_object_mut() else {
        bail!("not a JSON object");
    };
    let servers = obj.entry("mcpServers").or_insert_with(|| json!({}));
    let Some(servers) = servers.as_object_mut() else {
        bail!("mcpServers isn't an object");
    };
    servers.insert(NAME.into(), json_entry(cli, program));
    Ok(())
}

/// Takes amux's entry out; drops `mcpServers` if that leaves it empty. Whether it did.
pub fn json_remove(v: &mut Value) -> bool {
    if json_state(v) != State::Installed {
        return false;
    }
    let Some(obj) = v.as_object_mut() else {
        return false;
    };
    if let Some(servers) = obj.get_mut("mcpServers").and_then(Value::as_object_mut) {
        servers.remove(NAME);
        if servers.is_empty() {
            obj.remove("mcpServers");
        }
    }
    true
}

fn toml_string(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn codex_block(program: &str) -> String {
    format!(
        "{BEGIN}\n[mcp_servers.{NAME}]\ncommand = {}\nargs = [\"mcp\"]\nenv_vars = [\"AMUX_RUN\", \"AMUX_HOME\"]\ntool_timeout_sec = {TOOL_TIMEOUT_SECS}\n{END}\n",
        toml_string(program)
    )
}

fn is_amux_table(line: &str) -> bool {
    let l: String = line
        .split('#')
        .next()
        .unwrap_or_default()
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    l == format!("[mcp_servers.{NAME}]") || l == format!("[mcp_servers.\"{NAME}\"]")
}

/// Where amux's block is in a config.toml: the lines from the begin marker through the
/// end marker, or, if something dropped the end marker, up to the next table.
fn codex_block_range(lines: &[&str]) -> Option<(usize, usize)> {
    let start = lines.iter().position(|l| l.trim() == BEGIN)?;
    let end = match lines[start..].iter().position(|l| l.trim() == END) {
        Some(i) => start + i,
        None => {
            let table = lines[start..].iter().position(|l| is_amux_table(l))? + start;
            lines[table + 1..]
                .iter()
                .position(|l| l.trim_start().starts_with('['))
                .map_or(lines.len() - 1, |i| table + i)
        }
    };
    Some((start, end))
}

pub fn toml_state(text: &str) -> State {
    let lines: Vec<&str> = text.lines().collect();
    let ours = codex_block_range(&lines);
    let other = lines
        .iter()
        .enumerate()
        .any(|(i, l)| is_amux_table(l) && !ours.is_some_and(|(a, b)| (a..=b).contains(&i)));
    match (ours, other) {
        (_, true) => State::Other,
        (Some(_), false) => State::Installed,
        (None, false) => State::Missing,
    }
}

/// The text without amux's block (and the blank line `toml_add` put before it).
pub fn toml_remove(text: &str) -> Option<String> {
    let lines: Vec<&str> = text.lines().collect();
    let (mut start, end) = codex_block_range(&lines)?;
    if start > 0 && lines[start - 1].trim().is_empty() {
        start -= 1;
    }
    let mut out: Vec<&str> = lines[..start].to_vec();
    out.extend(&lines[end + 1..]);
    let mut s = out.join("\n");
    if !s.is_empty() {
        s.push('\n');
    }
    Some(s)
}

/// The text with amux's block at the end (replacing an older one).
pub fn toml_add(text: &str, program: &str) -> Result<String> {
    if toml_state(text) == State::Other {
        bail!("config.toml already has an [mcp_servers.{NAME}] of its own; left it alone");
    }
    let mut s = toml_remove(text).unwrap_or_else(|| text.to_string());
    if !s.is_empty() {
        if !s.ends_with('\n') {
            s.push('\n');
        }
        s.push('\n');
    }
    s.push_str(&codex_block(program));
    Ok(s)
}

/// One config file amux manages, for one CLI.
struct Target {
    cli: &'static str,
    path: PathBuf,
    /// For Claude Code: the account (or "new accounts") whose file this is.
    whose: Option<String>,
}

/// The files for `only` (or every CLI), and lines about CLIs that have none yet.
fn targets(only: Option<&str>) -> Result<(Vec<Target>, Vec<String>)> {
    if let Some(o) = only {
        if !CLIS.contains(&o) {
            bail!("The MCP server can be installed for claude, codex and gemini, not \"{o}\".");
        }
    }
    let providers = load_providers()?;
    let accounts = load_accounts().unwrap_or_default();
    let mut out = vec![];
    let mut notes = vec![];
    for cli in CLIS
        .iter()
        .copied()
        .filter(|c| only.is_none_or(|o| o == *c))
    {
        let Some(spec) = providers.iter().find(|p| p.id == cli) else {
            continue;
        };
        if cli == "claude" {
            claude_targets(spec, &accounts, &mut out, &mut notes);
            continue;
        }
        if !spec.shared_home.exists() {
            notes.push(format!(
                "{cli}: no {} yet (install {} and run it once first)",
                tildify(&spec.shared_home),
                spec.label
            ));
            continue;
        }
        let file = if cli == "codex" {
            "config.toml"
        } else {
            "settings.json"
        };
        out.push(Target {
            cli,
            path: spec.shared_home.join(file),
            whose: None,
        });
    }
    Ok((out, notes))
}

fn claude_targets(
    spec: &ProviderSpec,
    accounts: &[crate::store::Account],
    out: &mut Vec<Target>,
    notes: &mut Vec<String>,
) {
    for a in accounts.iter().filter(|a| a.provider == "claude") {
        let f = overlay_home(spec, a).join(".claude.json");
        if f.exists() {
            out.push(Target {
                cli: "claude",
                path: f,
                whose: Some(a.id()),
            });
        } else {
            notes.push(format!(
                "claude: {} hasn't run yet; run it once, then `amux mcp --install claude` again",
                a.id()
            ));
        }
    }
    // The file new accounts are seeded from (providers::seed_overlay), which is also
    // plain `claude`'s own when it's ~/.claude.json.
    let seed = [
        spec.shared_home.join(".claude.json"),
        crate::paths::HOME.join(".claude.json"),
    ]
    .into_iter()
    .find(|p| p.exists());
    match seed {
        Some(p) => out.push(Target {
            cli: "claude",
            path: p,
            whose: Some("new accounts".into()),
        }),
        None if out.is_empty() => {
            notes.push("claude: no Claude Code accounts or ~/.claude.json yet".into())
        }
        None => {}
    }
}

fn label(t: &Target) -> String {
    match &t.whose {
        Some(w) => format!("{} ({w})", t.cli),
        None => t.cli.to_string(),
    }
}

fn state_of(t: &Target) -> Result<State> {
    if t.cli == "codex" {
        return Ok(match std::fs::read_to_string(&t.path) {
            Ok(text) => toml_state(&text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => State::Missing,
            Err(e) => return Err(e).with_context(|| format!("reading {}", t.path.display())),
        });
    }
    Ok(json_state(&read_settings(&t.path)?))
}

/// Writes text atomically, through a symlink to its target, keeping the file's mode.
fn write_text(path: &Path, text: &str) -> Result<()> {
    let target = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let mode = std::fs::metadata(&target).ok().map(|m| m.permissions());
    crate::fsx::write_atomic(&target, text.as_bytes())
        .with_context(|| format!("writing {}", target.display()))?;
    if let Some(mode) = mode {
        let _ = std::fs::set_permissions(&target, mode);
    }
    Ok(())
}

fn add(t: &Target, program: &str) -> Result<()> {
    if t.cli == "codex" {
        let text = match std::fs::read_to_string(&t.path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e).with_context(|| format!("reading {}", t.path.display())),
        };
        let new = toml_add(&text, program)?;
        if new != text {
            write_text(&t.path, &new)?;
        }
        return Ok(());
    }
    let mut v = read_settings(&t.path)?;
    let before = v.clone();
    json_add(&mut v, t.cli, program)?;
    if v != before {
        write_settings(&t.path, &v)?;
    }
    Ok(())
}

fn remove(t: &Target) -> Result<bool> {
    if t.cli == "codex" {
        let Ok(text) = std::fs::read_to_string(&t.path) else {
            return Ok(false);
        };
        return match toml_remove(&text) {
            Some(new) => {
                write_text(&t.path, &new)?;
                Ok(true)
            }
            None => Ok(false),
        };
    }
    if !t.path.exists() {
        return Ok(false);
    }
    let mut v = read_settings(&t.path)?;
    if json_remove(&mut v) {
        write_settings(&t.path, &v)?;
        return Ok(true);
    }
    Ok(false)
}

/// One line per config file: whether amux's server is in it. Also used by `amux doctor`.
pub fn status_lines() -> Vec<(bool, String)> {
    let Ok((targets, notes)) = targets(None) else {
        return vec![];
    };
    let mut out: Vec<(bool, String)> = notes.into_iter().map(|n| (true, n)).collect();
    for t in &targets {
        let at = tildify(&t.path);
        out.push(match state_of(t) {
            Ok(State::Installed) => (true, format!("{}: installed in {at}", label(t))),
            Ok(State::Missing) => (false, format!("{}: not installed ({at})", label(t))),
            Ok(State::Other) => (
                false,
                format!("{}: another server is called \"{NAME}\" in {at}", label(t)),
            ),
            Err(e) => (false, format!("{}: {e:#}", label(t))),
        });
    }
    out
}

pub fn status_cmd() -> Result<i32> {
    for (_, line) in status_lines() {
        println!("{line}");
    }
    Ok(0)
}

pub fn install_cmd(only: Option<&str>) -> Result<i32> {
    let (targets, notes) = targets(only)?;
    for n in notes {
        println!("{n}");
    }
    let program = program();
    let mut failed = 0;
    for t in &targets {
        match add(t, &program) {
            Ok(()) => println!("{}: added in {}", label(t), tildify(&t.path)),
            Err(e) => {
                failed += 1;
                eprintln!("{}: not added to {}: {e:#}", label(t), tildify(&t.path));
            }
        }
    }
    if !targets.is_empty() {
        println!(
            "Sessions started from now on have amux's tools (spawn, wait, peek, send, report, diff, \
             verify, land…). Inside an amux run, spawns are that run's workers. \
             `amux mcp --remove` takes it out."
        );
    }
    Ok(if failed > 0 { 1 } else { 0 })
}

pub fn remove_cmd(only: Option<&str>) -> Result<i32> {
    let (targets, _) = targets(only)?;
    let mut failed = 0;
    for t in &targets {
        match remove(t) {
            Ok(true) => println!("{}: removed from {}", label(t), tildify(&t.path)),
            Ok(false) => println!("{}: nothing to remove", label(t)),
            Err(e) => {
                failed += 1;
                eprintln!("{}: {e:#}", label(t));
            }
        }
    }
    Ok(if failed > 0 { 1 } else { 0 })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adds_and_removes_only_its_json_entry() {
        let mine = json!({ "theme": "dark", "mcpServers": { "gh": { "command": "gh-mcp" } } });
        let mut v = mine.clone();
        json_add(&mut v, "claude", "/opt/bin/amux").unwrap();
        assert_eq!(v["mcpServers"]["amux"]["args"], json!(["mcp"]));
        assert_eq!(v["mcpServers"]["amux"]["type"], "stdio");
        assert_eq!(v["mcpServers"]["gh"], mine["mcpServers"]["gh"]);
        assert_eq!(json_state(&v), State::Installed);
        // Again: the same.
        let once = v.clone();
        json_add(&mut v, "claude", "/opt/bin/amux").unwrap();
        assert_eq!(v, once);
        assert!(json_remove(&mut v));
        assert_eq!(v, mine);
        assert!(!json_remove(&mut v));

        let mut empty = json!({});
        json_add(&mut empty, "gemini", "amux").unwrap();
        assert_eq!(empty["mcpServers"]["amux"]["timeout"], 660_000);
        json_remove(&mut empty);
        assert_eq!(empty, json!({}));

        let mut theirs =
            json!({ "mcpServers": { "amux": { "command": "npx", "args": ["amux-mcp"] } } });
        let before = theirs.clone();
        assert_eq!(json_state(&theirs), State::Other);
        assert!(json_add(&mut theirs, "claude", "amux").is_err());
        assert!(!json_remove(&mut theirs));
        assert_eq!(theirs, before);
    }

    #[test]
    fn adds_and_removes_only_its_toml_block() {
        let mine = "model = \"o3\"\n\n[mcp_servers.gh]\ncommand = \"gh-mcp\"\n";
        let with = toml_add(mine, "/opt/my \"bin\"/amux").unwrap();
        assert!(with.starts_with(mine));
        assert!(with.contains(
            "[mcp_servers.amux]\ncommand = \"/opt/my \\\"bin\\\"/amux\"\nargs = [\"mcp\"]"
        ));
        assert!(with.contains("env_vars = [\"AMUX_RUN\", \"AMUX_HOME\"]"));
        assert!(with.contains("tool_timeout_sec = 660"));
        assert_eq!(toml_state(&with), State::Installed);
        assert_eq!(toml_add(&with, "/opt/my \"bin\"/amux").unwrap(), with);
        assert_eq!(toml_remove(&with).unwrap(), mine);
        assert_eq!(toml_remove(mine), None);
        assert_eq!(toml_remove(&toml_add("", "amux").unwrap()).unwrap(), "");

        // Something else appended a table after it and dropped the end marker.
        let edited = with.replace(&format!("{END}\n"), "")
            + "\n[projects.\"/x\"]\ntrust_level = \"trusted\"\n";
        assert_eq!(toml_state(&edited), State::Installed);
        let removed = toml_remove(&edited).unwrap();
        assert!(
            !removed.contains("amux") && removed.contains("[projects.\"/x\"]"),
            "{removed}"
        );

        let theirs = "[mcp_servers.amux]\ncommand = \"npx\"\n";
        assert_eq!(toml_state(theirs), State::Other);
        assert!(toml_add(theirs, "amux").is_err());
    }

    #[test]
    fn knows_its_own_entries() {
        assert!(is_ours(&json!({ "command": "amux", "args": ["mcp"] })));
        assert!(is_ours(
            &json!({ "command": "/usr/local/bin/amux", "args": ["mcp"] })
        ));
        assert!(!is_ours(
            &json!({ "command": "amux", "args": ["mcp", "--x"] })
        ));
        assert!(!is_ours(
            &json!({ "command": "/bin/notamux", "args": ["mcp"] })
        ));
    }
}
