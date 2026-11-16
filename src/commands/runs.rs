//! `amux spawn | ps | attach | logs | stop | rm`: parallel background runs.

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
    "amux spawn [--name <run>] [--worktree] [--branch <name>] [--base <ref>] [--attach] <provider>[@name] [args...]";

#[derive(Default)]
struct SpawnOpts {
    name: Option<String>,
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
            "--" => {
                i += 1;
                break;
            }
            _ => bail!("Unknown option {a}. Usage: {SPAWN_USAGE}"),
        }
        i += 1;
    }
    o.worktree |= o.branch.is_some() || o.base.is_some();
    let acct = need(argv.get(i), SPAWN_USAGE)?;
    Ok((o, acct, argv.get(i + 1..).unwrap_or_default()))
}

fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .context("could not run git")?;
    if !out.status.success() {
        bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
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
    Ok((
        Worktree {
            repo: top.to_string_lossy().into_owned(),
            path: path_s,
            branch,
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
    let (o, acct_ref, args) = parse_spawn(argv)?;
    if let Some(n) = &o.name {
        if !valid_name(n) {
            bail!("Run names use letters, digits, '.', '_' and '-'.");
        }
    }
    let acct = resolve_account(acct_ref)?;
    let (spec, env) = prepare(&acct)?;
    let cwd = std::env::current_dir()?;
    let id = runs::claim_id(o.name.as_deref(), &acct.provider)?;

    let mut run = Run {
        id: id.clone(),
        account: acct.id(),
        bin: spec.bin.clone(),
        args: args.to_vec(),
        cwd: cwd.to_string_lossy().into_owned(),
        started_at: time::now_ms(),
        waiting_patterns: spec.waiting_patterns.clone(),
        ..Default::default()
    };
    (run.rows, run.cols) = runs::term_size().unwrap_or((40, 120));
    let started = (|| -> Result<()> {
        if o.worktree {
            let (wt, start) = add_worktree(&cwd, &id, &o)?;
            run.cwd = start.to_string_lossy().into_owned();
            run.worktree = Some(wt);
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
        if r.pid.is_some() {
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

pub fn ps_cmd(_argv: &[String]) -> Result<i32> {
    let all = runs::list();
    if all.is_empty() {
        println!("No runs. Start one with: amux spawn <provider>[@name] [args...]");
        return Ok(0);
    }
    let now = time::now_ms();
    let rows: Vec<Vec<String>> = all
        .iter()
        .map(|r| {
            vec![
                r.id.clone(),
                r.account.clone(),
                state_label(r, now),
                since(r.started_at, now),
                tildify(Path::new(&r.cwd)),
                r.worktree.as_ref().map_or("-".into(), |w| w.branch.clone()),
                one_line(&r.args.join(" "), 50),
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

fn wait_exit(id: &str, limit: Duration) -> Option<Run> {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        match runs::load(id) {
            Ok(r) if r.state() != State::Running => return Some(r),
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
    if run.state() != State::Running {
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

pub fn rm_cmd(argv: &[String]) -> Result<i32> {
    let force = argv.iter().any(|a| a == "--force" || a == "-f");
    let done = argv.iter().any(|a| a == "--done");
    let ids: Vec<&String> = argv.iter().filter(|a| !a.starts_with('-')).collect();
    let targets: Vec<Run> = if done {
        runs::list()
            .into_iter()
            .filter(|r| r.state() != State::Running && r.state() != State::Starting)
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
        if matches!(run.state(), State::Running | State::Starting) {
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
