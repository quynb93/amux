//! Guards against runaway and stuck agents.
//!
//! An agent can keep "working" for hours without changing anything (a loop, a hung tool,
//! a test it can't fix), and use up a subscription's window while it does. The run host
//! keeps an eye on that: every so often it takes a fingerprint of the run's files (HEAD
//! and uncommitted changes). If the run has been working all along and the fingerprint
//! hasn't moved for the `stall` time, the run is flagged as stalled. A run alive longer
//! than `maxTime` is flagged too. Flagged runs show under "needs you", with a
//! notification and the `stalled` hook, and `onStall` can do more:
//!
//!   notify     (default) only tell you
//!   interrupt  press Esc in the run, which stops the agent's current turn
//!   stop       end the run (SIGTERM)
//!
//! From `"limits": { "stall": "30m", "maxTime": "4h", "onStall": "notify" }` in
//! config.json, or per run with `amux spawn --stall 20m --max-time 2h`. "0" turns one off.

use crate::store::load_config;
use serde::{Deserialize, Serialize};
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::process::Stdio;

/// By default a run that works for half an hour without changing a file is flagged.
const DEFAULT_STALL_MS: u64 = 30 * 60_000;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct Guard {
    /// Working this long with no change to the run's files is a stall. 0: off.
    pub stall_ms: u64,
    /// Alive this long is too long. 0: off.
    pub max_ms: u64,
    /// notify, interrupt or stop.
    pub action: String,
}

/// "30m", "2h", "90s", "1h30m", "45" (minutes) → ms. "0" or "off" → 0.
pub fn parse_duration(s: &str) -> Option<u64> {
    let s = s.trim().to_ascii_lowercase();
    if s == "off" || s == "0" {
        return Some(0);
    }
    if let Ok(m) = s.parse::<u64>() {
        return Some(m * 60_000);
    }
    let mut total = 0u64;
    let mut num = String::new();
    for c in s.chars() {
        if c.is_ascii_digit() {
            num.push(c);
            continue;
        }
        let n: u64 = num.parse().ok()?;
        num.clear();
        total += n * match c {
            'd' => 86_400_000,
            'h' => 3_600_000,
            'm' => 60_000,
            's' => 1000,
            _ => return None,
        };
    }
    (num.is_empty() && total > 0).then_some(total)
}

impl Guard {
    /// The configured guard, with per-run overrides (`--stall`, `--max-time`).
    pub fn from_config(stall: Option<u64>, max: Option<u64>) -> Guard {
        let cfg = load_config().unwrap_or_default();
        let l = cfg.extra.get("limits");
        let get = |k: &str| {
            l.and_then(|l| l.get(k))
                .and_then(|v| v.as_str().map(str::to_string))
        };
        Guard {
            stall_ms: stall
                .or_else(|| get("stall").and_then(|s| parse_duration(&s)))
                .unwrap_or(DEFAULT_STALL_MS),
            max_ms: max
                .or_else(|| get("maxTime").and_then(|s| parse_duration(&s)))
                .unwrap_or(0),
            action: get("onStall")
                .filter(|a| matches!(a.as_str(), "notify" | "interrupt" | "stop"))
                .unwrap_or_else(|| "notify".into()),
        }
    }
}

/// A fingerprint of a run's files: its commit and its uncommitted changes. None outside
/// a git repository, where a stall can't be told from slow work.
pub fn fingerprint(dir: &Path) -> Option<u64> {
    let run = |args: &[&str]| -> Option<Vec<u8>> {
        let out = crate::git::run_git_cmd(dir)
            .args(args)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()?;
        out.status.success().then_some(out.stdout)
    };
    let mut h = std::collections::hash_map::DefaultHasher::new();
    run(&["rev-parse", "HEAD"])?.hash(&mut h);
    run(&["status", "--porcelain"])?.hash(&mut h);
    run(&["diff", "--no-ext-diff"])?.hash(&mut h);
    Some(h.finish())
}

/// "34m", "2h 5m".
pub fn minutes(ms: u64) -> String {
    let m = ms / 60_000;
    if m >= 60 {
        format!("{}h {}m", m / 60, m % 60)
    } else {
        format!("{m}m")
    }
}

/// Follows one run: call `tick` now and then with what the host knows.
#[derive(Debug)]
pub struct Watch {
    pub guard: Guard,
    last_print: Option<u64>,
    /// Since when it has been working without its files changing.
    unchanged_since: i64,
    pub stalled: bool,
    pub overtime: bool,
}

/// What `tick` found.
#[derive(Debug, PartialEq)]
pub enum Verdict {
    Fine,
    /// Flag it, with this reason.
    Stalled(String),
    Overtime(String),
    /// Its files changed or it stopped working: a stall flag can go.
    Recovered,
}

impl Watch {
    pub fn new(guard: Guard, now: i64) -> Watch {
        Watch {
            guard,
            last_print: None,
            unchanged_since: now,
            stalled: false,
            overtime: false,
        }
    }

    /// `working`: the CLI is busy (reports working, or printed in the last seconds).
    /// `print`: the files' fingerprint now. `started`: when the run started.
    pub fn tick(&mut self, now: i64, working: bool, print: Option<u64>, started: i64) -> Verdict {
        if self.guard.max_ms > 0 && !self.overtime && (now - started) as u64 > self.guard.max_ms {
            self.overtime = true;
            return Verdict::Overtime(format!(
                "over its time limit: running {}",
                minutes((now - started) as u64)
            ));
        }
        let changed = print.is_some() && print != self.last_print;
        if changed || !working || print.is_none() {
            self.last_print = print;
            self.unchanged_since = now;
            if self.stalled {
                self.stalled = false;
                return Verdict::Recovered;
            }
            return Verdict::Fine;
        }
        let quiet = (now - self.unchanged_since) as u64;
        if self.guard.stall_ms > 0 && !self.stalled && quiet >= self.guard.stall_ms {
            self.stalled = true;
            return Verdict::Stalled(format!(
                "stalled: working {} with no change to its files",
                minutes(quiet)
            ));
        }
        Verdict::Fine
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_durations() {
        assert_eq!(parse_duration("30m"), Some(1_800_000));
        assert_eq!(parse_duration("1h30m"), Some(5_400_000));
        assert_eq!(parse_duration("45"), Some(2_700_000));
        assert_eq!(parse_duration("90s"), Some(90_000));
        assert_eq!(parse_duration("off"), Some(0));
        assert_eq!(parse_duration("soon"), None);
        assert_eq!(parse_duration("5x"), None);
    }

    #[test]
    fn flags_work_that_changes_nothing_and_clears_when_it_does() {
        let g = Guard {
            stall_ms: 60_000,
            max_ms: 0,
            action: "notify".into(),
        };
        let mut w = Watch::new(g, 0);
        assert_eq!(w.tick(0, true, Some(1), 0), Verdict::Fine);
        assert_eq!(w.tick(30_000, true, Some(1), 0), Verdict::Fine);
        // Idle in between: waiting on you isn't a stall, and restarts the clock.
        assert_eq!(w.tick(50_000, false, Some(1), 0), Verdict::Fine);
        assert_eq!(w.tick(100_000, true, Some(1), 0), Verdict::Fine);
        assert!(
            matches!(w.tick(111_000, true, Some(1), 0), Verdict::Stalled(r) if r.contains("1m"))
        );
        assert_eq!(
            w.tick(120_000, true, Some(1), 0),
            Verdict::Fine,
            "flagged once"
        );
        assert_eq!(w.tick(130_000, true, Some(2), 0), Verdict::Recovered);
        // Outside a repo it can't tell, so it never flags.
        let mut w = Watch::new(
            Guard {
                stall_ms: 1,
                ..Default::default()
            },
            0,
        );
        assert_eq!(w.tick(10_000, true, None, 0), Verdict::Fine);
    }

    #[test]
    fn flags_runs_over_their_time_limit_once() {
        let g = Guard {
            stall_ms: 0,
            max_ms: 3_600_000,
            action: "stop".into(),
        };
        let mut w = Watch::new(g, 0);
        assert_eq!(w.tick(3_000_000, true, Some(1), 0), Verdict::Fine);
        assert!(
            matches!(w.tick(3_700_000, true, Some(2), 0), Verdict::Overtime(r) if r.contains("1h 1m"))
        );
        assert_eq!(w.tick(3_800_000, true, Some(3), 0), Verdict::Fine);
    }
}
