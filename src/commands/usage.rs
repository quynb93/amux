//! `amux usage`: how much of each account's 5-hour and 7-day windows is used, which
//! accounts have hit their limit and until when, and how many runs each has going.
//! `amux usage --track on|off` puts `amux __statusline` in Claude Code's status line, so
//! Claude Code's own numbers get recorded. See work/limits.rs and work/quota.rs.

use crate::store::{get_provider, load_accounts};
use crate::util::table;
use crate::work::{limits, quota};
use crate::{fsx, paths, runs, time};
use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Map, Value};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const USAGE: &str =
    "amux usage [--json] | amux usage --track on|off | amux usage --clear <provider>@<name>";

/// "▓▓▓▓░░░░░░ 42%".
pub fn bar(pct: f64, cells: usize) -> String {
    let filled = ((pct.clamp(0.0, 100.0) / 100.0) * cells as f64).round() as usize;
    format!(
        "{}{} {:>3.0}%",
        "▓".repeat(filled),
        "░".repeat(cells - filled),
        pct
    )
}

pub fn usage_cmd(argv: &[String]) -> Result<i32> {
    match argv.first().map(String::as_str) {
        Some("--clear") => {
            let Some(a) = argv.get(1) else {
                bail!("Usage: {USAGE}");
            };
            if limits::clear(a) {
                println!("{a} is no longer marked limited.");
            } else {
                println!("{a} wasn't marked limited.");
            }
            return Ok(0);
        }
        Some("--track") => {
            return match argv.get(1).map(String::as_str) {
                Some("on") => track_on(),
                Some("off") => track_off(),
                _ => bail!("Usage: {USAGE}"),
            }
        }
        _ => {}
    }
    let json = argv.iter().any(|a| a == "--json");
    if let Some(a) = argv.iter().find(|a| *a != "--json") {
        bail!("Unknown argument {a}. Usage: {USAGE}");
    }
    quota::refresh_live_codex_runs();
    let now = time::now_ms();
    let active = limits::active();
    let all_runs = runs::list();
    let accounts = load_accounts()?;
    let quotas = quota::all(&accounts);
    let live = |id: &str| {
        all_runs
            .iter()
            .filter(|r| r.account == id && r.state().is_live())
            .count()
    };
    if json {
        let out: Vec<Value> = accounts
            .iter()
            .map(|a| {
                let id = a.id();
                json!({
                    "account": id,
                    "liveRuns": live(&id),
                    "limit": active.get(&id),
                    "quota": quotas.get(&id),
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(0);
    }
    if accounts.is_empty() {
        println!("No accounts yet. Add one with `amux account add <provider> <name>`.");
        return Ok(0);
    }
    let mut reasons = Vec::new();
    let rows: Vec<Vec<String>> = accounts
        .iter()
        .map(|a| {
            let id = a.id();
            let q = quotas.get(&id);
            let five = q
                .and_then(|q| q.five_hour.as_ref())
                .map_or("-".into(), |w| bar(w.used_percent, 10));
            let seven = q
                .and_then(|q| q.seven_day.as_ref())
                .map_or("-".into(), |w| format!("{:.0}%", w.used_percent));
            let resets = q.map_or(String::new(), |q| {
                [("5h", &q.five_hour), ("7d", &q.seven_day)]
                    .iter()
                    .filter_map(|(n, w)| {
                        w.as_ref()
                            .map(|w| format!("{n} {}", quota::until(w.resets_at, now)))
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            });
            let updated = q.map_or(String::new(), |q| {
                format!("{} ago", runs::state::short_age(now - q.at))
            });
            let state = match active.get(&id) {
                Some(l) => {
                    reasons.push(format!("{id}: “{}”", l.reason));
                    format!("limited, resets {}", quota::until(l.resets_at, now))
                }
                None => "ok".into(),
            };
            vec![
                id.clone(),
                five,
                seven,
                resets,
                updated,
                state,
                live(&id).to_string(),
            ]
        })
        .collect();
    println!(
        "{}",
        table(
            &["ACCOUNT", "5H", "7D", "RESETS", "UPDATED", "LIMIT", "RUNS"],
            &rows
        )
    );
    for r in reasons {
        println!("{r}");
    }
    println!();
    if tracking().unwrap_or(false) {
        println!("Claude Code reports its numbers through the status line (`amux usage --track off` stops it).");
    } else {
        println!("Claude Code's numbers need `amux usage --track on`. Codex's are read from its runs' sessions; Gemini CLI keeps none.");
    }
    match quota::switch_at() {
        Some(at) => println!(
            "Runs move to another account at {at:.0}% when \"onLimit\" is \"failover\" (now: {}); `amux spawn <cli>` takes the account with the most headroom.",
            limits::on_limit()
        ),
        None => println!(
            "Set \"limits\": {{ \"switchAt\": 90, \"onLimit\": \"failover\" }} in ~/.amux/config.json to move runs to another account before the limit."
        ),
    }
    println!(
        "When a run hits its limit: {}. `amux failover <run>` moves one by hand.",
        if limits::on_limit() == "failover" {
            "it moves to another account of the same CLI"
        } else {
            "it waits for the reset (the CLI's own behavior)"
        }
    );
    Ok(0)
}

/// Where amux keeps the status line command it wrapped.
fn saved_file() -> PathBuf {
    paths::AMUX_HOME.join("statusline.json")
}

/// Claude Code's shared settings.json.
fn settings_file() -> Result<PathBuf> {
    Ok(get_provider("claude")?.shared_home.join("settings.json"))
}

/// `amux __statusline` or `'<path>' __statusline`, not a command of your own whose name
/// merely contains it.
fn is_wrapper(cmd: &str) -> bool {
    cmd.trim_end().ends_with(" __statusline")
}

fn wrapped(settings: &Value) -> bool {
    settings["statusLine"]["command"]
        .as_str()
        .is_some_and(is_wrapper)
}

fn tracking() -> Result<bool> {
    Ok(wrapped(&read_settings(&settings_file()?)?))
}

/// The command for settings.json: `amux __statusline`, by full path when the `amux` on
/// PATH isn't this one.
fn wrapper_command() -> String {
    let me = std::env::current_exe().ok();
    let on_path = std::env::var_os("PATH").and_then(|p| {
        std::env::split_paths(&p)
            .map(|d| d.join("amux"))
            .find(|f| f.is_file())
    });
    let same = match (&me, &on_path) {
        (Some(a), Some(b)) => a.canonicalize().ok() == b.canonicalize().ok(),
        _ => false,
    };
    match me {
        Some(me) if !same => format!(
            "'{}' __statusline",
            me.to_string_lossy().replace('\'', r"'\''")
        ),
        _ => "amux __statusline".into(),
    }
}

/// Puts the wrapper in `settings`. Returns the `statusLine` it replaces (Null when there
/// was none), to keep for `off`; its other fields (padding, refreshInterval) stay.
pub fn install(settings: &mut Value, command: &str) -> Result<Value> {
    let obj = settings
        .as_object_mut()
        .ok_or_else(|| anyhow!("settings.json isn't a JSON object"))?;
    let original = obj.get("statusLine").cloned().unwrap_or(Value::Null);
    let mut line = match &original {
        Value::Object(m) => m.clone(),
        _ => Map::new(),
    };
    line.insert("type".into(), "command".into());
    line.insert("command".into(), command.into());
    obj.insert("statusLine".into(), Value::Object(line));
    Ok(original)
}

/// Puts back what `install` replaced: the original `statusLine`, or none.
pub fn uninstall(settings: &mut Value, original: &Value) {
    if let Some(obj) = settings.as_object_mut() {
        if original.is_null() {
            obj.remove("statusLine");
        } else {
            obj.insert("statusLine".into(), original.clone());
        }
    }
}

fn read_settings(path: &Path) -> Result<Value> {
    match std::fs::read(path) {
        Ok(b) if b.iter().all(u8::is_ascii_whitespace) => Ok(json!({})),
        Ok(b) => serde_json::from_slice(&b).with_context(|| {
            format!(
                "Cannot parse {}; amux leaves it alone. Fix it, or set the status line by hand to `amux __statusline`.",
                paths::tildify(path)
            )
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(json!({})),
        Err(e) => Err(e).with_context(|| format!("Cannot read {}", path.display())),
    }
}

/// Writes settings.json atomically, through a symlink to wherever the real file is (a
/// dotfiles repo, say), keeping its permissions.
fn write_settings(path: &Path, v: &Value) -> Result<()> {
    let real = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let mode = std::fs::metadata(&real).ok().map(|m| {
        use std::os::unix::fs::PermissionsExt;
        m.permissions().mode() & 0o777
    });
    if let Some(dir) = real.parent() {
        fsx::mkdir_private(dir)?;
    }
    let text = serde_json::to_string_pretty(v)? + "\n";
    fsx::write_atomic(&real, text.as_bytes())
        .with_context(|| format!("Cannot write {}", real.display()))?;
    if let Some(m) = mode {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(m))?;
    }
    Ok(())
}

fn track_on() -> Result<i32> {
    let path = settings_file()?;
    let mut settings = read_settings(&path)?;
    if wrapped(&settings) {
        println!(
            "Already on: {} runs `amux __statusline`.",
            paths::tildify(&path)
        );
        return Ok(0);
    }
    let original = install(&mut settings, &wrapper_command())?;
    crate::store::write_json(&saved_file(), &json!({ "original": original }))?;
    write_settings(&path, &settings)?;
    println!(
        "Claude Code's status line now records each account's 5-hour and 7-day usage for amux."
    );
    match original["command"].as_str() {
        Some(c) => println!("Your own status line still shows: amux runs `{c}` after it."),
        None => println!("You had no status line, so it stays empty."),
    }
    println!(
        "Changed {} (statusLine only). `amux usage --track off` puts it back.",
        paths::tildify(&path)
    );
    Ok(0)
}

fn track_off() -> Result<i32> {
    let path = settings_file()?;
    let mut settings = read_settings(&path)?;
    let saved: Value = crate::store::read_json(&saved_file())?;
    if !wrapped(&settings) {
        println!(
            "Not on: {} doesn't run `amux __statusline`; left it as it is.",
            paths::tildify(&path)
        );
        let _ = std::fs::remove_file(saved_file());
        return Ok(0);
    }
    let original = saved.get("original").cloned().unwrap_or(Value::Null);
    uninstall(&mut settings, &original);
    write_settings(&path, &settings)?;
    let _ = std::fs::remove_file(saved_file());
    println!("Put back your status line in {}.", paths::tildify(&path));
    Ok(0)
}

/// `amux __statusline`: Claude Code's status line command while tracking is on. Records
/// the rate limits in its input for `AMUX_ACCOUNT`, then runs the status line command it
/// replaced with the same input, and prints what that prints. Never fails the status line
/// over amux's own trouble.
pub fn statusline_main() -> i32 {
    let mut input = Vec::new();
    let _ = std::io::stdin().read_to_end(&mut input);
    if let Ok(account) = std::env::var("AMUX_ACCOUNT") {
        if let Ok(v) = serde_json::from_slice::<Value>(&input) {
            if let Some(q) = quota::from_statusline(&v, time::now_ms()) {
                quota::record(&account, &q);
            }
        }
    }
    let saved: Value = crate::store::read_json(&saved_file()).unwrap_or_default();
    let Some(cmd) = saved["original"]["command"]
        .as_str()
        .filter(|c| !c.trim().is_empty() && !is_wrapper(c))
    else {
        return 0;
    };
    let child = Command::new("sh")
        .args(["-c", cmd])
        .stdin(Stdio::piped())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn();
    let Ok(mut child) = child else {
        return 1;
    };
    if let Some(mut stdin) = child.stdin.take() {
        // A command that doesn't read its input closes the pipe; that's fine.
        let _ = stdin.write_all(&input);
    }
    child.wait().ok().and_then(|s| s.code()).unwrap_or(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraps_and_restores_the_status_line_exactly() {
        let before = json!({
            "model": "opus",
            "statusLine": {"type": "command", "command": "~/.claude/line.sh", "padding": 2},
            "hooks": {"Stop": []}
        });
        let mut s = before.clone();
        let original = install(&mut s, "amux __statusline").unwrap();
        assert_eq!(original, before["statusLine"]);
        assert_eq!(
            s["statusLine"],
            json!({"type": "command", "command": "amux __statusline", "padding": 2})
        );
        assert!(wrapped(&s));
        // Everything else, in the same order.
        let keys: Vec<&String> = s.as_object().unwrap().keys().collect();
        assert_eq!(keys, ["model", "statusLine", "hooks"]);
        uninstall(&mut s, &original);
        assert_eq!(s, before);
        assert_eq!(
            serde_json::to_string(&s).unwrap(),
            serde_json::to_string(&before).unwrap()
        );

        // No status line before: none after.
        let before = json!({"model": "opus"});
        let mut s = before.clone();
        let original = install(&mut s, "amux __statusline").unwrap();
        assert!(original.is_null());
        assert_eq!(s["statusLine"]["type"], "command");
        uninstall(&mut s, &original);
        assert_eq!(s, before);
        assert!(install(&mut json!([]), "x").is_err());
    }

    #[test]
    fn draws_usage_bars() {
        assert_eq!(bar(0.0, 10), "░░░░░░░░░░   0%");
        assert_eq!(bar(42.4, 10), "▓▓▓▓░░░░░░  42%");
        assert_eq!(bar(100.0, 4), "▓▓▓▓ 100%");
        assert_eq!(bar(130.0, 4), "▓▓▓▓ 130%");
    }
}
