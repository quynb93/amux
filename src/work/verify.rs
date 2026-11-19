//! Checking a run's work once the agent is done, one of two ways: run the project's tests
//! in the run's folder, waiting for a `test` slot so five finished agents don't start five
//! test suites at once, or push the run's branch so CI runs them, which costs this machine
//! nothing. `Plan` says which is better right now.
//!
//! The outcome is kept in `runs/<id>/verify.json` (the host owns run.json), and the test
//! output in `runs/<id>/verify.log`.

use super::slots::{self, Holder, Limits, Pressure, Wait};
use crate::git::run_git as git;
use crate::paths::tildify;
use crate::runs::{self, Run};
use crate::store::{load_config, read_json, write_json};
use crate::time;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// The last verification of a run, as `amux ui` shows it.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Verify {
    /// test or push.
    pub kind: String,
    /// queued, running, passed, failed, pushed or error.
    pub state: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub detail: String,
    /// The pid doing it, while it is queued or running.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    pub since: i64,
}

impl Verify {
    /// A short label for the run list: "tests passed", "pushed", …
    pub fn label(&self) -> String {
        let live = self.pid.is_some_and(runs::alive);
        match (self.kind.as_str(), self.state.as_str()) {
            // Its process went away without recording the end (killed, or the UI quit).
            (_, "queued" | "running") if !live => format!("{} stopped", self.noun()),
            ("push", s) => s.to_string(),
            (_, "sent") => "failures sent to agent".into(),
            (_, s) => format!("tests {s}"),
        }
    }

    fn noun(&self) -> &'static str {
        if self.kind == "push" {
            "push"
        } else {
            "tests"
        }
    }
}

fn verify_file(id: &str) -> PathBuf {
    runs::run_dir(id).join("verify.json")
}

pub fn load(id: &str) -> Option<Verify> {
    read_json::<Option<Verify>>(&verify_file(id)).ok().flatten()
}

pub fn record(id: &str, kind: &str, state: &str, detail: &str, live: bool) {
    let v = Verify {
        kind: kind.into(),
        state: state.into(),
        detail: crate::util::one_line(detail, 200),
        pid: live.then(std::process::id),
        since: time::now_ms(),
    };
    let _ = write_json(&verify_file(id), &v);
    if v.pid.is_none() {
        let msg = format!("{kind} {state}: {}", v.detail);
        crate::plugins::hook("verify", id, &msg);
        crate::runs::push::send("verify", id, &format!("amux: {id}"), &msg);
    }
}

/// What can be done for a run, and which way is better right now.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Plan {
    pub id: String,
    /// The folder the tests run in: the run's own.
    pub dir: PathBuf,
    pub test: Option<String>,
    /// The branch and remote a push would use; `None` if the run has no worktree.
    pub branch: Option<String>,
    pub remote: Option<String>,
    /// Uncommitted or untracked files in the worktree. A push leaves them out.
    pub dirty: usize,
    /// Commits on the branch that its remote branch doesn't have, when it has one.
    pub ahead: Option<usize>,
    /// Why running tests here would wait now, if it would.
    pub busy: Option<String>,
    /// The last test run failed and the run is alive to fix it.
    pub failed: bool,
    /// Its PR's CI checks failed last time we looked, and the run is alive to fix them.
    pub ci_failed: bool,
}

impl Plan {
    pub fn for_run(run: &Run) -> Plan {
        let dir = PathBuf::from(&run.cwd);
        let mut p = Plan {
            id: run.id.clone(),
            test: test_command(&dir),
            ..Default::default()
        };
        if let Some(wt) = &run.worktree {
            let wt_dir = Path::new(&wt.path);
            p.branch = Some(wt.branch.clone());
            p.remote = remote(wt_dir);
            p.dirty = git(wt_dir, &["status", "--porcelain"])
                .map(|s| s.lines().filter(|l| !l.trim().is_empty()).count())
                .unwrap_or(0);
            p.ahead = git(wt_dir, &["rev-list", "--count", "@{upstream}..HEAD"])
                .ok()
                .and_then(|n| n.parse().ok());
        }
        p.busy = busy();
        let alive = run.state() == runs::State::Running;
        p.failed = alive && load(&run.id).is_some_and(|v| v.kind == "test" && v.state == "failed");
        p.ci_failed = alive
            && crate::work::github::load(&run.id)
                .is_some_and(|pr| pr.state == "OPEN" && pr.checks == "failing");
        p.dir = dir;
        p
    }

    /// Push when the machine is busy and there is something to push; test otherwise.
    pub fn recommend_push(&self) -> bool {
        self.branch.is_some()
            && self.remote.is_some()
            && (self.busy.is_some() || self.test.is_none())
    }

    pub fn lines(&self) -> Vec<String> {
        let mut out = vec![];
        match &self.test {
            Some(t) => out.push(format!("Tests: {t}  (in {})", tildify(&self.dir))),
            None => out.push(
                "Tests: none found. Set \"verify\": { \"test\": \"<command>\" } in ~/.amux/config.json."
                    .into(),
            ),
        }
        match (&self.branch, &self.remote) {
            (Some(b), Some(r)) => {
                let mut s = format!("Push: {b} to {r}");
                match self.ahead {
                    Some(0) => s.push_str(" (already up to date)"),
                    Some(n) => s.push_str(&format!(" ({n} new commit{})", plural(n))),
                    None => s.push_str(" (new branch)"),
                }
                out.push(s);
            }
            (Some(b), None) => out.push(format!("Push: {b} has no remote to push to.")),
            (None, _) => out.push("Push: the run has no worktree of its own.".into()),
        }
        if self.dirty > 0 {
            out.push(format!(
                "{} uncommitted file{}: a push leaves {} out; tests see {}.",
                self.dirty,
                plural(self.dirty),
                if self.dirty == 1 { "it" } else { "them" },
                if self.dirty == 1 { "it" } else { "them" },
            ));
        }
        if let Some(b) = &self.busy {
            out.push(format!("Tests would wait now: {b}."));
        }
        out
    }
}

fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

/// Why a test run would wait now: the test slots are full, or the machine is busy.
fn busy() -> Option<String> {
    let limits = Limits::from_config();
    let limit = limits.of("test");
    let held = slots::holders(&slots::slots_dir(), "test").len();
    if held >= limit {
        return Some(format!("all {limit} test slot{} are taken", plural(limit)));
    }
    let waiting = slots::waiters(&slots::slots_dir(), "test").len();
    if waiting > 0 {
        return Some(format!(
            "{waiting} test run{} already waiting",
            plural(waiting)
        ));
    }
    Pressure::now().too_high(&limits)
}

/// The remote a branch pushes to: its upstream's, else origin, else the only one.
fn remote(dir: &Path) -> Option<String> {
    if let Ok(up) = git(dir, &["rev-parse", "--abbrev-ref", "@{upstream}"]) {
        if let Some((r, _)) = up.split_once('/') {
            return Some(r.to_string());
        }
    }
    let all = git(dir, &["remote"]).ok()?;
    let all: Vec<&str> = all.lines().collect();
    if all.contains(&"origin") {
        return Some("origin".into());
    }
    match all.as_slice() {
        [one] => Some(one.to_string()),
        _ => None,
    }
}

/// The project's test command: `"verify": { "test": … }` in config.json, else guessed from
/// the files in `dir` or the nearest folder above it that has any.
pub fn test_command(dir: &Path) -> Option<String> {
    let cfg = load_config().unwrap_or_default();
    if let Some(t) = cfg
        .extra
        .get("verify")
        .and_then(|v| v.get("test"))
        .and_then(Value::as_str)
        .filter(|t| !t.trim().is_empty())
    {
        return Some(t.to_string());
    }
    dir.ancestors().take(8).find_map(guess_test_command)
}

/// Plain Python tests with no test runner configured: test_*.py here or in tests/.
fn has_unittests(d: &Path) -> bool {
    [d.to_path_buf(), d.join("tests")].iter().any(|dir| {
        std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .any(|e| {
                let n = e.file_name().to_string_lossy().into_owned();
                n.starts_with("test_") && n.ends_with(".py")
            })
    })
}

fn guess_test_command(d: &Path) -> Option<String> {
    let has = |f: &str| d.join(f).exists();
    if let Ok(text) = std::fs::read_to_string(d.join("package.json")) {
        let test = serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|v| v.get("scripts")?.get("test")?.as_str().map(str::to_string));
        // npm init's placeholder only fails.
        if test.is_some_and(|t| !t.contains("no test specified")) {
            let pm = if has("pnpm-lock.yaml") {
                "pnpm test"
            } else if has("yarn.lock") {
                "yarn test"
            } else if has("bun.lockb") || has("bun.lock") {
                "bun run test"
            } else {
                "npm test"
            };
            return Some(pm.into());
        }
    }
    let make_test = || {
        std::fs::read_to_string(d.join("Makefile"))
            .is_ok_and(|m| m.lines().any(|l| l.starts_with("test:")))
    };
    let cmd = if has("Cargo.toml") {
        "cargo test"
    } else if has("go.mod") {
        "go test ./..."
    } else if make_test() {
        "make test"
    } else if has("pytest.ini") || has("pyproject.toml") || has("setup.py") || has("tox.ini") {
        "python3 -m pytest"
    } else if has_unittests(d) {
        "python3 -m unittest"
    } else if has("gradlew") {
        "./gradlew test"
    } else if has("pom.xml") {
        "mvn test"
    } else if has("mix.exs") {
        "mix test"
    } else if has("Package.swift") {
        "swift test"
    } else {
        return None;
    };
    Some(cmd.into())
}

/// Pushes the run's branch (never forced) and returns what to tell the user.
pub fn push(run: &Run, plan: &Plan) -> Result<String> {
    let Some(branch) = &plan.branch else {
        bail!(
            "{} has no worktree of its own, so there is no branch to push. Push from {}.",
            run.id,
            tildify(&plan.dir)
        );
    };
    let Some(remote) = &plan.remote else {
        bail!("{branch} has no remote to push to. Add one with `git remote add`.");
    };
    let wt = run
        .worktree
        .as_ref()
        .map(|w| PathBuf::from(&w.path))
        .unwrap_or_default();
    record(
        &run.id,
        "push",
        "running",
        &format!("{branch} to {remote}"),
        true,
    );
    // Never forced: if the remote branch moved, the push fails and says so.
    let refspec = format!("HEAD:refs/heads/{branch}");
    let out = crate::git::run_git_cmd(&wt)
        .args([
            "push",
            "--no-verify",
            "--set-upstream",
            remote.as_str(),
            refspec.as_str(),
        ])
        .output()
        .context("could not run git")?;
    let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
    if !out.status.success() {
        let why = err
            .lines()
            .rfind(|l| !l.trim().is_empty())
            .unwrap_or("git push failed");
        record(&run.id, "push", "error", why, false);
        bail!("Pushing {branch} failed: {why}");
    }
    let mut msg = format!("Pushed {branch} to {remote}; CI runs the tests.");
    if plan.dirty > 0 {
        msg.push_str(&format!(
            " {} uncommitted file{} stayed behind.",
            plan.dirty,
            plural(plan.dirty)
        ));
    }
    record(
        &run.id,
        "push",
        "pushed",
        &format!("{branch} to {remote}"),
        false,
    );
    Ok(msg)
}

/// How a test run ended.
#[derive(Debug, PartialEq)]
pub enum TestOutcome {
    Passed {
        cmd: String,
        log: PathBuf,
    },
    Failed {
        cmd: String,
        code: i32,
        log: PathBuf,
    },
    /// No test slot came free in time; nothing ran.
    NoSlot,
}

/// Runs the tests in the run's folder once a `test` slot is free, keeping verify.json
/// current (queued, running, passed or failed) for `amux ui`. A sandboxed run's tests are
/// the agent's code, so they run in its image with its mounts, unless `host` says to run
/// them on this machine (`--host-test`).
pub fn run_tests(run: &Run, cmd: Option<String>, host: bool) -> Result<TestOutcome> {
    let Some(cmd) = cmd else {
        bail!(
            "No test command found for {}. Pass --command, or set \"verify\": {{ \"test\": \"<command>\" }} in ~/.amux/config.json.",
            run.id
        );
    };
    let dir = PathBuf::from(&run.cwd);
    let log_path = runs::run_dir(&run.id).join("verify.log");
    let log = crate::fsx::open_append(&log_path)?;
    log.set_len(0)?;
    let me = Holder {
        run: Some(run.id.clone()),
        cwd: Some(tildify(&dir)),
        ..Holder::me(&cmd)
    };
    record(&run.id, "test", "queued", &cmd, true);
    let wait = Wait {
        timeout: None,
        pressure: true,
    };
    let limit = Limits::from_config().of("test");
    let slot = slots::acquire("test", &me, &wait, |b| {
        record(&run.id, "test", "queued", &b.describe("test", limit), true);
    })?;
    let Some(slot) = slot else {
        record(&run.id, "test", "error", "no test slot came free", false);
        return Ok(TestOutcome::NoSlot);
    };
    record(&run.id, "test", "running", &cmd, true);
    let mut c = match run.sandbox.as_ref().filter(|_| !host) {
        Some(sb) => {
            let argv = sb.test_command(&run.id, &run.cwd, &cmd);
            let mut c = Command::new(&argv[0]);
            c.args(&argv[1..]);
            c
        }
        None => {
            let mut c = Command::new("sh");
            c.arg("-c").arg(&cmd);
            c
        }
    };
    let status = c
        .current_dir(&dir)
        .env("AMUX_RUN", &run.id)
        .env("CI", "1")
        // Keep Python from leaving __pycache__ folders in the run's worktree.
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .status();
    drop(slot);
    let status = status.with_context(|| format!("could not run {cmd}"))?;
    if status.success() {
        record(&run.id, "test", "passed", &cmd, false);
        Ok(TestOutcome::Passed { cmd, log: log_path })
    } else {
        let code = status.code().unwrap_or(1);
        record(
            &run.id,
            "test",
            "failed",
            &format!("{cmd} exited {code}"),
            false,
        );
        Ok(TestOutcome::Failed {
            cmd,
            code,
            log: log_path,
        })
    }
}

/// The last `max` bytes of a test log as plain text: escape sequences and carriage-return
/// redraws (progress bars) removed, cut at a line start.
pub fn log_tail(bytes: &[u8], max: usize) -> String {
    let start = bytes.len().saturating_sub(max * 4);
    let mut text = String::new();
    let mut chars = bytes[start..].iter().copied().peekable();
    let mut line = Vec::new();
    while let Some(b) = chars.next() {
        match b {
            0x1b => {
                // CSI: ESC [ … final byte; other escapes: ESC and one byte.
                if chars.next_if_eq(&b'[').is_some() {
                    for c in chars.by_ref() {
                        if (0x40..=0x7e).contains(&c) {
                            break;
                        }
                    }
                } else {
                    chars.next();
                }
            }
            b'\r' if chars.peek() != Some(&b'\n') => line.clear(),
            b'\n' => {
                text.push_str(String::from_utf8_lossy(&line).trim_end());
                text.push('\n');
                line.clear();
            }
            b'\t' => line.push(b' '),
            c if c < 0x20 || c == 0x7f => {}
            c => line.push(c),
        }
    }
    text.push_str(String::from_utf8_lossy(&line).trim_end());
    let text = text.trim_end();
    if text.len() <= max {
        return text.to_string();
    }
    let mut cut = text.len() - max;
    while !text.is_char_boundary(cut) {
        cut += 1;
    }
    let tail = &text[cut..];
    tail.split_once('\n')
        .map_or(tail, |(_, rest)| rest)
        .to_string()
}

/// `--send-failures`: hands the last failed test run's output to the run's agent to fix,
/// the way a reviewer would paste a failing CI log into the chat.
pub fn send_failures(run: &Run) -> Result<()> {
    let v = load(&run.id).filter(|v| v.kind == "test");
    let Some(v) = v.filter(|v| v.state == "failed") else {
        bail!(
            "{} has no failed test run to send. `amux verify {} --test` runs them.",
            run.id,
            run.id
        );
    };
    let log = std::fs::read(runs::run_dir(&run.id).join("verify.log")).unwrap_or_default();
    let tail = log_tail(&log, 6000);
    let msg = format!(
        "The tests failed when I ran them ({}). The end of the output:\n\n```\n{tail}\n```\n\nPlease find the cause and fix it, then tell me when the tests should pass.",
        v.detail
    );
    crate::runs::client::send_text(run, &msg, true)?;
    record(&run.id, "test", "sent", &v.detail, false);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{write, TempDir};

    #[test]
    fn guesses_the_test_command_from_project_files() {
        let t = TempDir::new();
        let d = t.path();
        assert_eq!(guess_test_command(d), None);
        write(
            &d.join("package.json"),
            r#"{"scripts":{"test":"echo \"Error: no test specified\" && exit 1"}}"#,
        );
        assert_eq!(guess_test_command(d), None);
        write(
            &d.join("package.json"),
            r#"{"scripts":{"test":"vitest run"}}"#,
        );
        assert_eq!(guess_test_command(d).as_deref(), Some("npm test"));
        write(&d.join("pnpm-lock.yaml"), "");
        assert_eq!(guess_test_command(d).as_deref(), Some("pnpm test"));

        let t = TempDir::new();
        let d = t.path();
        write(&d.join("Makefile"), "build:\n\tcc x.c\ntest:\n\t./t\n");
        assert_eq!(guess_test_command(d).as_deref(), Some("make test"));
        write(&d.join("Cargo.toml"), "");
        assert_eq!(guess_test_command(d).as_deref(), Some("cargo test"));

        // Python: pytest when it's configured, plain unittest when there are only tests.
        let t = TempDir::new();
        let d = t.path();
        write(&d.join("calc.py"), "");
        assert_eq!(guess_test_command(d), None);
        write(&d.join("tests/test_calc.py"), "");
        assert_eq!(
            guess_test_command(d).as_deref(),
            Some("python3 -m unittest")
        );
        write(&d.join("pyproject.toml"), "");
        assert_eq!(guess_test_command(d).as_deref(), Some("python3 -m pytest"));
    }

    #[test]
    fn log_tail_is_plain_text_from_a_line_start() {
        let log = b"ok 1\nprogress 10%\rprogress 100%\n\x1b[31mFAIL\x1b[0m a.rs\n";
        assert_eq!(log_tail(log, 1000), "ok 1\nprogress 100%\nFAIL a.rs");
        assert_eq!(log_tail(log, 12), "FAIL a.rs");
    }

    #[test]
    fn labels_and_recommendations() {
        let v = Verify {
            kind: "test".into(),
            state: "passed".into(),
            ..Default::default()
        };
        assert_eq!(v.label(), "tests passed");
        let v = Verify {
            kind: "test".into(),
            state: "running".into(),
            pid: Some(std::process::id()),
            ..Default::default()
        };
        assert_eq!(v.label(), "tests running");
        let gone = Verify {
            pid: Some(u32::MAX - 1),
            ..v
        };
        assert_eq!(gone.label(), "tests stopped");

        let mut p = Plan {
            test: Some("npm test".into()),
            branch: Some("amux/a".into()),
            remote: Some("origin".into()),
            ..Default::default()
        };
        assert!(!p.recommend_push());
        p.busy = Some("all 2 test slots are taken".into());
        assert!(p.recommend_push());
        p.remote = None;
        assert!(!p.recommend_push(), "nothing to push to");
        p.dirty = 2;
        assert!(p.lines().iter().any(|l| l.contains("2 uncommitted files")));
    }
}
