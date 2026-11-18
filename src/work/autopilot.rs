//! Autopilot: takes a run from its first commit to a pull request that is ready to merge,
//! or merged, without anyone watching. When the agent's turn ends with commits on its
//! branch it opens the PR; then it keeps an eye on CI and on reviews, hands failing checks
//! and new review comments back to the agent, and says on your phone when the PR is
//! ready, merged, or stuck.
//!
//! The scheduler daemon (`amux __scheduler`) drives it, checking each run about once a
//! minute. A run's settings and progress are in `runs/<id>/autopilot.json`, what happened
//! in `runs/<id>/autopilot.log`. `~/.amux/autopilot.on` tells amux to keep the daemon
//! going while any run is on autopilot.

use super::github::{self, PullRequest};
use crate::git::git;
use crate::runs::state::{run_kind, Kind};
use crate::runs::{self, Run};
use crate::store::{read_json, write_json};
use crate::{fsx, paths, time};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};

/// How many times failing CI is handed back to the agent before it's left to you.
pub const RETRIES: u32 = 3;

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Pilot {
    /// Merge the PR when it's ready, instead of only saying so.
    #[serde(default)]
    pub merge: bool,
    /// merge, squash or rebase.
    #[serde(default = "squash")]
    pub method: String,
    #[serde(default = "retries")]
    pub retries: u32,
    /// working, checks, fixing, review, ready, merged, closed or stuck.
    #[serde(default)]
    pub phase: String,
    #[serde(default)]
    pub detail: String,
    #[serde(default)]
    pub since: i64,
    #[serde(default)]
    pub checked_at: i64,
    /// Times CI failures were handed back.
    #[serde(default)]
    pub fixes: u32,
    /// The commit whose CI failures were handed back last.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub sent_for: String,
    /// The PR's head commit, and when it was first seen.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub head: String,
    #[serde(default)]
    pub head_at: i64,
    /// Review comments already handed back.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub seen: Vec<String>,
    /// What you were last told, so each news is told once.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub told: String,
}

fn squash() -> String {
    "squash".into()
}
fn retries() -> u32 {
    RETRIES
}

impl Pilot {
    pub fn new(merge: bool) -> Pilot {
        Pilot {
            merge,
            method: squash(),
            retries: RETRIES,
            phase: "working".into(),
            since: time::now_ms(),
            ..Default::default()
        }
    }

    /// Still has something to do: not merged or closed.
    pub fn active(&self) -> bool {
        !matches!(self.phase.as_str(), "merged" | "closed")
    }
}

pub fn file(id: &str) -> PathBuf {
    runs::run_dir(id).join("autopilot.json")
}

fn log_file(id: &str) -> PathBuf {
    runs::run_dir(id).join("autopilot.log")
}

pub fn marker() -> PathBuf {
    paths::AMUX_HOME.join("autopilot.on")
}

pub fn load(id: &str) -> Option<Pilot> {
    read_json::<Option<Pilot>>(&file(id)).ok().flatten()
}

/// Saves it, unless autopilot was turned off for the run (or the run removed) meanwhile.
fn save(id: &str, p: &Pilot) -> Result<()> {
    if file(id).exists() {
        write_json(&file(id), p)?;
    }
    Ok(())
}

/// Turns autopilot on for a run (keeping its progress if it was on), and makes sure the
/// daemon that drives it is going.
pub fn enable(id: &str, merge: bool, retries: Option<u32>) -> Result<Pilot> {
    let mut p = load(id).unwrap_or_else(|| Pilot::new(merge));
    p.merge = merge;
    if let Some(r) = retries {
        p.retries = r;
    }
    if !p.active() {
        // A finished one starts over.
        p = Pilot::new(merge);
    }
    p.checked_at = 0;
    write_json(&file(id), &p)?;
    fsx::write_atomic(&marker(), b"")?;
    crate::commands::schedule::ensure_running();
    Ok(p)
}

pub fn disable(id: &str) -> bool {
    std::fs::remove_file(file(id)).is_ok()
}

/// What happened to the run, newest last.
pub fn history(id: &str) -> String {
    std::fs::read_to_string(log_file(id)).unwrap_or_default()
}

/// How often each run is looked at, in ms (`AMUX_AUTOPILOT_POLL_MS` in tests).
fn poll_ms() -> i64 {
    std::env::var("AMUX_AUTOPILOT_POLL_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|v: &i64| *v > 0)
        .unwrap_or(60_000)
}

/// One pass over every run on autopilot. Returns whether any still has something to do;
/// when none has, the daemon may stop.
pub fn tick() -> bool {
    if !marker().exists() {
        return false;
    }
    let mut any = false;
    for run in runs::list() {
        let Some(mut p) = load(&run.id) else { continue };
        if !p.active() {
            continue;
        }
        any = true;
        let before = p.clone();
        if let Err(e) = step(&run, &mut p, time::now_ms()) {
            let why = format!("{e:#}");
            set(&run.id, &mut p, "stuck", &why);
            tell(
                &run.id,
                &mut p,
                &format!("stuck:{why}"),
                &format!("stuck: {why}"),
            );
        }
        if p != before {
            let _ = save(&run.id, &p);
        }
    }
    if !any {
        let _ = std::fs::remove_file(marker());
    }
    any
}

fn set(id: &str, p: &mut Pilot, phase: &str, detail: &str) {
    if p.phase == phase && p.detail == detail {
        return;
    }
    if p.phase != phase {
        p.since = time::now_ms();
    }
    p.phase = phase.into();
    p.detail = detail.into();
    if let Ok(mut f) = fsx::open_append(&log_file(id)) {
        let _ = writeln!(f, "{} {phase}: {detail}", time::iso(time::now_ms()));
    }
}

/// Tells your phone and the `on-autopilot` hook, once for each `key`.
fn tell(id: &str, p: &mut Pilot, key: &str, msg: &str) {
    if p.told == key {
        return;
    }
    p.told = key.into();
    crate::plugins::hook("autopilot", id, msg);
    runs::push::send("autopilot", id, &format!("amux: {id} autopilot"), msg);
}

/// The agent's turn has ended and it waits for a new message, not for an answer: quiet,
/// or showing only Claude Code's "waiting for your input" notice.
fn turn_over(run: &Run, kind: Kind) -> bool {
    match kind {
        Kind::Idle => true,
        Kind::Attention => run
            .waiting
            .as_ref()
            .is_some_and(|w| w.reason.to_lowercase().contains("waiting for your input")),
        _ => false,
    }
}

/// Commits on the run's branch since it started.
fn commits(run: &Run) -> Option<usize> {
    let wt = run.worktree.as_ref()?;
    let dir = Path::new(&wt.path);
    let range = match &wt.base {
        Some(b) => git(dir, &["rev-list", "--count", &format!("{b}..HEAD")]),
        None => git(dir, &["rev-list", "--count", "HEAD", "--not", "--remotes"]),
    };
    range.ok()?.trim().parse().ok()
}

fn step(run: &Run, p: &mut Pilot, now: i64) -> Result<()> {
    let poll = poll_ms();
    let every = if p.phase == "stuck" { poll * 5 } else { poll };
    if now - p.checked_at < every {
        return Ok(());
    }
    p.checked_at = now;
    let id = run.id.as_str();
    if run.worktree.is_none() {
        set(
            id,
            p,
            "stuck",
            "it has no worktree, so no branch for a pull request",
        );
        return Ok(());
    }
    let (kind, _) = run_kind(run, now);
    match github::current(run)? {
        None => no_pr(run, p, kind),
        Some(pr) if pr.state == "MERGED" => {
            set(id, p, "merged", &format!("PR #{} merged", pr.number));
            tell(
                id,
                p,
                "merged",
                &format!("PR #{} merged: {}", pr.number, pr.url),
            );
            Ok(())
        }
        Some(pr) if pr.state == "CLOSED" => {
            set(
                id,
                p,
                "closed",
                &format!("PR #{} closed without merging", pr.number),
            );
            tell(
                id,
                p,
                "closed",
                &format!("PR #{} was closed: {}", pr.number, pr.url),
            );
            Ok(())
        }
        Some(_) => open(run, p, kind, now),
    }
}

/// No PR yet: open one once the agent's turn ends with commits on the branch.
fn no_pr(run: &Run, p: &mut Pilot, kind: Kind) -> Result<()> {
    let id = run.id.as_str();
    let n = commits(run).unwrap_or(0);
    if n == 0 {
        if !kind.is_live() {
            set(id, p, "stuck", "the run ended without committing anything");
            tell(
                id,
                p,
                "stuck:nothing",
                "the run ended without committing anything",
            );
        } else if kind == Kind::Idle {
            set(
                id,
                p,
                "working",
                "its turn ended with nothing committed yet",
            );
        } else {
            set(id, p, "working", "working, nothing committed yet");
        }
        return Ok(());
    }
    if matches!(kind, Kind::Working | Kind::Starting) {
        let s = if n == 1 { "" } else { "s" };
        set(id, p, "working", &format!("working, {n} commit{s} so far"));
        return Ok(());
    }
    let o = github::open_pr(run, false, None)?;
    let pr = o.pr;
    set(id, p, "checks", &format!("opened PR #{}", pr.number));
    tell(
        id,
        p,
        "opened",
        &format!("opened PR #{}: {}", pr.number, pr.url),
    );
    Ok(())
}

/// An open PR: hand back what needs fixing, or say it's ready (or merge it).
fn open(run: &Run, p: &mut Pilot, kind: Kind, now: i64) -> Result<()> {
    let id = run.id.as_str();
    let (pr, checks) = github::refresh_checks(run)?;
    let rv = github::review(run, pr.number)?;
    if rv.head != p.head {
        p.head = rv.head.clone();
        p.head_at = now;
    }
    let live = kind.is_live();
    // Its turn is over and nothing on screen waits for an answer: safe to type into.
    let free = turn_over(run, kind);

    let new: Vec<_> = rv
        .feedback
        .iter()
        .filter(|f| !p.seen.contains(&f.id))
        .cloned()
        .collect();
    if !new.is_empty() {
        let n = new.len();
        let s = if n == 1 { "" } else { "s" };
        if free {
            github::send_feedback(run, &pr, &new)?;
            p.seen.extend(new.into_iter().map(|f| f.id));
            set(
                id,
                p,
                "fixing",
                &format!("handed back {n} review comment{s}"),
            );
        } else if !live {
            set(
                id,
                p,
                "stuck",
                &format!("{n} review comment{s}, but the run has ended"),
            );
            tell(
                id,
                p,
                &format!("stuck:review:{}", rv.head),
                &format!(
                    "PR #{} has review comments and the run has ended: {}",
                    pr.number, pr.url
                ),
            );
        } else {
            set(
                id,
                p,
                "review",
                &format!("{n} new review comment{s}; waiting for the agent's turn to end"),
            );
        }
        return Ok(());
    }

    match pr.checks.as_str() {
        "failing" => {
            let failing = pr.failing.join(", ");
            if p.sent_for == rv.head {
                if live {
                    set(id, p, "fixing", &format!("waiting for a fix for {failing}"));
                } else {
                    set(
                        id,
                        p,
                        "stuck",
                        &format!("CI fails ({failing}) and the run has ended"),
                    );
                    tell(
                        id,
                        p,
                        &format!("stuck:ended:{}", rv.head),
                        &format!(
                            "CI fails on PR #{} and the run has ended: {}",
                            pr.number, pr.url
                        ),
                    );
                }
            } else if p.fixes >= p.retries {
                set(
                    id,
                    p,
                    "stuck",
                    &format!("CI still fails ({failing}) after {} fixes", p.fixes),
                );
                tell(
                    id,
                    p,
                    &format!("stuck:retries:{}", rv.head),
                    &format!(
                        "CI still fails on PR #{} after {} tries: {}",
                        pr.number, p.fixes, pr.url
                    ),
                );
            } else if free {
                github::send_ci_failures(run, &pr, &checks)?;
                p.fixes += 1;
                p.sent_for = rv.head.clone();
                set(
                    id,
                    p,
                    "fixing",
                    &format!("handed back {failing} (try {} of {})", p.fixes, p.retries),
                );
            } else if !live {
                set(
                    id,
                    p,
                    "stuck",
                    &format!("CI fails ({failing}) and the run has ended"),
                );
                tell(
                    id,
                    p,
                    &format!("stuck:ended:{}", rv.head),
                    &format!(
                        "CI fails on PR #{} and the run has ended: {}",
                        pr.number, pr.url
                    ),
                );
            } else {
                set(
                    id,
                    p,
                    "checks",
                    &format!("{failing} failing; waiting for the agent's turn to end"),
                );
            }
        }
        "pending" => set(id, p, "checks", "checks running"),
        state => {
            // Checks may not have registered yet for a commit just pushed.
            if state == "none" && now - p.head_at < 2 * poll_ms().max(60_000) {
                set(id, p, "checks", "waiting for checks to start");
                return Ok(());
            }
            ready(run, p, &pr, &rv.decision)?;
        }
    }
    Ok(())
}

fn ready(run: &Run, p: &mut Pilot, pr: &PullRequest, decision: &str) -> Result<()> {
    let id = run.id.as_str();
    let head = p.head.clone();
    match decision {
        "CHANGES_REQUESTED" => set(
            id,
            p,
            "review",
            "changes requested; waiting for another review",
        ),
        "REVIEW_REQUIRED" => {
            set(id, p, "ready", "checks pass; waiting for a review");
            tell(
                id,
                p,
                &format!("review:{head}"),
                &format!("PR #{} passes CI and needs a review: {}", pr.number, pr.url),
            );
        }
        _ if p.merge => {
            github::merge(run, pr.number, &p.method)?;
            set(id, p, "merged", &format!("merged PR #{}", pr.number));
            tell(
                id,
                p,
                "merged",
                &format!("merged PR #{}: {}", pr.number, pr.url),
            );
        }
        _ => {
            set(id, p, "ready", "ready to merge");
            tell(
                id,
                p,
                &format!("ready:{head}"),
                &format!("PR #{} is ready to merge: {}", pr.number, pr.url),
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_old_or_partial_files() {
        let p: Pilot = serde_json::from_str(r#"{"merge":true}"#).unwrap();
        assert_eq!((p.method.as_str(), p.retries), ("squash", RETRIES));
        assert!(p.active());
        let done = Pilot {
            phase: "merged".into(),
            ..Pilot::new(false)
        };
        assert!(!done.active());
    }
}
