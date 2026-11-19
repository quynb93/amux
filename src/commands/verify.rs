//! `amux verify <run>`: check a run's work by running its tests here (`--test`) or
//! pushing its branch for CI (`--push`). With neither, says which is better right now.

use crate::paths::tildify;
use crate::runs;
use crate::util::need;
use crate::work::slots::EX_TEMPFAIL;
use crate::work::verify::{self, Plan, TestOutcome};
use anyhow::{anyhow, bail, Result};

const USAGE: &str =
    "amux verify <run> [--test [--command <cmd>] | --host-test | --push | --send-failures]";

pub fn verify_cmd(argv: &[String]) -> Result<i32> {
    let mut action = None;
    let mut command = None;
    let mut host = false;
    let mut id = None;
    let mut it = argv.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--test" | "-t" => action = Some("test"),
            "--push" | "-p" => action = Some("push"),
            "--send-failures" | "-f" => action = Some("send"),
            "--host-test" => {
                action = Some("test");
                host = true;
            }
            "--command" | "-c" => {
                command = Some(
                    it.next()
                        .cloned()
                        .ok_or_else(|| anyhow!("--command needs a value. Usage: {USAGE}"))?,
                )
            }
            f if f.starts_with('-') => bail!("Unknown option {f}. Usage: {USAGE}"),
            _ if id.is_none() => id = Some(a.clone()),
            _ => bail!("Usage: {USAGE}"),
        }
    }
    let run = runs::find(need(id.as_ref(), USAGE)?)?;
    let plan = Plan::for_run(&run);
    match action {
        None => {
            println!("Verify {}:", run.id);
            for l in plan.lines() {
                println!("  {l}");
            }
            let (how, flag) = if plan.recommend_push() {
                ("push the branch and let CI run the tests", "--push")
            } else {
                ("run the tests here", "--test")
            };
            println!("\nRecommended now: {how}: amux verify {} {flag}", run.id);
            Ok(0)
        }
        Some("push") => {
            println!("{}", verify::push(&run, &plan)?);
            Ok(0)
        }
        Some("send") => {
            verify::send_failures(&run)?;
            println!("Sent the failing output to {}.", run.id);
            Ok(0)
        }
        Some(_) => match verify::run_tests(&run, command.or(plan.test.clone()), host)? {
            TestOutcome::Passed { cmd, log } => {
                println!(
                    "Tests passed for {} ({cmd}). Output: {}",
                    run.id,
                    tildify(&log)
                );
                Ok(0)
            }
            TestOutcome::Failed { cmd, code, log } => {
                println!(
                    "Tests failed for {} ({cmd} exited {code}). Output: {}",
                    run.id,
                    tildify(&log)
                );
                Ok(code)
            }
            TestOutcome::NoSlot => Ok(EX_TEMPFAIL),
        },
    }
}
