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

/// Settings for every git command amux runs in a run's checkout (not your own commands):
/// an agent can write hooks, or an fsmonitor or other program into the repo's config, and
/// these keep git from running them on this machine. Diffs also take `--no-ext-diff`.
/// amux never runs git on a terminal, so no pager or editor is wanted either.
/// `core.sshCommand` and `core.askPass` aren't overridden: that would also override
/// yours, and break pushes that need them (a sandboxed run can't change the repo's config).
pub const RUN_SAFE: [&str; 10] = [
    "-c",
    "core.hooksPath=/dev/null",
    "-c",
    "core.fsmonitor=false",
    "-c",
    "protocol.ext.allow=never",
    "-c",
    "core.pager=cat",
    "-c",
    "core.editor=true",
];

/// `RUN_SAFE` as environment (`GIT_CONFIG_COUNT`…), for a program that runs git in a run's
/// checkout itself, such as `gh`. Settings already passed that way are kept.
pub fn run_safe_env(c: &mut Command) {
    let n: usize = std::env::var("GIT_CONFIG_COUNT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let pairs = RUN_SAFE.chunks(2).filter_map(|p| p[1].split_once('='));
    let mut i = n;
    for (k, v) in pairs {
        c.env(format!("GIT_CONFIG_KEY_{i}"), k)
            .env(format!("GIT_CONFIG_VALUE_{i}"), v);
        i += 1;
    }
    c.env("GIT_CONFIG_COUNT", i.to_string());
}

/// `git -C <dir>` with `RUN_SAFE`, for a git command in a run's checkout.
pub fn run_git_cmd(dir: &Path) -> Command {
    let mut c = Command::new("git");
    c.arg("-C").arg(dir).args(RUN_SAFE).stdin(Stdio::null());
    c
}

/// Like `git`, in a run's checkout: with `RUN_SAFE`.
pub fn run_git(dir: &Path, args: &[&str]) -> Result<String> {
    Ok(run_git_raw(dir, args)?.trim().to_string())
}

/// Like `git_raw`, in a run's checkout: with `RUN_SAFE`.
pub fn run_git_raw(dir: &Path, args: &[&str]) -> Result<String> {
    let out = run_git_cmd(dir)
        .args(args)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{write, TempDir};
    use std::os::unix::fs::PermissionsExt;

    /// An agent's hook and fsmonitor in the repo never run from amux's own git commands.
    #[test]
    fn git_in_a_run_checkout_runs_no_hooks_or_fsmonitor() {
        let t = TempDir::new();
        let repo = t.path().join("repo");
        let ran = t.path().join("ran");
        write(&repo.join("a.txt"), "a");
        let g = |args: &[&str]| {
            git(
                &repo,
                &[&["-c", "user.name=t", "-c", "user.email=t@x"], args].concat(),
            )
            .unwrap()
        };
        g(&["init", "-q"]);
        let evil = t.path().join("evil.sh");
        write(
            &evil,
            &format!("#!/bin/sh\necho \"$0\" >> {}\n", ran.display()),
        );
        std::fs::set_permissions(&evil, std::fs::Permissions::from_mode(0o755)).unwrap();
        for h in ["pre-commit", "post-commit", "pre-push"] {
            std::fs::copy(&evil, repo.join(".git/hooks").join(h)).unwrap();
        }
        g(&["config", "core.fsmonitor", evil.to_str().unwrap()]);
        run_git(&repo, &["add", "."]).unwrap();
        run_git(
            &repo,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@x",
                "commit",
                "-qm",
                "x",
            ],
        )
        .unwrap();
        run_git(&repo, &["status", "--porcelain"]).unwrap();
        assert!(!ran.exists(), "{}", std::fs::read_to_string(&ran).unwrap());
        // Plain `git` (your own commands) still runs them: the check above means something.
        let _ = git(&repo, &["status", "--porcelain"]);
        assert!(ran.exists());
    }
}
