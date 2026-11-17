//! `amux intake`: a run for each open GitHub issue (with a label) that has none yet.
//! Made to be scheduled: `amux schedule add issues "every 1h" intake --label amux claude`.

use crate::work::issues;
use anyhow::{anyhow, bail, Result};
use std::path::PathBuf;

const USAGE: &str = "amux intake [--label <label>] [--limit <n>] [--dry-run] [spawn options] <provider>[@name] [args...]";

pub fn intake_cmd(argv: &[String]) -> Result<i32> {
    let mut label = None;
    let mut limit = 3usize;
    let mut dry = false;
    let mut i = 0;
    while let Some(a) = argv.get(i) {
        match a.as_str() {
            "--label" | "-l" => {
                i += 1;
                label = Some(
                    argv.get(i)
                        .filter(|v| !v.starts_with('-'))
                        .ok_or_else(|| anyhow!("--label needs a label. Usage: {USAGE}"))?
                        .clone(),
                );
            }
            "--limit" => {
                i += 1;
                limit = argv
                    .get(i)
                    .and_then(|v| v.parse().ok())
                    .filter(|n| (1..=50).contains(n))
                    .ok_or_else(|| anyhow!("--limit is a number from 1 to 50"))?;
            }
            "--dry-run" | "-n" => dry = true,
            _ => break,
        }
        i += 1;
    }
    let spawn: &[String] = argv.get(i..).unwrap_or_default();
    if spawn.iter().all(|a| a.starts_with('-')) {
        bail!("Which account should work on the issues? Usage: {USAGE}");
    }
    if spawn.iter().any(|a| a == "--issue" || a == "--name") {
        bail!("intake names each run after its issue (issue-<n>); leave out --issue and --name.");
    }
    // The repo: spawn's --dir, or here.
    let dir = spawn
        .iter()
        .position(|a| a == "--dir" || a == "-C")
        .and_then(|p| spawn.get(p + 1))
        .map(|d| crate::paths::expand_home(d))
        .map_or_else(std::env::current_dir, |p: PathBuf| Ok(p))?;
    let repo = issues::repo_key(&dir);
    let taken = issues::taken(&repo);
    // Ask for more than the limit, since some may be taken already.
    let new: Vec<_> = issues::open(&dir, label.as_deref(), 100)?
        .into_iter()
        .filter(|i| !taken.contains(&i.number))
        .take(limit)
        .collect();
    let which = label.map_or_else(String::new, |l| format!(" labeled {l}"));
    if new.is_empty() {
        println!("No new open issues{which}.");
        return Ok(0);
    }
    let mut failed = 0;
    for issue in &new {
        if dry {
            println!(
                "Would start issue-{}: #{} {}",
                issue.number, issue.number, issue.title
            );
            continue;
        }
        // Started by hand already (`amux spawn --issue`).
        if crate::runs::run_dir(&format!("issue-{}", issue.number)).exists() {
            issues::mark_taken(&repo, issue.number)?;
            continue;
        }
        let mut args = vec!["--issue".to_string(), issue.number.to_string()];
        args.extend(spawn.iter().cloned());
        match super::runs::spawn_cmd(&args) {
            Ok(0) => {
                issues::mark_taken(&repo, issue.number)?;
                println!(
                    "Started issue-{}: #{} {}",
                    issue.number, issue.number, issue.title
                );
            }
            Ok(_) => failed += 1,
            Err(e) => {
                eprintln!("amux: issue #{}: {e:#}", issue.number);
                failed += 1;
            }
        }
    }
    Ok(if failed > 0 { 1 } else { 0 })
}
