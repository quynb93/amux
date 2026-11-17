//! `amux usage`: which accounts have hit their usage limit, until when, and how many runs
//! each has going. See work/limits.rs.

use crate::store::load_accounts;
use crate::util::table;
use crate::work::limits;
use crate::{runs, time};
use anyhow::{bail, Result};

const USAGE: &str = "amux usage [--json] | amux usage --clear <provider>@<name>";

/// "in 2h 5m", "in 40m".
fn until(ms: i64, now: i64) -> String {
    let m = ((ms - now).max(0) + 59_999) / 60_000;
    if m >= 60 {
        format!("in {}h {}m", m / 60, m % 60)
    } else {
        format!("in {m}m")
    }
}

pub fn usage_cmd(argv: &[String]) -> Result<i32> {
    if argv.first().map(String::as_str) == Some("--clear") {
        let Some(a) = argv.get(1) else {
            bail!("Usage: {USAGE}");
        };
        if limits::clear(a) {
            println!("{a} is no longer marked limited.");
        } else {
            println!("{a} wasn't marked limited.");
        }
        return Ok(0);
    }
    let json = argv.iter().any(|a| a == "--json");
    if let Some(a) = argv.iter().find(|a| *a != "--json") {
        bail!("Unknown argument {a}. Usage: {USAGE}");
    }
    let now = time::now_ms();
    let active = limits::active();
    let all_runs = runs::list();
    let accounts = load_accounts()?;
    if json {
        let out: Vec<serde_json::Value> = accounts
            .iter()
            .map(|a| {
                let live = all_runs
                    .iter()
                    .filter(|r| r.account == a.id() && r.state().is_live())
                    .count();
                serde_json::json!({ "account": a.id(), "liveRuns": live, "limit": active.get(&a.id()) })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(0);
    }
    if accounts.is_empty() {
        println!("No accounts yet. Add one with `amux account add <provider> <name>`.");
        return Ok(0);
    }
    let rows: Vec<Vec<String>> = accounts
        .iter()
        .map(|a| {
            let live = all_runs
                .iter()
                .filter(|r| r.account == a.id() && r.state().is_live())
                .count();
            let (state, why) = match active.get(&a.id()) {
                Some(l) => (
                    format!("limited, resets {}", until(l.resets_at, now)),
                    l.reason.clone(),
                ),
                None => ("ok".into(), String::new()),
            };
            vec![a.id(), state, live.to_string(), why]
        })
        .collect();
    println!(
        "{}",
        table(&["ACCOUNT", "LIMIT", "RUNS", "LAST MESSAGE"], &rows)
    );
    println!(
        "\nWhen a run hits its limit: {}. Set \"limits\": {{ \"onLimit\": \"failover\" }} in ~/.amux/config.json to move it to another account at once, or run `amux failover <run>`.",
        if limits::on_limit() == "failover" {
            "it moves to another account of the same CLI"
        } else {
            "it waits for the reset (the CLI's own behavior)"
        }
    );
    Ok(0)
}
