//! `amux slot` runs a heavy command when the machine has room for it, `amux slots` shows
//! who holds and waits for slots, and `amux __slot-hook` keeps agents from skipping them.

use crate::util::table;
use crate::work::slots::{self, Blocked, Holder, Limits, Pressure, Wait, EX_TEMPFAIL};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::io::Read;
use std::os::unix::process::ExitStatusExt;
use std::process::Command;
use std::time::{Duration, Instant};

const USAGE: &str =
    "amux slot [--class test|build|dev|<name>] [--timeout <secs>] [--no-wait] [--] <command...>";

struct Opts {
    class: Option<String>,
    timeout: Option<Duration>,
    pressure: bool,
}

fn parse(argv: &[String]) -> Result<(Opts, &[String])> {
    let mut o = Opts {
        class: None,
        timeout: None,
        pressure: true,
    };
    let mut i = 0;
    while let Some(a) = argv.get(i) {
        let (flag, inline) = match a.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f, Some(v.to_string())),
            _ => (a.as_str(), None),
        };
        let mut value = || -> Result<String> {
            match &inline {
                Some(v) => Ok(v.clone()),
                None => {
                    i += 1;
                    argv.get(i)
                        .cloned()
                        .with_context(|| format!("{flag} needs a value. Usage: {USAGE}"))
                }
            }
        };
        match flag {
            "--" => {
                i += 1;
                break;
            }
            "--class" | "-c" => o.class = Some(value()?),
            "--timeout" | "-t" => {
                let v = value()?;
                let secs: f64 = v
                    .parse()
                    .ok()
                    .filter(|s: &f64| s.is_finite() && *s >= 0.0)
                    .with_context(|| format!("--timeout takes seconds, not \"{v}\"."))?;
                o.timeout = Some(Duration::from_secs_f64(secs));
            }
            "--no-wait" => o.timeout = Some(Duration::ZERO),
            // Only wait for a free slot, not for load or memory.
            "--no-pressure" => o.pressure = false,
            f if f.starts_with('-') => bail!("Unknown option {f}. Usage: {USAGE}"),
            _ => break,
        }
        i += 1;
    }
    let cmd = argv.get(i..).unwrap_or_default();
    if cmd.is_empty() {
        bail!("Usage: {USAGE}");
    }
    if let Some(c) = &o.class {
        if !slots::valid_class(c) {
            bail!("A slot class uses letters, digits, '-' and '_'.");
        }
    }
    Ok((o, cmd))
}

/// One argument with shell syntax in it runs through `sh -c`, so `amux slot 'cd web && npm
/// test'` works. Several arguments run as they are, with no shell in between.
fn command(cmd: &[String]) -> Command {
    match cmd {
        [one] if one.contains(|c: char| c.is_whitespace() || "&|;<>()$`*?".contains(c)) => {
            let mut c = Command::new("sh");
            c.arg("-c").arg(one);
            c
        }
        [bin, args @ ..] => {
            let mut c = Command::new(bin);
            c.args(args);
            c
        }
        [] => unreachable!("parse checks for a command"),
    }
}

pub fn slot_cmd(argv: &[String]) -> Result<i32> {
    let (o, cmd) = parse(argv)?;
    let text = cmd.join(" ");
    let class = o
        .class
        .clone()
        .unwrap_or_else(|| slots::guess_class(&text).to_string());
    let me = Holder::me(&text);
    let limit = Limits::from_config().of(&class);
    let started = Instant::now();
    let mut waited = false;
    let wait = Wait {
        timeout: o.timeout,
        pressure: o.pressure,
    };
    let slot = slots::acquire(&class, &me, &wait, |b: &Blocked| {
        waited = true;
        eprintln!(
            "amux: waiting for a {class} slot: {}",
            b.describe(&class, limit)
        );
    })?;
    let Some(slot) = slot else {
        eprintln!(
            "amux: no {class} slot came free{}. Nothing was run.\n\
             amux: try again later, or commit and push the branch and let CI run it.\n\
             amux: `amux slots` shows what is running.",
            match o.timeout {
                Some(t) if !t.is_zero() => format!(" within {}s", t.as_secs()),
                _ => String::new(),
            }
        );
        return Ok(EX_TEMPFAIL);
    };
    if waited {
        eprintln!(
            "amux: got {class} slot {} after {}s",
            slot.index + 1,
            started.elapsed().as_secs()
        );
    }
    // Whatever the command leaves running (a dev server's workers) keeps the slot.
    slot.inherit();
    let status = command(cmd)
        .status()
        .with_context(|| format!("could not run {}", cmd[0]))?;
    drop(slot);
    Ok(status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(0)))
}

pub fn slots_cmd(argv: &[String]) -> Result<i32> {
    if argv.iter().any(|a| a == "--hook") {
        println!("{}", serde_json::to_string_pretty(&hook_config())?);
        eprintln!(
            "Add this to the shared settings.json (for example ~/.claude/settings.json) so every\n\
             account's Claude Code runs tests, builds and dev servers through `amux slot`.\n\
             For Codex and other CLIs, add this to AGENTS.md instead:\n\n{}",
            agent_instructions()
        );
        return Ok(0);
    }
    let limits = Limits::from_config();
    let root = slots::slots_dir();
    let mut classes: Vec<String> = limits.classes.iter().map(|(c, _)| c.clone()).collect();
    if let Ok(rd) = std::fs::read_dir(&root) {
        let mut seen: Vec<String> = rd
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|c| slots::valid_class(c) && !classes.contains(c))
            .collect();
        seen.sort();
        classes.extend(seen);
    }
    if argv.iter().any(|a| a == "--json") {
        let p = Pressure::now();
        let classes: Vec<serde_json::Value> = classes
            .iter()
            .map(|class| {
                let limit = limits.of(class);
                json!({
                    "class": class,
                    "limit": (limit != usize::MAX).then_some(limit),
                    "holders": slots::holders(&root, class).into_iter().filter_map(|(_, h)| h).collect::<Vec<_>>(),
                    "waiting": slots::waiters(&root, class),
                })
            })
            .collect();
        let out = json!({
            "classes": classes,
            "load": p.load,
            "maxLoad": limits.max_load,
            "freeMb": p.free_mb,
            "minFreeMb": limits.min_free_mb,
            "cpus": slots::cpus(),
            "busy": p.too_high(&limits),
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(0);
    }
    let now = crate::time::now_ms();
    let mut rows = vec![];
    for class in &classes {
        let limit = limits.of(class);
        let held = slots::holders(&root, class);
        let waiting = slots::waiters(&root, class);
        let lim = if limit == usize::MAX {
            "-".to_string()
        } else {
            limit.to_string()
        };
        let mut who: Vec<String> = held
            .iter()
            .map(|(_, h)| match h {
                Some(h) => format!(
                    "{} ({})",
                    h.label(),
                    crate::runs::state::short_age(now - h.since)
                ),
                None => "?".into(),
            })
            .collect();
        who.extend(waiting.iter().map(|h| {
            format!(
                "waiting: {} ({})",
                h.label(),
                crate::runs::state::short_age(now - h.since)
            )
        }));
        rows.push(vec![
            class.clone(),
            format!("{}/{lim}", held.len()),
            waiting.len().to_string(),
            who.first().cloned().unwrap_or_else(|| "-".into()),
        ]);
        for w in who.iter().skip(1) {
            rows.push(vec![String::new(), String::new(), String::new(), w.clone()]);
        }
    }
    println!("{}", table(&["CLASS", "BUSY", "WAITING", "WHO"], &rows));
    let p = Pressure::now();
    println!(
        "\nLoad {:.2} per CPU (new work waits above {:.1}), {} free (waits below {} MB).",
        p.load,
        limits.max_load,
        p.free_mb
            .map_or("unknown memory".into(), |m| format!("{m} MB")),
        limits.min_free_mb
    );
    if let Some(why) = p.too_high(&limits) {
        println!("Right now {why}, so new work waits until something finishes.");
    }
    println!("Change the limits under \"limits\" in ~/.amux/config.json.");
    Ok(0)
}

/// The Claude Code hook that sends heavy commands through `amux slot`.
fn hook_config() -> Value {
    json!({ "hooks": { "PreToolUse": [{
        "matcher": "Bash",
        "hooks": [{ "type": "command", "command": "amux __slot-hook" }]
    }]}})
}

pub fn agent_instructions() -> &'static str {
    "Several agents share this machine. Run test suites, builds and dev servers through\n\
     `amux slot`, for example `amux slot --timeout 100 -- 'npm test'`. It waits for a free\n\
     slot and runs the command. If it exits with code 75, no slot came free in time: do not\n\
     retry in a loop. Commit and push the branch so CI runs the tests, or tell the user."
}

fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// `amux __slot-hook`: a Claude Code PreToolUse hook. It denies a Bash command that would
/// start a test suite, build or dev server outside `amux slot`, with the wrapped command to
/// use instead. Anything else, and any input it can't read, passes untouched.
pub fn hook_main() -> i32 {
    let mut input = String::new();
    if std::io::stdin()
        .take(1 << 20)
        .read_to_string(&mut input)
        .is_err()
    {
        return 0;
    }
    if let Some(out) = hook_decision(&input) {
        println!("{out}");
    }
    0
}

fn hook_decision(input: &str) -> Option<Value> {
    let v: Value = serde_json::from_str(input).ok()?;
    if v.get("tool_name")?.as_str()? != "Bash" {
        return None;
    }
    let cmd = v.get("tool_input")?.get("command")?.as_str()?;
    let class = slots::heavy_class(cmd)?;
    // A dev server started in the background holds its slot for as long as it runs.
    let wrapped = format!(
        "amux slot --class {class} --timeout 100 -- {}",
        sh_quote(cmd)
    );
    let reason = format!(
        "Several agents share this machine, so {class} commands take turns. Run this instead:\n\
         {wrapped}\n\
         It waits up to 100s for a free slot. If it exits with code 75, no slot came free: \
         don't retry in a loop. Commit and push so CI runs it, or tell the user. \
         `amux slots` shows who is running what."
    );
    Some(json!({ "hookSpecificOutput": {
        "hookEventName": "PreToolUse",
        "permissionDecision": "deny",
        "permissionDecisionReason": reason,
    }}))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parses_flags_then_the_command() {
        let a = args(&["--class", "dev", "--timeout=5", "npm", "run", "dev"]);
        let (o, cmd) = parse(&a).unwrap();
        assert_eq!(o.class.as_deref(), Some("dev"));
        assert_eq!(o.timeout, Some(Duration::from_secs(5)));
        assert_eq!(cmd, ["npm", "run", "dev"]);
        let a = args(&["--no-wait", "--", "--weird"]);
        let (o, cmd) = parse(&a).unwrap();
        assert_eq!(o.timeout, Some(Duration::ZERO));
        assert_eq!(cmd, ["--weird"]);
        assert!(parse(&args(&["--class", "../x", "ls"])).is_err());
        assert!(parse(&args(&["--timeout", "soon", "ls"])).is_err());
        assert!(parse(&args(&["--bogus", "ls"])).is_err());
        assert!(parse(&args(&["--class", "test"])).is_err());
    }

    #[test]
    fn one_argument_with_shell_syntax_goes_through_sh() {
        let c = command(&args(&["cd web && npm test"]));
        assert_eq!(c.get_program(), "sh");
        let c = command(&args(&["npm", "test"]));
        assert_eq!(c.get_program(), "npm");
    }

    #[test]
    fn hook_denies_unwrapped_heavy_commands_only() {
        let input =
            |cmd: &str| json!({"tool_name": "Bash", "tool_input": {"command": cmd}}).to_string();
        let out = hook_decision(&input("cd app && npm test")).expect("denied");
        let why = out["hookSpecificOutput"]["permissionDecisionReason"]
            .as_str()
            .unwrap();
        assert_eq!(out["hookSpecificOutput"]["permissionDecision"], "deny");
        assert!(why.contains("amux slot --class test --timeout 100 -- 'cd app && npm test'"));
        assert!(hook_decision(&input("git status")).is_none());
        assert!(hook_decision(&input("amux slot -- npm test")).is_none());
        assert!(hook_decision(&json!({"tool_name": "Read"}).to_string()).is_none());
        assert!(hook_decision("not json").is_none());
        assert_eq!(sh_quote("it's"), r"'it'\''s'");
    }
}
