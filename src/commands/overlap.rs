//! `amux overlap` and `amux land`: parallel runs on one repo, before they collide, and a
//! local merge queue for them. The logic is in work/overlap.rs.

use crate::util::table;
use crate::work::overlap::{self, LandOpts};
use anyhow::{bail, Result};

const OVERLAP_USAGE: &str = "amux overlap [<run>...] [--into <branch>] [--json] [--check]";

fn list(names: &[String]) -> String {
    let shown: Vec<&str> = names.iter().take(4).map(String::as_str).collect();
    let more = names.len().saturating_sub(4);
    let mut s = shown.join(", ");
    if more > 0 {
        s.push_str(&format!(" +{more}"));
    }
    s
}

pub fn overlap_cmd(argv: &[String]) -> Result<i32> {
    let mut into = None;
    let mut json = false;
    let mut check = false;
    let mut only = vec![];
    let mut it = argv.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--into" => into = Some(crate::util::need(it.next(), OVERLAP_USAGE)?.to_string()),
            "--json" => json = true,
            "--check" => check = true,
            f if f.starts_with('-') => bail!("Unknown option {f}. Usage: {OVERLAP_USAGE}"),
            r => only.push(crate::runs::find(r)?.id),
        }
    }
    let reports = overlap::reports(into.as_deref(), &only)?;
    let conflicted = reports.iter().any(|r| r.has_conflicts());
    if json {
        println!("{}", serde_json::to_string_pretty(&reports)?);
        return Ok(if check && conflicted { 1 } else { 0 });
    }
    if reports.iter().all(|r| r.runs.is_empty()) {
        println!("No run with a worktree has changed anything yet.");
        return Ok(0);
    }
    for r in &reports {
        if r.runs.is_empty() {
            continue;
        }
        println!(
            "{} (into {})",
            crate::paths::tildify(std::path::Path::new(&r.repo)),
            r.target
        );
        let rows: Vec<Vec<String>> = r
            .runs
            .iter()
            .map(|c| {
                vec![
                    c.run.clone(),
                    c.branch.clone(),
                    c.files.len().to_string(),
                    if c.conflicts_with_target.is_empty() {
                        "-".into()
                    } else {
                        format!("✗ {}", list(&c.conflicts_with_target))
                    },
                ]
            })
            .collect();
        println!(
            "{}",
            table(
                &[
                    "RUN",
                    "BRANCH",
                    "FILES",
                    &format!("CONFLICTS WITH {}", r.target.to_uppercase())
                ],
                &rows
            )
        );
        if r.pairs.is_empty() {
            println!("No two runs touch the same file.\n");
            continue;
        }
        for p in &r.pairs {
            if p.conflicts.is_empty() {
                println!(
                    "  {} ↔ {}: both change {}, but merge cleanly",
                    p.a,
                    p.b,
                    list(&p.shared)
                );
            } else {
                println!("  ✗ {} ↔ {}: conflict in {}", p.a, p.b, list(&p.conflicts));
            }
        }
        println!();
    }
    if conflicted {
        println!("Land runs one at a time with `amux land <run>...`: each rebases onto what landed before it, and a run that conflicts is told what to fix.");
    }
    Ok(if check && conflicted { 1 } else { 0 })
}

const LAND_USAGE: &str =
    "amux land <run>... [--into <branch>] [--command <test cmd> | --no-test] [--host-test] [--no-tell] [--dry-run] [--json]";

pub fn land_cmd(argv: &[String]) -> Result<i32> {
    let mut o = LandOpts {
        test: true,
        tell: true,
        ..Default::default()
    };
    let mut json = false;
    let mut ids = vec![];
    let mut it = argv.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--into" => o.into = Some(crate::util::need(it.next(), LAND_USAGE)?.to_string()),
            "--no-test" => o.test = false,
            "--host-test" => o.host_test = true,
            "--command" => o.command = Some(crate::util::need(it.next(), LAND_USAGE)?.to_string()),
            "--no-tell" => o.tell = false,
            "--dry-run" | "-n" => o.dry_run = true,
            "--json" => json = true,
            f if f.starts_with('-') => bail!("Unknown option {f}. Usage: {LAND_USAGE}"),
            r => ids.push(crate::runs::find(r)?.id),
        }
    }
    if ids.is_empty() {
        bail!("Usage: {LAND_USAGE}");
    }
    let quiet = json;
    let done = overlap::land(&ids, &o, |msg| {
        if !quiet {
            eprintln!("amux: {msg}");
        }
    })?;
    if json {
        println!("{}", serde_json::to_string_pretty(&done)?);
    } else {
        let rows: Vec<Vec<String>> = done
            .iter()
            .map(|l| {
                let mut d = l.detail.clone();
                if l.told {
                    d.push_str(" (told the agent)");
                }
                vec![l.run.clone(), l.outcome.clone(), d]
            })
            .collect();
        println!("{}", table(&["RUN", "RESULT", "DETAIL"], &rows));
    }
    let all_ok = done
        .iter()
        .all(|l| matches!(l.outcome.as_str(), "landed" | "ready"));
    Ok(if all_ok { 0 } else { 1 })
}
