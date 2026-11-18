//! `amux spawn | ps | attach | logs | stop | rm`: parallel background runs.

use crate::git::git;
use crate::launch::{log_launch, prepare};
use crate::paths::{self, tildify};
use crate::runs::{self, attach::attach, Run, State, Worktree};
use crate::store::{resolve_account, valid_name};
use crate::util::{ago, need, one_line, table};
use crate::{fsx, time};
use anyhow::{anyhow, bail, Context, Result};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const SPAWN_USAGE: &str =
    "amux spawn [--on <machine>[:<dir>]] [--dir <folder>] [--name <run>] [--worktree] [--branch <name>] [--base <ref>] [--prompt <text>] [--issue <n>|linear:<KEY>|jira:<KEY>] [--autopilot[=merge]] [--attach] <provider>[@name] [args...]";

/// What an agent on autopilot is told after its prompt.
const AUTOPILOT_POSTSCRIPT: &str = "\n\nWhen you're done, commit your work on this branch, and don't push: amux pushes it, opens the pull request, and brings you CI failures and review comments to fix.";

#[derive(Default)]
struct SpawnOpts {
    name: Option<String>,
    /// Start in this folder instead of the current one.
    dir: Option<String>,
    /// Continue in this run's folder and worktree, instead of a new one (failover).
    adopt: Option<String>,
    /// One of several attempts at the same task (`amux try`).
    group: Option<String>,
    /// Starts a team lead that may have this many workers (`amux lead`).
    workers: Option<u32>,
    /// Flag the run after this long working without changing a file, or alive at all.
    stall: Option<u64>,
    max_time: Option<u64>,
    /// A first prompt, passed the way the provider takes one (its `promptArgs`).
    prompt: Option<String>,
    /// Permission prompts always wait for you (`--no-auto-approve`).
    no_auto: bool,
    /// A GitHub, Linear or Jira issue to work on: it becomes the first prompt (`--issue`).
    issue: Option<String>,
    /// Autopilot (`--autopilot`, `--autopilot=merge`): Some(true) merges when ready.
    autopilot: Option<bool>,
    worktree: bool,
    branch: Option<String>,
    base: Option<String>,
    attach: bool,
}

/// amux's own flags come before the account; everything after it goes to the CLI as is.
fn parse_spawn(argv: &[String]) -> Result<(SpawnOpts, &str, &[String])> {
    let mut o = SpawnOpts::default();
    let mut i = 0;
    while let Some(a) = argv.get(i) {
        if !a.starts_with('-') {
            break;
        }
        let (flag, inline) = match a.split_once('=') {
            Some((f, v)) => (f, Some(v.to_string())),
            None => (a.as_str(), None),
        };
        let mut value = || -> Result<String> {
            let v = match &inline {
                Some(v) => v.clone(),
                None => {
                    i += 1;
                    argv.get(i).cloned().unwrap_or_default()
                }
            };
            // Values end up as git arguments, so they must not read as options.
            if v.is_empty() || v.starts_with('-') {
                bail!("{flag} needs a value. Usage: {SPAWN_USAGE}");
            }
            Ok(v)
        };
        match flag {
            "--name" => o.name = Some(value()?),
            "--branch" | "-b" => o.branch = Some(value()?),
            "--base" => o.base = Some(value()?),
            "--worktree" | "-w" => o.worktree = true,
            "--attach" | "-a" => o.attach = true,
            "--no-auto-approve" => o.no_auto = true,
            "--autopilot" => {
                o.autopilot = Some(match inline.as_deref() {
                    None | Some("on") => false,
                    Some("merge") => true,
                    Some(v) => bail!("--autopilot takes nothing or =merge, not \"{v}\"."),
                })
            }
            "--dir" | "-C" => o.dir = Some(value()?),
            "--adopt" => o.adopt = Some(value()?),
            "--group" => o.group = Some(value()?),
            "--workers" => {
                o.workers = Some(
                    value()?
                        .parse()
                        .ok()
                        .filter(|n| (1..=32).contains(n))
                        .ok_or_else(|| anyhow!("--workers is a number from 1 to 32"))?,
                )
            }
            "--stall" | "--max-time" => {
                let v = value()?;
                let ms = crate::work::guards::parse_duration(&v).ok_or_else(|| {
                    anyhow!("{flag} takes a time such as 30m or 2h, not \"{v}\".")
                })?;
                if flag == "--stall" {
                    o.stall = Some(ms);
                } else {
                    o.max_time = Some(ms);
                }
            }
            "--issue" => o.issue = Some(value()?),
            "--prompt" => {
                // A prompt may start with anything but "-", like any value here.
                o.prompt = Some(value()?)
            }
            "--" => {
                i += 1;
                break;
            }
            _ => bail!("Unknown option {a}. Usage: {SPAWN_USAGE}"),
        }
        i += 1;
    }
    // An issue gets its own worktree and branch, ready for a pull request.
    // So does a run on autopilot.
    o.worktree |=
        o.branch.is_some() || o.base.is_some() || o.issue.is_some() || o.autopilot.is_some();
    let acct = need(argv.get(i), SPAWN_USAGE)?;
    Ok((o, acct, argv.get(i + 1..).unwrap_or_default()))
}

/// Adds a worktree on a new branch for the run. Returns it and the directory to start in:
/// the same subdirectory of the new checkout that `cwd` is in the current one.
fn add_worktree(cwd: &Path, id: &str, o: &SpawnOpts) -> Result<(Worktree, PathBuf)> {
    let top = git(cwd, &["rev-parse", "--show-toplevel"])
        .map(PathBuf::from)
        .map_err(|_| {
            anyhow!(
                "--worktree needs a git repository, and {} is not in one.",
                tildify(cwd)
            )
        })?;
    let sub = cwd
        .canonicalize()
        .ok()
        .and_then(|c| Some(c.strip_prefix(top.canonicalize().ok()?).ok()?.to_path_buf()))
        .unwrap_or_default();
    let repo_name = top
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "repo".into());
    let path = paths::worktrees_dir().join(format!("{repo_name}-{id}"));
    let branch = o.branch.clone().unwrap_or_else(|| format!("amux/{id}"));
    fsx::mkdir_private(&paths::worktrees_dir())?;
    let path_s = path.to_string_lossy().into_owned();
    git(
        &top,
        &[
            "worktree",
            "add",
            "-b",
            &branch,
            &path_s,
            o.base.as_deref().unwrap_or("HEAD"),
        ],
    )
    .map_err(|e| anyhow!("git worktree add failed: {e}"))?;
    let start = path.join(&sub);
    let start = if start.is_dir() { start } else { path.clone() };
    let base = git(&path, &["rev-parse", "HEAD"]).ok();
    Ok((
        Worktree {
            repo: top.to_string_lossy().into_owned(),
            path: path_s,
            branch,
            base,
        },
        start,
    ))
}

/// Removes a run's worktree when git agrees it holds no uncommitted work. The branch
/// is kept unless `drop_branch` (used only for a worktree spawn just created). Returns a
/// note for the user when something was kept.
fn remove_worktree(wt: &Worktree, drop_branch: bool) -> Option<String> {
    let repo = Path::new(&wt.repo);
    if Path::new(&wt.path).exists() {
        if let Err(e) = git(repo, &["worktree", "remove", &wt.path]) {
            // Usually uncommitted or untracked files: never delete work.
            let why = if e.to_string().contains("modified or untracked") {
                "it has uncommitted changes".to_string()
            } else {
                e.to_string()
            };
            return Some(format!(
                "kept worktree {} because {why}, and branch {}",
                tildify(Path::new(&wt.path)),
                wt.branch
            ));
        }
    } else {
        let _ = git(repo, &["worktree", "prune"]);
    }
    if drop_branch {
        let _ = git(repo, &["branch", "-d", &wt.branch]);
        None
    } else {
        Some(format!("kept branch {}", wt.branch))
    }
}

pub fn spawn_cmd(argv: &[String]) -> Result<i32> {
    // `--on <machine>[:<dir>]`: the same spawn, on that machine, in that folder.
    if let Some(i) = argv.iter().position(|a| a == "--on") {
        let on = need(argv.get(i + 1), SPAWN_USAGE)?;
        let (name, dir) = match on.split_once(':') {
            Some((n, d)) => (n, Some(d)),
            None => (on, None),
        };
        let m = crate::remote::find(name)
            .ok_or_else(|| anyhow!("No machine \"{name}\". `amux machine list` shows them."))?;
        let mut rest: Vec<String> = vec!["spawn".into()];
        rest.extend(argv[..i].iter().cloned());
        rest.extend(argv[i + 2..].iter().cloned());
        return m.run(&rest, dir);
    }
    let (mut o, acct_ref, args) = parse_spawn(argv)?;
    // An easy mistake, for people and agents alike: amux's options after the account go
    // to the CLI instead.
    if let Some(f) = args.iter().find(|a| {
        [
            "--prompt",
            "--worktree",
            "--name",
            "--branch",
            "--base",
            "--dir",
            "--issue",
            "--autopilot",
        ]
        .contains(&a.as_str())
    }) {
        eprintln!(
            "amux: {f} comes after the account, so it goes to the CLI, not to amux. \
             Put amux's options first: amux spawn {f} … {acct_ref}"
        );
    }
    if let Some(n) = &o.name {
        if !valid_name(n) {
            bail!("Run names use letters, digits, '.', '_' and '-'.");
        }
    }
    // No account named: one that hasn't hit its usage limit and, with `limits.switchAt`,
    // the one with the most headroom.
    let acct = if acct_ref.contains('@') {
        resolve_account(acct_ref)?
    } else {
        let default = resolve_account(acct_ref);
        match crate::work::limits::pick_for_spawn(acct_ref, default.as_ref().ok()) {
            Some((other, why)) => {
                eprintln!("amux: {why}");
                other
            }
            None => default?,
        }
    };
    let (spec, mut env) = prepare(&acct)?;
    let cwd = match &o.dir {
        Some(d) => {
            let p = paths::expand_home(d);
            if !p.is_dir() {
                bail!("No folder {}.", tildify(&p));
            }
            p
        }
        None => std::env::current_dir()?,
    };
    // Started from inside a run: it's that run's worker, within its team's limits.
    let lead = match o.adopt {
        Some(_) => None,
        None => crate::commands::team::lead_for_spawn()?,
    };
    let issue = match &o.issue {
        Some(r) => Some(crate::work::issues::view(&cwd, r)?),
        None => None,
    };
    let first = match &issue {
        Some(i) => {
            o.name.get_or_insert_with(|| i.run_name());
            Some(crate::work::issues::prompt(i, o.prompt.as_deref()))
        }
        None => o.prompt.clone(),
    };
    let mut args = args.to_vec();
    if let Some(p) = first.as_deref().map(str::trim).filter(|p| !p.is_empty()) {
        // The repo's shared decisions (`amux note`) come first; a worker is told how to
        // report back at the end.
        let mut p = format!("{}{p}", crate::work::notes::preamble(&cwd));
        if let Some(l) = &lead {
            p.push_str(&crate::commands::team::worker_postscript(l));
        }
        if o.autopilot.is_some() && issue.is_none() {
            p.push_str(AUTOPILOT_POSTSCRIPT);
        }
        let prompt = match &spec.prompt_args {
            Some(t) => crate::launch::fill_args(t, &[("prompt", &p)]),
            None => vec![p],
        };
        // Before the CLI's own flags, so one that takes several values (claude's
        // `--allowedTools a b`) can't swallow it; after a subcommand (`codex exec`).
        if args.first().is_none_or(|a| a.starts_with('-')) {
            args.splice(0..0, prompt);
        } else {
            args.extend(prompt);
        }
    }
    let args = args.as_slice();
    let id = runs::claim_id(o.name.as_deref(), &acct.provider)?;

    let mut run = Run {
        id: id.clone(),
        account: acct.id(),
        bin: spec.bin.clone(),
        args: args.to_vec(),
        cwd: cwd.to_string_lossy().into_owned(),
        started_at: time::now_ms(),
        waiting_patterns: spec.waiting_patterns.clone(),
        limit_patterns: spec.limit_patterns.clone(),
        guard: Some(crate::work::guards::Guard::from_config(o.stall, o.max_time)),
        group: o.group.clone(),
        lead: lead.clone(),
        team: o.workers,
        issue: issue.as_ref().and_then(|i| i.github_number()),
        tracker: issue.as_ref().and_then(|i| i.tracked()),
        auto_approve: o.no_auto.then_some(false),
        ..Default::default()
    };
    (run.rows, run.cols) = runs::term_size().unwrap_or((40, 120));
    let started = (|| -> Result<()> {
        if let Some(old) = &o.adopt {
            let old = runs::find(old)?;
            run.cwd = old.cwd.clone();
            run.worktree = old.worktree.clone();
        } else if o.worktree {
            let (wt, start) = add_worktree(&cwd, &id, &o)?;
            run.cwd = start.to_string_lossy().into_owned();
            let copied = crate::work::notes::copy_into(Path::new(&wt.repo), Path::new(&wt.path));
            if !copied.is_empty() {
                eprintln!("amux: copied {} into the worktree", copied.join(", "));
            }
            run.worktree = Some(wt);
        }
        // A worktree run gets its own ports and compose project, so parallel dev servers
        // don't fight over :3000.
        if run.worktree.is_some() {
            let taken: Vec<u16> = runs::list()
                .iter()
                .filter(|r| r.state().is_live())
                .filter_map(|r| r.port)
                .collect();
            if let Some(port) = crate::work::notes::free_port_block(&taken) {
                run.port = Some(port);
                env.set.push(("PORT".into(), port.to_string()));
                env.set.push(("AMUX_PORT".into(), port.to_string()));
                env.set
                    .push(("COMPOSE_PROJECT_NAME".into(), format!("amux-{}", run.id)));
            }
        }
        runs::save(&run)?;
        log_launch(&acct, args, Path::new(&run.cwd));
        start_host(&run, &env)
    })();
    if let Err(e) = started {
        // Nothing ran, so leave nothing behind: not the run, and not a fresh worktree.
        if let Some(wt) = &run.worktree {
            remove_worktree(wt, true);
        }
        let _ = std::fs::remove_dir_all(run.dir());
        return Err(e);
    }

    let place = match &run.worktree {
        Some(wt) => format!("{} (branch {})", tildify(Path::new(&run.cwd)), wt.branch),
        None => tildify(Path::new(&run.cwd)),
    };
    if let Some(merge) = o.autopilot {
        crate::work::autopilot::enable(&id, merge, None)?;
        eprintln!(
            "amux: autopilot is on for {id}: it opens a PR when the agent commits, hands CI failures and review comments back, and {}. `amux autopilot` shows how it's doing.",
            if merge { "merges it when it's ready" } else { "tells you when it's ready to merge" }
        );
    }
    let now = runs::load(&id)?;
    if now.state() == State::Queued {
        let why = now.queued.map(|q| q.reason).unwrap_or_default();
        eprintln!("amux: queued {id} as {} in {place}: {why}", acct.id());
        eprintln!(
            "  It starts by itself when there is room.  amux ps    amux slots    amux stop {id}"
        );
        return Ok(0);
    }
    eprintln!("amux: started {id} as {} in {place}", acct.id());
    if o.attach {
        return attach(&runs::load(&id)?);
    }
    eprintln!("  amux attach {id}    amux ps    amux stop {id}");
    Ok(0)
}

/// Starts `amux __run-host <id>` in its own session with the account's environment and
/// waits until it has started the CLI (or failed to).
fn start_host(run: &Run, env: &crate::launch::EnvPlan) -> Result<()> {
    let mut cmd = Command::new(std::env::current_exe()?);
    cmd.arg("__run-host")
        .arg(&run.id)
        .current_dir(&run.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    env.apply(&mut cmd);
    unsafe {
        // Detach from this terminal, so closing it doesn't end the run.
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let mut host = cmd.spawn().context("could not start the run host")?;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let r = runs::load(&run.id)?;
        if let Some(e) = r.error {
            bail!("{e}");
        }
        // Queued for a run slot: it starts by itself later, so don't hold the terminal.
        if r.pid.is_some() || r.queued.is_some() {
            return Ok(());
        }
        if host.try_wait()?.is_some() {
            bail!("The run host exited before starting {}.", run.bin);
        }
        if Instant::now() > deadline {
            let _ = host.kill();
            bail!("The run host did not start {} within 10 seconds.", run.bin);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Like `ago`, but in seconds for the first minute: runs are often seconds old.
fn since(ms: i64, now: i64) -> String {
    let s = (now - ms) / 1000;
    if s < 60 {
        format!("{}s ago", s.max(0))
    } else {
        ago(ms, now)
    }
}

fn last_output(run: &Run) -> Option<i64> {
    std::fs::metadata(run.dir().join("output.log"))
        .and_then(|m| m.modified())
        .ok()
        .map(time::to_ms)
}

fn state_label(run: &Run, now: i64) -> String {
    match run.state() {
        State::Starting => "starting".into(),
        State::Queued => {
            let t = run.queued.as_ref().map_or(now, |q| q.since);
            format!("queued {}", since(t, now).trim_end_matches(" ago"))
        }
        State::Running if run.waiting.is_some() => {
            let t = run.waiting.as_ref().map_or(now, |w| w.since);
            format!("waiting {}", since(t, now).trim_end_matches(" ago"))
        }
        // The CLI reports its own state (OSC 7501): believe it over output timing.
        State::Running if run.status.is_some() => {
            let s = run.status.as_ref().expect("checked above");
            let age = since(s.since, now);
            let age = age.trim_end_matches(" ago");
            match s.state.as_str() {
                "working" => "running".into(),
                "error" => format!("error {age}"),
                "blocked" => format!("waiting {age}"),
                // idle, and done (the turn finished): either way it waits for your next prompt.
                _ => format!("idle {age}"),
            }
        }
        State::Running => match last_output(run) {
            // Output in the last few seconds means the CLI is working; quiet usually
            // means it is waiting for you.
            Some(t) if now - t < 5_000 => "running".into(),
            Some(t) => format!("idle {}", since(t, now).trim_end_matches(" ago")),
            None => "running".into(),
        },
        State::Exited(c) => format!("exited {c}"),
        State::Failed => "failed".into(),
        State::Lost => "lost".into(),
    }
}

/// One object per run, for `ps --json`, scripts, agents and other machines' `amux ui`.
pub(crate) fn ps_values(all: &[Run]) -> Vec<serde_json::Value> {
    use crate::runs::state::run_kind;
    let now = time::now_ms();
    all.iter()
        .map(|r| {
            let (kind, label) = run_kind(r, now);
            let state = if r.state() == State::Queued {
                "queued".to_string()
            } else {
                format!("{kind:?}").to_lowercase()
            };
            let verify = crate::work::verify::load(&r.id);
            serde_json::json!({
                "id": r.id,
                "account": r.account,
                "state": state,
                "kind": kind.name(),
                "label": label,
                "psLabel": state_label(r, now),
                "live": kind.is_live(),
                "waiting": r.waiting.as_ref().map(|w| &w.reason),
                "queued": r.queued.as_ref().map(|q| &q.reason),
                "exitCode": r.exit_code,
                "cwd": r.cwd,
                "branch": r.worktree.as_ref().map(|w| &w.branch),
                "worktree": r.worktree.as_ref().map(|w| &w.path),
                // The space it belongs to: the repo of its worktree, else its folder.
                "repo": r.worktree.as_ref().map_or(&r.cwd, |w| &w.repo),
                "verifyLabel": verify.as_ref().map(|v| v.label()),
                "verify": verify,
                "pr": crate::work::github::load(&r.id),
                "startedAt": r.started_at,
                "args": r.args,
                "run": r,
            })
        })
        .collect()
}

pub fn ps_cmd(argv: &[String]) -> Result<i32> {
    let local_only = argv.iter().any(|a| a == "--local");
    let all = runs::list();
    let mut values = ps_values(&all);
    if !local_only {
        values.extend(crate::remote::runs(false));
    }
    if argv.iter().any(|a| a == "--json") {
        println!("{}", serde_json::to_string_pretty(&values)?);
        return Ok(0);
    }
    if values.is_empty() {
        println!("No runs. Start one with: amux spawn <provider>[@name] [args...]");
        return Ok(0);
    }
    let now = time::now_ms();
    let s = |v: &serde_json::Value, k: &str| v[k].as_str().unwrap_or_default().to_string();
    let rows: Vec<Vec<String>> = values
        .iter()
        .map(|v| {
            let args: Vec<String> = v["args"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            let dir = s(v, "cwd");
            vec![
                s(v, "id"),
                s(v, "account"),
                s(v, "psLabel"),
                since(v["startedAt"].as_i64().unwrap_or(now), now),
                if v.get("machine").is_some() {
                    dir
                } else {
                    tildify(Path::new(&dir))
                },
                v["branch"].as_str().unwrap_or("-").to_string(),
                one_line(&args.join(" "), 50),
            ]
        })
        .collect();
    println!(
        "{}",
        table(
            &["RUN", "ACCOUNT", "STATE", "STARTED", "DIR", "BRANCH", "ARGS"],
            &rows
        )
    );
    Ok(0)
}

pub fn attach_cmd(argv: &[String]) -> Result<i32> {
    let run = runs::find(need(argv.first(), "amux attach <run>")?)?;
    match run.state() {
        State::Running | State::Starting => attach(&run),
        State::Queued => bail!(
            "{} is queued and hasn't started yet: {}. `amux slots` shows what it waits for.",
            run.id,
            run.queued
                .as_ref()
                .map_or("waiting for a run slot", |q| q.reason.as_str())
        ),
        State::Exited(c) => bail!(
            "{0} exited with code {c}. See its output with `amux logs {0}`.",
            run.id
        ),
        State::Failed => bail!(
            "{} failed to start: {}",
            run.id,
            run.error.as_deref().unwrap_or("unknown error")
        ),
        State::Lost => bail!(
            "{0}'s host is gone (killed, or the machine restarted). See `amux logs {0}`.",
            run.id
        ),
    }
}

/// `amux peek <run>`: the run's screen as it is now (or as it ended), as plain text.
pub fn peek_cmd(argv: &[String]) -> Result<i32> {
    let run = runs::find(need(argv.first(), "amux peek <run>")?)?;
    for line in runs::screen::text(&runs::screen::capture(&run)) {
        println!("{line}");
    }
    Ok(0)
}

pub fn logs_cmd(argv: &[String]) -> Result<i32> {
    let run = runs::find(need(argv.first(), "amux logs <run>")?)?;
    let mut out = std::io::stdout().lock();
    for f in run.log_files() {
        if let Ok(mut file) = std::fs::File::open(&f) {
            std::io::copy(&mut file, &mut out)?;
        }
    }
    if let Some(e) = &run.error {
        eprintln!("amux: {} failed to start: {e}", run.id);
    }
    Ok(0)
}

fn signal(run: &Run, sig: libc::c_int) -> Result<()> {
    if run.state() == State::Queued {
        return cancel_queued(run);
    }
    let pid = run
        .pid
        .ok_or_else(|| anyhow!("{} has no process yet.", run.id))?;
    // The CLI leads its own process group, so this reaches anything it started too.
    if unsafe { libc::killpg(pid as libc::pid_t, sig) } != 0 {
        return Err(std::io::Error::last_os_error())
            .context(format!("could not signal {}", run.id));
    }
    Ok(())
}

/// Ends a run that is still waiting for a slot: its host has started nothing yet, so
/// ending the host ends the run. Recorded as stopped (143), like `amux stop` would.
fn cancel_queued(run: &Run) -> Result<()> {
    let host = run
        .host_pid
        .ok_or_else(|| anyhow!("{} has no host.", run.id))?;
    if unsafe { libc::kill(host as libc::pid_t, libc::SIGTERM) } != 0 {
        return Err(std::io::Error::last_os_error()).context(format!("could not stop {}", run.id));
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    while runs::alive(host) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    let mut r = runs::load(&run.id)?;
    if r.exit_code.is_none() {
        r.exit_code = Some(128 + libc::SIGTERM);
        r.ended_at = Some(time::now_ms());
        r.queued = None;
        runs::save(&r)?;
    }
    Ok(())
}

fn wait_exit(id: &str, limit: Duration) -> Option<Run> {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        match runs::load(id) {
            Ok(r) if !matches!(r.state(), State::Running | State::Queued) => return Some(r),
            Err(_) => return None,
            _ => std::thread::sleep(Duration::from_millis(50)),
        }
    }
    None
}

pub fn stop_cmd(argv: &[String]) -> Result<i32> {
    let force = argv.iter().any(|a| a == "--force" || a == "-f");
    let id = need(
        argv.iter().find(|a| !a.starts_with('-')),
        "amux stop <run> [--force]",
    )?;
    let run = runs::find(id)?;
    if !matches!(run.state(), State::Running | State::Queued) {
        bail!("{} is not running.", run.id);
    }
    signal(&run, if force { libc::SIGKILL } else { libc::SIGTERM })?;
    match wait_exit(&run.id, Duration::from_secs(5)) {
        Some(r) => println!("Stopped {} ({}).", r.id, state_label(&r, time::now_ms())),
        None => println!(
            "Sent {} to {}; it is still running. Use `amux stop {} --force` to kill it.",
            if force { "SIGKILL" } else { "SIGTERM" },
            run.id,
            run.id
        ),
    }
    Ok(0)
}

/// Once a resumed session is up (it printed since `since`) and has gone quiet, asks it to
/// go on where it stopped. Gives up after 40 seconds.
fn nudge_when_ready(name: &str, since: i64, msg: &str) -> bool {
    let deadline = Instant::now() + Duration::from_secs(40);
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(500));
        let Ok(r) = runs::load(name) else { continue };
        let printed = last_output(&r).filter(|t| *t >= since - 1_000);
        let quiet = printed.is_some_and(|t| time::now_ms() - t > 2_500);
        if r.state() == State::Running && quiet {
            return crate::runs::client::send_text(&r, msg, true).is_ok();
        }
    }
    false
}

/// The session a run was having, so another account can resume it: the newest session of
/// its CLI in its folder that was active since the run started.
fn session_of(run: &Run) -> Option<String> {
    let provider = run.account.split('@').next()?;
    let spec = crate::store::get_provider(provider).ok()?;
    crate::sessions::list_sessions(&spec, None)
        .into_iter()
        .filter(|s| s.cwd.as_deref() == Some(run.cwd.as_str()))
        .filter(|s| s.updated_at >= run.started_at - 60_000)
        .max_by_key(|s| s.updated_at)
        .map(|s| s.id)
}

const FAILOVER_USAGE: &str = "amux failover <run> [--to <provider>@<name>]";

/// `amux failover <run>`: the run's account hit its usage limit (or you want it on another
/// one): stop it, resume the same session under another account in the same folder and
/// worktree, and tell the agent to carry on.
pub fn failover_cmd(argv: &[String]) -> Result<i32> {
    let mut to = None;
    let mut id = None;
    // Set by the run host when it moves a run before the limit (`limits.switchAt`).
    let mut near = false;
    let mut it = argv.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--to" => to = Some(need(it.next(), FAILOVER_USAGE)?.to_string()),
            "--near-limit" => near = true,
            f if f.starts_with('-') => bail!("Unknown option {f}. Usage: {FAILOVER_USAGE}"),
            r => id = Some(r.to_string()),
        }
    }
    let run = runs::find(need(id.as_ref(), FAILOVER_USAGE)?)?;
    let provider = run
        .account
        .split('@')
        .next()
        .unwrap_or_default()
        .to_string();
    let target = match &to {
        Some(t) => resolve_account(t)?,
        None => crate::work::limits::pick(&provider, &run.account).ok_or_else(|| {
            anyhow!("No other {provider} account without a usage limit. `amux usage` shows them; add one with `amux account add {provider} <name>`.")
        })?,
    };
    if target.provider != provider {
        bail!(
            "{} is a {provider} run; it can only continue under another {provider} account.",
            run.id
        );
    }
    if target.id() == run.account {
        bail!("{} already runs as {}.", run.id, run.account);
    }
    let spec = crate::store::get_provider(&provider)?;
    let session = session_of(&run).ok_or_else(|| {
        anyhow!(
            "Can't find {}'s session to resume (no {provider} session in {} since it started).",
            run.id,
            tildify(Path::new(&run.cwd))
        )
    })?;
    let resume = spec.resume_args.clone().ok_or_else(|| {
        anyhow!("{provider} has no resumeArgs, so its sessions can't be resumed.")
    })?;
    if run.state().is_live() {
        signal(&run, libc::SIGTERM)?;
        if wait_exit(&run.id, Duration::from_secs(10)).is_none() {
            let _ = signal(&run, libc::SIGKILL);
            wait_exit(&run.id, Duration::from_secs(3));
        }
    }
    // A name next to the old one: fix-login → fix-login-2.
    let base = run
        .id
        .trim_end_matches(|c: char| c.is_ascii_digit())
        .trim_end_matches('-');
    let name = (2..)
        .map(|n| format!("{base}-{n}"))
        .find(|n| !runs::run_dir(n).exists())
        .expect("a free name");
    let mut args = vec![
        "--name".to_string(),
        name.clone(),
        "--adopt".into(),
        run.id.clone(),
        target.id(),
    ];
    args.extend(crate::launch::fill_args(&resume, &[("id", &session)]));
    let since = time::now_ms();
    spawn_cmd(&args)?;
    nudge_when_ready(
        &name,
        since,
        if near {
            "The previous account was close to its usage limit, so this session continues under another account. If you were in the middle of the task, carry on where you left off; otherwise wait for the next instruction."
        } else {
            "The previous account hit its usage limit, so this session continues under another account. Please carry on with the task where you left off."
        },
    );
    println!(
        "{} continues as {name} under {} (session {session}, same folder{}).",
        run.id,
        target.id(),
        if run.worktree.is_some() {
            " and worktree"
        } else {
            ""
        }
    );
    Ok(0)
}

const REVIVE_USAGE: &str =
    "amux revive [<run>...] [--all] [--fresh] [--dry-run] | --at-login | --no-at-login";

/// What reviving one run did.
#[derive(Debug)]
pub struct Revived {
    pub run: String,
    pub ok: bool,
    pub detail: String,
}

/// Brings a lost run back in place: same name, folder, worktree, ports, group and guards.
/// Its CLI resumes the session it was having (or, with `fresh`, starts again with the
/// command it was started with), and is told why once it's up.
pub fn revive_one(run: &Run, fresh: bool, dry: bool) -> Revived {
    let out = |ok: bool, detail: String| Revived {
        run: run.id.clone(),
        ok,
        detail,
    };
    if run.state() != State::Lost {
        return out(false, "it isn't lost".into());
    }
    let provider = run
        .account
        .split('@')
        .next()
        .unwrap_or_default()
        .to_string();
    let (acct, spec) = match (
        resolve_account(&run.account),
        crate::store::get_provider(&provider),
    ) {
        (Ok(a), Ok(s)) => (a, s),
        _ => return out(false, format!("its account {} is gone", run.account)),
    };
    if !Path::new(&run.cwd).is_dir() {
        return out(
            false,
            format!("its folder {} is gone", tildify(Path::new(&run.cwd))),
        );
    }
    let args = match (session_of(run), &spec.resume_args) {
        (Some(id), Some(t)) => crate::launch::fill_args(t, &[("id", &id)]),
        _ if fresh => run.args.clone(),
        (None, _) => {
            return out(
                false,
                "no session of it to resume (--fresh starts it again)".into(),
            )
        }
        (Some(_), None) => {
            return out(
                false,
                format!("{provider} has no resumeArgs (--fresh starts it again)"),
            )
        }
    };
    if dry {
        return out(true, format!("would run {} {}", run.bin, args.join(" ")));
    }
    let (_, mut env) = match prepare(&acct) {
        Ok(p) => p,
        Err(e) => return out(false, format!("{e:#}")),
    };
    if let Some(port) = run.port {
        env.set.push(("PORT".into(), port.to_string()));
        env.set.push(("AMUX_PORT".into(), port.to_string()));
        env.set
            .push(("COMPOSE_PROJECT_NAME".into(), format!("amux-{}", run.id)));
    }
    let mut r = run.clone();
    r.args = args;
    r.started_at = time::now_ms();
    r.host_pid = None;
    r.pid = None;
    r.exit_code = None;
    r.ended_at = None;
    r.error = None;
    r.waiting = None;
    r.status = None;
    r.queued = None;
    // What it printed before goes on in the same log, after a marker.
    let _ = fsx::append_private(
        &r.dir().join("output.log"),
        "\r\n\x1b[2m--- amux: the run was lost (restart); resuming its session ---\x1b[0m\r\n",
    );
    let started = runs::save(&r).and_then(|_| start_host(&r, &env));
    if let Err(e) = started {
        return out(false, format!("{e:#}"));
    }
    let since = r.started_at;
    let told = nudge_when_ready(
        &r.id,
        since,
        "The computer restarted while you were working, and amux resumed this session. Please check where you were (git status, the last steps) and carry on with the task.",
    );
    out(
        true,
        format!(
            "resumed{}",
            if told {
                " and told to carry on"
            } else {
                " (it didn't settle in time to be told)"
            }
        ),
    )
}

/// `amux revive`: brings runs lost in a restart (or a crash of their host) back. With
/// `--at-login`, installs a login item that does it after every restart.
pub fn revive_cmd(argv: &[String]) -> Result<i32> {
    match argv.first().map(String::as_str) {
        Some("--at-login") => return crate::commands::login_item::install(),
        Some("--no-at-login") => return crate::commands::login_item::uninstall(),
        _ => {}
    }
    let all = argv.iter().any(|a| a == "--all");
    let fresh = argv.iter().any(|a| a == "--fresh");
    let dry = argv.iter().any(|a| a == "--dry-run" || a == "-n");
    let quiet = argv.iter().any(|a| a == "--quiet");
    // After a restart the network may still be coming up: the login item waits a bit.
    if let Some(i) = argv.iter().position(|a| a == "--after") {
        let ms = crate::work::guards::parse_duration(need(argv.get(i + 1), REVIVE_USAGE)?)
            .ok_or_else(|| anyhow!("--after takes a time such as 30s."))?;
        std::thread::sleep(Duration::from_millis(ms));
    }
    let named: Vec<&String> = argv
        .iter()
        .enumerate()
        .filter(|(i, a)| !a.starts_with('-') && (*i == 0 || argv[i - 1] != "--after"))
        .map(|(_, a)| a)
        .collect();
    if let Some(f) = argv.iter().find(|a| {
        a.starts_with('-')
            && !matches!(
                a.as_str(),
                "--all" | "--fresh" | "--dry-run" | "-n" | "--quiet" | "--after"
            )
    }) {
        bail!("Unknown option {f}. Usage: {REVIVE_USAGE}");
    }
    let targets: Vec<Run> = if named.is_empty() {
        let lost: Vec<Run> = runs::list()
            .into_iter()
            .filter(|r| r.state() == State::Lost)
            .collect();
        if !all && !lost.is_empty() {
            for r in &lost {
                println!(
                    "{}  {}  lost {}",
                    r.id,
                    r.account,
                    if r.lost_in_restart() {
                        "in a restart"
                    } else {
                        "(its host died)"
                    }
                );
            }
            println!("\n`amux revive --all` brings them back, or name them: amux revive <run>...");
            return Ok(0);
        }
        lost
    } else {
        named.iter().map(|n| runs::find(n)).collect::<Result<_>>()?
    };
    if targets.is_empty() {
        if !quiet {
            println!("No lost runs.");
        }
        return Ok(0);
    }
    let done: Vec<Revived> = targets.iter().map(|r| revive_one(r, fresh, dry)).collect();
    let rows: Vec<Vec<String>> = done
        .iter()
        .map(|d| {
            vec![
                d.run.clone(),
                if d.ok { "✓".into() } else { "✗".into() },
                d.detail.clone(),
            ]
        })
        .collect();
    println!("{}", table(&["RUN", "", "DETAIL"], &rows));
    Ok(if done.iter().all(|d| d.ok) { 0 } else { 1 })
}

pub fn rm_cmd(argv: &[String]) -> Result<i32> {
    let force = argv.iter().any(|a| a == "--force" || a == "-f");
    let done = argv.iter().any(|a| a == "--done");
    let ids: Vec<&String> = argv.iter().filter(|a| !a.starts_with('-')).collect();
    let targets: Vec<Run> = if done {
        runs::list()
            .into_iter()
            .filter(|r| !r.state().is_live())
            .collect()
    } else {
        if ids.is_empty() {
            bail!("Usage: amux rm <run>... [--force] | amux rm --done");
        }
        ids.iter().map(|id| runs::find(id)).collect::<Result<_>>()?
    };
    if targets.is_empty() {
        println!("Nothing to remove.");
    }
    for run in targets {
        if run.state().is_live() {
            if !force {
                bail!(
                    "{0} is still running. Stop it first (`amux stop {0}`) or use --force.",
                    run.id
                );
            }
            let _ = signal(&run, libc::SIGKILL);
            wait_exit(&run.id, Duration::from_secs(3));
        }
        let note = run
            .worktree
            .as_ref()
            .and_then(|wt| remove_worktree(wt, false));
        std::fs::remove_dir_all(run.dir())?;
        match note {
            Some(n) => println!("Removed {}; {n}.", run.id),
            None => println!("Removed {}.", run.id),
        }
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn spawn_flags_stop_at_the_account() {
        let a = args(&[
            "--worktree",
            "--name",
            "fix",
            "claude@work",
            "--name",
            "x",
            "-p",
            "hi",
        ]);
        let (o, acct, rest) = parse_spawn(&a).unwrap();
        assert!(o.worktree && !o.attach);
        assert_eq!(o.name.as_deref(), Some("fix"));
        assert_eq!(acct, "claude@work");
        assert_eq!(rest, ["--name", "x", "-p", "hi"]);
    }

    #[test]
    fn branch_implies_worktree_and_rejects_option_like_values() {
        let (o, _, _) = parse_spawn(&args(&["--branch=feat/x", "codex"])).unwrap();
        assert!(o.worktree);
        assert_eq!(o.branch.as_deref(), Some("feat/x"));
        assert!(parse_spawn(&args(&["--base", "--detach", "codex"])).is_err());
        assert!(parse_spawn(&args(&["--bogus", "codex"])).is_err());
        assert!(parse_spawn(&args(&["--name"])).is_err());
    }
}
