//! `amux intake`: a run for each open issue (with a label) that has none yet, from
//! GitHub, Linear or Jira. Made to be scheduled:
//! `amux schedule add issues "every 1h" intake --label amux claude`.

use crate::work::issues::{self, Issue, Source};
use crate::work::trackers;
use anyhow::{anyhow, bail, Result};
use std::path::PathBuf;

const USAGE: &str = "amux intake [--from github|linear|jira] [--label <label>] [--team <key>] [--project <key>] [--jql <query>] [--limit <n>] [--dry-run] [spawn options] <provider>[@name] [args...]";

pub fn intake_cmd(argv: &[String]) -> Result<i32> {
    let mut from = Source::GitHub;
    let (mut label, mut team, mut project, mut jql) = (None, None, None, None);
    let mut limit = 3usize;
    let mut dry = false;
    let mut i = 0;
    while let Some(a) = argv.get(i) {
        let mut value = |what: &str| -> Result<String> {
            i += 1;
            argv.get(i)
                .filter(|v| !v.is_empty() && !v.starts_with('-'))
                .cloned()
                .ok_or_else(|| anyhow!("{a} needs {what}. Usage: {USAGE}"))
        };
        match a.as_str() {
            "--from" => {
                let v = value("github, linear or jira")?;
                from = Source::from_tag(&v)
                    .ok_or_else(|| anyhow!("--from is github, linear or jira, not \"{v}\"."))?;
            }
            "--label" | "-l" => label = Some(value("a label")?),
            "--team" => team = Some(value("a team key")?),
            "--project" => project = Some(value("a project key")?),
            "--jql" => jql = Some(value("a JQL query")?),
            "--limit" => {
                limit = value("a number")?
                    .parse()
                    .ok()
                    .filter(|n| (1..=50).contains(n))
                    .ok_or_else(|| anyhow!("--limit is a number from 1 to 50"))?;
            }
            "--dry-run" | "-n" => dry = true,
            _ => break,
        }
        i += 1;
    }
    match from {
        Source::GitHub if team.is_some() || project.is_some() || jql.is_some() => {
            bail!("--team, --project and --jql go with --from linear or --from jira.")
        }
        Source::Linear if project.is_some() || jql.is_some() => {
            bail!("--project and --jql are for Jira; Linear takes --label and --team.")
        }
        // A whole workspace's open issues is never what's meant.
        Source::Linear if label.is_none() && team.is_none() => {
            bail!("Which Linear issues? Give --label, --team or both. Usage: {USAGE}")
        }
        Source::Jira if team.is_some() => {
            bail!("--team is for Linear; Jira takes --jql, or --label and --project.")
        }
        Source::Jira if jql.is_some() && (label.is_some() || project.is_some()) => {
            bail!("Give Jira either --jql, or --label and --project, not both.")
        }
        Source::Jira if jql.is_none() && label.is_none() && project.is_none() => {
            bail!("Which Jira issues? Give --jql, or --label and --project. Usage: {USAGE}")
        }
        _ => {}
    }
    let spawn: &[String] = argv.get(i..).unwrap_or_default();
    if spawn.iter().all(|a| a.starts_with('-')) {
        bail!("Which account should work on the issues? Usage: {USAGE}");
    }
    if spawn.iter().any(|a| a == "--issue" || a == "--name") {
        bail!("intake names each run after its issue (issue-12, eng-123); leave out --issue and --name.");
    }
    // The repo: spawn's --dir, or here.
    let dir = spawn
        .iter()
        .position(|a| a == "--dir" || a == "-C")
        .and_then(|p| spawn.get(p + 1))
        .map(|d| crate::paths::expand_home(d))
        .map_or_else(std::env::current_dir, |p: PathBuf| Ok(p))?;
    let repo = issues::repo_key(&dir);
    // Ask for more than the limit, since some may be taken already.
    let found: Vec<Issue> = match from {
        Source::GitHub => issues::open(&dir, label.as_deref(), 100)?,
        Source::Linear => trackers::linear_open(label.as_deref(), team.as_deref())?,
        Source::Jira => trackers::jira_open(&trackers::jira_query(
            jql.as_deref(),
            label.as_deref(),
            project.as_deref(),
        ))?,
    };
    let taken = issues::taken(&repo);
    let new: Vec<_> = found
        .into_iter()
        .filter(|i| !taken.contains(&i.intake_id()))
        .take(limit)
        .collect();
    if new.is_empty() {
        let source = match from {
            Source::GitHub => String::new(),
            s => format!("{} ", s.name()),
        };
        let which = match (&jql, &label) {
            (Some(q), _) => format!(" matching {q}"),
            (None, Some(l)) => format!(" labeled {l}"),
            _ => String::new(),
        };
        let place = team
            .or(project)
            .map_or_else(String::new, |t| format!(" in {t}"));
        println!("No new open {source}issues{which}{place}.");
        return Ok(0);
    }
    let mut failed = 0;
    let runs = crate::runs::list();
    for issue in &new {
        // Started by hand already (`amux spawn --issue`) in this repo: the same issue from
        // the same tracker, whatever its run is called. Linear's ENG-1 and Jira's ENG-1
        // are different issues.
        if runs
            .iter()
            .any(|r| issue.is_run_of(r) && issues::run_repo(r) == repo)
        {
            if !dry {
                issues::mark_taken(&repo, issue)?;
            }
            continue;
        }
        let (name, label) = (issue.new_run_name(&repo), issue.label());
        if dry {
            println!("Would start {name}: {label} {}", issue.title);
            continue;
        }
        let mut args = vec!["--issue".to_string(), issue.spawn_ref()];
        args.extend(spawn.iter().cloned());
        match super::runs::spawn_cmd(&args) {
            Ok(0) => {
                issues::mark_taken(&repo, issue)?;
                println!("Started {name}: {label} {}", issue.title);
            }
            Ok(_) => failed += 1,
            Err(e) => {
                eprintln!("amux: issue {label}: {e:#}");
                failed += 1;
            }
        }
    }
    Ok(if failed > 0 { 1 } else { 0 })
}
