//! End to end: runs the built `amux` binary against a sandboxed HOME, with a fake
//! provider whose "CLI" prints the args and environment amux gives it.
#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

struct Sandbox {
    home: PathBuf,
    amux_home: PathBuf,
    shared: PathBuf,
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.home);
    }
}

struct Out {
    code: i32,
    out: String,
    err: String,
}

impl Sandbox {
    fn new(name: &str) -> Self {
        let home = std::env::temp_dir().join(format!("amux-e2e-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(&home).unwrap();
        let home = home.canonicalize().unwrap();
        let amux_home = home.join(".amux");
        let shared = home.join(".fake");
        let bin = home.join("bin/fake-cli");
        write(
            &bin,
            "#!/bin/sh\nfor a in \"$@\"; do echo \"ARG $a\"; done\n\
             for k in FAKE_HOME FAKE_BASE_URL FAKE_TELEMETRY AMUX_ACCOUNT FAKE_TOKEN AMUX_RUN; do eval \"v=\\${$k-<unset>}\"; echo \"ENV $k=$v\"; done\n\
             [ -t 0 ] && echo TTY yes\n\
             [ -n \"$FAKE_ASK\" ] && echo \"Approve this? [y/n]\"\n\
             [ -n \"$FAKE_7501\" ] && st() { printf '\\033]7501;state=%s\\033\\\\' \"$1\"; } \\
               && printf '\\033]7501;?\\033\\\\' && sleep 0.3 \\
               && st \"blocked:kind=permission:msg=$(printf 'approve Write: a.rs' | base64)\" && read a \\
               && st \"working:msg=$(printf 'writing a.rs' | base64)\" && read b && st done && read c && exit 3\n\
             [ -n \"$FAKE_CHAT\" ] && while read l; do echo \"got $l\"; [ \"$l\" = ask ] && echo \"Approve this? [y/n]\"; [ \"$l\" = quit ] && exit 7; done\n\
             exit 0\n",
        );
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
        write(&shared.join("settings.json"), "{}");
        let config = serde_json::json!({ "providers": [{
            "id": "fake", "label": "Fake CLI", "bin": bin, "sharedHome": shared, "homeEnv": "FAKE_HOME",
            "privateEntries": ["auth.json"], "sharedDirs": ["sessions"], "baseUrlEnv": "FAKE_BASE_URL",
            "tokenEnv": "FAKE_TOKEN", "privacyEnv": { "FAKE_TELEMETRY": "0" },
            "waitingPatterns": ["approve THIS?"]
        }], "notifyCommand": ["sh", "-c", "echo \"$AMUX_EVENT $AMUX_RUN $AMUX_MESSAGE\" >> \"$HOME/notes\""]});
        write(&amux_home.join("config.json"), &config.to_string());
        Sandbox {
            home,
            amux_home,
            shared,
        }
    }

    fn amux(&self, args: &[&str]) -> Out {
        self.amux_with(args, &[], None)
    }

    /// Runs amux with extra env vars, in `dir` when given.
    fn amux_with(&self, args: &[&str], env: &[(&str, &str)], dir: Option<&Path>) -> Out {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_amux"));
        cmd.args(args)
            .env("HOME", &self.home)
            .env("AMUX_HOME", &self.amux_home)
            .env("AMUX_NO_UPDATE_CHECK", "1")
            .env("FAKE_TOKEN", "inherited-from-shell")
            .envs(env.iter().copied());
        if let Some(d) = dir {
            cmd.current_dir(d);
        }
        let r = cmd.output().unwrap();
        Out {
            code: r.status.code().unwrap_or(-1),
            out: String::from_utf8_lossy(&r.stdout).into(),
            err: String::from_utf8_lossy(&r.stderr).into(),
        }
    }

    fn overlay(&self, name: &str) -> PathBuf {
        self.amux_home
            .join("accounts/fake")
            .join(name)
            .join(".fake")
    }
}

fn write(file: &Path, content: &str) {
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    fs::write(file, content).unwrap();
}

fn lines(out: &str, prefix: &str) -> Vec<String> {
    out.lines()
        .filter_map(|l| l.strip_prefix(prefix))
        .map(str::to_string)
        .collect()
}

#[test]
fn prints_version_and_help() {
    let s = Sandbox::new("help");
    let v = s.amux(&["--version"]);
    assert_eq!(v.out, format!("{}\n", env!("CARGO_PKG_VERSION")));
    let help = s.amux(&["help"]);
    assert_eq!(help.code, 0);
    assert!(help.out.contains("amux account add"));
}

#[test]
fn account_lifecycle() {
    let s = Sandbox::new("lifecycle");

    // Add with privacy and a gateway, then list it.
    let add = s.amux(&[
        "account",
        "add",
        "fake",
        "one",
        "--no-login",
        "--privacy",
        "--base-url",
        "https://gw.example.com/",
    ]);
    assert_eq!(add.code, 0, "{}", add.err);
    assert!(add.out.contains("Created fake@one"));
    assert_eq!(
        fs::read_link(s.overlay("one").join("settings.json")).unwrap(),
        s.shared.join("settings.json")
    );
    let list = s.amux(&["account", "list"]);
    assert!(
        list.out
            .contains("fake@one  login  on       https://gw.example.com"),
        "{}",
        list.out
    );
    assert_eq!(
        s.amux(&["account", "add", "fake", "one", "--no-login"])
            .code,
        1,
        "duplicates are rejected"
    );

    // Run the CLI as the account, passing unknown args through.
    assert_eq!(s.amux(&["account", "default", "fake@one"]).code, 0);
    let r = s.amux(&["fake", "--model", "opus", "hello world"]);
    assert_eq!(r.code, 0, "{}", r.err);
    assert_eq!(lines(&r.out, "ARG "), ["--model", "opus", "hello world"]);
    assert_eq!(
        lines(&r.out, "ENV "),
        [
            format!("FAKE_HOME={}", s.overlay("one").display()),
            "FAKE_BASE_URL=https://gw.example.com".into(),
            "FAKE_TELEMETRY=0".into(),
            "AMUX_ACCOUNT=fake@one".into(),
            "FAKE_TOKEN=<unset>".into(),
            "AMUX_RUN=<unset>".into(),
        ]
    );
    let mode = fs::metadata(s.amux_home.join("launches.jsonl"))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600, "launch log is private");

    // Shell exports, including privacy settings, and unsets for inherited account vars.
    let env = s.amux(&["env", "fake@one"]).out;
    assert!(
        env.contains(&format!(
            "export FAKE_HOME='{}'",
            s.overlay("one").display()
        )),
        "{env}"
    );
    assert!(env.contains("export FAKE_TELEMETRY='0'"));
    assert!(env.contains("export FAKE_BASE_URL='https://gw.example.com'"));
    assert!(env.contains("unset FAKE_TOKEN"));

    // Clear the gateway and privacy settings.
    assert_eq!(s.amux(&["account", "baseurl", "fake@one", "off"]).code, 0);
    assert_eq!(s.amux(&["account", "privacy", "fake@one", "off"]).code, 0);
    let r = s.amux(&["run", "fake@one"]);
    assert!(lines(&r.out, "ENV ").contains(&"FAKE_BASE_URL=<unset>".to_string()));
    assert!(lines(&r.out, "ENV ").contains(&"FAKE_TELEMETRY=<unset>".to_string()));

    // Errors exit non-zero with a useful message.
    let r = s.amux(&["nope"]);
    assert_eq!(r.code, 1);
    assert!(r.err.contains("Unknown command \"nope\""));
    assert!(s
        .amux(&["account", "add", "fake", "bad name", "--no-login"])
        .err
        .contains("Account names may use"));
    assert!(s
        .amux(&["account", "baseurl", "fake@one", "ftp://x"])
        .err
        .contains("http or https"));
    assert!(s
        .amux(&["sessions", "-n", "0"])
        .err
        .contains("positive whole number"));

    // Remove with --purge: the overlay goes, the shared home stays.
    let r = s.amux(&["account", "rm", "fake@one", "--purge"]);
    assert_eq!(r.code, 0, "{}", r.err);
    assert!(!s.amux_home.join("accounts/fake/one").exists());
    assert!(s.shared.join("settings.json").exists());
    assert!(s.amux(&["account", "list"]).out.contains("No accounts yet"));
}

#[test]
fn resolves_accounts_by_default_or_single_account() {
    let s = Sandbox::new("resolve");
    s.amux(&["account", "add", "fake", "a", "--no-login"]);
    assert!(
        lines(&s.amux(&["fake"]).out, "ENV ").contains(&"AMUX_ACCOUNT=fake@a".to_string()),
        "the only account is used"
    );
    s.amux(&["account", "add", "fake", "b", "--no-login"]);
    let r = s.amux(&["fake"]);
    assert_eq!(r.code, 1);
    assert!(r.err.contains("ambiguous"), "{}", r.err);
    assert!(s
        .amux(&["fake@nope"])
        .err
        .contains("No account \"fake@nope\""));
    s.amux(&["account", "default", "fake@b"]);
    assert!(lines(&s.amux(&["fake"]).out, "ENV ").contains(&"AMUX_ACCOUNT=fake@b".to_string()));
}

#[test]
fn reports_an_unparseable_config() {
    let s = Sandbox::new("badconfig");
    write(&s.amux_home.join("config.json"), "{ not json");
    let r = s.amux(&["providers"]);
    assert_eq!(r.code, 1);
    assert!(r.err.contains("Cannot parse"), "{}", r.err);
}

#[test]
fn rejects_account_entries_that_escape_the_accounts_dir() {
    let s = Sandbox::new("traversal");
    write(
        &s.amux_home.join("accounts.json"),
        r#"{"accounts": [{"provider": "fake", "name": "../../victim", "auth": "login"}]}"#,
    );
    let victim = s.amux_home.join("victim");
    fs::create_dir_all(&victim).unwrap();
    let r = s.amux(&["account", "rm", "fake@../../victim", "--purge"]);
    assert_eq!(r.code, 1);
    assert!(r.err.contains("Invalid account"), "{}", r.err);
    assert!(victim.exists());
}

#[test]
fn keeps_unknown_top_level_keys_in_accounts_json() {
    let s = Sandbox::new("extra");
    write(
        &s.amux_home.join("accounts.json"),
        r#"{"accounts": [], "future": {"x": 1}}"#,
    );
    assert_eq!(
        s.amux(&["account", "add", "fake", "a", "--no-login"]).code,
        0
    );
    let saved: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(s.amux_home.join("accounts.json")).unwrap())
            .unwrap();
    assert_eq!(saved["future"]["x"], 1);
    assert_eq!(saved["accounts"][0]["name"], "a");
}

/// Polls `amux ps` until the run's row contains `want`.
fn wait_ps(s: &Sandbox, run: &str, want: &str) -> String {
    for _ in 0..100 {
        let ps = s.amux(&["ps"]).out;
        if ps.lines().any(|l| l.starts_with(run) && l.contains(want)) {
            return ps;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    panic!("{run} never showed {want:?}:\n{}", s.amux(&["ps"]).out);
}

#[test]
fn spawns_a_background_run_in_a_terminal() {
    let s = Sandbox::new("spawn");
    s.amux(&["account", "add", "fake", "one", "--no-login"]);
    let r = s.amux(&["spawn", "fake", "--model", "x"]);
    assert_eq!(r.code, 0, "{}", r.err);
    assert!(r.err.contains("started fake-1 as fake@one"), "{}", r.err);

    let ps = wait_ps(&s, "fake-1", "exited 0");
    assert!(ps.contains("--model x"), "{ps}");
    let logs = s.amux(&["logs", "fake-1"]).out;
    assert!(logs.contains("ARG --model"), "{logs}");
    assert!(logs.contains("ENV AMUX_ACCOUNT=fake@one"), "{logs}");
    assert!(logs.contains("ENV AMUX_RUN=fake-1"), "{logs}");
    assert!(logs.contains("ENV FAKE_TOKEN=<unset>"), "{logs}");
    assert!(
        logs.contains("TTY yes"),
        "the CLI runs on a terminal: {logs}"
    );

    let a = s.amux(&["attach", "fake-1"]);
    assert!(a.err.contains("exited with code 0"), "{}", a.err);
    assert_eq!(s.amux(&["spawn", "fake"]).code, 0);
    wait_ps(&s, "fake-2", "exited");
    assert!(s.amux(&["rm", "--done"]).out.contains("Removed fake-1."));
    assert!(s.amux(&["ps"]).out.contains("No runs"));
}

#[test]
fn attaches_to_a_running_cli_and_stops_it() {
    let s = Sandbox::new("attach");
    s.amux(&["account", "add", "fake", "one", "--no-login"]);
    let chat = [("FAKE_CHAT", "1")];
    let r = s.amux_with(&["spawn", "--name", "chat", "fake"], &chat, None);
    assert_eq!(r.code, 0, "{}", r.err);
    wait_ps(&s, "chat", "running");
    assert!(s.amux(&["rm", "chat"]).err.contains("still running"));

    // Piped input reaches the CLI, and the exit code comes back when it ends.
    let mut child = Command::new(env!("CARGO_BIN_EXE_amux"))
        .args(["attach", "ch"])
        .env("HOME", &s.home)
        .env("AMUX_HOME", &s.amux_home)
        .env("AMUX_NO_UPDATE_CHECK", "1")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write;
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"hello\nquit\n")
        .unwrap();
    let out = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("got hello"), "{stdout}");
    assert_eq!(out.status.code(), Some(7));
    wait_ps(&s, "chat", "exited 7");

    // stop ends a run that is waiting for input.
    s.amux_with(&["spawn", "--name", "idle", "fake"], &chat, None);
    wait_ps(&s, "idle", "running");
    let stop = s.amux(&["stop", "idle"]);
    assert!(
        stop.out.contains("Stopped idle (exited 143)"),
        "{}",
        stop.out
    );
}

#[test]
fn gives_a_run_its_own_worktree() {
    let s = Sandbox::new("worktree");
    s.amux(&["account", "add", "fake", "one", "--no-login"]);
    let repo = s.home.join("repo");
    fs::create_dir_all(repo.join("src")).unwrap();
    write(&repo.join("src/main.txt"), "x");
    let git = |args: &[&str]| {
        let ok = Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
            .args(args)
            .output()
            .unwrap()
            .status
            .success();
        assert!(ok, "git {args:?}");
    };
    git(&["init", "-q"]);
    git(&["add", "."]);
    git(&["commit", "-qm", "init"]);

    let r = s.amux_with(
        &["spawn", "--worktree", "fake"],
        &[],
        Some(&repo.join("src")),
    );
    assert_eq!(r.code, 0, "{}", r.err);
    let wt = s.amux_home.join("worktrees/repo-fake-1");
    assert!(r.err.contains("(branch amux/fake-1)"), "{}", r.err);
    wait_ps(&s, "fake-1", "exited 0");
    // Started in the same subdirectory of the new checkout.
    let run: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(s.amux_home.join("runs/fake-1/run.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(run["cwd"], wt.join("src").to_string_lossy().as_ref());

    // A clean worktree is removed with the run; the branch stays.
    let rm = s.amux(&["rm", "fake-1"]);
    assert!(rm.out.contains("kept branch amux/fake-1"), "{}", rm.out);
    assert!(!wt.exists());

    // Outside a repository, --worktree fails and leaves nothing behind.
    let bad = s.amux_with(&["spawn", "--worktree", "fake"], &[], Some(&s.home));
    assert!(bad.err.contains("needs a git repository"), "{}", bad.err);
    assert!(s.amux(&["ps"]).out.contains("No runs"));
}

#[test]
fn shows_when_a_run_waits_for_you_and_notifies() {
    let s = Sandbox::new("waiting");
    s.amux(&["account", "add", "fake", "one", "--no-login"]);
    let env = [("FAKE_CHAT", "1"), ("FAKE_ASK", "1")];
    assert_eq!(
        s.amux_with(&["spawn", "--name", "ask", "fake"], &env, None)
            .code,
        0
    );
    wait_ps(&s, "ask", "waiting");
    let notes = s.home.join("notes");
    let mut text = String::new();
    for _ in 0..100 {
        text = fs::read_to_string(&notes).unwrap_or_default();
        if !text.is_empty() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert_eq!(text, "waiting ask Waiting for you: approve THIS?\n");

    // Answering clears it; with a terminal attached, the exit is not announced.
    let mut child = Command::new(env!("CARGO_BIN_EXE_amux"))
        .args(["attach", "ask"])
        .env("HOME", &s.home)
        .env("AMUX_HOME", &s.amux_home)
        .env("AMUX_NO_UPDATE_CHECK", "1")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write;
    child.stdin.as_mut().unwrap().write_all(b"y\n").unwrap();
    wait_ps(&s, "ask", "running");
    child.stdin.as_mut().unwrap().write_all(b"quit\n").unwrap();
    assert_eq!(child.wait_with_output().unwrap().status.code(), Some(7));
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert!(!fs::read_to_string(&notes).unwrap().contains("exited"));

    // Detached, the exit is announced.
    s.amux(&["spawn", "--name", "quick", "fake"]);
    wait_ps(&s, "quick", "exited 0");
    for _ in 0..100 {
        if fs::read_to_string(&notes)
            .unwrap()
            .contains("exited quick Exited with code 0")
        {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    panic!("no exit notice: {}", fs::read_to_string(&notes).unwrap());
}

/// Polls until `f` holds, for up to five seconds.
fn eventually(what: &str, mut f: impl FnMut() -> bool) {
    for _ in 0..100 {
        if f() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    panic!("timed out waiting for {what}");
}

#[test]
fn ui_dump_shows_runs_their_state_and_a_live_preview() {
    let s = Sandbox::new("ui");
    let empty = s.amux(&["ui", "--dump", "--size", "10x80"]);
    assert_eq!(empty.code, 0, "{}", empty.err);
    assert!(empty.out.contains("No runs yet."), "{}", empty.out);

    s.amux(&["account", "add", "fake", "one", "--no-login"]);
    let env = [("FAKE_CHAT", "1"), ("FAKE_ASK", "1")];
    s.amux_with(&["spawn", "--name", "ask", "fake", "hi"], &env, None);
    s.amux(&["spawn", "--name", "quick", "fake"]);
    wait_ps(&s, "ask", "waiting");
    wait_ps(&s, "quick", "exited 0");

    let ui = s.amux(&["ui", "--dump", "--size", "24x120", "--select", "ask"]);
    assert_eq!(ui.code, 0, "{}", ui.err);
    let out = ui.out;
    assert!(out.contains("1 waiting · 1 done"), "{out}");
    assert!(out.contains("ask") && out.contains("quick"), "{out}");
    // The preview is the run's screen, through a terminal emulator.
    assert!(out.contains("ARG hi"), "{out}");
    assert!(out.contains("Approve this? [y/n]"), "{out}");
    assert!(out.contains("◆ approve THIS?"), "{out}");
    // A finished run's preview comes from its log.
    let done = s
        .amux(&["ui", "--dump", "--size", "24x120", "--select", "qu"])
        .out;
    assert!(done.contains("ENV AMUX_RUN=quick"), "{done}");

    // The command palette, completing a run name.
    let pal = s
        .amux(&["ui", "--dump", "--size", "24x120", "--keys", "/stop q"])
        .out;
    assert!(
        pal.contains("Commands") && pal.contains("› /stop q"),
        "{pal}"
    );
    assert!(pal.contains("quick"), "{pal}");
    let pal = s
        .amux(&["ui", "--dump", "--size", "24x120", "--keys", "/swi"])
        .out;
    assert!(pal.contains("/switch-account"), "{pal}");
    assert!(s.amux(&["ui", "--bogus"]).err.contains("Unknown option"));
    let tty = s.amux(&["ui"]);
    assert_eq!(tty.code, 1);
    assert!(tty.err.contains("needs a terminal"), "{}", tty.err);
    // f shows only waiting runs; Esc shows all again.
    let only = s
        .amux(&["ui", "--dump", "--size", "24x120", "--keys", "f"])
        .out;
    assert!(
        only.contains("showing waiting") && only.contains("Runs 1 of 2"),
        "{only}"
    );
    assert!(only.contains("ask") && !only.contains("quick"), "{only}");
    let all = s
        .amux(&["ui", "--dump", "--size", "24x120", "--keys", "f\x1b"])
        .out;
    assert!(all.contains("quick") && !all.contains("showing"), "{all}");

    // Why ask is waiting: a prompt on its screen, with the screen as evidence.
    let why = s.amux(&["explain", "ask"]);
    assert_eq!(why.code, 0, "{}", why.err);
    assert!(why.out.starts_with("ask: waiting"), "{}", why.out);
    assert!(
        why.out.contains("Its screen shows “approve THIS?”"),
        "{}",
        why.out
    );
    assert!(
        why.out.contains("hasn't reported its own state"),
        "{}",
        why.out
    );
    assert!(why.out.contains("│ Approve this? [y/n]"), "{}", why.out);
    let j: serde_json::Value =
        serde_json::from_str(&s.amux(&["explain", "ask", "--json"]).out).unwrap();
    assert_eq!(
        (j["source"].as_str(), j["reports"].as_bool()),
        (Some("pattern"), Some(false))
    );
    assert!(s.amux(&["explain", "nope"]).err.contains("No run matches"));
    // In the UI, e shows the same explanation, whole.
    let ui = s
        .amux(&[
            "ui", "--dump", "--size", "40x120", "--select", "ask", "--keys", "e",
        ])
        .out;
    assert!(ui.contains("Why ask is waiting"), "{ui}");
    assert!(
        ui.contains("How amux reads this run") && ui.contains("Bottom of its screen"),
        "{ui}"
    );

    let help = s.amux(&["ui", "--help"]);
    assert_eq!(help.code, 0);
    assert!(help.out.starts_with("amux ui ["), "{}", help.out);

    // A run you stopped shows as stopped, not failed.
    s.amux(&["stop", "ask"]);
    wait_ps(&s, "ask", "exited 143");
    let ui = s
        .amux(&["ui", "--dump", "--size", "24x120", "--select", "ask"])
        .out;
    assert!(ui.contains("1 stopped") && ui.contains("■ ask"), "{ui}");
    assert!(!ui.contains("failed"), "{ui}");
}

#[test]
fn a_watching_client_does_not_silence_notifications() {
    use std::io::Write;
    use std::os::unix::net::UnixStream;
    let s = Sandbox::new("watch");
    s.amux(&["account", "add", "fake", "one", "--no-login"]);
    s.amux_with(
        &["spawn", "--name", "w", "fake"],
        &[("FAKE_CHAT", "1")],
        None,
    );
    wait_ps(&s, "w", "running");
    // Socket paths are limited to about 100 bytes; reach it through a short link.
    let link = PathBuf::from(format!("/tmp/amux-e2e-{}-w", std::process::id()));
    let _ = fs::remove_file(&link);
    std::os::unix::fs::symlink(s.amux_home.join("runs/w"), &link).unwrap();
    let mut c = UnixStream::connect(link.join("sock")).unwrap();
    let frame = |kind: u8, data: &[u8]| {
        let mut v = vec![kind];
        v.extend_from_slice(&(data.len() as u32).to_be_bytes());
        v.extend_from_slice(data);
        v
    };
    let notes = s.home.join("notes");
    let waiting = || s.amux(&["ps"]).out.contains("waiting");

    // An attached client sees the question itself: no notification.
    c.write_all(&frame(0, b"ask\n")).unwrap();
    eventually("waiting", waiting);
    std::thread::sleep(std::time::Duration::from_millis(200));
    assert!(!notes.exists());
    c.write_all(&frame(0, b"y\n")).unwrap();
    eventually("answered", || !waiting());

    // A watching client (a preview in `amux ui`) doesn't count as attached.
    c.write_all(&frame(2, &[1])).unwrap();
    c.write_all(&frame(0, b"ask\n")).unwrap();
    eventually("a notification", || {
        fs::read_to_string(&notes).is_ok_and(|t| t.contains("waiting w"))
    });
    let _ = fs::remove_file(&link);
    s.amux(&["stop", "w"]);
}

#[test]
fn reads_the_state_a_cli_reports_with_osc_7501() {
    use std::io::Write;
    let s = Sandbox::new("osc7501");
    s.amux(&["account", "add", "fake", "one", "--no-login"]);
    let r = s.amux_with(
        &["spawn", "--name", "st", "fake"],
        &[("FAKE_7501", "1")],
        None,
    );
    assert_eq!(r.code, 0, "{}", r.err);

    // The CLI reports it is blocked on a permission: waiting, with its own message.
    wait_ps(&s, "st", "waiting");
    let notes = s.home.join("notes");
    eventually("a notification", || {
        fs::read_to_string(&notes).is_ok_and(|t| !t.is_empty())
    });
    assert_eq!(
        fs::read_to_string(&notes).unwrap(),
        "waiting st Waiting for you: approve Write: a.rs\n"
    );
    let run_json = || -> serde_json::Value {
        serde_json::from_str(&fs::read_to_string(s.amux_home.join("runs/st/run.json")).unwrap())
            .unwrap()
    };
    assert_eq!(run_json()["status"]["state"], "blocked");
    assert_eq!(run_json()["status"]["kind"], "permission");
    assert_eq!(run_json()["waiting"]["source"], "report");
    let why = s.amux(&["explain", "st"]).out;
    assert!(
        why.contains(
            "The CLI said so itself (OSC 7501): it is blocked (permission): “approve Write: a.rs”"
        ),
        "{why}"
    );
    assert!(
        why.contains("reports its own state (OSC 7501), so its reports decide"),
        "{why}"
    );
    // The host answered the probe. The fake CLI's terminal echoes input, so the answer
    // shows up in its output.
    let log = fs::read_to_string(s.amux_home.join("runs/st/output.log")).unwrap();
    assert!(
        log.contains(
            "7501;?:states=idle,working,done,blocked,error:kinds=permission,question,auth"
        ),
        "{log}"
    );

    let mut child = Command::new(env!("CARGO_BIN_EXE_amux"))
        .args(["attach", "st"])
        .env("HOME", &s.home)
        .env("AMUX_HOME", &s.amux_home)
        .env("AMUX_NO_UPDATE_CHECK", "1")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    // Approved: it reports working, and that ends the wait.
    stdin.write_all(b"y\n").unwrap();
    wait_ps(&s, "st", "running");
    assert_eq!(run_json()["status"]["message"], "writing a.rs");
    assert!(run_json().get("waiting").is_none());
    let ui = s
        .amux(&["ui", "--dump", "--size", "20x120", "--select", "st"])
        .out;
    assert!(ui.contains(" writing a.rs "), "{ui}");
    // The turn is done: idle, however recently it printed.
    stdin.write_all(b"n\n").unwrap();
    wait_ps(&s, "st", "idle");
    stdin.write_all(b"q\n").unwrap();
    drop(stdin);
    assert_eq!(child.wait_with_output().unwrap().status.code(), Some(3));
    wait_ps(&s, "st", "exited 3");
    // done outlives the process; working and blocked would not.
    assert_eq!(run_json()["status"]["state"], "done");
}

/// Polls `amux peek` until the run's screen contains `want`.
fn wait_peek(s: &Sandbox, run: &str, want: &str) -> String {
    for _ in 0..200 {
        let out = s.amux(&["peek", run]).out;
        if out.contains(want) {
            return out;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    panic!(
        "{run} never showed {want:?}:\n{}",
        s.amux(&["peek", run]).out
    );
}

#[test]
fn shows_the_real_screen_of_a_cli_that_redraws_in_place() {
    let s = Sandbox::new("screen");
    s.amux(&["account", "add", "fake", "one", "--no-login"]);
    // Like Claude Code: a status area rewritten in place with relative cursor moves, far
    // more than 256 KB of it, so no slice of the output redraws the screen correctly.
    let bin = s.home.join("bin/fake-cli");
    write(
        &bin,
        "#!/bin/sh\nprintf 'TITLE fake\\r\\n'\ni=0\nwhile [ $i -lt 4000 ]; do\n\
         printf '\\033[2K\\rworking on step %d ................................................\\r\\n\\033[2K\\r  tokens %d\\033[1A\\r' $i $i\n\
         i=$((i+1)); done\n\
         printf '\\033[2K\\rall steps done\\r\\n\\033[2K\\r  tokens 4000\\r\\n> ready\\r\\n'\nread x\n",
    );
    assert_eq!(s.amux(&["spawn", "--name", "tui", "fake"]).code, 0);
    let expected = "TITLE fake\nall steps done\n  tokens 4000\n> ready\n";
    assert_eq!(wait_peek(&s, "tui", "> ready"), expected);
    let log = fs::metadata(s.amux_home.join("runs/tui/output.log"))
        .unwrap()
        .len();
    assert!(log > 256 * 1024, "the log is only {log} bytes");

    // The UI's preview gets the same screen from the host.
    let ui = s.amux(&["ui", "--dump", "--size", "20x100", "--select", "tui"]);
    for line in ["TITLE fake", "all steps done", "  tokens 4000", "> ready"] {
        assert!(
            ui.out.contains(&format!("│{line} ")),
            "{line:?} in:\n{}",
            ui.out
        );
    }
    assert!(!ui.out.contains("working on step"), "{}", ui.out);

    // After it ends, its last screen is kept.
    let mut child = Command::new(env!("CARGO_BIN_EXE_amux"))
        .args(["attach", "tui"])
        .env("HOME", &s.home)
        .env("AMUX_HOME", &s.amux_home)
        .env("AMUX_NO_UPDATE_CHECK", "1")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    use std::io::Write;
    child.stdin.as_mut().unwrap().write_all(b"bye\n").unwrap();
    assert_eq!(child.wait().unwrap().code(), Some(0));
    wait_ps(&s, "tui", "exited 0");
    assert!(s.amux_home.join("runs/tui/screen.ans").exists());
    assert!(s
        .amux(&["peek", "tui"])
        .out
        .starts_with("TITLE fake\nall steps done\n  tokens 4000\n> ready\n"));
}
