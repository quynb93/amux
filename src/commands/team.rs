//! Teams: a lead agent that splits a goal into parts, gives each to a worker agent in its
//! own worktree, waits for them, reviews and lands their work.
//!
//! - `amux lead <acct> -p <goal>` starts the lead, told how to run a team with amux.
//! - `amux spawn` from inside a run makes the new run that run's worker (`lead` in
//!   run.json). A lead has at most `--workers` going at once, and a worker can't start
//!   workers of its own, so a team can't grow without bound.
//! - A worker finishes with `amux report <summary>` (runs/<id>/report.md).
//! - `amux team` shows the lead its workers; `amux team wait` blocks until the next one
//!   finishes or needs help, and prints its report, so the lead doesn't poll.

use crate::commands::attempts::{measure, Attempt};
use crate::runs::state::{run_kind, Kind};
use crate::runs::{self, Run};
use crate::util::{need, one_line, table};
use crate::{fsx, time};
use anyhow::{anyhow, bail, Result};
use serde::Serialize;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::{IsTerminal, Read};

/// Workers a lead may have going at once when `amux lead` doesn't say.
const DEFAULT_WORKERS: u32 = 3;

/// The run this process is part of, from the environment amux gives a run's CLI.
fn this_run() -> Option<Run> {
    let id = std::env::var("AMUX_RUN").ok().filter(|s| !s.is_empty())?;
    runs::load(&id).ok()
}

/// For `amux spawn`: the lead of the run being started (the run we're inside), after
/// checking the team's limits. None outside a run.
pub fn lead_for_spawn() -> Result<Option<String>> {
    let Some(me) = this_run() else {
        return Ok(None);
    };
    if !me.state().is_live() {
        return Ok(None);
    }
    if let Some(my_lead) = &me.lead {
        bail!(
            "{} is a worker on {my_lead}'s team, and workers don't start workers of their own. \
             Do the part yourself, or say what else is needed in `amux report`.",
            me.id
        );
    }
    let max = me.team.unwrap_or(DEFAULT_WORKERS) as usize;
    let going: Vec<String> = workers(&me.id)
        .into_iter()
        .filter(|w| w.state().is_live())
        .map(|w| w.id)
        .collect();
    if going.len() >= max {
        bail!(
            "{} already has {} workers going ({}), its limit. Wait for one: `amux team wait`, \
             then `amux rm` the ones you're done with.",
            me.id,
            going.len(),
            going.join(", ")
        );
    }
    Ok(Some(me.id))
}

/// What a worker's prompt ends with.
pub fn worker_postscript(lead: &str) -> String {
    format!(
        "\n\n---\nYou are a worker on {lead}'s team (amux). Do only this task, in this folder. \
         Commit your work when it's done. Then run, in your shell:\n  \
         amux report \"<what you changed, how you checked it, and anything left or blocking>\"\n\
         If you are stuck or need a decision, report that instead of guessing."
    )
}

fn workers(lead: &str) -> Vec<Run> {
    let mut v: Vec<Run> = runs::list()
        .into_iter()
        .filter(|r| r.lead.as_deref() == Some(lead))
        .collect();
    v.sort_by_key(|r| r.started_at);
    v
}

fn report_file(id: &str) -> std::path::PathBuf {
    runs::run_dir(id).join("report.md")
}

fn report_of(id: &str) -> Option<(String, i64)> {
    let f = report_file(id);
    let text = std::fs::read_to_string(&f).ok()?;
    let at = std::fs::metadata(&f)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    Some((text.trim().to_string(), at))
}

// ---- amux lead ------------------------------------------------------------------------

const LEAD_USAGE: &str = "amux lead [--name <team>] [--workers <n>] [--worker <provider>@<name>]... [--dir <folder>] <provider>[@name] -p <goal> [-- <cli args>]";

/// The lead's instructions: how to run a team with amux.
fn lead_preamble(team: &str, max: u32, worker_accts: &[String], cli: &[String]) -> String {
    let accts = worker_accts.join(", ");
    let first = worker_accts.first().map(String::as_str).unwrap_or("claude");
    // Workers get the CLI flags the lead got (permissions, model…), after the account.
    let flags: String = cli
        .iter()
        .map(|a| format!(" {}", crate::util::sh_quote(a)))
        .collect();
    format!(
        "You are the lead of an amux team named {team}. Split the goal below into independent \
parts and give each part to a worker agent. You plan, delegate, review and integrate; leave \
the parts themselves to the workers.

Run these in your shell:
- Start a worker (at most {max} at a time; accounts: {accts}):
    amux spawn --worktree --name {team}-<part> --prompt \"<a self-contained task: \
the context it needs, which files, and how to know it's done>\" {first}{flags}
  (amux's options go before the account; the prompt must be one quoted argument)
- Wait for the next worker to finish or need you (blocks up to 9 minutes; run it again if \
it times out):
    amux team wait
- See all workers: amux team    A worker's changes: amux diff <worker>    Its screen: \
amux peek <worker>
- Answer or redirect a worker: amux send <worker> \"<text>\"   A menu or yes/no prompt: \
read it first with amux peek (the highlighted choice isn't always yes), then press keys: \
amux send <worker> --no-enter --key down --key enter
- Test a worker's work: amux verify <worker> --test
- Check for conflicts, then merge finished work one at a time, tested: amux overlap; \
amux land <worker> [--command \"<test command>\"]
- When you're done with a worker: amux rm <worker>

Give each worker a part that touches different files. Don't edit files the workers are \
working on. When every part is landed, summarize what was done and run: amux report \
\"<summary>\"

Goal:
"
    )
}

pub fn lead_cmd(argv: &[String]) -> Result<i32> {
    let mut name = None;
    let mut max = DEFAULT_WORKERS;
    let mut worker_accts: Vec<String> = vec![];
    let mut goal = None;
    let mut pass: Vec<String> = vec![];
    let mut acct = None;
    let (argv, cli): (&[String], &[String]) = match argv.iter().position(|a| a == "--") {
        Some(i) => (&argv[..i], &argv[i + 1..]),
        None => (argv, &[]),
    };
    let mut it = argv.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--name" => name = Some(need(it.next(), LEAD_USAGE)?.to_string()),
            "--workers" => {
                max = need(it.next(), LEAD_USAGE)?
                    .parse()
                    .ok()
                    .filter(|n| (1..=32).contains(n))
                    .ok_or_else(|| anyhow!("--workers is a number from 1 to 32"))?
            }
            "--worker" => worker_accts.push(need(it.next(), LEAD_USAGE)?.to_string()),
            "-p" | "--prompt" => goal = Some(need(it.next(), LEAD_USAGE)?.to_string()),
            "--dir" | "--stall" | "--max-time" => {
                pass.push(a.clone());
                pass.push(need(it.next(), LEAD_USAGE)?.to_string());
            }
            "--worktree" => pass.push(a.clone()),
            s if s.starts_with('-') => bail!("Unknown argument {a}. Usage: {LEAD_USAGE}"),
            _ if acct.is_none() => acct = Some(a.clone()),
            _ => bail!("Usage: {LEAD_USAGE}"),
        }
    }
    let Some(acct) = acct else {
        bail!("Usage: {LEAD_USAGE}")
    };
    let goal = match goal {
        Some(g) if !g.trim().is_empty() => g,
        _ => bail!("A lead needs a goal: -p \"<goal>\""),
    };
    if this_run().is_some_and(|r| r.state().is_live()) {
        bail!("A run can't start a lead; start workers with `amux spawn` instead.");
    }
    for w in &worker_accts {
        crate::store::resolve_account(w)?;
    }
    if worker_accts.is_empty() {
        worker_accts.push(acct.clone());
    }
    let team = match name {
        Some(n) => n,
        None => runs::claim_id(None, "team")?,
    };
    let prompt = format!("{}{goal}", lead_preamble(&team, max, &worker_accts, cli));
    let mut args: Vec<String> = vec![
        "--name".into(),
        team.clone(),
        "--workers".into(),
        max.to_string(),
        "--prompt".into(),
        prompt,
    ];
    args.extend(pass);
    args.push(acct);
    args.extend(cli.iter().cloned());
    // claim_id made a placeholder for the generated name; spawn claims it for real.
    let _ = std::fs::remove_dir_all(runs::run_dir(&team));
    let code = super::runs::spawn_cmd(&args)?;
    if code == 0 {
        eprintln!("  The lead starts workers itself (up to {max}).  amux team {team}    amux team wait {team}");
    }
    Ok(code)
}

// ---- amux report ----------------------------------------------------------------------

const REPORT_USAGE: &str = "amux report [--run <run>] <text...> | -";

pub fn report_cmd(argv: &[String]) -> Result<i32> {
    let (id, rest) = match argv.first().map(String::as_str) {
        Some("--run") => (need(argv.get(1), REPORT_USAGE)?.to_string(), &argv[2..]),
        _ => match std::env::var("AMUX_RUN").ok().filter(|s| !s.is_empty()) {
            Some(id) => (id, argv),
            None => bail!("Run this inside an amux run, or name it: {REPORT_USAGE}"),
        },
    };
    let run = runs::find(&id)?;
    let piped = rest.is_empty() && !std::io::stdin().is_terminal();
    let text = if (rest.len() == 1 && rest[0] == "-") || piped {
        let mut s = String::new();
        std::io::stdin().read_to_string(&mut s)?;
        s
    } else {
        rest.join(" ")
    };
    if text.trim().is_empty() {
        bail!("Usage: {REPORT_USAGE}");
    }
    println!("{}", save_report(&run, &text)?);
    Ok(0)
}

/// Saves a run's report for its lead (`amux report`, and the MCP `report` tool). Returns
/// what to tell whoever reported.
pub fn save_report(run: &Run, text: &str) -> Result<String> {
    let text = text.trim();
    if text.is_empty() {
        bail!("A report needs text: what was done, how it was checked, what's left.");
    }
    fsx::write_atomic(&report_file(&run.id), format!("{text}\n").as_bytes())?;
    Ok(match &run.lead {
        Some(l) => format!("Reported to {l}. It sees this in `amux team wait`."),
        None => format!("Saved as {}'s report (`amux team`).", run.id),
    })
}

// ---- amux team ------------------------------------------------------------------------

const TEAM_USAGE: &str = "amux team [<lead>] [--json] | amux team wait [<lead>] [--timeout <secs>]";

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Member {
    #[serde(flatten)]
    attempt: Attempt,
    branch: Option<String>,
    /// passed, failed… from `amux verify`, if it ran.
    verify: Option<String>,
    waiting: Option<String>,
    report: Option<String>,
}

fn member(r: &Run) -> Member {
    Member {
        attempt: measure(r, false),
        branch: r.worktree.as_ref().map(|w| w.branch.clone()),
        verify: crate::work::verify::load(&r.id).map(|v| format!("{} {}", v.kind, v.state)),
        waiting: r.waiting.as_ref().map(|w| w.reason.clone()),
        report: report_of(&r.id).map(|(t, _)| t),
    }
}

/// The lead named, or the run we're inside, or the only lead there is.
fn which_lead(arg: Option<&str>) -> Result<Run> {
    if let Some(a) = arg {
        return runs::find(a);
    }
    if let Some(me) = this_run() {
        return Ok(match &me.lead {
            Some(l) => runs::find(l)?,
            None => me,
        });
    }
    let leads: Vec<Run> = runs::list()
        .into_iter()
        .filter(|r| r.team.is_some() && r.state().is_live())
        .collect();
    match leads.as_slice() {
        [one] => Ok(one.clone()),
        [] => bail!("No team is running. Start one: amux lead <provider>[@name] -p \"<goal>\""),
        many => bail!(
            "Which team? {}",
            many.iter()
                .map(|r| r.id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

pub fn team_cmd(argv: &[String]) -> Result<i32> {
    if argv.first().map(String::as_str) == Some("wait") {
        return wait(&argv[1..]);
    }
    let json = argv.iter().any(|a| a == "--json");
    let pos: Vec<&String> = argv.iter().filter(|a| !a.starts_with('-')).collect();
    if pos.len() > 1 || argv.iter().any(|a| a.starts_with('-') && a != "--json") {
        bail!("Usage: {TEAM_USAGE}");
    }
    let lead = which_lead(pos.first().map(|s| s.as_str()))?;
    let members: Vec<Member> = workers(&lead.id).iter().map(member).collect();
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "lead": lead.id,
                "account": lead.account,
                "state": run_kind(&lead, time::now_ms()).1,
                "maxWorkers": lead.team,
                "workers": members,
            }))?
        );
        return Ok(0);
    }
    let (_, state) = run_kind(&lead, time::now_ms());
    println!(
        "{} ({}, {state}) · {} worker(s), up to {} at once\n",
        lead.id,
        lead.account,
        members.len(),
        lead.team.unwrap_or(DEFAULT_WORKERS)
    );
    if members.is_empty() {
        println!("No workers yet.");
        return Ok(0);
    }
    let rows: Vec<Vec<String>> = members
        .iter()
        .map(|m| {
            let a = &m.attempt;
            let note = match (&m.waiting, &m.report) {
                (Some(w), _) => format!("waiting: {}", one_line(w, 50)),
                (_, Some(r)) => one_line(r, 60),
                _ => String::new(),
            };
            vec![
                a.run.clone(),
                a.account.clone(),
                a.state.clone(),
                format!("{}", a.files),
                format!("+{} −{}", a.added, a.removed),
                m.verify.clone().unwrap_or_else(|| "-".into()),
                note,
            ]
        })
        .collect();
    print!(
        "{}",
        table(
            &["WORKER", "ACCOUNT", "STATE", "FILES", "LINES", "VERIFY", "REPORT"],
            &rows
        )
    );
    Ok(0)
}

/// Whether a worker has stopped and wants its lead: its turn ended, it waits for an
/// answer, or it exited. A run that's just started and quiet hasn't begun yet.
fn settled(r: &Run, now: i64) -> Option<Kind> {
    let (kind, _) = run_kind(r, now);
    match kind {
        Kind::Starting | Kind::Working => None,
        Kind::Idle if now - r.started_at < 15_000 => None,
        k => Some(k),
    }
}

/// What the lead has already been told about, so each change is told once.
fn seen_file(lead: &str) -> std::path::PathBuf {
    runs::run_dir(lead).join("team-seen.json")
}

fn fingerprint(r: &Run, k: Kind) -> String {
    let report = report_of(&r.id).map(|(_, at)| at).unwrap_or(0);
    let waiting = r.waiting.as_ref().map(|w| w.since).unwrap_or(0);
    format!("{k:?}/{report}/{waiting}")
}

fn wait(argv: &[String]) -> Result<i32> {
    let mut timeout = 540u64;
    let mut lead_arg = None;
    let mut it = argv.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--timeout" => {
                timeout = need(it.next(), TEAM_USAGE)?
                    .parse()
                    .map_err(|_| anyhow!("--timeout is a number of seconds"))?
            }
            s if s.starts_with('-') => bail!("Usage: {TEAM_USAGE}"),
            s => lead_arg = Some(s.to_string()),
        }
    }
    let mut out = String::new();
    let code = wait_team(lead_arg.as_deref(), timeout, &mut out, &|| false)?;
    print!("{out}");
    Ok(code)
}

fn tick_ms() -> u64 {
    std::env::var("AMUX_TEAM_TICK_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1000)
}

/// `amux team wait`, and the MCP `wait` tool: blocks until a worker of the lead finishes
/// its turn, needs an answer or ends, and writes what it has to say to `out`. Returns 0,
/// 3 (no workers), or 124 when `timeout` seconds pass first. `stop` ends the wait early
/// (a cancelled tool call), as a timeout.
pub fn wait_team(
    lead_arg: Option<&str>,
    timeout: u64,
    out: &mut String,
    stop: &dyn Fn() -> bool,
) -> Result<i32> {
    let lead = which_lead(lead_arg)?;
    let file = seen_file(&lead.id);
    let mut seen: BTreeMap<String, String> = crate::store::read_json(&file).unwrap_or_default();
    let tick = tick_ms();
    let deadline = time::now_ms() + timeout as i64 * 1000;
    loop {
        let now = time::now_ms();
        let all = workers(&lead.id);
        if all.is_empty() {
            let _ = writeln!(
                out,
                "{} has no workers yet. Start one with `amux spawn --worktree …`.",
                lead.id
            );
            return Ok(3);
        }
        let mut news = vec![];
        for r in &all {
            if let Some(k) = settled(r, now) {
                let fp = fingerprint(r, k);
                if seen.get(&r.id) != Some(&fp) {
                    news.push((r.clone(), k));
                    seen.insert(r.id.clone(), fp);
                }
            }
        }
        if !news.is_empty() {
            let _ = crate::store::write_json(&file, &seen);
            for (r, k) in &news {
                tell(out, r, *k);
            }
            let busy: Vec<&str> = all
                .iter()
                .filter(|r| settled(r, now).is_none())
                .map(|r| r.id.as_str())
                .collect();
            if busy.is_empty() {
                let _ = writeln!(out, "No other worker is busy.");
            } else {
                let _ = writeln!(out, "Still working: {}", busy.join(", "));
            }
            return Ok(0);
        }
        if all.iter().all(|r| !r.state().is_live()) {
            let _ = writeln!(
                out,
                "Every worker has ended, and you've seen them all. `amux team` lists them."
            );
            return Ok(0);
        }
        if now >= deadline || stop() {
            let busy: Vec<&str> = all.iter().map(|r| r.id.as_str()).collect();
            let _ = writeln!(
                out,
                "Still working: {}. Run `amux team wait` again.",
                busy.join(", ")
            );
            return Ok(124);
        }
        std::thread::sleep(std::time::Duration::from_millis(tick));
    }
}

/// For the MCP `wait` tool on runs that aren't a team's workers: blocks until every one
/// of `ids` has finished its turn or has ended, or until one needs an answer (so it isn't
/// left blocked behind the others), then writes what each settled run has to say. 124
/// when `timeout` seconds pass first.
pub fn wait_runs(
    ids: &[String],
    timeout: u64,
    out: &mut String,
    stop: &dyn Fn() -> bool,
) -> Result<i32> {
    let ids: Vec<String> = ids
        .iter()
        .map(|i| runs::find(i).map(|r| r.id))
        .collect::<Result<_>>()?;
    let tick = tick_ms();
    let deadline = time::now_ms() + timeout as i64 * 1000;
    loop {
        let now = time::now_ms();
        let all: Vec<(Run, Option<Kind>)> = ids
            .iter()
            .map(|i| {
                runs::load(i).map(|r| {
                    let k = settled(&r, now);
                    (r, k)
                })
            })
            .collect::<Result<_>>()?;
        let busy: Vec<&str> = all
            .iter()
            .filter(|(_, k)| k.is_none())
            .map(|(r, _)| r.id.as_str())
            .collect();
        let asking = all.iter().any(|(_, k)| matches!(k, Some(Kind::Attention)));
        if busy.is_empty() || asking {
            for (r, k) in &all {
                if let Some(k) = k {
                    tell(out, r, *k);
                }
            }
            if !busy.is_empty() {
                let _ = writeln!(out, "Still working: {}", busy.join(", "));
            }
            return Ok(0);
        }
        if now >= deadline || stop() {
            let _ = writeln!(out, "Still working: {}. Wait again.", busy.join(", "));
            return Ok(124);
        }
        std::thread::sleep(std::time::Duration::from_millis(tick));
    }
}

/// Tells the lead what a worker has to say: its report, or what it's waiting for and
/// the bottom of its screen.
fn tell(out: &mut String, r: &Run, k: Kind) {
    let m = member(r);
    let a = &m.attempt;
    let what = match k {
        Kind::Attention => "needs you".to_string(),
        Kind::Idle | Kind::Done if r.state().is_live() => "finished its turn".to_string(),
        _ => format!("ended ({})", a.state),
    };
    let _ = writeln!(
        out,
        "== {} {what} · {} file(s), +{} −{}, {} commit(s){}",
        r.id,
        a.files,
        a.added,
        a.removed,
        a.commits,
        m.verify
            .as_ref()
            .map(|v| format!(", {v}"))
            .unwrap_or_default()
    );
    if let Some(w) = &m.waiting {
        let _ = writeln!(out, "Waiting for: {w}");
    }
    match &m.report {
        Some(rep) => {
            let _ = writeln!(out, "Report:\n{rep}");
        }
        None => {
            // No report: the end of its screen says what it was doing.
            let p = crate::runs::screen::capture(r);
            let lines = crate::runs::screen::text(&p);
            let lines: Vec<&String> = lines.iter().filter(|l| !l.trim().is_empty()).collect();
            let tail = &lines[lines.len().saturating_sub(12)..];
            let _ = writeln!(out, "No report yet. The end of its screen:");
            for l in tail {
                let _ = writeln!(out, "  {}", l.trim_end());
            }
        }
    }
    let _ = writeln!(
        out,
        "Next: amux diff {id} · amux verify {id} --test · amux land {id} · amux send {id} \"…\"\n",
        id = r.id
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_lead_is_told_how_to_run_a_team() {
        let p = lead_preamble(
            "auth",
            3,
            &["claude@work".into(), "codex@me".into()],
            &["--permission-mode".into(), "acceptEdits".into()],
        );
        assert!(p.contains("amux spawn --worktree --name auth-<part> --prompt"));
        assert!(
            p.contains("how to know it's done>\" claude@work '--permission-mode' 'acceptEdits'")
        );
        assert!(p.contains("at most 3 at a time; accounts: claude@work, codex@me"));
        assert!(p.contains("amux team wait") && p.ends_with("Goal:\n"));
        assert!(worker_postscript("auth").contains("amux report"));
    }
}
