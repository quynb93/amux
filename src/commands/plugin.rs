//! `amux plugin list | new <name> | run <name> [--run <run>] [args]`.

use crate::paths::tildify;
use crate::plugins;
use anyhow::{bail, Result};

const USAGE: &str = "amux plugin list | new <name> | run <name> [--run <run>] [args...]";

pub fn plugin_cmd(argv: &[String]) -> Result<i32> {
    match argv.first().map(String::as_str) {
        None | Some("list" | "ls") => {
            let all = plugins::list();
            if all.is_empty() {
                println!(
                    "No plugins. Make one with `amux plugin new <name>`, or put an `amux-<name>` executable on PATH."
                );
            }
            for p in &all {
                println!("{:<16} {}  ({})", p.name, p.about, tildify(&p.path));
            }
            println!(
                "\nHooks: executables in {} named on-waiting, on-exited, on-verify or on-checks.",
                tildify(&plugins::hooks_dir())
            );
            Ok(0)
        }
        Some("new") => {
            let Some(name) = argv.get(1) else {
                bail!("Usage: {USAGE}");
            };
            let path = plugins::create(name)?;
            println!(
                "Created {}. Edit it, then run `amux {name}`.",
                tildify(&path)
            );
            Ok(0)
        }
        Some("run") => {
            let Some(name) = argv.get(1) else {
                bail!("Usage: {USAGE}");
            };
            let mut rest = &argv[2..];
            let mut run = None;
            if rest.first().map(String::as_str) == Some("--run") {
                run = rest.get(1).cloned();
                rest = rest.get(2..).unwrap_or_default();
            }
            let Some(p) = plugins::find(name) else {
                bail!("No plugin \"{name}\". `amux plugin list` shows them.");
            };
            plugins::run(&p, rest, run.as_deref())
        }
        Some(a) => bail!("Unknown argument {a}. Usage: {USAGE}"),
    }
}
