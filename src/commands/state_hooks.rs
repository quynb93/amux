//! `amux state-hooks install | remove | status`: hooks in Claude Code's and Gemini CLI's
//! settings that tell a run's host what the CLI is doing (runs/state_hooks.rs). Not to be
//! confused with `~/.amux/hooks/on-<event>`, your own scripts that amux runs.

use crate::paths::tildify;
use crate::providers::ProviderSpec;
use crate::runs::state_hooks::{self as sh, Cli};
use crate::store::load_providers;
use anyhow::{bail, Result};

const USAGE: &str = "amux state-hooks [status] | install [claude|gemini] | remove [claude|gemini]";

/// The command a hook entry runs: `amux` when that is this amux on PATH, else this
/// binary's full path.
fn program() -> String {
    let me = std::env::current_exe().ok();
    let on_path = super::maintenance::which("amux");
    let same = match (&me, &on_path) {
        (Some(a), Some(b)) => a.canonicalize().ok() == b.canonicalize().ok(),
        _ => false,
    };
    match me {
        Some(p) if !same => sh::sh_quote(&p.to_string_lossy()),
        _ => "amux".into(),
    }
}

/// The CLIs to act on, with their provider spec (None: not configured here).
fn targets(only: Option<&str>) -> Result<Vec<(&'static Cli, Option<ProviderSpec>)>> {
    if let Some(o) = only {
        if sh::cli(o).is_none() {
            bail!("State hooks are for claude and gemini, not \"{o}\". Usage: {USAGE}");
        }
    }
    let providers = load_providers()?;
    Ok(sh::CLIS
        .iter()
        .filter(|c| only.is_none_or(|o| o == c.id))
        .map(|c| (c, providers.iter().find(|p| p.id == c.id).cloned()))
        .collect())
}

/// One line per CLI: whether its state hooks are in place. Also used by `amux doctor`.
pub fn status_lines() -> Vec<(bool, String)> {
    let Ok(targets) = targets(None) else {
        return vec![];
    };
    let mut out = vec![];
    for (cli, spec) in targets {
        let Some(spec) = spec else {
            continue;
        };
        let path = sh::config_path(&spec, cli);
        if !spec.shared_home.exists() {
            out.push((
                true,
                format!("{}: no {} yet", cli.id, tildify(&spec.shared_home)),
            ));
            continue;
        }
        let line = match sh::read_settings(&path) {
            Err(e) => (false, format!("{}: {e:#}", cli.id)),
            Ok(v) => {
                let have = sh::installed_events(&v, cli);
                let all = cli.events.len();
                let at = tildify(&path);
                match have.len() {
                    0 => (false, format!("{}: not installed ({at})", cli.id)),
                    n if n == all => (true, format!("{}: installed in {at}", cli.id)),
                    _ => {
                        let missing: Vec<&str> = cli
                            .events
                            .iter()
                            .map(|(e, _)| *e)
                            .filter(|e| !have.contains(e))
                            .collect();
                        (
                            false,
                            format!(
                                "{}: partly installed in {at}, missing {}",
                                cli.id,
                                missing.join(", ")
                            ),
                        )
                    }
                }
            }
        };
        out.push(line);
    }
    out
}

pub fn state_hooks_cmd(args: &[String]) -> Result<i32> {
    let sub = args.first().map(String::as_str).unwrap_or("status");
    let only = args.get(1).map(String::as_str);
    match sub {
        "status" => {
            for (_, line) in status_lines() {
                println!("{line}");
            }
            println!("codex: not needed, Codex reports its own state (OSC 7501)");
            Ok(0)
        }
        "install" => {
            let prog = program();
            let mut failed = 0;
            for (cli, spec) in targets(only)? {
                let Some(spec) = spec else {
                    println!("{}: skipped, no such provider in this amux", cli.id);
                    continue;
                };
                if !spec.shared_home.exists() {
                    println!(
                        "{}: skipped, no {} (install {} and run it once first)",
                        cli.id,
                        tildify(&spec.shared_home),
                        spec.label
                    );
                    continue;
                }
                let path = sh::config_path(&spec, cli);
                let r = sh::read_settings(&path).and_then(|mut v| {
                    sh::add(&mut v, cli, &prog)?;
                    sh::write_settings(&path, &v)
                });
                match r {
                    Ok(()) => println!(
                        "{}: installed {} hooks in {}",
                        cli.id,
                        cli.events.len(),
                        tildify(&path)
                    ),
                    Err(e) => {
                        failed += 1;
                        eprintln!("{}: not installed: {e:#}", cli.id);
                    }
                }
            }
            println!(
                "Runs started from now on report their state through these hooks. \
                 Outside amux runs they do nothing. `amux state-hooks remove` takes them out."
            );
            Ok(if failed > 0 { 1 } else { 0 })
        }
        "remove" | "uninstall" => {
            let mut failed = 0;
            for (cli, spec) in targets(only)? {
                let Some(spec) = spec else {
                    continue;
                };
                let path = sh::config_path(&spec, cli);
                if !path.exists() {
                    println!("{}: nothing to remove", cli.id);
                    continue;
                }
                let r = sh::read_settings(&path).and_then(|mut v| {
                    let n = sh::strip(&mut v);
                    if n > 0 {
                        sh::write_settings(&path, &v)?;
                    }
                    Ok(n)
                });
                match r {
                    Ok(0) => println!("{}: nothing to remove", cli.id),
                    Ok(n) => println!("{}: removed {n} hooks from {}", cli.id, tildify(&path)),
                    Err(e) => {
                        failed += 1;
                        eprintln!("{}: {e:#}", cli.id);
                    }
                }
            }
            Ok(if failed > 0 { 1 } else { 0 })
        }
        _ => bail!("Usage: {USAGE}"),
    }
}
