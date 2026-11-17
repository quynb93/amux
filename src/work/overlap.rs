//! Parallel runs on one repo, before they collide.
//!
//! Each run works in its own worktree, which keeps agents from editing each other's files
//! but only moves the problem: two branches that change the same lines conflict when the
//! second one merges, after both agents have finished and moved on. Here amux looks
//! while they work:
//!
//! - `overlap`: which runs touch the same files, and whether their changes would actually
//!   conflict, with each other or with the branch they land in. Uncommitted work counts
//!   too (`git stash create` makes a commit of it without touching the worktree), and
//!   `git merge-tree` does the merge in memory, so nothing on disk changes.
//! - `land`: a local merge queue. One run at a time, it rebases onto the target branch,
//!   runs the tests in a test slot, and fast-forwards the target. A run that conflicts or
//!   fails is skipped, and its agent is told exactly what to fix.

use super::verify::{self, Plan, TestOutcome};
use crate::git::{git, git_raw};
use crate::runs::{self, Run};
use anyhow::{bail, Context, Result};
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// One run's changes, as of now.
#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Change {
    pub run: String,
    pub branch: String,
    /// The commit with the run's work, uncommitted changes included.
    #[serde(skip)]
    pub tip: String,
    pub files: Vec<String>,
    /// Files that would conflict with the target branch as it is now.
    pub conflicts_with_target: Vec<String>,
    pub live: bool,
}

/// Two runs that touch the same files.
#[derive(Serialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Pair {
    pub a: String,
    pub b: String,
    pub shared: Vec<String>,
    /// Of those, the ones where their changes conflict.
    pub conflicts: Vec<String>,
}

/// Everything about one repo's runs.
#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Report {
    pub repo: String,
    pub target: String,
    pub runs: Vec<Change>,
    pub pairs: Vec<Pair>,
}

impl Report {
    pub fn has_conflicts(&self) -> bool {
        self.pairs.iter().any(|p| !p.conflicts.is_empty())
            || self
                .runs
                .iter()
                .any(|r| !r.conflicts_with_target.is_empty())
    }
}

/// The branch checked out in the main checkout: where runs land by default.
pub fn default_target(repo: &Path) -> Result<String> {
    git(repo, &["symbolic-ref", "--short", "HEAD"])
        .context("the main checkout has no branch checked out; pass --into <branch>")
}

/// The files `a` and `b` would conflict in if merged, worked out in memory.
fn merge_conflicts(repo: &Path, a: &str, b: &str) -> Result<Vec<String>> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args([
            "merge-tree",
            "--write-tree",
            "--name-only",
            "--no-messages",
            a,
            b,
        ])
        .stdin(Stdio::null())
        .output()
        .context("could not run git")?;
    match out.status.code() {
        Some(0) => Ok(vec![]),
        // Conflicts: the tree it would write, then one conflicted path per line.
        Some(1) => Ok(String::from_utf8_lossy(&out.stdout)
            .lines()
            .skip(1)
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect()),
        _ => bail!(
            "git merge-tree failed (it needs git 2.38 or newer): {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ),
    }
}

/// A run's current work as a commit, and the files it changed since it started.
fn change_of(run: &Run, repo: &Path, target: &str) -> Result<Option<Change>> {
    let Some(wt) = &run.worktree else {
        return Ok(None);
    };
    let dir = PathBuf::from(&wt.path);
    if !dir.is_dir() {
        return Ok(None);
    }
    let Some(base) = crate::git::base_of(run) else {
        return Ok(None);
    };
    // A commit of the uncommitted changes on top of HEAD, made without touching the
    // worktree or the stash list; empty output means there are none.
    let tip = match git(&dir, &["stash", "create"])? {
        s if s.is_empty() => git(&dir, &["rev-parse", "HEAD"])?,
        s => s,
    };
    let mut files: Vec<String> =
        git_raw(&dir, &["diff", "--name-only", "--no-renames", &base, &tip])?
            .lines()
            .map(str::to_string)
            .collect();
    // New files not added to git yet are changes too, and collide with the same names.
    files.extend(
        git_raw(&dir, &["ls-files", "--others", "--exclude-standard"])?
            .lines()
            .map(str::to_string),
    );
    files.sort();
    files.dedup();
    let conflicts_with_target = if files.is_empty() {
        vec![]
    } else {
        merge_conflicts(repo, target, &tip)?
    };
    Ok(Some(Change {
        run: run.id.clone(),
        branch: wt.branch.clone(),
        tip,
        files,
        conflicts_with_target,
        live: run.state().is_live(),
    }))
}

/// Overlap reports, one per repo that has runs with worktrees. `into`: the target branch
/// (else each repo's checked-out branch). `only`: limit to these runs.
pub fn reports(into: Option<&str>, only: &[String]) -> Result<Vec<Report>> {
    let mut by_repo: BTreeMap<String, Vec<Run>> = BTreeMap::new();
    for r in runs::list() {
        if !only.is_empty() && !only.contains(&r.id) {
            continue;
        }
        if let Some(wt) = &r.worktree {
            by_repo.entry(wt.repo.clone()).or_default().push(r);
        }
    }
    let mut out = vec![];
    for (repo, list) in by_repo {
        let repo_path = PathBuf::from(&repo);
        if !repo_path.is_dir() {
            continue;
        }
        let target = match into {
            Some(t) => t.to_string(),
            None => default_target(&repo_path)?,
        };
        let mut changes = vec![];
        for r in &list {
            if let Some(c) = change_of(r, &repo_path, &target)? {
                if !c.files.is_empty() {
                    changes.push(c);
                }
            }
        }
        let mut pairs = vec![];
        for (i, a) in changes.iter().enumerate() {
            for b in &changes[i + 1..] {
                let shared: Vec<String> = a
                    .files
                    .iter()
                    .filter(|f| b.files.binary_search(f).is_ok())
                    .cloned()
                    .collect();
                if shared.is_empty() {
                    continue;
                }
                let conflicts = merge_conflicts(&repo_path, &a.tip, &b.tip)?;
                pairs.push(Pair {
                    a: a.run.clone(),
                    b: b.run.clone(),
                    shared,
                    conflicts,
                });
            }
        }
        out.push(Report {
            repo,
            target,
            runs: changes,
            pairs,
        });
    }
    Ok(out)
}

/// What landing one run did.
#[derive(Serialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Landed {
    pub run: String,
    /// landed, skipped, conflict or failed.
    pub outcome: String,
    pub detail: String,
    /// The agent was told what to fix.
    pub told: bool,
}

#[derive(Clone, Debug, Default)]
pub struct LandOpts {
    pub into: Option<String>,
    pub test: bool,
    /// The test command, instead of the configured or detected one.
    pub command: Option<String>,
    /// Tell a live run's agent what to fix when its run can't land.
    pub tell: bool,
    pub dry_run: bool,
}

/// Lands runs into their target branch one at a time (see the module docs). Never
/// rewrites the target's history or touches the main checkout's uncommitted work.
pub fn land(ids: &[String], o: &LandOpts, mut progress: impl FnMut(&str)) -> Result<Vec<Landed>> {
    let mut out = vec![];
    for id in ids {
        let run = runs::find(id)?;
        progress(&format!("{}: checking…", run.id));
        let res = land_one(&run, o, &mut progress)?;
        verify::record(&run.id, "land", &res.outcome, &res.detail, false);
        progress(&format!("{}: {} — {}", res.run, res.outcome, res.detail));
        out.push(res);
    }
    Ok(out)
}

fn tell(run: &Run, o: &LandOpts, msg: &str) -> bool {
    o.tell
        && run.state() == runs::State::Running
        && crate::runs::client::send_text(run, msg, true).is_ok()
}

fn land_one(run: &Run, o: &LandOpts, progress: &mut impl FnMut(&str)) -> Result<Landed> {
    let res = |outcome: &str, detail: String, told: bool| Landed {
        run: run.id.clone(),
        outcome: outcome.into(),
        detail,
        told,
    };
    let Some(wt) = &run.worktree else {
        return Ok(res(
            "skipped",
            "it has no worktree of its own".into(),
            false,
        ));
    };
    let repo = PathBuf::from(&wt.repo);
    let dir = PathBuf::from(&wt.path);
    let target = match &o.into {
        Some(t) => t.clone(),
        None => default_target(&repo)?,
    };
    if !dir.is_dir() {
        return Ok(res("skipped", "its worktree is gone".into(), false));
    }
    // Only tracked changes: untracked files (a test run's __pycache__, build output)
    // aren't part of the work and don't stop it landing.
    let dirty = git(&dir, &["status", "--porcelain", "--untracked-files=no"])?;
    if !dirty.is_empty() {
        let n = dirty.lines().count();
        let told = tell(
            run,
            o,
            &format!("Please commit your work (git add and git commit) so it can be merged into {target}. {n} file(s) are uncommitted."),
        );
        return Ok(res(
            "skipped",
            format!("{n} uncommitted file(s); commit first"),
            told,
        ));
    }
    let ahead = git(&dir, &["rev-list", "--count", &format!("{target}..HEAD")])?;
    if ahead == "0" {
        return Ok(res("skipped", format!("nothing new for {target}"), false));
    }
    let conflicts = merge_conflicts(&repo, &target, &wt.branch)?;
    if !conflicts.is_empty() {
        let told = tell(
            run,
            o,
            &format!(
                "Your branch {} conflicts with {target} in: {}. Please rebase onto {target} (git rebase {target}), resolve the conflicts, run the tests, and commit. Tell me when it's done.",
                wt.branch,
                conflicts.join(", ")
            ),
        );
        return Ok(res(
            "conflict",
            format!("conflicts with {target} in {}", conflicts.join(", ")),
            told,
        ));
    }
    if o.dry_run {
        return Ok(res(
            "ready",
            format!("{ahead} commit(s) would land cleanly in {target}"),
            false,
        ));
    }
    // Onto the target's current tip, which the runs landed before this one moved.
    progress(&format!("{}: rebasing onto {target}…", run.id));
    if let Err(e) = git(&dir, &["rebase", "--no-autostash", &target]) {
        let _ = git(&dir, &["rebase", "--abort"]);
        let why = e.to_string();
        // Rebasing writes commits, which needs to know who you are.
        if why.contains("identity unknown") || why.contains("user.email") {
            return Ok(res(
                "failed",
                "git doesn't know who you are; set user.name and user.email (git config --global)"
                    .into(),
                false,
            ));
        }
        let last = why
            .lines()
            .rfind(|l| !l.trim().is_empty())
            .unwrap_or("rebase failed");
        return Ok(res("conflict", format!("rebase failed: {last}"), false));
    }
    if o.test {
        progress(&format!("{}: testing (waits for a test slot)…", run.id));
        let cmd = o.command.clone().or_else(|| Plan::for_run(run).test);
        match verify::run_tests(run, cmd)? {
            TestOutcome::Passed { .. } => {}
            TestOutcome::NoSlot => {
                return Ok(res("skipped", "no test slot came free".into(), false))
            }
            TestOutcome::Failed { cmd, code, .. } => {
                let told = o.tell && verify::send_failures(run).is_ok();
                return Ok(res(
                    "failed",
                    format!("{cmd} exited {code} after rebasing onto {target}"),
                    told,
                ));
            }
        }
    }
    let new = git(&dir, &["rev-parse", "HEAD"])?;
    let old = git(&repo, &["rev-parse", &format!("refs/heads/{target}")])?;
    if git(&repo, &["merge-base", "--is-ancestor", &old, &new]).is_err() {
        return Ok(res(
            "skipped",
            format!("{target} moved while landing; run land again"),
            false,
        ));
    }
    // Fast-forward only. In the main checkout, through git merge, so its files follow
    // (and git refuses if that would overwrite uncommitted work there). Otherwise, move
    // the branch only if it still points where we read it.
    let checked_out = default_target(&repo).ok().as_deref() == Some(target.as_str());
    let moved = if checked_out {
        git(&repo, &["merge", "--ff-only", "--quiet", &new])
    } else {
        git(
            &repo,
            &["update-ref", &format!("refs/heads/{target}"), &new, &old],
        )
    };
    if let Err(e) = moved {
        return Ok(res(
            "skipped",
            format!("could not move {target}: {e}"),
            false,
        ));
    }
    Ok(res(
        "landed",
        format!(
            "{ahead} commit(s) into {target} ({})",
            &new[..new.len().min(9)]
        ),
        false,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{write, TempDir};

    fn g(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@x"])
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    #[test]
    fn finds_conflicts_in_memory_without_touching_files() {
        let t = TempDir::new();
        let d = t.path();
        g(d, &["init", "-q", "-b", "main"]);
        write(&d.join("f"), "a\nb\nc\n");
        g(d, &["add", "."]);
        g(d, &["commit", "-qm", "i"]);
        g(d, &["branch", "x"]);
        write(&d.join("f"), "a\nB1\nc\n");
        g(d, &["commit", "-qam", "m"]);
        g(d, &["checkout", "-q", "x"]);
        write(&d.join("f"), "a\nB2\nc\n");
        let tip = g(d, &["stash", "create"]);
        assert_eq!(merge_conflicts(d, "main", &tip).unwrap(), ["f"]);
        assert!(
            merge_conflicts(d, "main", "x").unwrap().is_empty(),
            "x itself is clean"
        );
        assert_eq!(std::fs::read_to_string(d.join("f")).unwrap(), "a\nB2\nc\n");
        assert_eq!(g(d, &["stash", "list"]), "", "the stash list is untouched");
    }
}
