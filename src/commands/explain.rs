//! `amux explain <run> [--json]`: why a run shows the state it does, in plain words, with
//! the evidence: which signal decided (the CLI's own report, a prompt on screen, a bell,
//! a notification, or output timing), what amux watches for in this run, and the bottom of
//! its screen.

use crate::runs::state::{classify, last_output, short_age, IDLE_AFTER_MS};
use crate::runs::{self, Run, State};
use crate::time;
use crate::util::need;
use anyhow::Result;
use serde_json::json;

const USAGE: &str = "amux explain <run> [--json]";
/// Screen lines shown as evidence.
const SCREEN_LINES: usize = 10;

pub fn explain_cmd(argv: &[String]) -> Result<i32> {
    let json = argv.iter().any(|a| a == "--json");
    let id = need(argv.iter().find(|a| !a.starts_with('-')), USAGE)?;
    let run = runs::find(id)?;
    let state = run.state();
    let now = time::now_ms();
    let out = last_output(&run);
    let e = explain(&run, &state, out, now);
    let screen = screen_tail(&run);
    if json {
        let v = json!({
            "run": run.id,
            "state": e.label,
            "why": e.why,
            "source": e.source,
            "reports": e.reports,
            "status": run.status,
            "waiting": run.waiting,
            "lastOutput": out,
            "waitingPatterns": run.waiting_patterns,
            "how": e.how,
            "screen": screen,
        });
        println!("{}", serde_json::to_string_pretty(&v)?);
        return Ok(0);
    }
    println!("{}: {}\n", run.id, e.label);
    println!("Why");
    for l in &e.why {
        println!("  {l}");
    }
    println!("\nHow amux reads this run");
    for l in &e.how {
        println!("  {l}");
    }
    if !screen.is_empty() {
        println!("\nBottom of its screen");
        for l in &screen {
            println!("  │ {l}");
        }
    }
    Ok(0)
}

pub struct Explanation {
    /// The state as `amux ui` shows it: "waiting 2m", "idle 5s", "stopped"…
    pub label: String,
    /// What decided it: report, pattern, bell, notification, output, exit, host.
    pub source: &'static str,
    /// Whether the CLI reports its own state (OSC 7501).
    pub reports: bool,
    pub why: Vec<String>,
    pub how: Vec<String>,
}

fn quote_list(v: &[String]) -> String {
    if v.is_empty() {
        return "none set".into();
    }
    v.iter()
        .map(|p| format!("“{p}”"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The explanation, from what the run's files say. Pure, so it can be tested.
pub fn explain(run: &Run, state: &State, out: Option<i64>, now: i64) -> Explanation {
    let (_, label) = classify(state, run.waiting.as_ref(), run.status.as_ref(), out, now);
    let reports = run.status.is_some()
        || run
            .waiting
            .as_ref()
            .is_some_and(|w| w.source.as_deref() == Some("report"));
    let age = |t: i64| short_age(now - t);
    let mut why = vec![];
    let mut source = "output";
    match state {
        State::Queued => {
            source = "slots";
            why.push(format!(
                "It hasn't started yet: {}.",
                run.queued.as_ref().map_or("it waits for a run slot", |q| q.reason.as_str())
            ));
            why.push(
                "It starts on its own once a run slot is free and the machine has room. `amux slots` shows who holds them; the limits are under \"limits\" in ~/.amux/config.json.".into(),
            );
        }
        State::Starting => {
            source = "host";
            why.push("Its host is starting the CLI.".into());
        }
        State::Failed => {
            source = "host";
            why.push(format!(
                "The CLI could not start: {}",
                run.error.as_deref().unwrap_or("unknown error")
            ));
        }
        State::Lost => {
            source = "host";
            why.push(format!(
                "Its host process{} is gone without recording an exit: the run was killed, or the computer restarted.",
                run.host_pid.map(|p| format!(" (pid {p})")).unwrap_or_default()
            ));
            why.push(format!("`amux logs {}` shows what it printed.", run.id));
        }
        State::Exited(code) => {
            source = "exit";
            why.push(match code {
                0 => "The CLI exited normally (code 0).".to_string(),
                130 => "The CLI was interrupted with Ctrl-C (code 130).".into(),
                137 => "The CLI was killed (SIGKILL, code 137), for example by `amux stop --force`.".into(),
                143 => "The CLI was stopped (SIGTERM, code 143), for example by `amux stop`.".into(),
                c => format!(
                    "The CLI exited with code {c}, which usually means an error. `amux logs {}` shows what it printed.",
                    run.id
                ),
            });
            if let Some(s) = &run.status {
                why.push(format!("Its last report before that: {}.", s.state));
            }
        }
        State::Running => match (&run.waiting, &run.status) {
            (Some(w), _) => {
                source = match w.source.as_deref() {
                    Some("report") => "report",
                    Some("bell") => "bell",
                    Some("notification") => "notification",
                    Some("limit") => "limit",
                    Some("stall") => "stall",
                    Some(_) => "pattern",
                    None => "unknown",
                };
                why.push(match source {
                    "report" => {
                        let kind = run
                            .status
                            .as_ref()
                            .and_then(|s| s.kind.as_deref())
                            .map(|k| format!(" ({k})"))
                            .unwrap_or_default();
                        format!("The CLI said so itself (OSC 7501): it is blocked{kind}: “{}”.", w.reason)
                    }
                    "bell" => "The CLI rang the terminal bell, which CLIs do when they need you.".into(),
                    "notification" => format!("The CLI sent a notification: “{}”.", w.reason),
                    "stall" => format!(
                        "amux's watchdog flagged it: {}. It clears when its files change. Tune it with --stall/--max-time on spawn, or \"limits\": {{ \"stall\", \"maxTime\", \"onStall\" }} in config.json.",
                        w.reason
                    ),
                    "limit" => format!(
                        "{} hit its usage limit: “{}”. `amux failover {}` continues the session under another account; `amux usage` shows them.",
                        run.account, w.reason, run.id
                    ),
                    "pattern" => format!(
                        "Its screen shows “{}”, one of the prompts this provider is watched for (waitingPatterns).",
                        w.reason
                    ),
                    _ => format!(
                        "The host saw “{}”. (This run's host is older and doesn't record what showed it.)",
                        w.reason
                    ),
                });
                why.push(format!(
                    "For {} now. {}",
                    age(w.since),
                    if source == "report" {
                        "Its next report ends it."
                    } else {
                        "Typing into the run ends it."
                    }
                ));
            }
            (None, Some(s)) => {
                source = "report";
                let msg = s.message.as_deref().map(|m| format!(": “{m}”")).unwrap_or_default();
                why.push(match s.state.as_str() {
                    "working" => format!("The CLI reports it is working{msg}."),
                    "done" => "The CLI reports its turn is done; it waits for your next prompt.".into(),
                    "error" => format!("The CLI reports an error{msg}."),
                    _ => "The CLI reports it is idle, waiting for your next prompt.".into(),
                });
                why.push(format!("Reported {} ago (OSC 7501).", age(s.since)));
            }
            (None, None) => why.push(match out {
                Some(t) if now - t < IDLE_AFTER_MS => format!(
                    "It printed something {} ago; output in the last {} seconds counts as working.",
                    age(t),
                    IDLE_AFTER_MS / 1000
                ),
                Some(t) => format!(
                    "Nothing printed for {}, and nothing on screen matched a prompt, so it counts as idle.",
                    age(t)
                ),
                None => "It hasn't printed anything yet.".into(),
            }),
        },
    }
    let patterns = quote_list(&run.waiting_patterns);
    let mut how = vec![];
    if reports {
        how.push("This CLI reports its own state (OSC 7501), so its reports decide.".into());
        how.push(format!(
            "While it reports idle or done, prompts on screen still count: {patterns}."
        ));
        how.push("Bells and notification codes are ignored once it reports.".into());
    } else {
        how.push(
            "This CLI hasn't reported its own state (OSC 7501), so amux reads its output:".into(),
        );
        how.push(format!("  prompts (waitingPatterns): {patterns}"));
        how.push("  a terminal bell, or a notification code (OSC 9, 777 or 99)".into());
        how.push(format!(
            "  otherwise, output in the last {} seconds means working; quiet means idle.",
            IDLE_AFTER_MS / 1000
        ));
    }
    how.push(match out {
        Some(t) => format!("Last output: {} ago.", age(t)),
        None => "No output yet.".into(),
    });
    how.push(format!(
        "Processes: host {}, CLI {}.",
        run.host_pid
            .map_or("not started".into(), |p| format!("pid {p}")),
        run.pid.map_or("not started".into(), |p| format!("pid {p}")),
    ));
    Explanation {
        label,
        source,
        reports,
        why,
        how,
    }
}

/// The last non-empty lines of the run's screen, emulated from its log.
fn screen_tail(run: &Run) -> Vec<String> {
    let lines = crate::runs::screen::text(&crate::runs::screen::capture(run));
    lines[lines.len().saturating_sub(SCREEN_LINES)..].to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runs::{ProgramStatus, Waiting};

    const NOW: i64 = 1_000_000;

    fn run() -> Run {
        Run {
            id: "r".into(),
            waiting_patterns: vec!["Do you want to".into()],
            host_pid: Some(10),
            pid: Some(11),
            ..Default::default()
        }
    }

    fn waiting(source: Option<&str>) -> Waiting {
        Waiting {
            reason: "Do you want to".into(),
            since: NOW - 30_000,
            source: source.map(Into::into),
        }
    }

    #[test]
    fn explains_each_kind_of_wait() {
        let mut r = run();
        r.waiting = Some(waiting(Some("pattern")));
        let e = explain(&r, &State::Running, Some(NOW), NOW);
        assert_eq!(
            (e.label.as_str(), e.source, e.reports),
            ("waiting 30s", "pattern", false)
        );
        assert!(e.why[0].contains("waitingPatterns"), "{:?}", e.why);
        assert!(e.how.iter().any(|h| h.contains("“Do you want to”")));

        r.waiting = Some(waiting(Some("bell")));
        assert_eq!(explain(&r, &State::Running, None, NOW).source, "bell");

        r.waiting = Some(Waiting {
            reason: "approve Edit: a.rs".into(),
            source: Some("report".into()),
            ..waiting(None)
        });
        r.status = Some(ProgramStatus {
            state: "blocked".into(),
            kind: Some("permission".into()),
            message: Some("approve Edit: a.rs".into()),
            since: NOW - 30_000,
        });
        let e = explain(&r, &State::Running, None, NOW);
        assert_eq!((e.source, e.reports), ("report", true));
        assert!(
            e.why[0].contains("blocked (permission): “approve Edit: a.rs”"),
            "{:?}",
            e.why
        );
        assert!(e.why[1].contains("next report"), "{:?}", e.why);

        // From an older host: the reason, but no source.
        let mut old = run();
        old.waiting = Some(waiting(None));
        assert_eq!(explain(&old, &State::Running, None, NOW).source, "unknown");
    }

    #[test]
    fn explains_reports_output_timing_and_endings() {
        let mut r = run();
        r.status = Some(ProgramStatus {
            state: "working".into(),
            kind: None,
            message: Some("editing a.rs".into()),
            since: NOW - 2_000,
        });
        let e = explain(&r, &State::Running, Some(NOW - 60_000), NOW);
        assert_eq!(e.label, "working");
        assert!(e.why[0].contains("working: “editing a.rs”"), "{:?}", e.why);

        let r = run();
        let e = explain(&r, &State::Running, Some(NOW - 120_000), NOW);
        assert_eq!((e.label.as_str(), e.source), ("idle 2m", "output"));
        assert!(e.why[0].contains("Nothing printed for 2m"), "{:?}", e.why);
        assert!(e.how[0].contains("hasn't reported"), "{:?}", e.how);

        let e = explain(&r, &State::Exited(143), None, NOW);
        assert_eq!((e.label.as_str(), e.source), ("stopped", "exit"));
        assert!(e.why[0].contains("SIGTERM"));
        assert!(explain(&r, &State::Exited(2), None, NOW).why[0].contains("code 2"));
        assert!(explain(&r, &State::Lost, None, NOW).why[0].contains("pid 10"));
    }
}
