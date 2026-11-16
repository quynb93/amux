//! Driving runs from scripts and other agents, without attaching to them:
//!
//!   amux send <run> <text>       type a prompt into a run and press Enter
//!   amux wait <run>... --until   block until runs need you, finish their turn, or exit
//!   amux diff <run>              what a run changed in its worktree
//!
//! With these, one agent can start helpers (`amux spawn -p …`), steer them (`send`), and
//! pick up their results (`wait`, `diff`, `peek`), and a script can do the same.

use crate::git::{base_of, git};
use crate::runs::client::send_text;
use crate::runs::state::{run_kind, Kind};
use crate::runs::{self, Run};
use crate::time;
use crate::util::need;
use anyhow::{anyhow, bail, Context, Result};
use std::io::{IsTerminal, Read};
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

/// The exit code for "the time ran out", as timeout(1) uses.
const TIMED_OUT: i32 = 124;

const SEND_USAGE: &str =
    "amux send <run> [--no-enter] [--wait [--timeout <secs>]] <text...> | - | --key <key>...";

pub fn send_cmd(argv: &[String]) -> Result<i32> {
    let mut submit = true;
    let mut wait = false;
    let mut timeout = None;
    let mut rest = vec![];
    let mut keys = vec![];
    let mut it = argv.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--key" | "-k" => keys.push(need(it.next(), SEND_USAGE)?.to_string()),
            "--no-enter" => submit = false,
            "--wait" | "-w" => wait = true,
            "--timeout" | "-t" => timeout = Some(secs(it.next())?),
            "--" => {
                rest.extend(it.by_ref().cloned());
                break;
            }
            f if f.starts_with("--") => bail!("Unknown option {f}. Usage: {SEND_USAGE}"),
            _ => rest.push(a.clone()),
        }
    }
    let run = runs::find(need(rest.first(), SEND_USAGE)?)?;
    if !keys.is_empty() {
        for k in &keys {
            crate::runs::client::send_key(&run, k)?;
        }
        eprintln!("amux: sent {} to {}", keys.join(" "), run.id);
        return Ok(0);
    }
    let words = &rest[1..];
    let text = if words.is_empty() || words == ["-"] {
        if std::io::stdin().is_terminal() {
            bail!("Usage: {SEND_USAGE}");
        }
        let mut s = String::new();
        std::io::stdin().take(1 << 20).read_to_string(&mut s)?;
        s
    } else {
        words.join(" ")
    };
    send_text(&run, &text, submit)?;
    if !wait {
        eprintln!("amux: sent to {}", run.id);
        return Ok(0);
    }
    // The run may still look idle from before the prompt: wait for it to take it first.
    let started = Instant::now();
    let took = poll(
        std::slice::from_ref(&run.id),
        Some(Duration::from_secs(10)),
        |k| matches!(k, Kind::Working | Kind::Attention) || !k.is_live(),
    )?;
    if !took {
        eprintln!("amux: {} didn't start working within 10s.", run.id);
    }
    let left = timeout.map(|t| t.saturating_sub(started.elapsed()));
    report(&[run.id], left, Until::Idle)
}

fn secs(v: Option<&String>) -> Result<Duration> {
    let v = v.ok_or_else(|| anyhow!("--timeout needs seconds."))?;
    v.parse::<f64>()
        .ok()
        .filter(|s| s.is_finite() && *s >= 0.0)
        .map(Duration::from_secs_f64)
        .ok_or_else(|| anyhow!("--timeout takes seconds, not \"{v}\"."))
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Until {
    /// Asking you something.
    Waiting,
    /// Its turn is over: waiting for you, idle, or ended.
    Idle,
    /// Ended.
    Exited,
}

impl Until {
    fn parse(s: &str) -> Option<Until> {
        Some(match s {
            "waiting" | "blocked" => Until::Waiting,
            "idle" | "done" | "turn" => Until::Idle,
            "exited" | "exit" | "ended" => Until::Exited,
            _ => return None,
        })
    }

    fn reached(self, k: Kind) -> bool {
        match self {
            Until::Waiting => k == Kind::Attention,
            Until::Idle => matches!(k, Kind::Attention | Kind::Idle) || !k.is_live(),
            Until::Exited => !k.is_live(),
        }
    }
}

fn kind_of(run: &Run) -> (Kind, String) {
    run_kind(run, time::now_ms())
}

/// Polls until `done` holds for every run, or the time runs out (returns false).
fn poll(ids: &[String], timeout: Option<Duration>, done: impl Fn(Kind) -> bool) -> Result<bool> {
    let start = Instant::now();
    loop {
        let mut all = true;
        for id in ids {
            let run = runs::load(id)?;
            // A run that ended can't change any more: count it as there, so a wait
            // for "waiting" doesn't hang on it.
            let (k, _) = kind_of(&run);
            if !(done(k) || !k.is_live()) {
                all = false;
            }
        }
        if all {
            return Ok(true);
        }
        if timeout.is_some_and(|t| start.elapsed() >= t) {
            return Ok(false);
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

fn report(ids: &[String], timeout: Option<Duration>, until: Until) -> Result<i32> {
    let reached = poll(ids, timeout, |k| until.reached(k))?;
    for id in ids {
        let run = runs::load(id)?;
        let (_, label) = kind_of(&run);
        println!("{id}\t{label}");
    }
    Ok(if reached { 0 } else { TIMED_OUT })
}

const WAIT_USAGE: &str = "amux wait <run>... [--until waiting|idle|exited] [--timeout <secs>]";

pub fn wait_cmd(argv: &[String]) -> Result<i32> {
    let mut until = Until::Idle;
    let mut timeout = None;
    let mut ids = vec![];
    let mut it = argv.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--until" | "-u" => {
                let v = it.next().map(String::as_str).unwrap_or_default();
                until = Until::parse(v)
                    .ok_or_else(|| anyhow!("--until takes waiting, idle or exited."))?;
            }
            "--timeout" | "-t" => timeout = Some(secs(it.next())?),
            f if f.starts_with('-') => bail!("Unknown option {f}. Usage: {WAIT_USAGE}"),
            id => ids.push(runs::find(id)?.id),
        }
    }
    if ids.is_empty() {
        bail!("Usage: {WAIT_USAGE}");
    }
    report(&ids, timeout, until)
}

const DIFF_USAGE: &str = "amux diff <run> [--stat] [--color]";

/// `amux diff <run>`: everything the run changed, committed or not, against the commit
/// its worktree started from (for a run without a worktree, its folder against HEAD).
pub fn diff_cmd(argv: &[String]) -> Result<i32> {
    let stat = argv.iter().any(|a| a == "--stat");
    let color = argv.iter().any(|a| a == "--color");
    let id = need(argv.iter().find(|a| !a.starts_with('-')), DIFF_USAGE)?;
    let run = runs::find(id)?;
    let (dir, base) = match &run.worktree {
        Some(wt) => (
            PathBuf::from(&wt.path),
            base_of(&run)
                .ok_or_else(|| anyhow!("Can't tell where {}'s branch started.", run.id))?,
        ),
        None => (PathBuf::from(&run.cwd), "HEAD".to_string()),
    };
    if !dir.is_dir() {
        bail!(
            "{}'s folder {} is gone.",
            run.id,
            crate::paths::tildify(&dir)
        );
    }
    let untracked = git(&dir, &["ls-files", "--others", "--exclude-standard"]).unwrap_or_default();
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(&dir).arg("diff");
    if color {
        cmd.arg("--color=always");
    }
    if stat {
        cmd.arg("--stat");
    }
    // `--` so a base can never be read as an option or a path.
    cmd.arg(&base).arg("--");
    let status = cmd.status().context("could not run git")?;
    let new: Vec<&str> = untracked.lines().filter(|l| !l.is_empty()).collect();
    if !new.is_empty() {
        println!(
            "\nNew files not yet added to git ({}): {}",
            new.len(),
            crate::util::one_line(&new.join(", "), 400)
        );
    }
    Ok(status.code().unwrap_or(1))
}

/// What `amux events` compares between looks: whatever a person or script would act on.
#[derive(Clone, Debug, PartialEq)]
struct Snapshot {
    kind: Kind,
    queued: bool,
    reason: Option<String>,
    verify: Option<String>,
    checks: Option<String>,
}

fn snapshot(run: &Run, now: i64) -> Snapshot {
    Snapshot {
        kind: run_kind(run, now).0,
        queued: run.state() == runs::State::Queued,
        reason: run.waiting.as_ref().map(|w| w.reason.clone()),
        verify: crate::work::verify::load(&run.id).map(|v| v.label()),
        checks: crate::work::github::load(&run.id).map(|p| p.label()),
    }
}

/// The events between two looks at one run, as JSON objects.
fn changes(
    id: &str,
    before: Option<&Snapshot>,
    now: Option<&Snapshot>,
    at: i64,
) -> Vec<serde_json::Value> {
    use serde_json::json;
    let ev = |event: &str, extra: serde_json::Value| {
        let mut v = json!({ "time": at, "run": id, "event": event });
        if let (Some(o), Some(e)) = (v.as_object_mut(), extra.as_object()) {
            o.extend(e.clone());
        }
        v
    };
    let state = |s: &Snapshot| if s.queued { "queued" } else { s.kind.name() };
    match (before, now) {
        (None, Some(n)) => vec![ev("added", json!({ "state": state(n) }))],
        (Some(_), None) => vec![ev("removed", json!({}))],
        (Some(b), Some(n)) => {
            let mut out = vec![];
            if state(b) != state(n) || (n.kind == Kind::Attention && b.reason != n.reason) {
                out.push(ev(
                    "state",
                    json!({ "from": state(b), "to": state(n), "reason": n.reason }),
                ));
            }
            if b.verify != n.verify && n.verify.is_some() {
                out.push(ev("verify", json!({ "to": n.verify })));
            }
            if b.checks != n.checks && n.checks.is_some() {
                out.push(ev("pr", json!({ "to": n.checks })));
            }
            out
        }
        (None, None) => vec![],
    }
}

const EVENTS_USAGE: &str = "amux events [<run>...] [--text]";

/// `amux events`: one line per change, as it happens: a run added or removed, its state
/// (working, waiting with the reason, idle, done…), a verify result, its PR checks. JSON
/// lines by default, for scripts and agents; `--text` for people.
pub fn events_cmd(argv: &[String]) -> Result<i32> {
    use std::collections::HashMap;
    use std::io::Write;
    let text = argv.iter().any(|a| a == "--text");
    if let Some(f) = argv.iter().find(|a| a.starts_with('-') && *a != "--text") {
        bail!("Unknown option {f}. Usage: {EVENTS_USAGE}");
    }
    let only: Vec<String> = argv
        .iter()
        .filter(|a| !a.starts_with('-'))
        .map(|a| runs::find(a).map(|r| r.id))
        .collect::<Result<_>>()?;
    let mut seen: HashMap<String, Snapshot> = HashMap::new();
    let mut first = true;
    loop {
        let now = time::now_ms();
        let mut current: HashMap<String, Snapshot> = HashMap::new();
        for run in runs::list() {
            if only.is_empty() || only.contains(&run.id) {
                current.insert(run.id.clone(), snapshot(&run, now));
            }
        }
        if !first {
            let mut ids: Vec<&String> = seen.keys().chain(current.keys()).collect();
            ids.sort();
            ids.dedup();
            let mut out = std::io::stdout().lock();
            for id in ids {
                for e in changes(id, seen.get(id), current.get(id), now) {
                    let line = if text {
                        let what = match e["event"].as_str().unwrap_or_default() {
                            "state" => format!(
                                "{} → {}{}",
                                e["from"].as_str().unwrap_or_default(),
                                e["to"].as_str().unwrap_or_default(),
                                e["reason"]
                                    .as_str()
                                    .map(|r| format!(": {r}"))
                                    .unwrap_or_default()
                            ),
                            "added" => {
                                format!("started ({})", e["state"].as_str().unwrap_or_default())
                            }
                            "removed" => "removed".into(),
                            other => format!("{other}: {}", e["to"].as_str().unwrap_or_default()),
                        };
                        // UTC, like the times in JSON.
                        format!("{}Z  {id}  {what}", &time::iso(now)[11..19])
                    } else {
                        e.to_string()
                    };
                    // A reader that went away (`| head`) ends the stream.
                    if writeln!(out, "{line}").and_then(|_| out.flush()).is_err() {
                        return Ok(0);
                    }
                }
            }
        }
        first = false;
        seen = current;
        std::thread::sleep(Duration::from_millis(250));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_report_what_changed() {
        let s = |kind, reason: Option<&str>| Snapshot {
            kind,
            queued: false,
            reason: reason.map(str::to_string),
            verify: None,
            checks: None,
        };
        let work = s(Kind::Working, None);
        let ask = s(Kind::Attention, Some("approve Write: a.rs"));
        assert_eq!(changes("r", None, Some(&work), 1)[0]["event"], "added");
        let e = &changes("r", Some(&work), Some(&ask), 1)[0];
        assert_eq!(
            (e["from"].as_str(), e["to"].as_str()),
            (Some("working"), Some("waiting"))
        );
        assert_eq!(e["reason"], "approve Write: a.rs");
        assert!(changes("r", Some(&work), Some(&work), 1).is_empty());
        let mut tested = work.clone();
        tested.verify = Some("tests passed".into());
        assert_eq!(
            changes("r", Some(&work), Some(&tested), 1)[0]["event"],
            "verify"
        );
        assert_eq!(changes("r", Some(&work), None, 1)[0]["event"], "removed");
    }

    #[test]
    fn until_conditions() {
        assert!(Until::Idle.reached(Kind::Idle));
        assert!(Until::Idle.reached(Kind::Attention));
        assert!(Until::Idle.reached(Kind::Done));
        assert!(!Until::Idle.reached(Kind::Working));
        assert!(!Until::Waiting.reached(Kind::Idle));
        assert!(Until::Exited.reached(Kind::Failed));
        assert!(!Until::Exited.reached(Kind::Attention));
        assert_eq!(Until::parse("blocked"), Some(Until::Waiting));
        assert_eq!(Until::parse("soon"), None);
    }
}
