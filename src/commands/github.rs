//! `amux pr` and `amux checks`: a run's pull request and CI, through the GitHub CLI.

use crate::runs;
use crate::util::need;
use crate::work::github;
use anyhow::{anyhow, bail, Result};

const PR_USAGE: &str = "amux pr <run> [--draft] [--base <branch>]";

pub fn pr_cmd(argv: &[String]) -> Result<i32> {
    let mut draft = false;
    let mut base = None;
    let mut id = None;
    let mut it = argv.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--draft" | "-d" => draft = true,
            "--base" | "-B" => {
                let b = it.next().ok_or_else(|| anyhow!("--base needs a branch."))?;
                if b.starts_with('-') {
                    bail!("--base needs a branch.");
                }
                base = Some(b.clone());
            }
            f if f.starts_with('-') => bail!("Unknown option {f}. Usage: {PR_USAGE}"),
            _ => id = Some(a.clone()),
        }
    }
    let run = runs::find(need(id.as_ref(), PR_USAGE)?)?;
    let o = github::open_pr(&run, draft, base.as_deref())?;
    if o.left_out > 0 {
        let n = o.left_out;
        eprintln!(
            "amux: {n} uncommitted file{} in the worktree {} not in the PR.",
            if n == 1 { "" } else { "s" },
            if n == 1 { "is" } else { "are" }
        );
    }
    match &o.commented {
        Some(Ok(key)) => eprintln!("amux: posted the PR's link on {key}."),
        Some(Err(e)) => eprintln!("amux: could not post the PR's link on the issue: {e}"),
        None => {}
    }
    if o.created {
        println!("Opened PR #{} for {}: {}", o.pr.number, run.id, o.pr.url);
    } else {
        println!("{} already has PR #{}: {}", run.id, o.pr.number, o.pr.url);
    }
    Ok(0)
}

const CHECKS_USAGE: &str = "amux checks <run> [--send-failures]";

pub fn checks_cmd(argv: &[String]) -> Result<i32> {
    let send = argv.iter().any(|a| a == "--send-failures" || a == "-f");
    let quiet = argv.iter().any(|a| a == "--quiet" || a == "-q");
    if let Some(f) = argv.iter().find(|a| {
        a.starts_with('-') && !matches!(a.as_str(), "--send-failures" | "-f" | "--quiet" | "-q")
    }) {
        bail!("Unknown option {f}. Usage: {CHECKS_USAGE}");
    }
    let run = runs::find(need(
        argv.iter().find(|a| !a.starts_with('-')),
        CHECKS_USAGE,
    )?)?;
    let (pr, checks) = github::refresh_checks(&run)?;
    if !quiet {
        println!("PR #{} {}: checks {}", pr.number, pr.url, pr.checks);
        for c in &checks {
            let mark = match c.bucket.as_str() {
                "pass" => "✓",
                "fail" | "cancel" => "✗",
                "pending" => "…",
                _ => "-",
            };
            let name = if c.workflow.is_empty() {
                c.name.clone()
            } else {
                format!("{} / {}", c.workflow, c.name)
            };
            println!("  {mark} {}", crate::util::clean(&name, false));
        }
    }
    if send {
        let n = github::send_ci_failures(&run, &pr, &checks, false)?;
        println!(
            "Sent {n} failing check{} to {}.",
            if n == 1 { "" } else { "s" },
            run.id
        );
        return Ok(0);
    }
    Ok(if pr.checks == "failing" { 1 } else { 0 })
}
