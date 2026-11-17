//! A run's pull request and CI checks, through the GitHub CLI (`gh`): open (or find) the
//! PR for its branch, see how its checks are doing, and hand failures to the agent.
//!
//! What was found is kept in `runs/<id>/pr.json`, so `amux ui` and `amux ps --json` can
//! show it without asking GitHub on every refresh. amux never stores a GitHub token: `gh`
//! keeps its own sign-in.

use super::verify::{self, Plan};
use crate::runs::client::send_text;
use crate::runs::{self, Run};
use crate::store::{read_json, write_json};
use crate::time;
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// What amux last learned about a run's pull request.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PullRequest {
    pub url: String,
    #[serde(default)]
    pub number: u64,
    /// OPEN, MERGED or CLOSED.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub state: String,
    /// The CI checks overall: passing, failing, pending or none.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub checks: String,
    /// Names of failing checks.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failing: Vec<String>,
    pub checked_at: i64,
}

impl PullRequest {
    /// A short label for the run list: "PR #12 ✓", "PR #12 ✗ 2 checks", …
    pub fn label(&self) -> String {
        let mut s = format!("PR #{}", self.number);
        match self.state.as_str() {
            "MERGED" => return s + " merged",
            "CLOSED" => return s + " closed",
            _ => {}
        }
        match self.checks.as_str() {
            "passing" => s.push_str(" checks ✓"),
            "failing" => s.push_str(&format!(" ✗ {} failing", self.failing.len().max(1))),
            "pending" => s.push_str(" checks…"),
            _ => {}
        }
        s
    }
}

fn pr_file(id: &str) -> PathBuf {
    runs::run_dir(id).join("pr.json")
}

pub fn load(id: &str) -> Option<PullRequest> {
    read_json::<Option<PullRequest>>(&pr_file(id))
        .ok()
        .flatten()
}

/// The `gh` to run: `AMUX_GH` if set (tests point it at a stand-in), else `gh` on PATH.
fn gh(dir: &Path) -> Command {
    let bin = std::env::var_os("AMUX_GH")
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "gh".into());
    let mut c = Command::new(bin);
    c.current_dir(dir).stdin(Stdio::null());
    c
}

/// Runs gh and returns its stdout, with a readable error when gh is missing or fails.
pub(crate) fn gh_out(dir: &Path, args: &[&str], ok_codes: &[i32]) -> Result<String> {
    let out = gh(dir).args(args).output().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            anyhow!("This needs the GitHub CLI: install it from https://cli.github.com and run `gh auth login`.")
        } else {
            anyhow!("could not run gh: {e}")
        }
    })?;
    let code = out.status.code().unwrap_or(1);
    if !out.status.success() && !ok_codes.contains(&code) {
        let err = String::from_utf8_lossy(&out.stderr);
        let why = err
            .lines()
            .rfind(|l| !l.trim().is_empty())
            .unwrap_or("gh failed");
        bail!("{}", why.trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The run's worktree and branch; PRs and checks need both.
fn branch_of(run: &Run) -> Result<(PathBuf, String)> {
    let wt = run.worktree.as_ref().ok_or_else(|| {
        anyhow!(
            "{} has no worktree of its own, so it has no branch for a pull request.",
            run.id
        )
    })?;
    Ok((PathBuf::from(&wt.path), wt.branch.clone()))
}

/// The open (or latest) PR for a branch, if there is one.
fn find_pr(dir: &Path, branch: &str) -> Result<Option<PullRequest>> {
    let out = gh(dir)
        .args(["pr", "view", branch, "--json", "url,number,state"])
        .output()
        .context("could not run gh")?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).to_lowercase();
        if err.contains("no pull requests found") || err.contains("no open pull requests") {
            return Ok(None);
        }
        let why = String::from_utf8_lossy(&out.stderr);
        bail!(
            "{}",
            why.lines()
                .rfind(|l| !l.trim().is_empty())
                .unwrap_or("gh failed")
                .trim()
        );
    }
    let v: Value =
        serde_json::from_slice(&out.stdout).context("unexpected output from gh pr view")?;
    Ok(Some(PullRequest {
        url: v["url"].as_str().unwrap_or_default().to_string(),
        number: v["number"].as_u64().unwrap_or(0),
        state: v["state"].as_str().unwrap_or_default().to_string(),
        checked_at: time::now_ms(),
        ..Default::default()
    }))
}

/// What `open_pr` did.
pub struct Opened {
    pub pr: PullRequest,
    /// False when the branch already had an open PR.
    pub created: bool,
    /// Uncommitted files left out of the PR.
    pub left_out: usize,
}

/// Pushes the run's branch if GitHub doesn't have all of it, then opens a PR for it
/// through `gh`, or finds the one already open.
pub fn open_pr(run: &Run, draft: bool, base: Option<&str>) -> Result<Opened> {
    let (dir, branch) = branch_of(run)?;
    let plan = Plan::for_run(run);
    // GitHub can only open a PR for commits it has.
    if plan.ahead != Some(0) {
        verify::push(run, &plan)?;
    }
    let (pr, created) = match find_pr(&dir, &branch)?.filter(|p| p.state == "OPEN") {
        Some(pr) => (pr, false),
        None => {
            let mut args = vec!["pr", "create", "--head", branch.as_str(), "--fill"];
            if draft {
                args.push("--draft");
            }
            if let Some(b) = base {
                args.extend(["--base", b]);
            }
            // gh fills the title from the commits and leaves a given body alone.
            let body = run
                .issue
                .map(|n| format!("Fixes #{n}\n\nStarted by amux from issue #{n}."));
            if let Some(b) = &body {
                args.extend(["--body", b.as_str()]);
            }
            gh_out(&dir, &args, &[])?;
            let pr = find_pr(&dir, &branch)?
                .ok_or_else(|| anyhow!("gh created the PR, but it can't be found."))?;
            (pr, true)
        }
    };
    write_json(&pr_file(&run.id), &pr)?;
    Ok(Opened {
        pr,
        created,
        left_out: plan.dirty,
    })
}

/// One CI check, as `gh pr checks --json` reports it.
#[derive(Deserialize, Debug, Clone)]
pub struct Check {
    pub name: String,
    /// pass, fail, pending, skipping or cancel.
    pub bucket: String,
    #[serde(default)]
    pub link: String,
    #[serde(default)]
    pub workflow: String,
}

impl Check {
    pub fn failed(&self) -> bool {
        matches!(self.bucket.as_str(), "fail" | "cancel")
    }
}

/// The overall state of a set of checks.
fn summarize(checks: &[Check]) -> (&'static str, Vec<String>) {
    let failing: Vec<String> = checks
        .iter()
        .filter(|c| c.failed())
        .map(|c| c.name.clone())
        .collect();
    let state = if !failing.is_empty() {
        "failing"
    } else if checks.iter().any(|c| c.bucket == "pending") {
        "pending"
    } else if checks.is_empty() {
        "none"
    } else {
        "passing"
    };
    (state, failing)
}

/// The Actions run id in a check's link (…/actions/runs/<id>/job/<job>), for its log.
fn actions_run_id(link: &str) -> Option<&str> {
    let rest = link.split("/actions/runs/").nth(1)?;
    let id = rest.split('/').next()?;
    (!id.is_empty() && id.bytes().all(|b| b.is_ascii_digit())).then_some(id)
}

/// Asks GitHub how the run's PR checks are doing, and remembers it in pr.json.
pub fn refresh_checks(run: &Run) -> Result<(PullRequest, Vec<Check>)> {
    let (dir, branch) = branch_of(run)?;
    let Some(mut pr) = find_pr(&dir, &branch)? else {
        bail!(
            "{} has no pull request yet. `amux pr {}` opens one.",
            run.id,
            run.id
        );
    };
    // Exit code 8 means some checks are still pending; that's an answer, not an error.
    let out = gh_out(
        &dir,
        &[
            "pr",
            "checks",
            &branch,
            "--json",
            "name,bucket,link,workflow",
        ],
        &[1, 8],
    )?;
    let checks: Vec<Check> = if out.trim().is_empty() {
        vec![]
    } else {
        serde_json::from_str(&out).context("unexpected output from gh pr checks")?
    };
    let (state, failing) = summarize(&checks);
    pr.checks = state.into();
    pr.failing = failing;
    pr.checked_at = time::now_ms();
    let before = load(&run.id).map(|p| p.checks);
    write_json(&pr_file(&run.id), &pr)?;
    if before.as_deref() != Some(pr.checks.as_str()) {
        let msg = format!("{} {}", pr.checks, pr.url);
        crate::plugins::hook("checks", &run.id, &msg);
        crate::runs::push::send("checks", &run.id, &format!("amux: {} CI", run.id), &msg);
    }
    Ok((pr, checks))
}

/// Hands the failing checks, with the end of their failed steps' logs, to the run's agent
/// to fix. Returns how many checks failed.
pub fn send_ci_failures(run: &Run, pr: &PullRequest, checks: &[Check]) -> Result<usize> {
    if pr.failing.is_empty() {
        bail!("No failing checks to send ({}).", pr.checks);
    }
    let (dir, _) = branch_of(run)?;
    // The failed steps' logs of each failing Actions run, once per run.
    let mut logs = String::new();
    let mut seen = vec![];
    for c in checks.iter().filter(|c| c.failed()) {
        let Some(rid) = actions_run_id(&c.link) else {
            continue;
        };
        if seen.contains(&rid) {
            continue;
        }
        seen.push(rid);
        if let Ok(log) = gh_out(&dir, &["run", "view", rid, "--log-failed"], &[]) {
            logs.push_str(&log);
            logs.push('\n');
        }
    }
    let tail = verify::log_tail(logs.as_bytes(), 6000);
    let mut msg = format!(
        "CI failed on PR #{} ({}). Failing checks: {}.",
        pr.number,
        pr.url,
        pr.failing.join(", ")
    );
    if !tail.is_empty() {
        msg.push_str(&format!(
            "\n\nThe end of the failed steps' logs:\n\n```\n{tail}\n```"
        ));
    }
    msg.push_str("\n\nPlease find the cause, fix it, commit, and push again.");
    send_text(run, &msg, true)?;
    Ok(pr.failing.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(name: &str, bucket: &str) -> Check {
        Check {
            name: name.into(),
            bucket: bucket.into(),
            link: String::new(),
            workflow: String::new(),
        }
    }

    #[test]
    fn sums_up_checks() {
        assert_eq!(summarize(&[]).0, "none");
        assert_eq!(
            summarize(&[check("a", "pass"), check("b", "skipping")]).0,
            "passing"
        );
        assert_eq!(
            summarize(&[check("a", "pass"), check("b", "pending")]).0,
            "pending"
        );
        let (s, f) = summarize(&[check("a", "fail"), check("b", "pending")]);
        assert_eq!((s, f), ("failing", vec!["a".to_string()]));
    }

    #[test]
    fn finds_the_actions_run_in_a_link() {
        let l = "https://github.com/o/r/actions/runs/123456/job/789";
        assert_eq!(actions_run_id(l), Some("123456"));
        assert_eq!(actions_run_id("https://ci.example.com/build/1"), None);
        assert_eq!(
            actions_run_id("https://github.com/o/r/actions/runs/;rm/job"),
            None
        );
    }

    #[test]
    fn labels() {
        let mut pr = PullRequest {
            number: 7,
            state: "OPEN".into(),
            checks: "failing".into(),
            failing: vec!["test".into(), "lint".into()],
            ..Default::default()
        };
        assert_eq!(pr.label(), "PR #7 ✗ 2 failing");
        pr.checks = "passing".into();
        assert_eq!(pr.label(), "PR #7 checks ✓");
        pr.state = "MERGED".into();
        assert_eq!(pr.label(), "PR #7 merged");
    }
}
