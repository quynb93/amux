//! `amux allow`: the rules for answering permission prompts by themselves (work/approve.rs).

use crate::runs;
use crate::time;
use crate::util::{ago, one_line, table};
use crate::work::approve::{self, Ask, Rules, Verdict};
use anyhow::{anyhow, bail, Result};

const USAGE: &str = "amux allow [<rule>... | --deny <rule>... | --rm <rule>... | --starter | --clear | --check \"<kind>: <what>\" | --log [<run>]]";

fn show(r: &Rules) {
    if r.allow.is_empty() && r.deny.is_empty() {
        println!("No rules: every permission prompt waits for you.");
        println!("  amux allow --starter          a safe start: read-only git, builds, tests, edits in the run's folder");
        println!("  amux allow \"bash: make lint*\"  allow a command (* is any text)");
        return;
    }
    if r.allow.is_empty() {
        println!("Nothing is allowed, so every prompt waits for you.");
    }
    for a in &r.allow {
        println!("  allow  {a}");
    }
    for d in &r.deny {
        println!("  deny   {d}");
    }
    println!(
        "Claude Code runs answer these prompts by themselves; the rest wait for you. `amux allow --log` shows what was answered."
    );
}

fn check_rules(rules: &[String]) -> Result<()> {
    if rules.is_empty() {
        bail!("Which rule? Usage: {USAGE}");
    }
    for r in rules {
        approve::parse_rule(r)?;
    }
    Ok(())
}

pub fn allow_cmd(argv: &[String]) -> Result<i32> {
    let mut r = approve::load();
    let rest: Vec<String> = argv.iter().skip(1).cloned().collect();
    match argv.first().map(String::as_str) {
        None | Some("--list") => {
            show(&r);
            return Ok(0);
        }
        Some("--deny") => {
            check_rules(&rest)?;
            for d in rest {
                if !r.deny.contains(&d) {
                    r.deny.push(d);
                }
            }
        }
        Some("--rm") => {
            check_rules(&rest)?;
            let before = r.allow.len() + r.deny.len();
            r.allow.retain(|a| !rest.contains(a));
            r.deny.retain(|d| !rest.contains(d));
            if r.allow.len() + r.deny.len() == before {
                bail!("No such rule. `amux allow` lists them, exactly as written.");
            }
        }
        Some("--starter") => {
            let s = approve::starter();
            for a in s.allow {
                if !r.allow.contains(&a) {
                    r.allow.push(a);
                }
            }
            for d in s.deny {
                if !r.deny.contains(&d) {
                    r.deny.push(d);
                }
            }
        }
        Some("--clear") => r = Rules::default(),
        Some("--check") => return check(&r, &rest),
        Some("--log") => return log(rest.first()),
        Some(f) if f.starts_with('-') => bail!("Unknown option {f}. Usage: {USAGE}"),
        Some(_) => {
            check_rules(argv)?;
            for a in argv {
                if !r.allow.contains(a) {
                    r.allow.push(a.clone());
                }
            }
            if argv.iter().any(|a| a.trim().eq_ignore_ascii_case("bash")) {
                eprintln!(
                    "amux: \"bash\" alone allows every shell command; deny rules still apply."
                );
            }
        }
    }
    approve::save(&r)?;
    show(&r);
    Ok(0)
}

/// `--check "bash: cargo test"`: what amux would do with such a prompt, here.
fn check(r: &Rules, rest: &[String]) -> Result<i32> {
    let q = rest
        .first()
        .ok_or_else(|| anyhow!("Usage: amux allow --check \"bash: cargo test\""))?;
    let (kind, subject) = approve::parse_rule(q)?;
    let ask = Ask {
        kind,
        subject,
        yes: "1",
        doubt: None,
    };
    let cwd = std::env::current_dir()?;
    match approve::judge(r, &ask, &cwd) {
        Verdict::Allow(by) => {
            println!("approve: {} (by {})", ask.label(), by.join(", "));
            Ok(0)
        }
        Verdict::Deny(rule) => {
            println!("wait for you: {} (denied by {rule})", ask.label());
            Ok(1)
        }
        Verdict::Ask(why) => {
            println!("wait for you: {} ({why})", ask.label());
            Ok(1)
        }
    }
}

/// What was judged, newest last: one run's, or every run's.
fn log(run: Option<&String>) -> Result<i32> {
    let ids: Vec<String> = match run {
        Some(r) => vec![runs::find(r)?.id],
        None => runs::list().into_iter().map(|r| r.id).collect(),
    };
    let mut rows: Vec<(i64, Vec<String>)> = vec![];
    for id in ids {
        let Ok(t) = std::fs::read_to_string(runs::run_dir(&id).join("approvals.jsonl")) else {
            continue;
        };
        for l in t.lines() {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(l) else {
                continue;
            };
            let at = v["at"].as_i64().unwrap_or(0);
            let s = |k: &str| v[k].as_str().unwrap_or("").to_string();
            let what = if s("subject").is_empty() {
                s("kind")
            } else {
                format!("{}: {}", s("kind"), s("subject"))
            };
            rows.push((
                at,
                vec![
                    id.clone(),
                    s("action"),
                    one_line(&what, 60),
                    one_line(&s("why"), 50),
                    at.to_string(),
                ],
            ));
        }
    }
    if rows.is_empty() {
        println!("No prompts judged yet.");
        return Ok(0);
    }
    rows.sort_by_key(|(at, _)| *at);
    let now = time::now_ms();
    let rows: Vec<Vec<String>> = rows
        .into_iter()
        .rev()
        .take(50)
        .rev()
        .map(|(at, mut r)| {
            r[4] = ago(at, now);
            r
        })
        .collect();
    println!(
        "{}",
        table(&["RUN", "ACTION", "PROMPT", "WHY", "WHEN"], &rows).trim_end()
    );
    Ok(0)
}
