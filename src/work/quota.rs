//! Quota: how much of each account's 5-hour and 7-day windows is used, read from local
//! files only. amux never asks a provider's servers.
//!
//! - Claude Code hands its status line command a JSON document on stdin that includes
//!   `rate_limits.five_hour` and `rate_limits.seven_day` (`used_percentage`, `resets_at` in
//!   epoch seconds) for claude.ai subscribers. `amux usage --track on` puts
//!   `amux __statusline` in front of your own status line command; it records the numbers
//!   for the account in `AMUX_ACCOUNT`, then runs your command.
//! - Codex writes `token_count` events to its session rollout files, with
//!   `rate_limits.primary` and `secondary` (`used_percent`, `window_minutes`, `resets_at` in
//!   epoch seconds). amux reads the newest one from the session of each Codex run.
//! - Gemini CLI keeps no such numbers on disk, so it has none.
//!
//! Each account's latest numbers are kept in ~/.amux/usage/<provider>@<name>.json. A window
//! whose reset time has passed is dropped when read: its numbers no longer say anything.

use crate::runs::{ProgramStatus, Run, Waiting};
use crate::store::{load_accounts, load_config, read_json, valid_name, write_json, Account};
use crate::{paths, time};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// Used to rank accounts when `limits.switchAt` isn't set: above this, an account counts
/// as nearly out.
pub const HIGH: f64 = 90.0;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Window {
    /// 0 to 100 (Codex can report more once over).
    pub used_percent: f64,
    /// When the window resets, in ms since the epoch.
    pub resets_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_minutes: Option<i64>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct Quota {
    /// When the CLI reported these numbers, in ms since the epoch.
    pub at: i64,
    /// Where they came from: "statusline" (Claude Code) or "rollout" (Codex).
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub five_hour: Option<Window>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seven_day: Option<Window>,
}

impl Quota {
    /// Without the windows that have reset since; None when none is left.
    pub fn fresh(mut self, now: i64) -> Option<Quota> {
        self.five_hour = self.five_hour.filter(|w| w.resets_at > now);
        self.seven_day = self.seven_day.filter(|w| w.resets_at > now);
        (self.five_hour.is_some() || self.seven_day.is_some()).then_some(self)
    }

    /// The tighter of the two windows: its name ("5-hour" or "7-day") and numbers.
    pub fn tightest(&self) -> Option<(&'static str, &Window)> {
        let a = self.five_hour.as_ref().map(|w| ("5-hour", w));
        let b = self.seven_day.as_ref().map(|w| ("7-day", w));
        match (a, b) {
            (Some(a), Some(b)) => Some(if b.1.used_percent > a.1.used_percent {
                b
            } else {
                a
            }),
            (a, b) => a.or(b),
        }
    }

    /// How much of the account is used: the higher of its two windows.
    pub fn used(&self) -> Option<f64> {
        self.tightest().map(|(_, w)| w.used_percent)
    }
}

pub fn dir() -> PathBuf {
    paths::AMUX_HOME.join("usage")
}

/// `provider@name` → its file, for valid account ids only (they become a file name).
fn file(account: &str) -> Option<PathBuf> {
    let (p, n) = account.split_once('@')?;
    (valid_name(p) && valid_name(n)).then(|| dir().join(format!("{account}.json")))
}

/// The account's numbers as last recorded, without windows that have reset.
pub fn load(account: &str) -> Option<Quota> {
    let f = file(account)?;
    let q: Option<Quota> = read_json(&f).ok()?;
    q?.fresh(time::now_ms())
}

/// Every account's numbers that are still current, by account id.
pub fn all(accounts: &[Account]) -> BTreeMap<String, Quota> {
    accounts
        .iter()
        .filter_map(|a| Some((a.id(), load(&a.id())?)))
        .collect()
}

/// Saves `q` as the account's latest numbers, unless what is stored is newer, or the same
/// numbers were saved less than a minute ago (Claude Code runs its status line often).
/// True when it wrote.
pub fn record(account: &str, q: &Quota) -> bool {
    let Some(f) = file(account) else {
        return false;
    };
    let old: Option<Quota> = read_json(&f).ok().flatten();
    if let Some(o) = &old {
        let same = o.five_hour == q.five_hour && o.seven_day == q.seven_day;
        if o.at > q.at || (same && q.at - o.at < 60_000) {
            return false;
        }
    }
    write_json(&f, q).is_ok()
}

/// A window from a percentage and a reset time in epoch seconds.
fn window(used: Option<f64>, resets_s: Option<i64>, minutes: Option<i64>) -> Option<Window> {
    Some(Window {
        used_percent: used?,
        resets_at: resets_s? * 1000,
        window_minutes: minutes,
    })
}

/// Claude Code's status line input → its numbers, if it has any (claude.ai subscribers,
/// after the session's first response).
pub fn from_statusline(v: &Value, now: i64) -> Option<Quota> {
    let rl = v.get("rate_limits")?;
    let w = |key: &str, minutes: i64| {
        let o = rl.get(key)?;
        window(
            o.get("used_percentage").and_then(Value::as_f64),
            o.get("resets_at").and_then(as_secs),
            Some(minutes),
        )
    };
    let q = Quota {
        at: now,
        source: "statusline".into(),
        five_hour: w("five_hour", 300),
        seven_day: w("seven_day", 10_080),
    };
    (q.five_hour.is_some() || q.seven_day.is_some()).then_some(q)
}

/// Epoch seconds, as an integer or a float.
fn as_secs(v: &Value) -> Option<i64> {
    v.as_i64().or_else(|| v.as_f64().map(|f| f as i64))
}

/// One Codex `RateLimitWindow`: `used_percent`, `window_minutes`, and `resets_at` (epoch
/// seconds), or `resets_in_seconds` in older versions (from the event's time).
fn codex_window(v: &Value, at: i64) -> Option<Window> {
    let used = v.get("used_percent").and_then(Value::as_f64)?;
    let minutes = v.get("window_minutes").and_then(Value::as_i64);
    let resets_at = match v.get("resets_at").and_then(as_secs) {
        Some(s) => s * 1000,
        None => match v.get("resets_in_seconds").and_then(as_secs) {
            Some(s) => at + s * 1000,
            None => at + minutes? * 60_000,
        },
    };
    Some(Window {
        used_percent: used,
        resets_at,
        window_minutes: minutes,
    })
}

/// The newest rate limits in a Codex rollout (JSON lines): the last
/// `{"type":"event_msg","payload":{"type":"token_count","rate_limits":{…}}}`. Codex's own
/// limit (`limit_id` "codex", or none) wins over per-model ones in the same file.
pub fn from_codex_rollout(text: &str) -> Option<Quota> {
    let mut other = None;
    for line in text.lines().rev() {
        if !line.contains("\"token_count\"") || !line.contains("rate_limits") {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let p = &v["payload"];
        if v["type"] != "event_msg" || p["type"] != "token_count" {
            continue;
        }
        let rl = &p["rate_limits"];
        if !rl.is_object() {
            continue;
        }
        let at = v["timestamp"]
            .as_str()
            .and_then(time::parse_iso)
            .unwrap_or_else(time::now_ms);
        let mut q = Quota {
            at,
            source: "rollout".into(),
            ..Default::default()
        };
        for (i, key) in ["primary", "secondary"].into_iter().enumerate() {
            let Some(w) = rl.get(key).and_then(|w| codex_window(w, at)) else {
                continue;
            };
            // By length when Codex says it; otherwise primary is the short window.
            let short = w.window_minutes.map_or(i == 0, |m| m <= 24 * 60);
            let slot = if short {
                &mut q.five_hour
            } else {
                &mut q.seven_day
            };
            if slot.is_none() {
                *slot = Some(w);
            }
        }
        if q.five_hour.is_none() && q.seven_day.is_none() {
            continue;
        }
        match rl["limit_id"].as_str() {
            None | Some("codex") => return Some(q),
            Some(_) => {
                other.get_or_insert(q);
            }
        }
    }
    other
}

/// The end of a file, up to `max` bytes.
fn tail(f: &Path, max: u64) -> Option<String> {
    let mut file = std::fs::File::open(f).ok()?;
    let len = file.metadata().ok()?.len();
    file.seek(SeekFrom::Start(len.saturating_sub(max))).ok()?;
    let mut buf = Vec::new();
    file.take(max).read_to_end(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// The newest rate limits in a rollout file: its last 256 KB, then its last 4 MB.
pub fn read_codex_rollout(f: &Path) -> Option<Quota> {
    for max in [256 * 1024, 4 * 1024 * 1024] {
        let text = tail(f, max)?;
        if let Some(q) = from_codex_rollout(&text) {
            return Some(q);
        }
        if (text.len() as u64) < max {
            break;
        }
    }
    None
}

/// What a rollout's `session_meta` line says: Codex writes it first when it creates a new
/// session's file (`rollout-<local time>-<thread id>.jsonl`); a resumed session appends to
/// its old file without a new one.
#[derive(Debug, PartialEq)]
struct CodexMeta {
    id: String,
    cwd: String,
    /// When the session started (`timestamp`, UTC with milliseconds), ms since the epoch.
    at: i64,
    /// A sub-agent's or Codex's own internal session (`source` is an object, or it has a
    /// `parent_thread_id`), not the one the run's CLI is having.
    child: bool,
}

/// The `session_meta` of a rollout file (it carries the base instructions, so it can run
/// long: read as much as sessions.rs does).
fn codex_meta(f: &Path) -> Option<CodexMeta> {
    let mut head = String::new();
    std::fs::File::open(f)
        .ok()?
        .take(256 * 1024)
        .read_to_string(&mut head)
        .ok();
    let v = head
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find(|v| v["type"] == "session_meta")?;
    let p = &v["payload"];
    Some(CodexMeta {
        id: p["id"].as_str()?.to_string(),
        cwd: p["cwd"].as_str()?.to_string(),
        at: p["timestamp"]
            .as_str()
            .or_else(|| v["timestamp"].as_str())
            .and_then(time::parse_iso)?,
        child: p["source"].is_object() || !p["parent_thread_id"].is_null(),
    })
}

/// The session a run's arguments resume, from the provider's `resumeArgs` template
/// (`["resume", "{id}"]`): `amux failover` and `amux revive` start runs that way.
pub fn resumed_session(args: &[String], template: &[String]) -> Option<String> {
    let at = template.iter().position(|t| t == "{id}")?;
    if args.len() < template.len() {
        return None;
    }
    (0..=args.len() - template.len()).find_map(|start| {
        let window = &args[start..start + template.len()];
        // `codex resume --last` names no session.
        let fits = !window[at].starts_with('-')
            && window
                .iter()
                .zip(template)
                .all(|(a, t)| t == "{id}" || a == t);
        fits.then(|| window[at].clone())
    })
}

/// The rollout file of a Codex run, and its session id. Every account's Codex sessions are
/// in the same `<home>/sessions`, so the folder alone doesn't tell runs apart:
///
/// - a run that knows its session (`run.session`: found earlier, or the one its arguments
///   resume) reads the file of that session;
/// - otherwise it is the session started in the run's folder soonest after the run itself
///   started that no other live run has claimed (`claimed`). Codex starts its session as
///   it starts up, after the run, so two runs in one folder each get their own.
///
/// Only files changed since the run started are opened. Every folder is listed: a
/// folder's own mtime doesn't change when a file in it is written to (a resumed session
/// appends to a file in an older day folder).
pub fn codex_rollout_of(run: &Run, home: &Path, claimed: &[String]) -> Option<(PathBuf, String)> {
    use crate::sessions::{mtime_ms, walk};
    let since = run.started_at - 60_000;
    let mut files = Vec::new();
    walk(
        &home.join("sessions"),
        &|n| n.ends_with(".jsonl"),
        3,
        &mut files,
    );
    let recent = files.into_iter().filter(|p| mtime_ms(p) >= since);
    if let Some(id) = &run.session {
        // Its file name ends with the thread id (or `<id>_<rollout id>` after a revert).
        return recent
            .filter(|p| p.to_string_lossy().contains(id.as_str()))
            .find(|p| codex_meta(p).is_some_and(|m| &m.id == id))
            .map(|p| (p, id.clone()));
    }
    // The slack covers a run record written just after its CLI started.
    let start = run.started_at - 2_000;
    recent
        .filter_map(|p| codex_meta(&p).map(|m| (m, p)))
        .filter(|(m, _)| m.cwd == run.cwd && !m.child && m.at >= start)
        .filter(|(m, _)| !claimed.contains(&m.id))
        .min_by_key(|(m, _)| m.at)
        .map(|(m, p)| (p, m.id))
}

/// The Codex sessions that other live runs in the run's folder already read.
fn claimed_sessions(run: &Run) -> Vec<String> {
    crate::runs::list()
        .into_iter()
        .filter(|r| r.id != run.id && r.cwd == run.cwd && r.state().is_live())
        .filter_map(|r| r.session)
        .collect()
}

/// Keeps a Codex run's numbers current: finds its rollout once, and rereads it when it
/// has changed. `seen` carries the file and its last change between calls. Returns the
/// session id when it was just found and the run didn't know it yet, for the caller to
/// keep in `run.session` (so other runs in the folder don't claim it).
pub fn refresh_codex(run: &Run, home: &Path, seen: &mut Option<(PathBuf, i64)>) -> Option<String> {
    let mut found = None;
    let path = match seen {
        Some((p, _)) if p.exists() => p.clone(),
        _ => {
            // Only a run still looking for its session needs the others' claims.
            let claimed = match run.session {
                Some(_) => vec![],
                None => claimed_sessions(run),
            };
            let (p, id) = codex_rollout_of(run, home, &claimed)?;
            if run.session.is_none() {
                found = Some(id);
            }
            p
        }
    };
    let modified = std::fs::metadata(&path)
        .and_then(|m| m.modified())
        .map(time::to_ms)
        .unwrap_or(0);
    if seen
        .as_ref()
        .is_some_and(|(p, t)| *p == path && *t == modified)
    {
        return found;
    }
    // Numbers from before the run started belong to whichever account ran the session
    // then (a session resumed by `amux failover` appends to its old file).
    if let Some(q) = read_codex_rollout(&path).filter(|q| q.at >= run.started_at) {
        record(&run.account, &q);
    }
    *seen = Some((path, modified));
    found
}

/// Reads the numbers of every live Codex run now, for `amux usage`.
pub fn refresh_live_codex_runs() {
    let Ok(specs) = crate::store::load_providers() else {
        return;
    };
    for run in crate::runs::list() {
        if !run.state().is_live() {
            continue;
        }
        let provider = run.account.split('@').next().unwrap_or_default();
        let Some(spec) = specs.iter().find(|s| s.id == provider) else {
            continue;
        };
        if spec.sessions.as_deref() != Some("codex") {
            continue;
        }
        // Only runs whose session is known: matching a new one is left to the run's own
        // host, which keeps what it finds in run.json for other runs to see.
        let mut run = run;
        if run.session.is_none() {
            run.session = spec
                .resume_args
                .as_deref()
                .and_then(|t| resumed_session(&run.args, t));
        }
        if run.session.is_some() {
            refresh_codex(&run, &spec.shared_home, &mut None);
        }
    }
}

/// `"limits": { "switchAt": 90 }`: the percentage of a window at which work moves to
/// another account before the limit. None when unset (or not between 1 and 100).
pub fn switch_at() -> Option<f64> {
    let cfg = load_config().ok()?;
    let v = cfg.extra.get("limits")?.get("switchAt")?;
    let n = v
        .as_f64()
        .or_else(|| v.as_str()?.trim_end_matches('%').parse().ok())?;
    (n > 0.0 && n <= 100.0).then_some(n)
}

/// Whether a run is between turns, so it can move to another account without cutting
/// the agent off mid-turn: the CLI reports it is idle or done (OSC 7501), or, for a CLI
/// that doesn't report, nothing is waiting on you and it has printed nothing for 10 s.
pub fn turn_boundary(
    status: Option<&ProgramStatus>,
    waiting: Option<&Waiting>,
    last_out: i64,
    now: i64,
) -> bool {
    if waiting.is_some() {
        return false;
    }
    match status {
        Some(s) => matches!(s.state.as_str(), "idle" | "done"),
        None => now - last_out >= 10_000,
    }
}

/// An account that work could go to.
#[derive(Clone, Debug)]
pub struct Candidate {
    pub id: String,
    pub used: Option<f64>,
    pub limited: bool,
    pub busy: usize,
    pub default: bool,
}

/// The best candidate: never `avoid` or a limited one, and none at or over `ceiling` when
/// given. Accounts under `high` come first, least used first; then accounts with no
/// numbers; then the rest. Ties go to the default account, then the one with fewer runs.
pub fn choose(c: &[Candidate], avoid: &str, ceiling: Option<f64>, high: f64) -> Option<usize> {
    let tier = |u: Option<f64>| match u {
        Some(u) if u < high => 0,
        None => 1,
        Some(_) => 2,
    };
    c.iter()
        .enumerate()
        .filter(|(_, a)| a.id != avoid && !a.limited)
        .filter(|(_, a)| match (ceiling, a.used) {
            (Some(m), Some(u)) => u < m,
            _ => true,
        })
        .min_by_key(|(_, a)| {
            (
                tier(a.used),
                (a.used.unwrap_or(0.0) * 10.0) as i64,
                !a.default,
                a.busy,
            )
        })
        .map(|(i, _)| i)
}

/// The accounts of `provider` as candidates, from what is on disk now.
pub fn candidates(provider: &str) -> Vec<(Account, Candidate)> {
    let limited = crate::work::limits::active();
    let runs = crate::runs::list();
    let default = load_config()
        .ok()
        .and_then(|c| c.default_for(provider).map(str::to_string));
    load_accounts()
        .unwrap_or_default()
        .into_iter()
        .filter(|a| a.provider == provider)
        .map(|a| {
            let id = a.id();
            let c = Candidate {
                used: load(&id).and_then(|q| q.used()),
                limited: limited.contains_key(&id),
                busy: runs
                    .iter()
                    .filter(|r| r.account == id && r.state().is_live())
                    .count(),
                default: default.as_deref() == Some(a.name.as_str()),
                id,
            };
            (a, c)
        })
        .collect()
}

/// The best account of `provider` for work, as `choose` ranks them, with its numbers.
pub fn best(
    provider: &str,
    avoid: &str,
    ceiling: Option<f64>,
    high: f64,
) -> Option<(Account, Candidate)> {
    let mut all = candidates(provider);
    let c: Vec<Candidate> = all.iter().map(|(_, c)| c.clone()).collect();
    let i = choose(&c, avoid, ceiling, high)?;
    Some(all.swap_remove(i))
}

/// Where a run on `account` should move before the limit: another account of the same
/// CLI that is under `switch_at`, if any. Accounts at or over it are never picked, so a
/// session doesn't bounce back to an account that is just as full.
pub fn pick_switch(provider: &str, account: &str, switch_at: f64) -> Option<Account> {
    best(provider, account, Some(switch_at), switch_at).map(|(a, _)| a)
}

/// "92% of its 5-hour window, resets in 1h 3m".
pub fn describe(q: &Quota, now: i64) -> String {
    match q.tightest() {
        Some((name, w)) => format!(
            "{:.0}% of its {name} window, resets {}",
            w.used_percent,
            until(w.resets_at, now)
        ),
        None => "no numbers".into(),
    }
}

/// "in 2h 5m", "in 40m", "in 3d 4h".
pub fn until(ms: i64, now: i64) -> String {
    let m = ((ms - now).max(0) + 59_999) / 60_000;
    if m >= 24 * 60 {
        format!("in {}d {}h", m / 1440, m % 1440 / 60)
    } else if m >= 60 {
        format!("in {}h {}m", m / 60, m % 60)
    } else {
        format!("in {m}m")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reads_claude_code_status_line_input() {
        let v = json!({
            "model": {"display_name": "Opus"},
            "rate_limits": {
                "five_hour": {"used_percentage": 23.5, "resets_at": 1738425600},
                "seven_day": {"used_percentage": 41.2, "resets_at": 1738857600}
            }
        });
        let q = from_statusline(&v, 1_738_420_000_000).unwrap();
        assert_eq!(q.source, "statusline");
        assert_eq!(q.five_hour.as_ref().unwrap().used_percent, 23.5);
        assert_eq!(q.five_hour.as_ref().unwrap().resets_at, 1_738_425_600_000);
        assert_eq!(q.seven_day.as_ref().unwrap().used_percent, 41.2);
        assert_eq!(q.used(), Some(41.2));
        assert_eq!(q.tightest().unwrap().0, "7-day");
        // One window only, and none at all (API keys, or before the first response).
        let one =
            json!({"rate_limits": {"five_hour": {"used_percentage": 5, "resets_at": 1738425600}}});
        assert!(from_statusline(&one, 0).unwrap().seven_day.is_none());
        assert!(from_statusline(&json!({"model": {}}), 0).is_none());
        assert!(from_statusline(&json!({"rate_limits": {}}), 0).is_none());
    }

    #[test]
    fn drops_windows_that_have_reset() {
        let q = Quota {
            at: 0,
            source: "statusline".into(),
            five_hour: Some(Window {
                used_percent: 95.0,
                resets_at: 1000,
                window_minutes: None,
            }),
            seven_day: Some(Window {
                used_percent: 30.0,
                resets_at: 5000,
                window_minutes: None,
            }),
        };
        let f = q.clone().fresh(2000).unwrap();
        assert!(f.five_hour.is_none());
        assert_eq!(f.used(), Some(30.0));
        assert!(q.fresh(6000).is_none());
    }

    #[test]
    fn reads_the_newest_rate_limits_from_a_codex_rollout() {
        let line = |ts: &str, rl: Value| {
            json!({"timestamp": ts, "type": "event_msg",
                   "payload": {"type": "token_count", "info": null, "rate_limits": rl}})
            .to_string()
        };
        let text = [
            json!({"timestamp": "2026-11-19T10:00:00.000Z", "type": "session_meta",
                   "payload": {"id": "x", "cwd": "/w"}})
            .to_string(),
            line(
                "2026-11-19T10:01:00.000Z",
                json!({"limit_id": "codex", "primary": {"used_percent": 10.0, "window_minutes": 300, "resets_at": 1_800_000_000},
                       "secondary": {"used_percent": 3.0, "window_minutes": 10080, "resets_at": 1_800_500_000}}),
            ),
            line(
                "2026-11-19T10:02:00.000Z",
                json!({"limit_id": "codex", "primary": {"used_percent": 42.5, "window_minutes": 300, "resets_at": 1_800_000_000},
                       "secondary": {"used_percent": 7.0, "window_minutes": 10080, "resets_at": 1_800_500_000}}),
            ),
            // A per-model limit after it doesn't replace Codex's own.
            line(
                "2026-11-19T10:02:30.000Z",
                json!({"limit_id": "other_model", "primary": {"used_percent": 99.0, "window_minutes": 300, "resets_at": 1_800_000_000}}),
            ),
            // No rate limits on this one.
            json!({"timestamp": "2026-11-19T10:03:00.000Z", "type": "event_msg",
                   "payload": {"type": "token_count", "info": {}, "rate_limits": null}})
            .to_string(),
            "{\"cut off".to_string(),
        ]
        .join("\n");
        let q = from_codex_rollout(&text).unwrap();
        assert_eq!(q.source, "rollout");
        assert_eq!(q.at, time::parse_iso("2026-11-19T10:02:00.000Z").unwrap());
        assert_eq!(q.five_hour.as_ref().unwrap().used_percent, 42.5);
        assert_eq!(q.five_hour.as_ref().unwrap().resets_at, 1_800_000_000_000);
        assert_eq!(q.seven_day.as_ref().unwrap().used_percent, 7.0);
        assert!(from_codex_rollout("{\"type\":\"session_meta\"}").is_none());
    }

    #[test]
    fn reads_older_codex_rate_limits_with_relative_resets() {
        let text = json!({"timestamp": "2025-09-20T10:00:00.000Z", "type": "event_msg",
            "payload": {"type": "token_count", "rate_limits": {
                "primary": {"used_percent": 50.0, "window_minutes": 299, "resets_in_seconds": 600},
                "secondary": {"used_percent": 20.0, "window_minutes": 10079, "resets_in_seconds": 86400}}}})
        .to_string();
        let q = from_codex_rollout(&text).unwrap();
        let at = time::parse_iso("2025-09-20T10:00:00.000Z").unwrap();
        assert_eq!(q.five_hour.unwrap().resets_at, at + 600_000);
        assert_eq!(q.seven_day.unwrap().resets_at, at + 86_400_000);
    }

    /// A rollout as Codex writes it: `session_meta` first (its own timestamp is when the
    /// session started), then a `token_count` event with the account's rate limits.
    fn rollout(day: &Path, id: &str, cwd: &str, at: i64, used: f64) -> PathBuf {
        let meta = json!({"timestamp": time::iso(at), "type": "session_meta",
               "payload": {"id": id, "session_id": id, "timestamp": time::iso(at), "cwd": cwd,
                           "originator": "codex_cli_rs", "cli_version": "0.80.0", "source": "cli"}})
        .to_string();
        let count = json!({"timestamp": time::iso(at + 1_000), "type": "event_msg",
            "payload": {"type": "token_count", "rate_limits": {
                "primary": {"used_percent": used, "window_minutes": 300, "resets_at": 4_000_000_000i64}}}})
        .to_string();
        let f = day.join(format!("rollout-2026-11-19T10-00-00-{id}.jsonl"));
        crate::testutil::write(&f, &format!("{meta}\n{count}\n"));
        f
    }

    #[test]
    fn finds_a_codex_runs_rollout_by_its_folder() {
        let home = crate::testutil::TempDir::new();
        let day = home.path().join("sessions/2026/11/19");
        let now = time::now_ms();
        rollout(&day, "aaaa", "/work/api", now + 500, 61.0);
        rollout(&day, "bbbb", "/work/other", now + 400, 5.0);
        let run = Run {
            cwd: "/work/api".into(),
            started_at: now,
            ..Default::default()
        };
        let (f, id) = codex_rollout_of(&run, home.path(), &[]).unwrap();
        assert_eq!(id, "aaaa");
        assert_eq!(read_codex_rollout(&f).unwrap().used(), Some(61.0));
        // Sessions from before the run started aren't its own.
        let later = Run {
            started_at: now + 3_600_000,
            ..run
        };
        assert!(codex_rollout_of(&later, home.path(), &[]).is_none());
    }

    #[test]
    fn two_codex_runs_in_one_folder_each_read_their_own_rollout() {
        let home = crate::testutil::TempDir::new();
        let day = home.path().join("sessions/2026/11/19");
        let now = time::now_ms();
        // Run a (work@a) started first; run b (work@b) ten seconds later, same folder.
        // b's session is written to last, so it is also the newest file.
        rollout(&day, "sess-a", "/work/api", now - 9_000, 20.0);
        rollout(&day, "sess-b", "/work/api", now + 1_000, 80.0);
        // A sub-agent of b's session, started later still, is nobody's run.
        let sub = day.join("rollout-2026-11-19T10-00-01-sub.jsonl");
        crate::testutil::write(
            &sub,
            &format!(
                "{}\n",
                json!({"type": "session_meta", "payload": {"id": "sub", "timestamp": time::iso(now + 2_000),
                    "cwd": "/work/api", "source": {"subagent": "review"}}})
            ),
        );
        let a = Run {
            id: "a".into(),
            account: "codex@a".into(),
            cwd: "/work/api".into(),
            started_at: now - 10_000,
            ..Default::default()
        };
        let b = Run {
            id: "b".into(),
            account: "codex@b".into(),
            started_at: now,
            ..a.clone()
        };
        let (fa, ida) = codex_rollout_of(&a, home.path(), &[]).unwrap();
        let (fb, idb) = codex_rollout_of(&b, home.path(), &[]).unwrap();
        assert_eq!((ida.as_str(), idb.as_str()), ("sess-a", "sess-b"));
        assert_eq!(read_codex_rollout(&fa).unwrap().used(), Some(20.0));
        assert_eq!(read_codex_rollout(&fb).unwrap().used(), Some(80.0));
        // Once a has claimed its session, a run started at the same time as a takes the
        // next one rather than a's.
        let twin = Run {
            id: "twin".into(),
            ..a.clone()
        };
        let (_, id) = codex_rollout_of(&twin, home.path(), &["sess-a".into()]).unwrap();
        assert_eq!(id, "sess-b");
        // A run that knows its session reads that one, whenever it started.
        let known = Run {
            session: Some("sess-a".into()),
            ..b.clone()
        };
        let (f, _) = codex_rollout_of(&known, home.path(), &[]).unwrap();
        assert_eq!(f, fa);
    }

    #[test]
    fn a_resumed_runs_session_comes_from_its_arguments() {
        let t: Vec<String> = vec!["resume".into(), "{id}".into()];
        let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            resumed_session(&args(&["--yolo", "resume", "abc-123"]), &t).as_deref(),
            Some("abc-123")
        );
        assert_eq!(resumed_session(&args(&["exec", "fix it"]), &t), None);
        assert_eq!(resumed_session(&args(&["resume"]), &t), None);
        assert_eq!(resumed_session(&args(&["resume", "--last"]), &t), None);
    }

    fn cand(id: &str, used: Option<f64>, limited: bool, busy: usize, default: bool) -> Candidate {
        Candidate {
            id: id.into(),
            used,
            limited,
            busy,
            default,
        }
    }

    #[test]
    fn picks_the_account_with_the_most_headroom() {
        let c = [
            cand("c@a", Some(50.0), false, 0, true),
            cand("c@b", Some(10.0), false, 2, false),
            cand("c@c", Some(5.0), true, 0, false),
            cand("c@d", None, false, 0, false),
            cand("c@e", Some(95.0), false, 0, false),
        ];
        // Least used, never a limited one.
        assert_eq!(choose(&c, "", Some(90.0), 90.0), Some(1));
        // Accounts at or over the ceiling are skipped; unknown ones come after known ones.
        let high = [
            cand("c@a", Some(92.0), false, 0, true),
            cand("c@d", None, false, 0, false),
        ];
        assert_eq!(choose(&high, "c@x", Some(90.0), 90.0), Some(1));
        assert_eq!(choose(&high, "c@d", Some(90.0), 90.0), None);
        // Without a ceiling (a hard limit), a high account is still better than none.
        assert_eq!(choose(&high, "c@d", None, 90.0), Some(0));
        // Ties go to the default account, then the one with fewer runs.
        let tie = [
            cand("c@a", None, false, 3, false),
            cand("c@b", None, false, 1, false),
            cand("c@c", None, false, 5, true),
        ];
        assert_eq!(choose(&tie, "", None, 90.0), Some(2));
        assert_eq!(choose(&tie, "c@c", None, 90.0), Some(1));
    }

    #[test]
    fn switches_only_between_turns() {
        let status = |s: &str| ProgramStatus {
            state: s.into(),
            kind: None,
            message: None,
            since: 0,
            via: None,
        };
        let now = 100_000;
        // Reported state decides when the CLI reports it.
        assert!(turn_boundary(Some(&status("idle")), None, now, now));
        assert!(turn_boundary(Some(&status("done")), None, now, now));
        assert!(!turn_boundary(Some(&status("working")), None, 0, now));
        assert!(!turn_boundary(Some(&status("blocked")), None, 0, now));
        // A prompt waiting on you is mid-turn.
        let w = Waiting {
            reason: "Do you want to proceed?".into(),
            since: 0,
            source: None,
        };
        assert!(!turn_boundary(Some(&status("idle")), Some(&w), 0, now));
        // Otherwise: quiet for 10 seconds.
        assert!(!turn_boundary(None, None, now - 3_000, now));
        assert!(turn_boundary(None, None, now - 10_000, now));
    }

    #[test]
    fn formats_time_until_a_reset() {
        assert_eq!(until(0, 0), "in 0m");
        assert_eq!(until(40 * 60_000, 0), "in 40m");
        assert_eq!(until(125 * 60_000, 0), "in 2h 5m");
        assert_eq!(until((3 * 24 + 4) * 3_600_000, 0), "in 3d 4h");
    }
}
