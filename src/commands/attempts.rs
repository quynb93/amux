//! Several attempts at one task, compared on evidence, and the best one kept.
//!
//!   amux try 3 claude -p "fix the flaky login test" --name login
//!   amux try claude@work,codex@me -p "…" --name login      one per account or CLI
//!   amux compare login          test each in a test slot, rank them
//!   amux pick login-2 --land    keep it (land it), stop and clear away the others
//!
//! Agents given the same task come back with different answers, and the first one that
//! looks fine isn't always the best. Picking by hand means reading N diffs. `compare` runs
//! each attempt's tests in a slot, so the machine isn't flooded, and ranks them: tests
//! passing first, then the smaller change (fewer files, fewer lines), since the smallest
//! change that passes is usually the one to review and merge.

use crate::git::{base_of, git};
use crate::runs::{self, Run};
use crate::util::{need, table};
use crate::work::overlap::{self, LandOpts};
use crate::work::verify::{self, Plan, TestOutcome};
use anyhow::{anyhow, bail, Result};
use serde::Serialize;
use std::path::Path;

const TRY_USAGE: &str =
    "amux try <count> <provider>[@name] | <account>,<account>... -p <prompt> [--name <task>] [--base <ref>]";

pub fn try_cmd(argv: &[String]) -> Result<i32> {
    let mut prompt = None;
    let mut name = None;
    let mut base = None;
    let mut pos = vec![];
    let mut it = argv.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-p" | "--prompt" => prompt = Some(need(it.next(), TRY_USAGE)?.to_string()),
            "--name" => name = Some(need(it.next(), TRY_USAGE)?.to_string()),
            "--base" => base = Some(need(it.next(), TRY_USAGE)?.to_string()),
            f if f.starts_with('-') => bail!("Unknown option {f}. Usage: {TRY_USAGE}"),
            p => pos.push(p.to_string()),
        }
    }
    let prompt = prompt.ok_or_else(|| anyhow!("Give the task with -p. Usage: {TRY_USAGE}"))?;
    // Either "3 claude" (three attempts by one account) or "claude@work,codex@me".
    let accounts: Vec<String> = match pos.as_slice() {
        [n, acct] if n.parse::<usize>().is_ok() => {
            let n: usize = n.parse()?;
            if !(1..=8).contains(&n) {
                bail!("Between 1 and 8 attempts.");
            }
            vec![acct.clone(); n]
        }
        [list] => list
            .split(',')
            .map(str::trim)
            .filter(|a| !a.is_empty())
            .map(String::from)
            .collect(),
        _ => bail!("Usage: {TRY_USAGE}"),
    };
    if accounts.is_empty() || accounts.len() > 8 {
        bail!("Between 1 and 8 attempts.");
    }
    let name = name.unwrap_or_else(|| "try".into());
    if !crate::store::valid_name(&name) {
        bail!("Task names use letters, digits, '.', '_' and '-'.");
    }
    if runs::list()
        .iter()
        .any(|r| r.group.as_deref() == Some(name.as_str()))
    {
        bail!("There are attempts named \"{name}\" already. Pick another --name, or `amux compare {name}`.");
    }
    for (i, acct) in accounts.iter().enumerate() {
        let mut args = vec![
            "--name".to_string(),
            format!("{name}-{}", i + 1),
            "--worktree".into(),
            "--group".into(),
            name.clone(),
            "--prompt".into(),
            prompt.clone(),
        ];
        if let Some(b) = &base {
            args.extend(["--base".to_string(), b.clone()]);
        }
        args.push(acct.clone());
        super::runs::spawn_cmd(&args)?;
    }
    eprintln!(
        "amux: {} attempts at \"{name}\". When they're done: amux compare {name}",
        accounts.len()
    );
    Ok(0)
}

/// One attempt, measured.
#[derive(Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Attempt {
    pub run: String,
    pub account: String,
    pub state: String,
    /// passed, failed, untested (no test command) or skipped.
    pub tests: String,
    pub files: usize,
    pub added: usize,
    pub removed: usize,
    pub commits: usize,
    pub minutes: i64,
}

/// "3 files changed, 40 insertions(+), 2 deletions(-)" → (40, 2).
fn shortstat(s: &str) -> (usize, usize) {
    let num = |word: &str| {
        s.split(',')
            .find(|p| p.contains(word))
            .and_then(|p| p.split_whitespace().next()?.parse().ok())
            .unwrap_or(0)
    };
    (num("insertion"), num("deletion"))
}

fn measure(run: &Run, test: bool) -> Attempt {
    let now = crate::time::now_ms();
    let (kind, label) = crate::runs::state::run_kind(run, now);
    let mut a = Attempt {
        run: run.id.clone(),
        account: run.account.clone(),
        state: label,
        tests: "skipped".into(),
        files: 0,
        added: 0,
        removed: 0,
        commits: 0,
        minutes: (run.ended_at.unwrap_or(now) - run.started_at) / 60_000,
    };
    let (Some(wt), Some(base)) = (&run.worktree, base_of(run)) else {
        return a;
    };
    let dir = Path::new(&wt.path);
    // Against where it started, uncommitted changes included, plus new files.
    let tracked = git(dir, &["diff", "--name-only", &base]).unwrap_or_default();
    let untracked = git(dir, &["ls-files", "--others", "--exclude-standard"]).unwrap_or_default();
    a.files = tracked
        .lines()
        .chain(untracked.lines())
        .filter(|l| !l.is_empty())
        .count();
    (a.added, a.removed) =
        shortstat(&git(dir, &["diff", "--shortstat", &base]).unwrap_or_default());
    a.commits = git(dir, &["rev-list", "--count", &format!("{base}..HEAD")])
        .ok()
        .and_then(|n| n.parse().ok())
        .unwrap_or(0);
    // A run still working could change under the tests; compare what's finished.
    if test && kind != crate::runs::state::Kind::Working {
        a.tests = match verify::run_tests(run, Plan::for_run(run).test) {
            Ok(TestOutcome::Passed { .. }) => "passed".into(),
            Ok(TestOutcome::Failed { .. }) => "failed".into(),
            Ok(TestOutcome::NoSlot) => "skipped".into(),
            Err(_) => "untested".into(),
        };
    }
    a
}

/// Best first: tests passing, then untested, then failing; within that, fewer files
/// touched, then fewer lines changed. Attempts that changed nothing go last.
pub fn rank(mut v: Vec<Attempt>) -> Vec<Attempt> {
    let tests = |t: &str| match t {
        "passed" => 0,
        "untested" | "skipped" => 1,
        _ => 2,
    };
    v.sort_by_key(|a| (a.files == 0, tests(&a.tests), a.files, a.added + a.removed));
    v
}

const COMPARE_USAGE: &str = "amux compare <task> [--no-test] [--json]";

pub fn compare_cmd(argv: &[String]) -> Result<i32> {
    let test = !argv.iter().any(|a| a == "--no-test");
    let json = argv.iter().any(|a| a == "--json");
    let task = need(argv.iter().find(|a| !a.starts_with('-')), COMPARE_USAGE)?;
    let group: Vec<Run> = runs::list()
        .into_iter()
        .filter(|r| r.group.as_deref() == Some(task))
        .collect();
    if group.is_empty() {
        bail!("No attempts named \"{task}\". Start some with `amux try`.");
    }
    if test && !json {
        eprintln!(
            "amux: testing {} attempts, one test slot at a time…",
            group.len()
        );
    }
    let ranked = rank(group.iter().map(|r| measure(r, test)).collect());
    if json {
        println!("{}", serde_json::to_string_pretty(&ranked)?);
        return Ok(0);
    }
    let rows: Vec<Vec<String>> = ranked
        .iter()
        .enumerate()
        .map(|(i, a)| {
            vec![
                if i == 0 && a.files > 0 {
                    "★".into()
                } else {
                    String::new()
                },
                a.run.clone(),
                a.account.clone(),
                a.state.clone(),
                a.tests.clone(),
                a.files.to_string(),
                format!("+{} −{}", a.added, a.removed),
                a.commits.to_string(),
                format!("{}m", a.minutes),
            ]
        })
        .collect();
    println!(
        "{}",
        table(
            &["", "ATTEMPT", "ACCOUNT", "STATE", "TESTS", "FILES", "LINES", "COMMITS", "TIME"],
            &rows
        )
    );
    if let Some(best) = ranked.first().filter(|a| a.files > 0) {
        println!(
            "\nBest: {} ({}{} file(s), +{} −{}). Read it with `amux diff {}`; keep it with `amux pick {} --land`.",
            best.run,
            if best.tests == "passed" { "tests pass, " } else { "" },
            best.files,
            best.added,
            best.removed,
            best.run,
            best.run
        );
    } else {
        println!("\nNo attempt has changed anything yet.");
    }
    Ok(0)
}

const PICK_USAGE: &str = "amux pick <run> [--land] [--keep-others]";

pub fn pick_cmd(argv: &[String]) -> Result<i32> {
    let land = argv.iter().any(|a| a == "--land");
    let keep = argv.iter().any(|a| a == "--keep-others");
    let run = runs::find(need(argv.iter().find(|a| !a.starts_with('-')), PICK_USAGE)?)?;
    let Some(group) = run.group.clone() else {
        bail!("{} isn't one of several attempts (`amux try`).", run.id);
    };
    if !keep {
        for other in runs::list()
            .into_iter()
            .filter(|r| r.group.as_deref() == Some(group.as_str()) && r.id != run.id)
        {
            // rm stops it and removes its worktree only if git says it holds no
            // uncommitted work; its branch is always kept.
            super::runs::rm_cmd(&[other.id.clone(), "--force".into()])?;
        }
    }
    if !land {
        println!("Kept {}. Land it with `amux land {}`.", run.id, run.id);
        return Ok(0);
    }
    let done = overlap::land(
        std::slice::from_ref(&run.id),
        &LandOpts {
            test: true,
            tell: true,
            ..Default::default()
        },
        |m| eprintln!("amux: {m}"),
    )?;
    let l = &done[0];
    println!("{}: {} — {}", l.run, l.outcome, l.detail);
    Ok(if l.outcome == "landed" { 0 } else { 1 })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(run: &str, tests: &str, files: usize, lines: usize) -> Attempt {
        Attempt {
            run: run.into(),
            account: String::new(),
            state: String::new(),
            tests: tests.into(),
            files,
            added: lines,
            removed: 0,
            commits: 0,
            minutes: 0,
        }
    }

    #[test]
    fn ranks_passing_small_changes_first() {
        let r = rank(vec![
            a("big-pass", "passed", 9, 400),
            a("fail", "failed", 1, 2),
            a("nothing", "passed", 0, 0),
            a("small-pass", "passed", 2, 30),
            a("untested", "untested", 1, 5),
        ]);
        let order: Vec<&str> = r.iter().map(|a| a.run.as_str()).collect();
        assert_eq!(
            order,
            ["small-pass", "big-pass", "untested", "fail", "nothing"]
        );
        assert_eq!(
            shortstat(" 3 files changed, 40 insertions(+), 2 deletions(-)"),
            (40, 2)
        );
        assert_eq!(shortstat(" 1 file changed, 1 deletion(-)"), (0, 1));
    }
}
