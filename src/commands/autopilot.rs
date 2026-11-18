//! `amux autopilot`: runs that take themselves from commits to a PR that's ready to merge
//! (work/autopilot.rs).

use crate::runs;
use crate::time;
use crate::util::{ago, one_line, table};
use crate::work::{autopilot, github};
use anyhow::{anyhow, bail, Result};

const USAGE: &str =
    "amux autopilot [--json] | <run> [--merge] [--retries <n>] | <run> --off | <run> --log";

pub fn autopilot_cmd(argv: &[String]) -> Result<i32> {
    let mut run = None;
    let (mut merge, mut off, mut log, mut json) = (false, false, false, false);
    let mut retries = None;
    let mut it = argv.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--merge" => merge = true,
            "--off" => off = true,
            "--log" => log = true,
            "--json" => json = true,
            "--retries" => {
                retries = Some(
                    it.next()
                        .and_then(|v| v.parse().ok())
                        .filter(|n| *n <= 20)
                        .ok_or_else(|| anyhow!("--retries is a number from 0 to 20"))?,
                )
            }
            f if f.starts_with('-') => bail!("Unknown option {f}. Usage: {USAGE}"),
            _ if run.is_some() => bail!("One run at a time. Usage: {USAGE}"),
            _ => run = Some(a.clone()),
        }
    }
    let Some(r) = run else {
        if merge || off || log || retries.is_some() {
            bail!("Which run? Usage: {USAGE}");
        }
        return list(json);
    };
    let run = runs::find(&r)?;
    if log {
        let h = autopilot::history(&run.id);
        if h.is_empty() {
            println!("Nothing yet for {}.", run.id);
        } else {
            print!("{h}");
        }
        return Ok(0);
    }
    if off {
        if autopilot::disable(&run.id) {
            println!("Autopilot is off for {}; its PR is left as it is.", run.id);
        } else {
            println!("{} wasn't on autopilot.", run.id);
        }
        return Ok(0);
    }
    if run.worktree.is_none() {
        bail!(
            "{} has no worktree of its own, so no branch for a pull request. Start one with `amux spawn --autopilot`.",
            run.id
        );
    }
    let p = autopilot::enable(&run.id, merge, retries)?;
    println!(
        "Autopilot is on for {}: it opens a PR when the agent commits, hands back failing CI (up to {} times) and review comments, and {}.",
        run.id,
        p.retries,
        if p.merge { "merges the PR when it's ready" } else { "tells you when the PR is ready to merge" }
    );
    Ok(0)
}

fn list(json: bool) -> Result<i32> {
    let now = time::now_ms();
    let all: Vec<_> = runs::list()
        .into_iter()
        .filter_map(|r| autopilot::load(&r.id).map(|p| (r, p)))
        .collect();
    if json {
        let v: Vec<_> = all
            .iter()
            .map(|(r, p)| {
                serde_json::json!({
                    "run": r.id,
                    "phase": p.phase,
                    "detail": p.detail,
                    "since": p.since,
                    "merge": p.merge,
                    "fixes": p.fixes,
                    "pr": github::load(&r.id).map(|pr| pr.url),
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&v)?);
        return Ok(0);
    }
    if all.is_empty() {
        println!("No runs on autopilot.");
        println!("  amux spawn --autopilot claude           opens a PR, fixes CI and review comments, says when it's ready");
        println!("  amux spawn --autopilot=merge claude     … and merges it");
        println!("  amux intake --label amux --autopilot claude   the same for each new issue");
        return Ok(0);
    }
    let rows: Vec<Vec<String>> = all
        .iter()
        .map(|(r, p)| {
            vec![
                r.id.clone(),
                p.phase.clone(),
                github::load(&r.id)
                    .map(|pr| pr.label())
                    .unwrap_or_else(|| "-".into()),
                one_line(&p.detail, 60),
                ago(p.since, now),
            ]
        })
        .collect();
    println!(
        "{}",
        table(&["RUN", "PHASE", "PR", "DETAIL", "SINCE"], &rows).trim_end()
    );
    Ok(0)
}
