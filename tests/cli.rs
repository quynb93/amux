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
             [ -n \"$FAKE_PERM\" ] && stty -icanon -echo && for c in 'cargo test --all' 'git push origin main'; do \
               printf '%s\\n' '────────────────' ' Bash command' ' approve this?' '╌╌╌╌╌╌╌╌╌╌' \" $c\" '╌╌╌╌╌╌╌╌╌╌' ' Do you want to proceed?' ' ❯ 1. Yes' '   2. No'; \
               a=$(dd bs=1 count=1 2>/dev/null); echo \"answer $a to $c\"; done\n\
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
        }], "limits": { "maxLoad": 1000 }, "notifyCommand": ["sh", "-c", "echo \"$AMUX_EVENT $AMUX_RUN $AMUX_MESSAGE\" >> \"$HOME/notes\""]});
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

#[test]
fn queues_runs_past_the_limit_and_starts_them_when_one_ends() {
    let s = Sandbox::new("queue");
    let cfg = s.amux_home.join("config.json");
    let mut c: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&cfg).unwrap()).unwrap();
    c["limits"]["runs"] = 1.into();
    write(&cfg, &c.to_string());
    s.amux(&["account", "add", "fake", "one", "--no-login"]);
    let chat = [("FAKE_CHAT", "1")];
    let a = s.amux_with(&["spawn", "--name", "a", "fake"], &chat, None);
    assert_eq!(a.code, 0, "{}", a.err);
    let b = s.amux_with(&["spawn", "--name", "b", "fake"], &chat, None);
    assert_eq!(b.code, 0, "{}", b.err);
    assert!(b.err.contains("queued b"), "{}", b.err);
    assert!(b.err.contains("all 1 runs slot"), "{}", b.err);
    wait_ps(&s, "b", "queued");
    assert!(s.amux(&["attach", "b"]).err.contains("queued"));
    let slots = s.amux(&["slots"]).out;
    assert!(slots.contains("waiting: b"), "{slots}");

    assert_eq!(s.amux(&["stop", "a"]).code, 0);
    wait_ps(&s, "b", "running");
    assert_eq!(s.amux(&["stop", "b"]).code, 0);
}

#[test]
fn a_queued_run_can_be_stopped_before_it_starts() {
    let s = Sandbox::new("queue-stop");
    let cfg = s.amux_home.join("config.json");
    let mut c: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&cfg).unwrap()).unwrap();
    c["limits"]["runs"] = 1.into();
    write(&cfg, &c.to_string());
    s.amux(&["account", "add", "fake", "one", "--no-login"]);
    let chat = [("FAKE_CHAT", "1")];
    s.amux_with(&["spawn", "--name", "a", "fake"], &chat, None);
    s.amux_with(&["spawn", "--name", "b", "fake"], &chat, None);
    wait_ps(&s, "b", "queued");
    assert_eq!(s.amux(&["stop", "b"]).code, 0);
    wait_ps(&s, "b", "exited 143");
    s.amux(&["stop", "a"]);
    assert_eq!(s.amux(&["rm", "--done"]).code, 0);
}

#[test]
fn slot_runs_the_command_and_gives_up_with_75_when_full() {
    let s = Sandbox::new("slot");
    let cfg = s.amux_home.join("config.json");
    let mut c: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&cfg).unwrap()).unwrap();
    c["limits"]["test"] = 1.into();
    write(&cfg, &c.to_string());
    let r = s.amux(&["slot", "--", "sh", "-c", "exit 4"]);
    assert_eq!(r.code, 4, "the command's own exit code: {}", r.err);
    // Hold the only test slot from a background command, then ask for another.
    let mut holder = Command::new(env!("CARGO_BIN_EXE_amux"))
        .args(["slot", "--class", "test", "--", "sleep", "2"])
        .env("HOME", &s.home)
        .env("AMUX_HOME", &s.amux_home)
        .env("AMUX_NO_UPDATE_CHECK", "1")
        .spawn()
        .unwrap();
    for _ in 0..100 {
        if s.amux(&["slots"]).out.contains("sleep 2") {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let r = s.amux(&["slot", "--class", "test", "--no-wait", "--", "true"]);
    assert_eq!(r.code, 75, "{}", r.err);
    assert!(r.err.contains("sleep 2"), "{}", r.err);
    assert!(r.err.contains("push"), "{}", r.err);
    // A waiter gets the slot once the holder is done.
    let r = s.amux(&["slot", "--class", "test", "--timeout", "10", "--", "true"]);
    assert_eq!(r.code, 0, "{}", r.err);
    assert!(r.err.contains("waiting for a test slot"), "{}", r.err);
    holder.wait().unwrap();
    assert_eq!(
        s.amux(&["slot", "--class", "test", "--no-wait", "--", "true"])
            .code,
        0
    );
}

#[test]
fn verifies_a_run_by_testing_here_or_pushing_for_ci() {
    let s = Sandbox::new("verify");
    s.amux(&["account", "add", "fake", "one", "--no-login"]);
    let repo = s.home.join("repo");
    let remote = s.home.join("remote.git");
    fs::create_dir_all(&repo).unwrap();
    write(&repo.join("Makefile"), "test:\n\techo tests-ran-here\n");
    let git = |dir: &Path, args: &[&str]| {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    git(&s.home, &["init", "-q", "--bare", remote.to_str().unwrap()]);
    git(&repo, &["init", "-q"]);
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "init"]);
    git(
        &repo,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );

    let r = s.amux_with(
        &["spawn", "--name", "v", "--worktree", "fake"],
        &[],
        Some(&repo),
    );
    assert_eq!(r.code, 0, "{}", r.err);
    wait_ps(&s, "v", "exited 0");

    let plan = s.amux(&["verify", "v"]);
    assert_eq!(plan.code, 0, "{}", plan.err);
    assert!(plan.out.contains("Tests: make test"), "{}", plan.out);
    assert!(
        plan.out.contains("Push: amux/v to origin (new branch)"),
        "{}",
        plan.out
    );
    assert!(plan.out.contains("amux verify v --test"), "{}", plan.out);

    let t = s.amux(&["verify", "v", "--test"]);
    assert_eq!(t.code, 0, "{}{}", t.out, t.err);
    assert!(t.out.contains("Tests passed for v"), "{}", t.out);
    let log = fs::read_to_string(s.amux_home.join("runs/v/verify.log")).unwrap();
    assert!(log.contains("tests-ran-here"), "{log}");
    let failing = s.amux(&["verify", "v", "--test", "--command", "exit 3"]);
    assert_eq!(failing.code, 3);
    let state = fs::read_to_string(s.amux_home.join("runs/v/verify.json")).unwrap();
    assert!(state.contains("\"failed\""), "{state}");

    let p = s.amux(&["verify", "v", "--push"]);
    assert_eq!(p.code, 0, "{}", p.err);
    assert!(p.out.contains("Pushed amux/v to origin"), "{}", p.out);
    assert!(git(&remote, &["branch"]).contains("amux/v"));
    assert!(s
        .amux(&["verify", "v"])
        .out
        .contains("(already up to date)"));
    let ui = s.amux(&["ui", "--dump", "--size", "30x120", "--keys", "v"]);
    assert!(ui.out.contains("pushed"), "{}", ui.out);
    assert!(ui.out.contains("Verify v:"), "{}", ui.out);
    assert!(ui.out.contains("t test (make test)"), "{}", ui.out);
}

#[test]
fn drives_a_run_with_send_wait_diff_and_failure_feedback() {
    let s = Sandbox::new("drive");
    s.amux(&["account", "add", "fake", "one", "--no-login"]);
    let repo = s.home.join("repo");
    fs::create_dir_all(&repo).unwrap();
    write(&repo.join("a.txt"), "one\n");
    let git = |args: &[&str]| {
        let out = Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}");
    };
    git(&["init", "-q"]);
    git(&["add", "."]);
    git(&["commit", "-qm", "init"]);
    let chat = [("FAKE_CHAT", "1")];
    let r = s.amux_with(
        &["spawn", "--name", "d", "--worktree", "fake"],
        &chat,
        Some(&repo),
    );
    assert_eq!(r.code, 0, "{}", r.err);
    wait_ps(&s, "d", "running");

    // send types a line and presses Enter; the fake CLI echoes it.
    assert_eq!(s.amux(&["send", "d", "hello", "there"]).code, 0);
    let mut seen = String::new();
    for _ in 0..100 {
        seen = s.amux(&["peek", "d"]).out;
        if seen.contains("got hello there") {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(seen.contains("got hello there"), "{seen}");

    // wait: "ask" makes the CLI ask something, so the run turns to waiting.
    assert_eq!(s.amux(&["send", "d", "ask"]).code, 0);
    let w = s.amux(&["wait", "d", "--until", "waiting", "--timeout", "10"]);
    assert_eq!(w.code, 0, "{}{}", w.out, w.err);
    assert!(w.out.starts_with("d\twaiting"), "{}", w.out);
    let w = s.amux(&["wait", "d", "--until", "exited", "--timeout", "0.3"]);
    assert_eq!(w.code, 124, "a live run times out");

    let ps = s.amux(&["ps", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&ps.out).unwrap();
    assert_eq!(v[0]["id"], "d");
    assert_eq!(v[0]["state"], "attention");
    assert_eq!(v[0]["branch"], "amux/d");

    // diff shows committed and uncommitted changes since the worktree started.
    let wt = s.amux_home.join("worktrees/repo-d");
    write(&wt.join("a.txt"), "one\ntwo\n");
    write(&wt.join("new.txt"), "x");
    let d = s.amux(&["diff", "d"]);
    assert_eq!(d.code, 0, "{}", d.err);
    assert!(d.out.contains("+two"), "{}", d.out);
    assert!(
        d.out
            .contains("New files not yet added to git (1): new.txt"),
        "{}",
        d.out
    );

    // A failed test run goes back to the agent as one message.
    let f = s.amux(&[
        "verify",
        "d",
        "--test",
        "--command",
        "echo boom-failure; exit 2",
    ]);
    assert_eq!(f.code, 2);
    let sent = s.amux(&["verify", "d", "--send-failures"]);
    assert_eq!(sent.code, 0, "{}", sent.err);
    for _ in 0..100 {
        seen = s.amux(&["logs", "d"]).out;
        if seen.contains("got The tests failed") {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(seen.contains("got The tests failed"), "{seen}");
    assert!(seen.contains("boom-failure"), "{seen}");
    assert!(s.amux(&["ps", "--json"]).out.contains("\"sent\""));

    assert_eq!(s.amux(&["send", "d", "quit"]).code, 0);
    let w = s.amux(&["wait", "d", "--until", "exited", "--timeout", "10"]);
    assert_eq!(w.code, 0, "{}", w.out);
    assert!(s.amux(&["send", "d", "x"]).err.contains("not running"));
}

#[test]
fn opens_a_pr_and_sends_failing_ci_to_the_agent() {
    let s = Sandbox::new("github");
    s.amux(&["account", "add", "fake", "one", "--no-login"]);
    let repo = s.home.join("repo");
    let remote = s.home.join("remote.git");
    fs::create_dir_all(&repo).unwrap();
    write(&repo.join("a.txt"), "x");
    let git = |dir: &Path, args: &[&str]| {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}");
    };
    git(&s.home, &["init", "-q", "--bare", remote.to_str().unwrap()]);
    git(&repo, &["init", "-q"]);
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "init"]);
    git(
        &repo,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    // A stand-in for the GitHub CLI: a PR exists once created, and its CI fails.
    let gh = s.home.join("bin/gh");
    let state = s.home.join("gh-pr");
    write(
        &gh,
        &format!(
            "#!/bin/sh\necho \"$@\" >> {log}\ncase \"$1 $2\" in\n\
             'pr view') [ -f {st} ] && echo '{{\"url\":\"https://github.com/o/r/pull/7\",\"number\":7,\"state\":\"OPEN\"}}' && exit 0; echo 'no pull requests found for branch' >&2; exit 1;;\n\
             'pr create') touch {st}; echo https://github.com/o/r/pull/7;;\n\
             'pr checks') echo '[{{\"name\":\"test\",\"bucket\":\"fail\",\"link\":\"https://github.com/o/r/actions/runs/42/job/1\",\"workflow\":\"CI\"}},{{\"name\":\"lint\",\"bucket\":\"pass\",\"link\":\"\",\"workflow\":\"CI\"}}]'; exit 1;;\n\
             'run view') echo 'test\tRun tests\tassertion failed: parser-broke';;\n\
             esac\n",
            log = s.home.join("gh-log").display(),
            st = state.display()
        ),
    );
    fs::set_permissions(&gh, fs::Permissions::from_mode(0o755)).unwrap();
    let env = [("FAKE_CHAT", "1"), ("AMUX_GH", gh.to_str().unwrap())];
    let r = s.amux_with(
        &["spawn", "--name", "g", "--worktree", "fake"],
        &env,
        Some(&repo),
    );
    assert_eq!(r.code, 0, "{}", r.err);
    wait_ps(&s, "g", "running");

    let none = s.amux_with(&["checks", "g"], &env, None);
    assert!(none.err.contains("no pull request yet"), "{}", none.err);
    let pr = s.amux_with(&["pr", "g", "--draft"], &env, None);
    assert_eq!(pr.code, 0, "{}", pr.err);
    assert!(pr.out.contains("Opened PR #7"), "{}", pr.out);
    let log = fs::read_to_string(s.home.join("gh-log")).unwrap();
    assert!(
        log.contains("pr create --head amux/g --fill --draft"),
        "{log}"
    );
    assert!(s
        .amux_with(&["pr", "g"], &env, None)
        .out
        .contains("already has PR #7"));

    let c = s.amux_with(&["checks", "g"], &env, None);
    assert_eq!(c.code, 1, "failing checks: {}", c.err);
    assert!(
        c.out.contains("checks failing") && c.out.contains("✗ CI / test"),
        "{}",
        c.out
    );
    let ps = s.amux(&["ps", "--json"]).out;
    assert!(ps.contains("\"checks\": \"failing\""), "{ps}");

    let sent = s.amux_with(&["checks", "g", "--send-failures"], &env, None);
    assert_eq!(sent.code, 0, "{}", sent.err);
    let mut seen = String::new();
    for _ in 0..100 {
        seen = s.amux(&["logs", "g"]).out;
        if seen.contains("parser-broke") {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(seen.contains("got CI failed on PR #7"), "{seen}");
    assert!(seen.contains("parser-broke"), "{seen}");
    let ui = s.amux(&["ui", "--dump", "--size", "30x140"]).out;
    assert!(ui.contains("PR #7 ✗ 1 fai"), "{ui}");
    s.amux(&["stop", "g"]);
}

#[test]
fn reviews_a_runs_changes_and_sends_line_comments() {
    let s = Sandbox::new("review");
    s.amux(&["account", "add", "fake", "one", "--no-login"]);
    let repo = s.home.join("repo");
    fs::create_dir_all(&repo).unwrap();
    write(&repo.join("a.rs"), "one\ntwo\nthree\n");
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
    let chat = [("FAKE_CHAT", "1")];
    let r = s.amux_with(
        &["spawn", "--name", "rv", "--worktree", "fake"],
        &chat,
        Some(&repo),
    );
    assert_eq!(r.code, 0, "{}", r.err);
    wait_ps(&s, "rv", "running");
    let wt = s.amux_home.join("worktrees/repo-rv");
    write(&wt.join("a.rs"), "one\nTWO\nthree\n");

    // From the CLI: the line's text is kept with the comment.
    let c = s.amux(&["comment", "rv", "a.rs:2", "Why", "uppercase?"]);
    assert_eq!(c.code, 0, "{}", c.err);
    assert!(s.amux(&["comment", "rv", "../x:1", "no"]).code != 0);
    // From the UI: c opens the review, ⏎ comments on the line under the cursor.
    let ui = s.amux(&[
        "ui",
        "--dump",
        "--size",
        "30x120",
        "--keys",
        "c\nkeep three\n",
    ]);
    assert!(
        ui.out.contains("Review rv · 1 file · 2 comments to send"),
        "{}",
        ui.out
    );
    assert!(ui.out.contains("+TWO"), "{}", ui.out);
    let list = s.amux(&["comments", "rv"]).out;
    assert!(
        list.contains("1. a.rs:2  [pending]\n    > TWO\n    Why uppercase?"),
        "{list}"
    );
    assert!(
        list.contains("2. a.rs:1  [pending]") && list.contains("keep three"),
        "{list}"
    );

    let sent = s.amux(&["comments", "rv", "--send"]);
    assert_eq!(sent.code, 0, "{}", sent.err);
    assert!(sent.out.contains("Sent 2 comments to rv."), "{}", sent.out);
    let mut seen = String::new();
    for _ in 0..100 {
        seen = s.amux(&["logs", "rv"]).out;
        if seen.contains("got Review of your changes: 2 comments.") {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(
        seen.contains("got Review of your changes: 2 comments."),
        "{seen}"
    );
    assert!(seen.contains("a.rs:2"), "{seen}");
    assert!(s
        .amux(&["comments", "rv", "--send"])
        .err
        .contains("no comments waiting"));
    assert_eq!(s.amux(&["comments", "rv", "--rm", "1"]).code, 0);
    s.amux(&["stop", "rv"]);
}

#[test]
fn streams_events_and_shows_board_and_tiles() {
    use std::io::{BufRead, BufReader};
    let s = Sandbox::new("events");
    s.amux(&["account", "add", "fake", "one", "--no-login"]);
    let mut ev = Command::new(env!("CARGO_BIN_EXE_amux"))
        .args(["events"])
        .env("HOME", &s.home)
        .env("AMUX_HOME", &s.amux_home)
        .env("AMUX_NO_UPDATE_CHECK", "1")
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(400));
    let env = [("FAKE_CHAT", "1")];
    assert_eq!(
        s.amux_with(&["spawn", "--name", "e", "fake"], &env, None)
            .code,
        0
    );
    wait_ps(&s, "e", "running");
    s.amux(&["send", "e", "ask"]);
    let mut lines = BufReader::new(ev.stdout.take().unwrap()).lines();
    let mut got = vec![];
    for line in lines.by_ref() {
        let line = line.unwrap();
        let waiting = line.contains("\"to\":\"waiting\"");
        got.push(line);
        if waiting {
            break;
        }
    }
    ev.kill().unwrap();
    ev.wait().unwrap();
    assert!(
        got[0].contains("\"event\":\"added\"") && got[0].contains("\"run\":\"e\""),
        "{got:?}"
    );
    let w: serde_json::Value = serde_json::from_str(got.last().unwrap()).unwrap();
    assert_eq!(w["event"], "state");
    assert_eq!(w["reason"], "approve THIS?");

    let board = s
        .amux(&["ui", "--dump", "--size", "20x120", "--keys", "b"])
        .out;
    assert!(
        board.contains("Needs you 1") && board.contains("Working 0"),
        "{board}"
    );
    let tiles = s
        .amux(&["ui", "--dump", "--size", "20x120", "--keys", "t"])
        .out;
    assert!(tiles.contains("╭ e  fake@one"), "{tiles}");
    s.amux(&["stop", "e"]);
}

#[test]
fn runs_and_drives_agents_on_another_machine_over_ssh() {
    let s = Sandbox::new("remote-local");
    let r = Sandbox::new("remote-box");
    r.amux(&["account", "add", "fake", "one", "--no-login"]);
    // A stand-in for ssh: runs the remote command as "the other machine" (the second
    // sandbox), the way sshd runs it in the user's shell there.
    let ssh = s.home.join("bin/ssh");
    write(
        &ssh,
        &format!(
            "#!/bin/sh\nfor a; do last=\"$a\"; done\nexport HOME={h} AMUX_HOME={a}\nexec sh -c \"$last\"\n",
            h = r.home.display(),
            a = r.amux_home.display()
        ),
    );
    fs::set_permissions(&ssh, fs::Permissions::from_mode(0o755)).unwrap();
    let env = [("AMUX_SSH", ssh.to_str().unwrap()), ("FAKE_CHAT", "1")];
    let bin = env!("CARGO_BIN_EXE_amux");
    let add = s.amux_with(
        &["machine", "add", "box", "me@box", "--amux", bin],
        &env,
        None,
    );
    assert_eq!(add.code, 0, "{}{}", add.out, add.err);
    assert!(add.out.contains("box: amux"), "{}", add.out);
    assert!(s.amux(&["machine", "add", "bad", "-oProxyCommand=x"]).code != 0);

    let sp = s.amux_with(
        &["spawn", "--on", "box:~", "--name", "w", "fake"],
        &env,
        None,
    );
    assert_eq!(sp.code, 0, "{}", sp.err);
    let mut ps = String::new();
    for _ in 0..100 {
        ps = s.amux_with(&["ps"], &env, None).out;
        if ps.contains("box:w") && ps.contains("running") {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(ps.contains("box:w") && ps.contains("running"), "{ps}");
    assert!(
        r.amux(&["ps"]).out.lines().any(|l| l.starts_with("w ")),
        "it runs on the box"
    );

    assert_eq!(
        s.amux_with(&["send", "box:w", "hello box"], &env, None)
            .code,
        0
    );
    let mut seen = String::new();
    for _ in 0..100 {
        seen = s.amux_with(&["peek", "box:w"], &env, None).out;
        if seen.contains("got hello box") {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(seen.contains("got hello box"), "{seen}");
    // The UI shows the remote run, and its live screen through the SSH pipe.
    let ui = s.amux_with(
        &["ui", "--dump", "--size", "24x120", "--select", "box:w"],
        &env,
        None,
    );
    assert!(ui.out.contains("box:w"), "{}{}", ui.out, ui.err);
    assert!(ui.out.contains("got hello box"), "{}", ui.out);
    assert_eq!(s.amux_with(&["stop", "box:w"], &env, None).code, 0);
    assert!(s.amux_with(&["peek", "nobox:w"], &env, None).code != 0);
}

/// One HTTP/1.1 request; returns the status line, headers and body as one string.
fn http(addr: &str, req: &str) -> String {
    use std::io::{Read, Write};
    let mut s = std::net::TcpStream::connect(addr).unwrap();
    s.write_all(req.as_bytes()).unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).unwrap();
    out
}

#[test]
fn serves_a_phone_app_behind_a_token() {
    use std::io::{BufRead, BufReader};
    let s = Sandbox::new("serve");
    s.amux(&["account", "add", "fake", "one", "--no-login"]);
    let env = [("FAKE_CHAT", "1")];
    assert_eq!(
        s.amux_with(&["spawn", "--name", "p", "fake"], &env, None)
            .code,
        0
    );
    wait_ps(&s, "p", "running");
    let mut srv = Command::new(env!("CARGO_BIN_EXE_amux"))
        .args(["serve", "--listen", "127.0.0.1:0"])
        .env("HOME", &s.home)
        .env("AMUX_HOME", &s.amux_home)
        .env("AMUX_NO_UPDATE_CHECK", "1")
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut url = String::new();
    for line in BufReader::new(srv.stdout.take().unwrap()).lines() {
        let line = line.unwrap();
        if let Some(u) = line.trim().strip_prefix("http://") {
            url = u.to_string();
            break;
        }
    }
    let (addr, query) = url.split_once('/').unwrap();
    let token = query.trim_start_matches("?t=");
    assert!(token.len() >= 32, "{url}");

    let get = |path: &str, cookie: &str| {
        http(
            addr,
            &format!("GET {path} HTTP/1.1\r\nHost: x\r\nCookie: {cookie}\r\n\r\n"),
        )
    };
    assert!(get("/api/runs", "").starts_with("HTTP/1.1 401"));
    assert!(get("/api/runs", "amux=wrong").starts_with("HTTP/1.1 401"));
    let login = get(&format!("/?t={token}"), "");
    assert!(
        login.starts_with("HTTP/1.1 303") && login.contains("HttpOnly; SameSite=Strict"),
        "{login}"
    );
    let cookie = format!("amux={token}");
    let runs = get("/api/runs", &cookie);
    assert!(
        runs.starts_with("HTTP/1.1 200") && runs.contains("\"id\": \"p\""),
        "{runs}"
    );
    assert!(get("/", &cookie).contains("<title>amux</title>"));
    assert!(get("/", &cookie).contains("Content-Security-Policy: default-src 'self'"));

    let post = |path: &str, body: &str, extra: &str| {
        http(
            addr,
            &format!(
                "POST {path} HTTP/1.1\r\nHost: x\r\nCookie: {cookie}\r\nContent-Type: application/json\r\n{extra}Content-Length: {}\r\n\r\n{body}",
                body.len()
            ),
        )
    };
    let body = r#"{"text":"from my phone"}"#;
    assert!(
        post("/api/runs/p/send", body, "").starts_with("HTTP/1.1 403"),
        "needs X-Amux"
    );
    let sent = post("/api/runs/p/send", body, "X-Amux: 1\r\n");
    assert!(sent.starts_with("HTTP/1.1 200"), "{sent}");
    assert!(
        post("/api/runs/p/key", r#"{"key":"rm -rf"}"#, "X-Amux: 1\r\n").starts_with("HTTP/1.1 400")
    );
    assert!(post("/api/runs/..%2Fx/stop", "{}", "X-Amux: 1\r\n").starts_with("HTTP/1.1 400"));
    let mut screen = String::new();
    for _ in 0..100 {
        screen = get("/api/runs/p/screen", &cookie);
        if screen.contains("got from my phone") {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(screen.contains("got from my phone"), "{screen}");
    // Everything else the app does: accounts, load, starting a run, live events, comments.
    assert!(get("/api/accounts", &cookie).contains("\"id\": \"fake@one\""));
    assert!(get("/api/slots", &cookie).contains("\"classes\""));
    let events = {
        use std::io::{Read, Write};
        let mut es = std::net::TcpStream::connect(addr).unwrap();
        es.write_all(
            format!("GET /api/events HTTP/1.1\r\nHost: x\r\nCookie: {cookie}\r\n\r\n").as_bytes(),
        )
        .unwrap();
        es.set_read_timeout(Some(std::time::Duration::from_secs(10)))
            .unwrap();
        let mut head = [0u8; 256];
        let n = es.read(&mut head).unwrap();
        assert!(String::from_utf8_lossy(&head[..n]).contains("text/event-stream"));
        std::thread::sleep(std::time::Duration::from_millis(600));
        es
    };
    let started = post(
        "/api/spawn",
        r#"{"account":"fake@one","name":"q","prompt":"hello from the phone"}"#,
        "X-Amux: 1\r\n",
    );
    assert!(
        started.starts_with("HTTP/1.1 200") && started.contains("started q"),
        "{started}"
    );
    assert!(post("/api/spawn", r#"{"account":"--x"}"#, "X-Amux: 1\r\n").starts_with("HTTP/1.1 400"));
    {
        use std::io::Read;
        let mut es = events;
        let mut got = String::new();
        let mut buf = [0u8; 4096];
        while !got.contains("\"run\":\"q\"") {
            let n = es.read(&mut buf).unwrap();
            assert!(n > 0, "the stream ended: {got}");
            got.push_str(&String::from_utf8_lossy(&buf[..n]));
        }
        assert!(got.contains("data: {"), "{got}");
    }
    assert!(!s.amux(&["ps"]).out.contains("--prompt"));
    let c = post(
        "/api/runs/p/comment",
        r#"{"file":"a.rs","line":3,"note":"from the phone"}"#,
        "X-Amux: 1\r\n",
    );
    assert!(c.starts_with("HTTP/1.1 200"), "{c}");
    assert!(get("/api/runs/p/comments", &cookie).contains("from the phone"));
    assert!(post("/api/runs/p/stop", "{}", "X-Amux: 1\r\n").starts_with("HTTP/1.1 200"));
    s.amux(&["stop", "q"]);
    srv.kill().unwrap();
    srv.wait().unwrap();
}

#[test]
fn plugins_hooks_and_the_agent_skill() {
    let s = Sandbox::new("plugins");
    s.amux(&["account", "add", "fake", "one", "--no-login"]);
    // A plugin: `amux plugin new`, then `amux <name>` runs it with AMUX_BIN set.
    let made = s.amux(&["plugin", "new", "hello"]);
    assert_eq!(made.code, 0, "{}", made.err);
    let p = s.amux_home.join("plugins/hello");
    write(
        &p,
        "#!/bin/sh\n# amux: says hello\necho \"hello $1 via $(basename \"$AMUX_BIN\")\"\n",
    );
    fs::set_permissions(&p, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(s
        .amux(&["plugin", "list"])
        .out
        .contains("hello            says hello"));
    assert_eq!(
        s.amux(&["hello", "world"]).out.trim(),
        "hello world via amux"
    );
    // Others can't change what amux runs: a group-writable plugin is ignored.
    fs::set_permissions(&p, fs::Permissions::from_mode(0o770)).unwrap();
    assert!(s.amux(&["hello"]).err.contains("Unknown command"));

    // A hook runs when a run exits.
    let hook = s.amux_home.join("hooks/on-exited");
    write(
        &hook,
        &format!(
            "#!/bin/sh\necho \"$AMUX_EVENT $AMUX_RUN $AMUX_MESSAGE\" >> {}\n",
            s.home.join("hooked").display()
        ),
    );
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(s.amux(&["spawn", "--name", "h", "fake"]).code, 0);
    wait_ps(&s, "h", "exited 0");
    let mut hooked = String::new();
    for _ in 0..100 {
        hooked = fs::read_to_string(s.home.join("hooked")).unwrap_or_default();
        if !hooked.is_empty() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert_eq!(hooked.trim(), "exited h 0");

    // The skill installs into the shared home that every account sees.
    assert!(s.amux(&["skill"]).out.starts_with("---\nname: amux"));
    let claude_home = s.home.join(".claude");
    fs::create_dir_all(&claude_home).unwrap();
    let i = s.amux(&["skill", "--install"]);
    assert_eq!(i.code, 0, "{}", i.err);
    assert!(
        claude_home.join("skills/amux/SKILL.md").exists(),
        "{}",
        i.out
    );
}

#[test]
fn finds_overlapping_runs_and_lands_them_one_at_a_time() {
    let s = Sandbox::new("land");
    s.amux(&["account", "add", "fake", "one", "--no-login"]);
    let repo = s.home.join("repo");
    fs::create_dir_all(&repo).unwrap();
    write(&repo.join("f.txt"), "one\ntwo\nthree\n");
    write(&repo.join("g.txt"), "g\n");
    let git = |dir: &Path, args: &[&str]| -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "init"]);
    for n in ["a", "b", "c"] {
        let r = s.amux_with(
            &["spawn", "--name", n, "--worktree", "fake"],
            &[],
            Some(&repo),
        );
        assert_eq!(r.code, 0, "{}", r.err);
    }
    let wt = |n: &str| s.amux_home.join(format!("worktrees/repo-{n}"));
    // a and b change the same line: a committed, b not yet. c changes another file.
    write(&wt("a").join("f.txt"), "one\nTWO from a\nthree\n");
    git(&wt("a"), &["commit", "-qam", "a"]);
    write(&wt("b").join("f.txt"), "one\nTWO from b\nthree\n");
    write(&wt("c").join("g.txt"), "g from c\n");
    git(&wt("c"), &["commit", "-qam", "c"]);

    let o = s.amux(&["overlap"]);
    assert_eq!(o.code, 0, "{}", o.err);
    assert!(o.out.contains("✗ a ↔ b: conflict in f.txt"), "{}", o.out);
    assert!(
        !o.out.contains("c ↔"),
        "c touches nothing the others do: {}",
        o.out
    );
    assert_eq!(s.amux(&["overlap", "--check"]).code, 1);
    assert_eq!(
        git(&wt("b"), &["status", "--porcelain"]).trim(),
        "M f.txt",
        "nothing changed on disk"
    );

    // a and c land, c rebased onto a; b is skipped until it commits, then conflicts.
    // Landing rebases, which writes commits as someone (CI machines have no git identity).
    let who = [
        ("GIT_AUTHOR_NAME", "t"),
        ("GIT_AUTHOR_EMAIL", "t@example.com"),
        ("GIT_COMMITTER_NAME", "t"),
        ("GIT_COMMITTER_EMAIL", "t@example.com"),
    ];
    let l = s.amux_with(&["land", "a", "c", "--no-test"], &who, None);
    assert_eq!(l.code, 0, "{}{}", l.out, l.err);
    assert!(l.out.contains("landed"), "{}", l.out);
    assert_eq!(
        fs::read_to_string(repo.join("f.txt")).unwrap(),
        "one\nTWO from a\nthree\n"
    );
    assert_eq!(
        fs::read_to_string(repo.join("g.txt")).unwrap(),
        "g from c\n"
    );
    let b = s.amux_with(&["land", "b", "--no-test"], &who, None);
    assert_eq!(b.code, 1);
    assert!(b.out.contains("uncommitted"), "{}", b.out);
    git(&wt("b"), &["commit", "-qam", "b"]);
    let b = s.amux_with(&["land", "b", "--no-test", "--json"], &who, None);
    let v: serde_json::Value = serde_json::from_str(&b.out).unwrap();
    assert_eq!(v[0]["outcome"], "conflict");
    assert!(v[0]["detail"].as_str().unwrap().contains("f.txt"));
    assert_eq!(
        fs::read_to_string(repo.join("f.txt")).unwrap(),
        "one\nTWO from a\nthree\n",
        "main untouched"
    );
    assert!(s.amux(&["ps", "--json"]).out.contains("\"kind\": \"land\""));
}

#[test]
fn spots_usage_limits_and_moves_the_session_to_another_account() {
    let s = Sandbox::new("limits");
    // A CLI that says it hit its limit when asked to, keeps Claude-style sessions in the
    // shared home, and resumes one with --resume <id>.
    let bin = s.home.join("bin/limit-cli");
    write(
        &bin,
        "#!/bin/sh\necho \"ARGS $*\"\nwhile read l; do echo \"got $l\"; \
         [ \"$l\" = limit ] && echo \"You've hit your limit · resets 7pm (Asia/Saigon) /upgrade\"; done\n",
    );
    fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
    let cfg = s.amux_home.join("config.json");
    let mut c: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&cfg).unwrap()).unwrap();
    c["providers"][0]["bin"] = bin.to_str().unwrap().into();
    c["providers"][0]["limitPatterns"] = serde_json::json!(["You've hit your limit"]);
    c["providers"][0]["resumeArgs"] = serde_json::json!(["--resume", "{id}"]);
    c["providers"][0]["sessions"] = "claude".into();
    c["providers"][0]["sharedDirs"] = serde_json::json!(["projects"]);
    write(&cfg, &c.to_string());
    s.amux(&["account", "add", "fake", "one", "--no-login"]);
    s.amux(&["account", "add", "fake", "two", "--no-login"]);
    s.amux(&["account", "default", "fake@one"]);
    let r = s.amux(&["spawn", "--name", "lim", "fake"]);
    assert_eq!(r.code, 0, "{}", r.err);
    wait_ps(&s, "lim", "running");
    // The session it is having, as Claude Code would have written it, in the run's folder.
    let started: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(s.amux_home.join("runs/lim/run.json")).unwrap())
            .unwrap();
    let cwd = started["cwd"].as_str().unwrap().to_string();
    let sid = "5e55a0b1-0000-4000-8000-000000000001";
    write(
        &s.shared.join(format!("projects/-x/{sid}.jsonl")),
        &format!(
            "{}\n",
            serde_json::json!({"type": "user", "cwd": cwd, "sessionId": sid,
                "message": {"role": "user", "content": "build the login page"}})
        ),
    );

    assert_eq!(s.amux(&["send", "lim", "limit"]).code, 0);
    wait_ps(&s, "lim", "waiting");
    let run: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(s.amux_home.join("runs/lim/run.json")).unwrap())
            .unwrap();
    assert_eq!(run["waiting"]["source"], "limit");
    assert_eq!(
        run["waiting"]["reason"],
        "You've hit your limit · resets 7pm (Asia/Saigon)"
    );
    let u = s.amux(&["usage"]).out;
    assert!(
        u.contains("fake@one") && u.contains("limited, resets in"),
        "{u}"
    );

    // A new run that names no account skips the limited one.
    let r = s.amux(&["spawn", "--name", "next", "fake"]);
    assert!(
        r.err.contains("fake@one is limited") && r.err.contains("using fake@two"),
        "{}",
        r.err
    );

    // Failover: the same session, under the other account, told to carry on.
    let f = s.amux(&["failover", "lim"]);
    assert_eq!(f.code, 0, "{}{}", f.out, f.err);
    assert!(
        f.out.contains("lim continues as lim-2 under fake@two"),
        "{}",
        f.out
    );
    let ps = s.amux(&["ps"]).out;
    assert!(ps.contains(&format!("--resume {sid}")), "{ps}");
    let mut log = String::new();
    for _ in 0..100 {
        log = s.amux(&["logs", "lim-2"]).out;
        if log.contains("got The previous account hit its usage limit") {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(
        log.contains("got The previous account hit its usage limit"),
        "{log}"
    );
    assert_eq!(s.amux(&["usage", "--clear", "fake@one"]).code, 0);
    assert!(
        s.amux(&["usage"]).out.contains("fake@one     ok")
            || !s.amux(&["usage"]).out.contains("limited, resets in")
    );
    for r in ["lim-2", "next"] {
        s.amux(&["stop", r]);
    }
}

#[test]
fn flags_a_run_that_works_without_changing_anything_and_cleans_up_leftovers() {
    let s = Sandbox::new("guards");
    s.amux(&["account", "add", "fake", "one", "--no-login"]);
    let repo = s.home.join("repo");
    fs::create_dir_all(&repo).unwrap();
    write(&repo.join("a.txt"), "a\n");
    for args in [
        &["init", "-q"][..],
        &["add", "."],
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@x",
            "commit",
            "-qm",
            "i",
        ],
    ] {
        assert!(Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(args)
            .status()
            .unwrap()
            .success());
    }
    // Busy forever, changing nothing.
    let busy = s.home.join("bin/busy");
    write(
        &busy,
        "#!/bin/sh\nwhile true; do echo thinking; sleep 0.2; done\n",
    );
    fs::set_permissions(&busy, fs::Permissions::from_mode(0o755)).unwrap();
    let cfg = s.amux_home.join("config.json");
    let mut c: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&cfg).unwrap()).unwrap();
    c["providers"][0]["bin"] = busy.to_str().unwrap().into();
    write(&cfg, &c.to_string());
    let tick = [("AMUX_GUARD_TICK_MS", "200")];
    let r = s.amux_with(
        &[
            "spawn",
            "--name",
            "loop",
            "--worktree",
            "--stall",
            "1s",
            "fake",
        ],
        &tick,
        Some(&repo),
    );
    assert_eq!(r.code, 0, "{}", r.err);
    wait_ps(&s, "loop", "waiting");
    let run = || -> serde_json::Value {
        serde_json::from_str(&fs::read_to_string(s.amux_home.join("runs/loop/run.json")).unwrap())
            .unwrap()
    };
    assert_eq!(run()["waiting"]["source"], "stall");
    assert!(run()["waiting"]["reason"]
        .as_str()
        .unwrap()
        .starts_with("stalled: working"));
    assert!(s
        .amux(&["explain", "loop"])
        .out
        .contains("watchdog flagged it"));
    // Its files change: the flag goes.
    write(&s.amux_home.join("worktrees/repo-loop/a.txt"), "changed\n");
    wait_ps(&s, "loop", "running");
    assert!(run().get("waiting").is_none());
    s.amux(&["stop", "loop"]);

    // A CLI that starts a server in the background and exits leaves it running.
    let leaky = s.home.join("bin/leaky");
    let up = s.home.join("daemon-up");
    write(
        &leaky,
        // Detached into a session of its own, as daemons and some dev servers do, so the
        // end of the run's terminal doesn't take it along. The CLI exits only once the
        // daemon has detached (it says so with a file), or a slow machine could end the
        // terminal first and take the daemon with it.
        &format!(
            "#!/bin/sh\nperl -e 'use POSIX; fork and exit; setsid(); open STDIN, \"</dev/null\"; open STDOUT, \">/dev/null\"; open STDERR, \">&STDOUT\"; open F, \">{up}\"; close F; sleep 345' &\n\
             echo started a server\nfor i in $(seq 1 100); do [ -f '{up}' ] && break; sleep 0.1; done\n",
            up = up.display()
        ),
    );
    fs::set_permissions(&leaky, fs::Permissions::from_mode(0o755)).unwrap();
    c["providers"][0]["bin"] = leaky.to_str().unwrap().into();
    write(&cfg, &c.to_string());
    let r = s.amux_with(
        &["spawn", "--name", "leak", "--worktree", "fake"],
        &[],
        Some(&repo),
    );
    assert_eq!(r.code, 0, "{}", r.err);
    wait_ps(&s, "leak", "exited 0");
    // The daemon detaches in the background; give it a moment to be there.
    let mut dry = s.amux(&["gc", "--dry-run"]);
    for _ in 0..50 {
        if dry.out.contains("leak") {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
        dry = s.amux(&["gc", "--dry-run"]);
    }
    // On failure, show what the system says about the leftover.
    let evidence = || {
        let ps = Command::new("sh")
            .arg("-c")
            .arg("ps -A -o pid=,pgid=,args= | grep 'sleep 345' | grep -v grep; for p in $(pgrep -f 'sleep 345'); do echo \"env $p:\"; tr '\\0' '\\n' < /proc/$p/environ 2>&1 | grep AMUX; ls -l /proc/$p/cwd 2>&1; done")
            .output()
            .unwrap();
        String::from_utf8_lossy(&ps.stdout).into_owned()
    };
    assert!(
        dry.out.contains("leak") && dry.out.contains("perl -e use POSIX"),
        "{}\n--- the system says:\n{}",
        dry.out,
        evidence()
    );
    let gc = s.amux(&["gc"]);
    assert!(gc.out.contains("Ended 1 process"), "{}", gc.out);
    assert!(s.amux(&["gc"]).out.contains("Nothing left behind"));
}

#[test]
fn tries_a_task_several_times_compares_and_keeps_the_best() {
    let s = Sandbox::new("attempts");
    s.amux(&["account", "add", "fake", "one", "--no-login"]);
    let repo = s.home.join("repo");
    fs::create_dir_all(&repo).unwrap();
    write(&repo.join("a.txt"), "broken\n");
    write(&repo.join("Makefile"), "test:\n\tgrep -q fixed a.txt\n");
    let git = |dir: &Path, args: &[&str]| {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "init"]);
    let t = s.amux_with(
        &["try", "3", "fake", "-p", "fix a.txt", "--name", "fix"],
        &[],
        Some(&repo),
    );
    assert_eq!(t.code, 0, "{}", t.err);
    for n in ["fix-1", "fix-2", "fix-3"] {
        wait_ps(&s, n, "exited");
    }
    let wt = |n: &str| s.amux_home.join(format!("worktrees/repo-{n}"));
    // fix-1: small and right. fix-2: right, but touches more. fix-3: wrong.
    write(&wt("fix-1").join("a.txt"), "fixed\n");
    git(&wt("fix-1"), &["commit", "-qam", "fix"]);
    write(&wt("fix-2").join("a.txt"), "fixed\n");
    write(&wt("fix-2").join("extra.txt"), "1\n2\n3\n");
    git(&wt("fix-2"), &["add", "."]);
    git(&wt("fix-2"), &["commit", "-qm", "fix and more"]);
    write(&wt("fix-3").join("a.txt"), "still broken\n");

    let c = s.amux(&["compare", "fix"]);
    assert_eq!(c.code, 0, "{}", c.err);
    let first = c.out.lines().nth(1).unwrap_or_default();
    assert!(
        first.starts_with("★") && first.contains("fix-1") && first.contains("passed"),
        "{}",
        c.out
    );
    let j: serde_json::Value =
        serde_json::from_str(&s.amux(&["compare", "fix", "--json"]).out).unwrap();
    let order: Vec<&str> = j
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["run"].as_str().unwrap())
        .collect();
    assert_eq!(order, ["fix-1", "fix-2", "fix-3"]);
    assert_eq!(j[2]["tests"], "failed");

    let who = [
        ("GIT_AUTHOR_NAME", "t"),
        ("GIT_AUTHOR_EMAIL", "t@example.com"),
        ("GIT_COMMITTER_NAME", "t"),
        ("GIT_COMMITTER_EMAIL", "t@example.com"),
    ];
    let p = s.amux_with(&["pick", "fix-1", "--land"], &who, None);
    assert_eq!(p.code, 0, "{}{}", p.out, p.err);
    assert!(p.out.contains("fix-1: landed"), "{}", p.out);
    assert_eq!(fs::read_to_string(repo.join("a.txt")).unwrap(), "fixed\n");
    let ps = s.amux(&["ps"]).out;
    assert!(!ps.contains("fix-2"), "clean losers are gone: {ps}");
    assert!(
        p.out.contains("kept worktree") || ps.contains("fix-3"),
        "uncommitted work is never deleted: {}{ps}",
        p.out
    );
}

#[test]
fn shares_notes_ports_and_env_files_between_parallel_runs() {
    let s = Sandbox::new("share");
    s.amux(&["account", "add", "fake", "one", "--no-login"]);
    let repo = s.home.join("repo");
    fs::create_dir_all(&repo).unwrap();
    write(&repo.join("a.txt"), "a\n");
    write(&repo.join(".gitignore"), ".env\n");
    write(&repo.join(".env"), "API_KEY=local\n");
    for args in [
        &["init", "-q"][..],
        &["add", "."],
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@x",
            "commit",
            "-qm",
            "i",
        ],
    ] {
        assert!(Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(args)
            .status()
            .unwrap()
            .success());
    }
    let cli = s.home.join("bin/env-cli");
    write(
        &cli,
        "#!/bin/sh\nfor a in \"$@\"; do echo \"ARG $a\"; done\necho \"PORT=$PORT COMPOSE=$COMPOSE_PROJECT_NAME\"\n\
         while read l; do echo \"got $l\"; done\n",
    );
    fs::set_permissions(&cli, fs::Permissions::from_mode(0o755)).unwrap();
    let cfg = s.amux_home.join("config.json");
    let mut c: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&cfg).unwrap()).unwrap();
    c["providers"][0]["bin"] = cli.to_str().unwrap().into();
    write(&cfg, &c.to_string());

    let n = s.amux_with(&["note", "use", "zod,", "not", "yup"], &[], Some(&repo));
    assert_eq!(n.code, 0, "{}", n.err);
    assert!(s
        .amux_with(&["notes"], &[], Some(&repo))
        .out
        .contains("1. use zod, not yup"));
    assert!(
        repo.join(".git/amux/notes.jsonl").exists(),
        "kept in the git folder, never committed"
    );
    for name in ["p1", "p2"] {
        let r = s.amux_with(
            &[
                "spawn",
                "--name",
                name,
                "--worktree",
                "--prompt",
                "add a schema",
                "fake",
            ],
            &[],
            Some(&repo),
        );
        assert_eq!(r.code, 0, "{}", r.err);
        assert!(r.err.contains("copied .env into the worktree"), "{}", r.err);
    }
    wait_ps(&s, "p1", "running");
    wait_ps(&s, "p2", "running");
    let p1 = wait_peek(&s, "p1", "PORT=");
    assert!(
        p1.contains("ARG Decisions for this repo that every agent here follows:"),
        "{p1}"
    );
    assert!(p1.contains("- use zod, not yup"), "{p1}");
    assert!(p1.contains("PORT=41000 COMPOSE=amux-p1"), "{p1}");
    assert!(s
        .amux(&["peek", "p2"])
        .out
        .contains("PORT=41010 COMPOSE=amux-p2"));
    assert_eq!(
        fs::read_to_string(s.amux_home.join("worktrees/repo-p1/.env")).unwrap(),
        "API_KEY=local\n"
    );

    let b = s.amux_with(
        &["broadcast", "main", "changed:", "rebase", "--note"],
        &[],
        Some(&repo),
    );
    assert!(b.out.contains("Sent to p1, p2, and noted"), "{}", b.out);
    for name in ["p1", "p2"] {
        let mut seen = String::new();
        for _ in 0..100 {
            seen = s.amux(&["peek", name]).out;
            if seen.contains("got main changed: rebase") {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(seen.contains("got main changed: rebase"), "{seen}");
        s.amux(&["stop", name]);
    }
    assert!(s
        .amux_with(&["notes"], &[], Some(&repo))
        .out
        .contains("2. main changed: rebase"));
    assert_eq!(
        s.amux_with(&["notes", "--rm", "1"], &[], Some(&repo)).code,
        0
    );
}

#[test]
fn revives_a_run_lost_in_a_restart_with_its_session() {
    let s = Sandbox::new("revive");
    let bin = s.home.join("bin/chat-cli");
    write(
        &bin,
        "#!/bin/sh\necho \"ARGS $*\"\nwhile read l; do echo \"got $l\"; done\n",
    );
    fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
    let cfg = s.amux_home.join("config.json");
    let mut c: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&cfg).unwrap()).unwrap();
    c["providers"][0]["bin"] = bin.to_str().unwrap().into();
    c["providers"][0]["resumeArgs"] = serde_json::json!(["--resume", "{id}"]);
    c["providers"][0]["sessions"] = "claude".into();
    c["providers"][0]["sharedDirs"] = serde_json::json!(["projects"]);
    write(&cfg, &c.to_string());
    s.amux(&["account", "add", "fake", "one", "--no-login"]);
    assert_eq!(s.amux(&["spawn", "--name", "rv", "fake"]).code, 0);
    wait_ps(&s, "rv", "running");
    let run_json = || -> serde_json::Value {
        serde_json::from_str(&fs::read_to_string(s.amux_home.join("runs/rv/run.json")).unwrap())
            .unwrap()
    };
    let cwd = run_json()["cwd"].as_str().unwrap().to_string();
    let sid = "7e57a0b1-0000-4000-8000-000000000002";
    write(
        &s.shared.join(format!("projects/-x/{sid}.jsonl")),
        &format!(
            "{}\n",
            serde_json::json!({"type": "user", "cwd": cwd, "sessionId": sid,
                "message": {"role": "user", "content": "build the cart page"}})
        ),
    );
    // The power goes: host and CLI end without a word.
    let (host, cli) = (
        run_json()["hostPid"].as_i64().unwrap(),
        run_json()["pid"].as_i64().unwrap(),
    );
    for p in [host, cli] {
        let _ = Command::new("kill").args(["-9", &p.to_string()]).status();
    }
    wait_ps(&s, "rv", "lost");
    let listed = s.amux(&["revive"]).out;
    assert!(
        listed.contains("rv") && listed.contains("revive --all"),
        "{listed}"
    );
    assert!(s.amux(&["explain", "rv"]).out.contains("amux revive rv"));
    let dry = s.amux(&["revive", "rv", "--dry-run"]);
    assert!(
        dry.out
            .contains(&format!("would run {} --resume {sid}", bin.display())),
        "{}",
        dry.out
    );

    let r = s.amux(&["revive", "--all"]);
    assert_eq!(r.code, 0, "{}{}", r.out, r.err);
    assert!(r.out.contains("resumed and told to carry on"), "{}", r.out);
    wait_ps(&s, "rv", "running");
    assert_eq!(run_json()["args"], serde_json::json!(["--resume", sid]));
    let logs = s.amux(&["logs", "rv"]).out;
    assert!(logs.contains("resuming its session"), "{logs}");
    assert!(
        logs.contains("got The computer restarted while you were working"),
        "{logs}"
    );
    s.amux(&["stop", "rv"]);

    // The login item that does it after every restart.
    let i = s.amux(&["revive", "--at-login"]);
    assert_eq!(i.code, 0, "{}", i.err);
    let item = if cfg!(target_os = "macos") {
        s.home.join("Library/LaunchAgents/dev.amux.revive.plist")
    } else {
        s.home.join(".config/systemd/user/amux-revive.service")
    };
    let text = fs::read_to_string(&item).unwrap();
    assert!(text.contains("revive") && text.contains("--all"), "{text}");
    assert_eq!(s.amux(&["revive", "--no-at-login"]).code, 0);
    assert!(!item.exists());
}

/// A stand-in notification server: answers 200 to every request and keeps each one.
fn receiver() -> (String, std::sync::mpsc::Receiver<String>) {
    use std::io::{BufRead, BufReader, Read, Write};
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = format!("http://{}", l.local_addr().unwrap());
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for s in l.incoming().flatten() {
            let mut r = BufReader::new(s.try_clone().unwrap());
            let mut head = String::new();
            let mut len = 0;
            loop {
                let mut line = String::new();
                if r.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                    break;
                }
                if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    len = v.trim().parse().unwrap_or(0);
                }
                head.push_str(&line);
            }
            let mut body = vec![0; len];
            let _ = r.read_exact(&mut body);
            let _ = (&s)
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}");
            let _ = tx.send(format!("{head}\n{}", String::from_utf8_lossy(&body)));
        }
    });
    (addr, rx)
}

#[test]
fn notifies_the_phone_when_a_run_needs_you() {
    let s = Sandbox::new("phone");
    s.amux(&["account", "add", "fake", "one", "--no-login"]);
    let (addr, rx) = receiver();
    let wait = || rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap();

    let out = s.amux(&["phone"]);
    assert!(out.out.contains("No phone set up"), "{}", out.out);
    assert_ne!(s.amux(&["phone", "test"]).code, 0);

    // ntfy, on a server of your own: the topic, a title, and a link to the run.
    let out = s.amux(&[
        "phone",
        "ntfy",
        &format!("{addr}/amux-secret"),
        "--token",
        "tk",
    ]);
    assert_eq!(out.code, 0, "{}", out.err);
    assert!(out.out.contains("amux-secret"));
    let mode = fs::metadata(s.amux_home.join("phone.json"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o077, 0, "phone.json is private");
    assert_eq!(s.amux(&["phone", "open", "http://100.64.0.1:7878"]).code, 0);
    let out = s.amux(&["phone", "test"]);
    assert_eq!(out.code, 0, "{}", out.err);
    let got = wait();
    assert!(got.starts_with("POST / "), "{got}");
    assert!(
        got.to_ascii_lowercase()
            .contains("authorization: bearer tk"),
        "{got}"
    );
    assert!(got.contains(r#""topic":"amux-secret""#), "{got}");

    // A run that starts waiting while nobody watches it.
    let env = [("FAKE_CHAT", "1"), ("FAKE_ASK", "1")];
    assert_eq!(
        s.amux_with(&["spawn", "--name", "ask", "fake"], &env, None)
            .code,
        0
    );
    wait_ps(&s, "ask", "waiting");
    let got = wait();
    assert!(got.contains("Waiting for you: approve THIS?"), "{got}");
    assert!(
        got.contains(r#""click":"http://100.64.0.1:7878/#run=ask""#),
        "{got}"
    );
    assert!(got.contains(r#""priority":4"#), "{got}");

    // A webhook, brief, only for exits: what the agent said stays home.
    assert_eq!(
        s.amux(&["phone", "webhook", &format!("{addr}/hook")]).code,
        0
    );
    assert_eq!(s.amux(&["phone", "events", "exited"]).code, 0);
    assert_eq!(s.amux(&["phone", "brief", "on"]).code, 0);
    let out = s.amux(&["phone"]);
    assert!(
        out.out.contains("Events:     exited") && out.out.contains("Brief:      on"),
        "{}",
        out.out
    );
    assert_ne!(s.amux(&["phone", "events", "bogus"]).code, 0);
    s.amux(&["send", "ask", "quit"]);
    wait_ps(&s, "ask", "exited 7");
    let got = wait();
    assert!(got.starts_with("POST /hook "), "{got}");
    assert!(
        got.contains(r#""event":"exited""#) && got.contains(r#""content":"amux: ask"#),
        "{got}"
    );
    assert!(
        !got.contains("Exited with code"),
        "brief leaves out the message: {got}"
    );

    assert!(s.amux(&["phone", "off"]).out.contains("off"));
    assert!(!s.amux_home.join("phone.json").exists());
}

/// Starts `amux serve` with `args` and returns it and the link it printed.
fn start_serve(s: &Sandbox, args: &[&str], env: &[(&str, &str)]) -> (std::process::Child, String) {
    use std::io::{BufRead, BufReader};
    let mut srv = Command::new(env!("CARGO_BIN_EXE_amux"))
        .arg("serve")
        .args(args)
        .env("HOME", &s.home)
        .env("AMUX_HOME", &s.amux_home)
        .env("AMUX_NO_UPDATE_CHECK", "1")
        .envs(env.iter().copied())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut out = BufReader::new(srv.stdout.take().unwrap());
    let mut link = String::new();
    let mut all = String::new();
    let mut line = String::new();
    while out.read_line(&mut line).unwrap_or(0) > 0 {
        all.push_str(&line);
        if line.contains("?t=") {
            link = line.trim().to_string();
        }
        if line.contains("keep it private") || line.contains("Trust Settings") {
            break;
        }
        line.clear();
    }
    assert!(!link.is_empty(), "{all}");
    (srv, all)
}

fn curl(args: &[&str]) -> String {
    let o = Command::new("curl")
        .args(["-s", "-i", "-k", "--max-time", "10"])
        .args(args)
        .output()
        .unwrap();
    String::from_utf8_lossy(&o.stdout).into_owned()
}

#[test]
fn serves_the_phone_app_over_https() {
    let s = Sandbox::new("https");
    let (mut srv, out) = start_serve(&s, &["--listen", "127.0.0.1:0", "--tls"], &[]);
    let link = out
        .lines()
        .find(|l| l.contains("?t="))
        .unwrap()
        .trim()
        .to_string();
    assert!(link.starts_with("https://127.0.0.1:"), "{out}");
    assert!(out.contains("fingerprint"), "{out}");
    let (base, token) = link.split_once("/?t=").unwrap();

    let login = curl(&[&link]);
    assert!(
        login.starts_with("HTTP/1.1 303")
            && login.contains("SameSite=Strict; Path=/; Max-Age=31536000; Secure"),
        "{login}"
    );
    let runs = curl(&[
        "-H",
        &format!("Cookie: amux={token}"),
        &format!("{base}/api/runs"),
    ]);
    assert!(runs.starts_with("HTTP/1.1 200"), "{runs}");
    assert!(curl(&[&format!("{base}/api/runs")]).starts_with("HTTP/1.1 401"));
    // The certificate is offered to install on the phone, and it is the one in use.
    let pem = curl(&[&format!("{base}/amux-cert.pem")]);
    assert!(pem.contains("BEGIN CERTIFICATE"), "{pem}");
    // Plain HTTP gets nothing.
    let plain = curl(&[&base.replace("https://", "http://")]);
    assert!(!plain.contains("200 OK"), "{plain}");
    let saved = fs::read_to_string(s.amux_home.join("serve.json")).unwrap();
    assert!(saved.contains(base), "{saved}");
    let _ = srv.kill();
    let _ = srv.wait();

    // Your own certificate, or none: --cert needs --key.
    assert_ne!(s.amux(&["serve", "--cert", "/x.pem"]).code, 0);
    assert_ne!(s.amux(&["serve", "--tls", "--tailscale"]).code, 0);
}

#[test]
fn serves_the_phone_app_through_tailscale() {
    let s = Sandbox::new("tailscale");
    let ts = s.home.join("bin/tailscale");
    let calls = s.home.join("ts-calls");
    write(
        &ts,
        &format!(
            "#!/bin/sh\necho \"$@\" >> '{}'\ncase \"$1\" in\n  status) echo '{{\"Self\":{{\"DNSName\":\"box.tail1.ts.net.\"}}}}';;\n  serve) [ -n \"$TS_FAIL\" ] && {{ echo 'HTTPS is not enabled' >&2; exit 1; }}; sleep 5;;\nesac\n",
            calls.display()
        ),
    );
    fs::set_permissions(&ts, fs::Permissions::from_mode(0o755)).unwrap();
    let env = [("AMUX_TAILSCALE", ts.to_str().unwrap())];

    let (mut srv, out) = start_serve(&s, &["--listen", "127.0.0.1:0", "--tailscale"], &env);
    assert!(out.contains("https://box.tail1.ts.net/?t="), "{out}");
    let called = fs::read_to_string(&calls).unwrap();
    let port = called
        .lines()
        .find_map(|l| l.strip_prefix("serve --https=443 http://127.0.0.1:"))
        .unwrap_or_else(|| panic!("{called}"));
    // Tailscale passes requests on as plain HTTP; the cookie is still marked Secure.
    let token = out
        .split("?t=")
        .nth(1)
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap();
    let login = http(
        &format!("127.0.0.1:{port}"),
        &format!("GET /?t={token} HTTP/1.1\r\nHost: x\r\n\r\n"),
    );
    assert!(login.contains("; Secure"), "{login}");
    assert!(fs::read_to_string(s.amux_home.join("serve.json"))
        .unwrap()
        .contains("https://box.tail1.ts.net"));
    let _ = srv.kill();
    let _ = srv.wait();

    let failed = s.amux_with(
        &["serve", "--listen", "127.0.0.1:0", "--tailscale"],
        &[env[0], ("TS_FAIL", "1")],
        None,
    );
    assert_ne!(failed.code, 0);
    assert!(
        failed.err.contains("HTTPS is not enabled") && failed.err.contains("HTTPS Certificates"),
        "{}",
        failed.err
    );
    let open = s.amux_with(
        &["serve", "--listen", "0.0.0.0:0", "--tailscale"],
        &env,
        None,
    );
    assert!(open.err.contains("this machine only"), "{}", open.err);
}

#[test]
fn themes_are_shared_by_the_tui_and_the_phone_app() {
    let s = Sandbox::new("themes");
    let list = s.amux(&["theme"]);
    assert!(
        list.out.contains("● default") && list.out.contains("○ catppuccin-mocha"),
        "{}",
        list.out
    );
    assert_ne!(s.amux(&["theme", "no-such-theme"]).code, 0);
    assert_eq!(s.amux(&["theme", "dracula"]).code, 0);
    let json: serde_json::Value = serde_json::from_str(&s.amux(&["theme", "--json"]).out).unwrap();
    let d = json
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "dracula")
        .unwrap();
    assert_eq!(d["current"], true);
    assert_eq!(d["swatch"][0], "#282a36");
    // A theme file of your own extends a palette, and the phone app follows it.
    write(
        &s.amux_home.join("themes/mine.json"),
        r##"{ "extends": "nord", "colors": { "accent": "#ff9e64" } }"##,
    );
    assert_eq!(s.amux(&["theme", "mine"]).code, 0);
    let dump = s.amux(&["ui", "--dump", "--ansi", "--size", "10x80"]);
    assert!(
        dump.out.contains("48;2;46;52;64"),
        "nord's background is painted"
    );
    assert!(dump.out.contains("255;158;100"), "the override applies");

    let (mut srv, out) = start_serve(&s, &["--listen", "127.0.0.1:0"], &[]);
    let link = out
        .lines()
        .find(|l| l.contains("?t="))
        .unwrap()
        .trim()
        .to_string();
    let (base, token) = link.split_once("/?t=").unwrap();
    let css = curl(&[&format!("{base}/theme.css")]);
    assert!(css.contains("--bg:#2e3440;"), "{css}");
    let cookie = format!("Cookie: amux={token}");
    let set = curl(&[
        "-H",
        &cookie,
        "-H",
        "X-Amux: 1",
        "-H",
        "Content-Type: application/json",
        "-d",
        r#"{"name":"gruvbox-light"}"#,
        &format!("{base}/api/theme"),
    ]);
    assert!(set.starts_with("HTTP/1.1 200"), "{set}");
    assert!(curl(&[&format!("{base}/theme.css")]).contains("--bg:#fbf1c7;"));
    let bad = curl(&[
        "-H",
        &cookie,
        "-H",
        "X-Amux: 1",
        "-H",
        "Content-Type: application/json",
        "-d",
        r#"{"name":"../x"}"#,
        &format!("{base}/api/theme"),
    ]);
    assert!(bad.starts_with("HTTP/1.1 400"), "{bad}");
    let _ = srv.kill();
    let _ = srv.wait();
    assert_eq!(s.amux(&["theme", "default"]).code, 0);
}

#[test]
fn a_lead_runs_a_team_of_workers() {
    let s = Sandbox::new("team");
    s.amux(&["account", "add", "fake", "one", "--no-login"]);
    let repo = s.home.join("repo");
    write(&repo.join("a.txt"), "x\n");
    for args in [
        &["init", "-q"][..],
        &["add", "."],
        &["commit", "-qm", "init"],
    ] {
        let ok = Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
            .args(args)
            .status()
            .unwrap()
            .success();
        assert!(ok);
    }
    let chat = ("FAKE_CHAT", "1");
    assert_ne!(s.amux(&["lead", "fake"]).code, 0, "a lead needs a goal");
    let r = s.amux_with(
        &[
            "lead",
            "--name",
            "t",
            "--workers",
            "2",
            "fake",
            "-p",
            "build the thing",
        ],
        &[chat],
        Some(&repo),
    );
    assert_eq!(r.code, 0, "{}", r.err);
    wait_ps(&s, "t", "running");
    let mut logs = String::new();
    eventually("the lead's prompt", || {
        logs = s.amux(&["logs", "t"]).out;
        logs.contains("build the thing")
    });
    assert!(
        logs.contains("You are the lead of an amux team named t"),
        "{logs}"
    );
    assert!(logs.contains("build the thing"));

    // The lead's shell starts workers: each in its own worktree, told how to report.
    let as_lead = [chat, ("AMUX_RUN", "t")];
    for w in ["t-a", "t-b"] {
        let r = s.amux_with(
            &[
                "spawn",
                "--worktree",
                "--name",
                w,
                "--prompt",
                "do a part",
                "fake",
            ],
            &as_lead,
            Some(&repo),
        );
        assert_eq!(r.code, 0, "{}", r.err);
    }
    wait_ps(&s, "t-a", "running");
    let mut wl = String::new();
    for _ in 0..60 {
        wl = s.amux(&["logs", "t-a"]).out;
        if wl.contains("amux report") {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(
        wl.contains("amux report"),
        "{wl}\n{}",
        fs::read_to_string(s.amux_home.join("runs/t-a/run.json")).unwrap_or_default()
    );
    let full = s.amux_with(&["spawn", "--name", "t-c", "fake"], &as_lead, Some(&repo));
    assert!(
        full.code != 0 && full.err.contains("its limit"),
        "{}",
        full.err
    );
    let nested = s.amux_with(
        &["spawn", "--name", "t-d", "fake"],
        &[("AMUX_RUN", "t-a")],
        Some(&repo),
    );
    assert!(
        nested.code != 0 && nested.err.contains("workers don't start workers"),
        "{}",
        nested.err
    );
    assert_ne!(
        s.amux_with(&["lead", "fake", "-p", "x"], &[("AMUX_RUN", "t")], None)
            .code,
        0
    );

    // A worker reports and ends; the lead hears about it once.
    let rep = s.amux_with(
        &["report", "did part a; tests pass"],
        &[("AMUX_RUN", "t-a")],
        None,
    );
    assert!(rep.out.contains("Reported to t"), "{}", rep.out);
    s.amux(&["send", "t-a", "quit"]);
    wait_ps(&s, "t-a", "exited 7");
    let tick = [("AMUX_TEAM_TICK_MS", "100")];
    let w = s.amux_with(&["team", "wait", "t", "--timeout", "10"], &tick, None);
    assert_eq!(w.code, 0, "{}{}", w.out, w.err);
    assert!(
        w.out.contains("== t-a ended") && w.out.contains("did part a; tests pass"),
        "{}",
        w.out
    );
    assert!(w.out.contains("Still working: t-b"), "{}", w.out);
    let again = s.amux_with(&["team", "wait", "t", "--timeout", "1"], &tick, None);
    assert_eq!(again.code, 124, "nothing new: {}", again.out);

    // A worker's committed work lands even with untracked leftovers (a test run's
    // __pycache__), tested with the command the lead gives.
    let wt = s.amux_home.join("worktrees/repo-t-b");
    write(&wt.join("b.txt"), "b\n");
    write(&wt.join("__pycache__/x.pyc"), "junk");
    for args in [&["add", "b.txt"][..], &["commit", "-qm", "part b"]] {
        let ok = Command::new("git")
            .arg("-C")
            .arg(&wt)
            .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
            .args(args)
            .status()
            .unwrap()
            .success();
        assert!(ok);
    }
    let landed = s.amux_with(
        &["land", "t-b", "--command", "test -f b.txt", "--no-tell"],
        &[
            ("GIT_AUTHOR_NAME", "t"),
            ("GIT_AUTHOR_EMAIL", "t@example.com"),
            ("GIT_COMMITTER_NAME", "t"),
            ("GIT_COMMITTER_EMAIL", "t@example.com"),
        ],
        None,
    );
    assert_eq!(landed.code, 0, "{}{}", landed.out, landed.err);
    let git_out = |args: &[&str]| {
        let o = Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(args)
            .output()
            .unwrap();
        String::from_utf8_lossy(&o.stdout).trim().to_string()
    };
    let branch = git_out(&["rev-parse", "--abbrev-ref", "HEAD"]);
    let top = git_out(&["log", "--format=%s", "-1", &branch]);
    assert_eq!(top, "part b", "landed on {branch}");

    // From the lead's shell, `amux team` needs no name.
    let t = s.amux_with(&["team"], &[("AMUX_RUN", "t")], None);
    assert!(
        t.out.contains("t (fake@one") && t.out.contains("up to 2 at once"),
        "{}",
        t.out
    );
    assert!(
        t.out.contains("t-b") && t.out.contains("did part a"),
        "{}",
        t.out
    );
    let j: serde_json::Value = serde_json::from_str(&s.amux(&["team", "t", "--json"]).out).unwrap();
    assert_eq!(j["maxWorkers"], 2);
    assert_eq!(j["workers"].as_array().unwrap().len(), 2);
    for r in ["t-b", "t"] {
        s.amux(&["stop", r]);
    }
}

/// A stand-in for GitHub: serves the files in `files` by path, and 404 for the rest.
fn file_server(files: Vec<(String, Vec<u8>)>) -> String {
    use std::io::{BufRead, BufReader, Write};
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = format!("http://{}", l.local_addr().unwrap());
    std::thread::spawn(move || {
        for s in l.incoming().flatten() {
            let mut r = BufReader::new(s.try_clone().unwrap());
            let mut first = String::new();
            let _ = r.read_line(&mut first);
            loop {
                let mut line = String::new();
                if r.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                    break;
                }
            }
            let path = first.split_whitespace().nth(1).unwrap_or("").to_string();
            let mut w = &s;
            match files.iter().find(|(p, _)| *p == path) {
                Some((_, body)) => {
                    let _ = write!(
                        w,
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = w.write_all(body);
                }
                None => {
                    let _ = w.write_all(
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    );
                }
            }
        }
    });
    addr
}

#[test]
fn update_installs_the_latest_release() {
    use sha2::Digest;
    let s = Sandbox::new("update");
    let target = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", a) => format!("{a}-apple-darwin"),
        (_, a) => format!("{a}-unknown-linux-musl"),
    };
    // The release: an "amux" that only knows its version.
    let pack = s.home.join("pack");
    write(&pack.join("amux"), "#!/bin/sh\necho 9.9.9\n");
    fs::set_permissions(pack.join("amux"), fs::Permissions::from_mode(0o755)).unwrap();
    let tgz = s.home.join("release.tar.gz");
    assert!(Command::new("tar")
        .arg("-czf")
        .arg(&tgz)
        .arg("-C")
        .arg(&pack)
        .arg("amux")
        .status()
        .unwrap()
        .success());
    let bytes = fs::read(&tgz).unwrap();
    let sum = format!("{:x}  amux-{target}.tar.gz\n", sha2::Sha256::digest(&bytes));
    let dl = format!("/quynb93/amux/releases/download/v9.9.9/amux-{target}.tar.gz");
    let latest = (
        "/repos/quynb93/amux/releases/latest".to_string(),
        br#"{"tag_name":"v9.9.9"}"#.to_vec(),
    );
    let good = file_server(vec![
        latest.clone(),
        (dl.clone(), bytes.clone()),
        (format!("{dl}.sha256"), sum.into_bytes()),
    ]);
    let bad = file_server(vec![
        latest,
        (dl.clone(), bytes),
        (
            format!("{dl}.sha256"),
            format!("{}  x\n", "0".repeat(64)).into_bytes(),
        ),
    ]);

    // A copy installed by the install script.
    let exe = s.home.join("local/bin/amux");
    fs::create_dir_all(exe.parent().unwrap()).unwrap();
    fs::copy(env!("CARGO_BIN_EXE_amux"), &exe).unwrap();
    let run = |args: &[&str], url: &str| {
        let mut c = Command::new(&exe);
        c.args(args)
            .env("HOME", &s.home)
            .env("AMUX_HOME", &s.amux_home)
            .env("AMUX_NO_UPDATE_CHECK", "1")
            .env("AMUX_GITHUB_URL", url);
        let o = c.output().unwrap();
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&o.stdout),
            String::from_utf8_lossy(&o.stderr)
        );
        (o.status.code().unwrap_or(-1), text)
    };
    let version = || {
        String::from_utf8_lossy(&Command::new(&exe).arg("--version").output().unwrap().stdout)
            .trim()
            .to_string()
    };

    let (code, out) = run(&["update", "--check"], &good);
    assert_eq!(code, 0, "{out}");
    assert!(
        out.contains("→ 9.9.9") && out.contains("install.sh"),
        "{out}"
    );
    assert_eq!(version(), env!("CARGO_PKG_VERSION"));

    let (code, out) = run(&["update"], &bad);
    assert_ne!(code, 0, "{out}");
    assert!(out.contains("checksum mismatch"), "{out}");
    assert_eq!(version(), env!("CARGO_PKG_VERSION"));

    let (code, out) = run(&["update"], &good);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("Updated amux"), "{out}");
    assert_eq!(version(), "9.9.9");
    let leftovers: Vec<_> = fs::read_dir(exe.parent().unwrap()).unwrap().collect();
    assert_eq!(leftovers.len(), 1, "temp files left behind");

    // A Homebrew copy is upgraded by brew.
    let cellar = s.home.join("brew/Cellar/amux/0.1.0/bin/amux");
    fs::create_dir_all(cellar.parent().unwrap()).unwrap();
    fs::copy(env!("CARGO_BIN_EXE_amux"), &cellar).unwrap();
    let tools = s.home.join("tools");
    write(
        &tools.join("brew"),
        &format!(
            "#!/bin/sh\necho \"$@\" > {}\n",
            s.home.join("brew-args").display()
        ),
    );
    fs::set_permissions(tools.join("brew"), fs::Permissions::from_mode(0o755)).unwrap();
    let o = Command::new(&cellar)
        .arg("update")
        .env("HOME", &s.home)
        .env("AMUX_HOME", &s.amux_home)
        .env("AMUX_GITHUB_URL", &good)
        .env("PATH", format!("{}:/usr/bin:/bin", tools.display()))
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert_eq!(
        fs::read_to_string(s.home.join("brew-args")).unwrap().trim(),
        "upgrade amux"
    );
}

#[test]
fn works_on_github_issues_and_runs_commands_on_a_schedule() {
    let s = Sandbox::new("issues");
    s.amux(&["account", "add", "fake", "one", "--no-login"]);
    let repo = s.home.join("repo");
    fs::create_dir_all(&repo).unwrap();
    write(&repo.join("a.txt"), "x");
    for args in [
        &["init", "-q"][..],
        &["add", "."],
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.com",
            "commit",
            "-qm",
            "init",
        ],
    ] {
        assert!(Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(args)
            .status()
            .unwrap()
            .success());
    }
    let remote = s.home.join("remote.git");
    assert!(Command::new("git")
        .args(["init", "-q", "--bare"])
        .arg(&remote)
        .status()
        .unwrap()
        .success());
    assert!(Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args(["remote", "add", "origin"])
        .arg(&remote)
        .status()
        .unwrap()
        .success());
    // A stand-in for the GitHub CLI with two open issues labeled bug.
    let gh = s.home.join("bin/gh");
    let issue = |n: u32, t: &str| {
        format!("{{\"number\":{n},\"title\":\"{t}\",\"body\":\"Steps for {n}.\",\"url\":\"https://github.com/o/r/issues/{n}\"}}")
    };
    write(
        &gh,
        &format!(
            "#!/bin/sh\necho \"$@\" >> {log}\ncase \"$1 $2\" in\n\
             'issue view') [ \"$3\" = 12 ] && echo '{i12}' && exit 0; [ \"$3\" = 13 ] && echo '{i13}' && exit 0; echo 'no such issue' >&2; exit 1;;\n\
             'issue list') echo '[{i13},{i12}]';;\n\
             'pr view') [ -f {st} ] && echo '{{\"url\":\"https://github.com/o/r/pull/7\",\"number\":7,\"state\":\"OPEN\"}}' && exit 0; echo 'no pull requests found' >&2; exit 1;;\n\
             'pr create') touch {st};;\n\
             esac\n",
            log = s.home.join("gh-log").display(),
            st = s.home.join("gh-pr").display(),
            i12 = issue(12, "Crash on empty input"),
            i13 = issue(13, "Typo in help"),
        ),
    );
    fs::set_permissions(&gh, fs::Permissions::from_mode(0o755)).unwrap();
    let env = [
        ("AMUX_GH", gh.to_str().unwrap()),
        ("AMUX_SCHEDULE_TICK_MS", "100"),
    ];
    let run_args = |id: &str| -> Vec<String> {
        let j: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(s.amux_home.join("runs").join(id).join("run.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(j["worktree"]["branch"], format!("amux/{id}"));
        serde_json::from_value(j["args"].clone()).unwrap()
    };

    // One issue, by number: its own worktree, the issue as the prompt, and a PR that fixes it.
    let r = s.amux_with(
        &[
            "spawn",
            "--issue",
            "#12",
            "--prompt",
            "Keep it small.",
            "fake",
        ],
        &env,
        Some(&repo),
    );
    assert_eq!(r.code, 0, "{}", r.err);
    assert!(r.err.contains("started issue-12"), "{}", r.err);
    let args = run_args("issue-12");
    assert!(
        args[0].starts_with("Work on GitHub issue #12: Crash on empty input"),
        "{args:?}"
    );
    assert!(
        args[0].contains("Steps for 12.") && args[0].contains("Keep it small."),
        "{args:?}"
    );
    let ps = s.amux(&["ps", "--json"]).out;
    assert!(ps.contains("\"issue\": 12"), "{ps}");
    let pr = s.amux_with(&["pr", "issue-12"], &env, None);
    assert_eq!(pr.code, 0, "{}", pr.err);
    let log = fs::read_to_string(s.home.join("gh-log")).unwrap();
    assert!(log.contains("--body Fixes #12"), "{log}");
    let bad = s.amux_with(&["spawn", "--issue", "99", "fake"], &env, Some(&repo));
    assert!(bad.err.contains("no such issue"), "{}", bad.err);
    assert!(!s.amux_home.join("runs/issue-99").exists());

    // Intake: a run for each open issue that has none yet, once.
    let dry = s.amux_with(
        &["intake", "--label", "bug", "--dry-run", "fake"],
        &env,
        Some(&repo),
    );
    assert!(
        dry.out.contains("Would start issue-12") && dry.out.contains("Would start issue-13"),
        "{}",
        dry.out
    );
    let r = s.amux_with(&["intake", "--label", "bug", "fake"], &env, Some(&repo));
    assert_eq!(r.code, 0, "{}{}", r.out, r.err);
    assert!(
        r.out.contains("Started issue-13: #13 Typo in help"),
        "{}",
        r.out
    );
    assert!(
        !r.out.contains("issue-12:"),
        "12 has a run already: {}",
        r.out
    );
    assert!(run_args("issue-13")[0].contains("issue #13"));
    let log = fs::read_to_string(s.home.join("gh-log")).unwrap();
    assert!(
        log.contains("issue list --state open") && log.contains("--label bug"),
        "{log}"
    );
    let again = s.amux_with(&["intake", "--label", "bug", "fake"], &env, Some(&repo));
    assert!(
        again.out.contains("No new open issues labeled bug"),
        "{}",
        again.out
    );

    // Schedules: checked when added, run by a background scheduler, and logged.
    for (when, cmd) in [("soon", "ps"), ("every 1s", "ui")] {
        let r = s.amux_with(&["schedule", "add", "x", when, cmd], &env, None);
        assert_ne!(r.code, 0, "{when} {cmd}");
    }
    let r = s.amux_with(
        &["schedule", "add", "tick", "every 1s", "amux", "ps"],
        &env,
        Some(&repo),
    );
    assert_eq!(r.code, 0, "{}", r.err);
    assert!(r.out.contains("Scheduled tick: amux ps"), "{}", r.out);
    let r = s.amux_with(
        &["schedule", "add", "bad", "every 1s", "stop", "nope"],
        &env,
        None,
    );
    assert_eq!(r.code, 0, "{}", r.err);
    let wait_for = |what: &dyn Fn(&str) -> bool| {
        for _ in 0..100 {
            let out = s.amux_with(&["schedule"], &env, None).out;
            if what(&out) {
                return out;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        panic!("{}", s.amux_with(&["schedule"], &env, None).out);
    };
    wait_for(&|o| {
        o.lines()
            .any(|l| l.starts_with("tick") && l.contains("ok "))
            && o.lines()
                .any(|l| l.starts_with("bad") && l.contains("exit 1"))
    });
    let log = s.amux_with(&["schedule", "log", "tick"], &env, None).out;
    assert!(log.contains("amux ps") && log.contains("issue-13"), "{log}");
    let json = s.amux_with(&["schedule", "--json"], &env, None).out;
    assert!(
        json.contains("\"nextAt\"") && json.contains(&format!("\"dir\": \"{}\"", repo.display())),
        "{json}"
    );

    // Paused, it stops; run by hand, it runs in the foreground; removed, the scheduler ends.
    s.amux_with(&["schedule", "pause", "tick"], &env, None);
    assert!(s
        .amux_with(&["schedule"], &env, None)
        .out
        .contains("paused"));
    let now = s.amux_with(&["schedule", "run", "tick"], &env, None);
    assert_eq!(now.code, 0);
    assert!(now.out.contains("issue-12"), "{}", now.out);
    s.amux_with(&["schedule", "rm", "tick"], &env, None);
    s.amux_with(&["schedule", "rm", "bad"], &env, None);
    for _ in 0..50 {
        let log = fs::read_to_string(s.amux_home.join("scheduler.log")).unwrap_or_default();
        if log.contains("scheduler exits") {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    panic!("the scheduler is still going");
}

#[test]
fn answers_permission_prompts_the_rules_allow() {
    let s = Sandbox::new("allow");
    s.amux(&["account", "add", "fake", "one", "--no-login"]);
    let env = [("FAKE_PERM", "1")];
    let peek = |run: &str, want: &str| {
        for _ in 0..100 {
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
    };

    // No rules: the prompt waits for you.
    let r = s.amux_with(&["spawn", "--name", "none", "fake"], &env, None);
    assert_eq!(r.code, 0, "{}", r.err);
    wait_ps(&s, "none", "waiting");
    assert!(!s.amux(&["peek", "none"]).out.contains("answer"));

    assert_eq!(s.amux(&["allow", "bash: cargo test*"]).code, 0);
    assert_eq!(s.amux(&["allow", "--deny", "bash: *git push*"]).code, 0);
    assert_ne!(s.amux(&["allow", "not a rule"]).code, 0);
    let list = s.amux(&["allow"]).out;
    assert!(
        list.contains("allow  bash: cargo test*") && list.contains("deny   bash: *git push*"),
        "{list}"
    );
    let c = s.amux(&["allow", "--check", "bash: cargo test && git push"]);
    assert_eq!(c.code, 1);
    assert!(c.out.contains("denied by bash: *git push*"), "{}", c.out);

    // With rules: cargo test is answered by itself, git push waits for you.
    let r = s.amux_with(&["spawn", "--name", "auto", "fake"], &env, None);
    assert_eq!(r.code, 0, "{}", r.err);
    peek("auto", "answer 1 to cargo test --all");
    wait_ps(&s, "auto", "waiting");
    assert!(!s.amux(&["peek", "auto"]).out.contains("to git push"));
    let log = s.amux(&["allow", "--log", "auto"]).out;
    assert!(
        log.contains("approved") && log.contains("bash: cargo test --all"),
        "{log}"
    );
    assert!(
        log.contains("denied") && log.contains("git push origin main"),
        "{log}"
    );

    // A run that must always ask.
    let r = s.amux_with(
        &["spawn", "--name", "manual", "--no-auto-approve", "fake"],
        &env,
        None,
    );
    assert_eq!(r.code, 0, "{}", r.err);
    wait_ps(&s, "manual", "waiting");
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert!(!s.amux(&["peek", "manual"]).out.contains("answer"));
    assert!(s
        .amux(&["allow", "--log", "manual"])
        .out
        .contains("No prompts judged"));
    for r in ["none", "auto", "manual"] {
        s.amux(&["stop", r, "--force"]);
    }
}

#[test]
fn autopilot_takes_a_run_from_commits_to_a_merged_pr() {
    let s = Sandbox::new("autopilot");
    s.amux(&["account", "add", "fake", "one", "--no-login"]);
    let repo = s.home.join("repo");
    let remote = s.home.join("remote.git");
    fs::create_dir_all(&repo).unwrap();
    write(&repo.join("a.txt"), "x");
    let git = |dir: &Path, args: &[&str]| {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}");
    };
    git(&s.home, &["init", "-q", "--bare", remote.to_str().unwrap()]);
    git(&repo, &["init", "-q"]);
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "init"]);
    git(
        &repo,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    // A stand-in for the GitHub CLI whose PR, checks and reviews come from files.
    let f = |n: &str| s.home.join(n);
    let gh = f("bin/gh");
    write(
        &gh,
        &format!(
            "#!/bin/sh\necho \"$@\" >> {log}\ncase \"$1 $2\" in\n\
             'pr view') [ -f {st} ] && echo \"{{\\\"url\\\":\\\"https://github.com/o/r/pull/7\\\",\\\"number\\\":7,\\\"state\\\":\\\"$(cat {st})\\\"}}\" && exit 0; echo 'no pull requests found for branch' >&2; exit 1;;\n\
             'pr create') echo OPEN > {st};;\n\
             'pr checks') cat {checks};;\n\
             'api graphql') cat {review};;\n\
             'run view') echo 'test\tRun tests\tassertion failed: parser-broke';;\n\
             'pr merge') echo MERGED > {st};;\n\
             esac\n",
            log = f("gh-log").display(),
            st = f("gh-pr").display(),
            checks = f("gh-checks").display(),
            review = f("gh-review").display(),
        ),
    );
    fs::set_permissions(&gh, fs::Permissions::from_mode(0o755)).unwrap();
    let checks = |bucket: &str| {
        write(
            &f("gh-checks"),
            &format!("[{{\"name\":\"test\",\"bucket\":\"{bucket}\",\"link\":\"https://github.com/o/r/actions/runs/42/job/1\",\"workflow\":\"CI\"}}]"),
        )
    };
    let review = |head: &str, decision: &str, threads: &str, mergeable: &str| {
        write(
            &f("gh-review"),
            &format!("{{\"data\":{{\"repository\":{{\"pullRequest\":{{\"headRefOid\":\"{head}\",\"baseRefName\":\"main\",\"mergeable\":\"{mergeable}\",\"reviewDecision\":\"{decision}\",\"reviews\":{{\"nodes\":[]}},\"reviewThreads\":{{\"nodes\":[{threads}]}}}}}}}}}}"),
        )
    };
    checks("fail");
    review("h1", "", "", "MERGEABLE");
    let env = [
        ("FAKE_CHAT", "1"),
        ("AMUX_GH", gh.to_str().unwrap()),
        ("AMUX_SCHEDULE_TICK_MS", "100"),
        ("AMUX_AUTOPILOT_POLL_MS", "200"),
    ];
    let bad = s.amux_with(&["spawn", "--autopilot=yes", "fake"], &env, Some(&repo));
    assert!(bad.err.contains("=merge"), "{}", bad.err);
    let r = s.amux_with(
        &[
            "spawn",
            "--name",
            "ap",
            "--autopilot=merge",
            "--prompt",
            "fix it",
            "fake",
        ],
        &env,
        Some(&repo),
    );
    assert_eq!(r.code, 0, "{}", r.err);
    assert!(r.err.contains("autopilot is on for ap"), "{}", r.err);
    wait_ps(&s, "ap", "running");
    eventually("the prompt tells the agent to commit", || {
        s.amux(&["logs", "ap"])
            .out
            .contains("commit your work on this branch")
    });

    // Nothing committed: no PR.
    let watch = |what: &str, want: &str| {
        let mut seen = String::new();
        for _ in 0..300 {
            seen = s.amux_with(&["autopilot"], &env, None).out;
            if seen.contains(want) {
                return seen;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        panic!(
            "{what}: wanted {want:?} in\n{seen}\n{}",
            s.amux(&["autopilot", "ap", "--log"]).out
        );
    };
    watch("idle without commits", "nothing committed yet");
    assert!(!fs::read_to_string(f("gh-log"))
        .unwrap_or_default()
        .contains("pr create"));

    // A commit and an idle agent: a PR, and its failing CI is handed back.
    let j: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(s.amux_home.join("runs/ap/run.json")).unwrap())
            .unwrap();
    let wt = PathBuf::from(j["worktree"]["path"].as_str().unwrap());
    write(&wt.join("b.txt"), "y");
    git(&wt, &["add", "."]);
    git(&wt, &["commit", "-qm", "work"]);
    watch("CI handed back", "handed back test (try 1 of 3)");
    let log = fs::read_to_string(f("gh-log")).unwrap();
    assert!(log.contains("pr create --head amux/ap --fill"), "{log}");
    eventually("the agent got the CI failure", || {
        s.amux(&["logs", "ap"]).out.contains("parser-broke")
    });
    assert!(s
        .amux(&["logs", "ap"])
        .out
        .contains("Don't push: amux pushes"));

    // The agent's fix is committed, not pushed: amux pushes it once the turn is over.
    write(&wt.join("c.txt"), "z");
    git(&wt, &["add", "."]);
    git(&wt, &["commit", "-qm", "fix"]);
    watch("fix pushed", "pushed 1 new commit");
    let pushed = Command::new("git")
        .args([
            "-C",
            remote.to_str().unwrap(),
            "log",
            "--oneline",
            "amux/ap",
        ])
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&pushed.stdout).contains("fix"));

    // A new commit with a review comment: handed back once the agent's turn ends.
    checks("pending");
    review(
        "h2",
        "CHANGES_REQUESTED",
        r#"{"isResolved":false,"path":"b.txt","line":1,"comments":{"nodes":[{"id":"C1","body":"Say why.","author":{"login":"ann"}}]}}"#,
        "MERGEABLE",
    );
    watch("review handed back", "handed back 1 review comment");
    eventually("the agent got the review", || {
        s.amux(&["logs", "ap"])
            .out
            .contains("ann on b.txt:1: Say why.")
    });

    // Green but in conflict with main: the agent is asked to merge main in.
    checks("pass");
    review("h3", "APPROVED", "", "CONFLICTING");
    watch(
        "conflicts handed back",
        "handed back merge conflicts with main",
    );
    eventually("the agent got the conflicts", || {
        s.amux(&["logs", "ap"]).out.contains("merge origin/main")
    });

    // Green and approved: merged, and the daemon has nothing left to do.
    review("h4", "APPROVED", "", "MERGEABLE");
    watch("merged", "merged");
    let log = fs::read_to_string(f("gh-log")).unwrap();
    assert!(log.contains("pr merge 7 --squash"), "{log}");
    let hist = s.amux(&["autopilot", "ap", "--log"]).out;
    for want in [
        "opened PR #7",
        "handed back test",
        "review comment",
        "merged PR #7",
    ] {
        assert!(hist.contains(want), "{want}: {hist}");
    }
    eventually("the scheduler exits", || {
        fs::read_to_string(s.amux_home.join("scheduler.log"))
            .unwrap_or_default()
            .contains("scheduler exits")
    });
    let j = s.amux(&["autopilot", "--json"]).out;
    assert!(j.contains("\"phase\": \"merged\""), "{j}");
    assert!(s
        .amux(&["autopilot", "ap", "--off"])
        .out
        .contains("off for ap"));
    assert!(s.amux(&["autopilot"]).out.contains("No runs on autopilot"));
    s.amux(&["stop", "ap"]);
}
