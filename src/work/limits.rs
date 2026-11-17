//! Usage limits, across accounts.
//!
//! Agent CLIs on a subscription stop when the account hits its usage limit (Claude Code's
//! five-hour and weekly windows, Codex's and Gemini's quotas), and show a message such as
//! "You've hit your limit · resets 7pm". amux has more than one account, so it can do
//! better than wait:
//!
//! - the run host spots the message (`limitPatterns`), marks the run waiting with it, and
//!   records the account as limited until the reset time the message names;
//! - `amux spawn <provider>` (no account named) skips accounts that are limited;
//! - `amux failover <run>` resumes the same session, in the same worktree, under another
//!   account; with `"limits": { "onLimit": "failover" }` it happens by itself.
//!
//! Limited accounts are kept in ~/.amux/limits.json until their reset time.

use crate::store::{load_accounts, load_config, read_json, write_json, Account};
use crate::{paths, time};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// When the message names no reset time: Claude Code's window is five hours.
const DEFAULT_WINDOW_MS: i64 = 5 * 3600 * 1000;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Limit {
    /// The message the CLI showed.
    pub reason: String,
    pub since: i64,
    /// When it should lift, from the message, or `since` plus five hours.
    pub resets_at: i64,
    /// The run that hit it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<String>,
}

fn file() -> std::path::PathBuf {
    paths::AMUX_HOME.join("limits.json")
}

/// Limits still in force, by account (`provider@name`).
pub fn active() -> BTreeMap<String, Limit> {
    let now = time::now_ms();
    read_json::<BTreeMap<String, Limit>>(&file())
        .unwrap_or_default()
        .into_iter()
        .filter(|(_, l)| l.resets_at > now)
        .collect()
}

pub fn of(account: &str) -> Option<Limit> {
    active().remove(account)
}

/// Records that `account` hit its limit, with the CLI's message. Returns the limit.
pub fn record(account: &str, reason: &str, run: Option<&str>) -> Limit {
    let now = time::now_ms();
    let reason = tidy(reason);
    let limit = Limit {
        resets_at: parse_reset(&reason, now).unwrap_or(now + DEFAULT_WINDOW_MS),
        reason,
        since: now,
        run: run.map(str::to_string),
    };
    let mut all = active();
    all.insert(account.to_string(), limit.clone());
    let _ = write_json(&file(), &all);
    limit
}

/// Forgets a limit, for when it lifted early (`amux usage --clear`).
pub fn clear(account: &str) -> bool {
    let mut all = active();
    let had = all.remove(account).is_some();
    let _ = write_json(&file(), &all);
    had
}

/// The message without what follows it on screen: up to the time zone in parentheses
/// after a reset time, or a reasonable length.
pub fn tidy(msg: &str) -> String {
    let msg = msg.trim();
    if let Some(r) = msg
        .to_lowercase()
        .find("reset")
        .or_else(|| msg.to_lowercase().find(" at "))
    {
        if let Some(close) = msg[r..].find(')') {
            return msg[..r + close + 1].trim().to_string();
        }
    }
    let cut: String = msg.chars().take(90).collect();
    cut.trim().to_string()
}

/// The time a limit message says it lifts, in ms since the epoch, from "resets 7pm",
/// "resets 12:10pm", "at 12:10 PM" (the next time it is that o'clock here) or "in 2 hours
/// 5 minutes" / "in 3d 4h" (from now).
pub fn parse_reset(msg: &str, now: i64) -> Option<i64> {
    let lower = msg.to_lowercase();
    // A clock time after "resets" or "at".
    for key in ["resets ", "reset at ", " at "] {
        let mut from = 0;
        while let Some(i) = lower[from..].find(key) {
            let rest = &lower[from + i + key.len()..];
            if let Some((h, m)) = clock(rest) {
                return Some(next_local(now, h, m));
            }
            from += i + key.len();
        }
    }
    // A duration after "in".
    if let Some(i) = lower.find(" in ") {
        let d = duration(&lower[i + 4..])?;
        return Some(now + d);
    }
    None
}

/// "7pm", "12:10pm", "7 pm", "19:00" at the start of `s` → (hour 0–23, minute).
fn clock(s: &str) -> Option<(u32, u32)> {
    let s = s.trim_start();
    let digits: String = s
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == ':')
        .collect();
    if digits.is_empty() {
        return None;
    }
    let (h, m) = match digits.split_once(':') {
        Some((h, m)) => (h.parse::<u32>().ok()?, m.get(..2)?.parse::<u32>().ok()?),
        None => (digits.parse::<u32>().ok()?, 0),
    };
    let after = s[digits.len()..].trim_start();
    let h = if after.starts_with("am") || after.starts_with("a.m") {
        if h == 12 {
            0
        } else {
            h
        }
    } else if after.starts_with("pm") || after.starts_with("p.m") {
        if h == 12 {
            12
        } else {
            h + 12
        }
    } else if digits.contains(':') {
        h
    } else {
        // A bare number isn't a time ("resets 5 times").
        return None;
    };
    (h < 24 && m < 60).then_some((h, m))
}

/// "2 hours 5 minutes", "3d 4h", "45m", "1 day" → milliseconds.
fn duration(s: &str) -> Option<i64> {
    let mut total = 0i64;
    let mut any = false;
    let words: Vec<String> = s
        .replace([',', '.'], " ")
        .split_whitespace()
        .take(8)
        .map(str::to_string)
        .collect();
    let mut i = 0;
    while i < words.len() {
        let w = &words[i];
        let num_end = w.find(|c: char| !c.is_ascii_digit()).unwrap_or(w.len());
        if num_end == 0 {
            if any {
                break;
            }
            i += 1;
            continue;
        }
        let n: i64 = w[..num_end].parse().ok()?;
        let unit = if num_end < w.len() {
            w[num_end..].to_string()
        } else {
            i += 1;
            words.get(i).cloned().unwrap_or_default()
        };
        let ms = match unit.trim_end_matches('s') {
            "d" | "day" => 86_400_000,
            "h" | "hr" | "hour" => 3_600_000,
            "m" | "min" | "minute" => 60_000,
            "sec" | "second" | "" if unit.starts_with('s') => 1000,
            _ => break,
        };
        total += n * ms;
        any = true;
        i += 1;
    }
    any.then_some(total)
}

/// The next moment it is `h:m` local time, after `now`.
fn next_local(now: i64, h: u32, m: u32) -> i64 {
    unsafe {
        let secs = (now / 1000) as libc::time_t;
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&secs, &mut tm);
        tm.tm_hour = h as i32;
        tm.tm_min = m as i32;
        tm.tm_sec = 0;
        tm.tm_isdst = -1;
        let mut t = libc::mktime(&mut tm) as i64 * 1000;
        if t <= now {
            tm.tm_mday += 1;
            tm.tm_isdst = -1;
            t = libc::mktime(&mut tm) as i64 * 1000;
        }
        t
    }
}

/// What to do when a run hits its limit: "wait" (the CLI waits for the reset itself),
/// or "failover" (resume it under another account at once). From `"limits": { "onLimit" }`.
pub fn on_limit() -> String {
    load_config()
        .ok()
        .and_then(|c| {
            c.extra
                .get("limits")?
                .get("onLimit")?
                .as_str()
                .map(str::to_string)
        })
        .unwrap_or_else(|| "wait".into())
}

/// Another account of `provider` that isn't limited, preferring the one with the fewest
/// live runs; never `avoid`.
pub fn pick(provider: &str, avoid: &str) -> Option<Account> {
    let limited = active();
    let runs = crate::runs::list();
    let busy = |a: &Account| {
        runs.iter()
            .filter(|r| r.account == a.id() && r.state().is_live())
            .count()
    };
    load_accounts()
        .ok()?
        .into_iter()
        .filter(|a| a.provider == provider && a.id() != avoid && !limited.contains_key(&a.id()))
        .min_by_key(|a| busy(a))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_reset_times_from_limit_messages() {
        let now = 1_700_000_000_000;
        let at7 = parse_reset("You've hit your limit · resets 7pm (Asia/Saigon)", now).unwrap();
        assert!(at7 > now && at7 - now <= 86_400_000);
        let at = parse_reset(
            "Usage limit reached · continuing automatically at 12:10pm",
            now,
        )
        .unwrap();
        assert!(at > now && at - now <= 86_400_000);
        assert_ne!(at, at7);
        assert_eq!(
            parse_reset("Try again in 2 hours 5 minutes.", now),
            Some(now + 7_500_000)
        );
        assert_eq!(
            parse_reset("try again in 3d 4h", now),
            Some(now + 3 * 86_400_000 + 4 * 3_600_000)
        );
        assert_eq!(parse_reset("resets 5 times", now), None);
        assert_eq!(parse_reset("limit reached", now), None);
    }

    #[test]
    fn clocks_and_tidy_messages() {
        assert_eq!(clock("7pm (x)"), Some((19, 0)));
        assert_eq!(clock("12:10pm"), Some((12, 10)));
        assert_eq!(clock("12am"), Some((0, 0)));
        assert_eq!(clock("19:30"), Some((19, 30)));
        assert_eq!(clock("25:00"), None);
        assert_eq!(
            tidy(
                "You've hit your limit · resets 7pm (Asia/Saigon) /upgrade to increase your usage"
            ),
            "You've hit your limit · resets 7pm (Asia/Saigon)"
        );
    }
}
