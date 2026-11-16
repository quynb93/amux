//! Running git, and what amux asks it about runs: one place for the error handling and
//! for where a run's branch started.

use crate::runs::Run;
use anyhow::{bail, Context, Result};
use std::path::Path;
use std::process::{Command, Stdio};

/// `git -C <dir> <args>`, its output with surrounding whitespace trimmed. On failure, the
/// error is what git printed.
pub fn git(dir: &Path, args: &[&str]) -> Result<String> {
    Ok(git_raw(dir, args)?.trim().to_string())
}

/// Like `git`, but the output exactly as printed (diffs keep their trailing newline).
pub fn git_raw(dir: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .context("could not run git")?;
    if !out.status.success() {
        bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The commit a worktree run started from. Runs from before amux recorded it fall back
/// to where the branch meets the main checkout's current commit.
pub fn base_of(run: &Run) -> Option<String> {
    let wt = run.worktree.as_ref()?;
    if let Some(b) = &wt.base {
        return Some(b.clone());
    }
    let head = git(Path::new(&wt.repo), &["rev-parse", "HEAD"]).ok()?;
    git(Path::new(&wt.path), &["merge-base", "HEAD", &head]).ok()
}
