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
        // Also when the test failed: its runs would otherwise go on forever.
        self.stop_runs();
        let _ = fs::remove_dir_all(&self.home);
    }
}

/// A process's command line and environment (`ps eww`), while it exists.
fn ps_env(pid: i32) -> Option<String> {
    let out = Command::new("ps")
        .args(["eww", "-o", "command=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && !text.is_empty()).then_some(text)
}

impl Sandbox {
    /// Ends the run hosts this sandbox started and their CLIs' process groups: only the
    /// pids recorded in its own runs/*/run.json, and only while ps says they still belong
    /// to it (their command line or environment names its AMUX_HOME or HOME), so a pid
    /// reused by someone else's process is left alone.
    fn stop_runs(&self) {
        let Ok(dirs) = fs::read_dir(self.amux_home.join("runs")) else {
            return;
        };
        let home = self.home.to_string_lossy().into_owned();
        let ours = |pid: i32| pid > 1 && ps_env(pid).is_some_and(|t| t.contains(&home));
        // Negative: a process group (the CLI's pid is its group).
        let mut targets: Vec<i32> = vec![];
        for d in dirs.flatten() {
            let Ok(text) = fs::read_to_string(d.path().join("run.json")) else {
                continue;
            };
            let Ok(run) = serde_json::from_str::<serde_json::Value>(&text) else {
                continue;
            };
            let pid = |k: &str| run[k].as_i64().and_then(|p| i32::try_from(p).ok());
            // The group, if its leader or any process left in it is ours (the CLI may
            // have exited and left children running).
            let group_is_ours = |g: i32| {
                ours(g)
                    || Command::new("pgrep")
                        .args(["-g", &g.to_string()])
                        .output()
                        .map(|o| {
                            String::from_utf8_lossy(&o.stdout)
                                .lines()
                                .filter_map(|l| l.trim().parse().ok())
                                .any(ours)
                        })
                        .unwrap_or(false)
            };
            if let Some(cli) = pid("pid").filter(|p| *p > 1 && group_is_ours(*p)) {
                targets.push(-cli);
            }
            if let Some(host) = pid("hostPid").filter(|p| ours(*p)) {
                targets.push(host);
            }
        }
        let alive = |t: i32| unsafe { libc::kill(t, 0) == 0 };
        for sig in [libc::SIGTERM, libc::SIGKILL] {
            targets.retain(|t| alive(*t));
            for t in &targets {
                unsafe {
                    libc::kill(*t, sig);
                }
            }
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            while targets.iter().any(|t| alive(*t)) && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
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
             [ -n \"$FAKE_HOOK\" ] && h() { printf '%s' \"$2\" | \"$FAKE_HOOK\" __hook claude \"$1\"; } \\
               && h UserPromptSubmit '{}' && sleep 0.3 \\
               && h PermissionRequest '{\"tool_name\":\"Bash\",\"tool_input\":{\"command\":\"cargo test\"}}' \\
               && read a && h Stop '{}' && read b && exit 4\n\
             [ -n \"$FAKE_HOOKPERM\" ] && stty -icanon -echo && h() { printf '%s' \"$2\" | \"$FAKE_HOOKPERM\" __hook claude \"$1\"; } \\
               && for c in 'cargo test --all' 'git push origin main'; do \
               h PermissionRequest \"$(printf '{\"tool_name\":\"Bash\",\"tool_input\":{\"command\":\"%s\"}}' \"$c\")\"; sleep 0.3; \
               printf '%s\\n' '────────────────' ' Bash command' ' approve this?' '╌╌╌╌╌╌╌╌╌╌' \" $c\" '╌╌╌╌╌╌╌╌╌╌' ' Do you want to proceed?' ' ❯ 1. Yes' '   2. No'; \
               a=$(dd bs=1 count=1 2>/dev/null); echo \"answer $a to $c\"; \
               h Notification '{\"notification_type\":\"permission_prompt\",\"message\":\"Claude needs your permission\"}'; done && \
               read x && exit 5\n\
             [ -n \"$FAKE_PERM\" ] && stty -icanon -echo && for c in 'cargo test --all' 'git push origin main'; do \
               printf '%s\\n' '────────────────' ' Bash command' ' approve this?' '╌╌╌╌╌╌╌╌╌╌' \" $c\" '╌╌╌╌╌╌╌╌╌╌' ' Do you want to proceed?' ' ❯ 1. Yes' '   2. No'; \
               a=$(dd bs=1 count=1 2>/dev/null); echo \"answer $a to $c\"; done\n\
             [ -n \"$FAKE_SH\" ] && [ \"$AMUX_RUN\" = \"$FAKE_SH_IN\" ] && sh \"$FAKE_SH\"\n\
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
fn maps_a_folder_to_an_account() {
    let s = Sandbox::new("map");
    s.amux(&["account", "add", "fake", "a", "--no-login"]);
    s.amux(&["account", "add", "fake", "b", "--no-login"]);
    s.amux(&["account", "default", "fake@a"]);
    let app = s.home.join("src/app");
    let deep = app.join("web");
    fs::create_dir_all(&deep).unwrap();
    let acct = |dir: &Path, who: &str| {
        let r = s.amux_with(&[who], &[], Some(dir));
        assert_eq!(r.code, 0, "{}", r.err);
        lines(&r.out, "ENV AMUX_ACCOUNT=").join(",")
    };

    assert!(s.amux(&["map"]).out.contains("No folders mapped"));
    let r = s.amux(&["map", app.to_str().unwrap(), "fake@nope"]);
    assert_eq!(r.code, 1);
    assert!(r.err.contains("No account \"fake@nope\""), "{}", r.err);
    let r = s.amux_with(&["map", ".", "fake@b"], &[], Some(&app));
    assert_eq!(r.code, 0, "{}", r.err);
    assert!(s.amux(&["map"]).out.contains("~/src/app  fake@b"));

    // The map beats the default, in the folder and below; a named account beats the map.
    assert_eq!(acct(&deep, "fake"), "fake@b");
    assert_eq!(acct(&s.home, "fake"), "fake@a");
    assert_eq!(acct(&deep, "fake@a"), "fake@a");
    let r = s.amux_with(&["spawn", "fake"], &[], Some(&deep));
    assert!(r.err.contains("as fake@b (map for ~/src/app)"), "{}", r.err);
    wait_ps(&s, "fake-1", "exited");

    // Removing the account drops its mappings; --rm removes one by hand.
    s.amux_with(&["map", ".", "fake@a"], &[], Some(&deep));
    s.amux(&["account", "rm", "fake@b"]);
    assert!(!s.amux(&["map"]).out.contains("fake@b"));
    assert_eq!(acct(&deep, "fake"), "fake@a");
    let r = s.amux(&["map", "--rm", deep.to_str().unwrap()]);
    assert_eq!(r.code, 0, "{}", r.err);
    assert!(s.amux(&["map"]).out.contains("No folders mapped"));
}

#[test]
fn doctor_skips_clis_you_have_no_accounts_for() {
    let s = Sandbox::new("doctor");
    // A provider you defined yourself, whose binary is missing: that one is a problem.
    let cfg = s.amux_home.join("config.json");
    let mut c: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&cfg).unwrap()).unwrap();
    c["providers"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!({
            "id": "ghost", "label": "Ghost", "bin": "ghost-cli-amux-test",
            "sharedHome": s.home.join(".ghost"), "homeEnv": "GHOST_HOME"
        }));
    write(&cfg, &c.to_string());
    s.amux(&["account", "add", "fake", "a", "--no-login"]);
    let empty = s.home.join("empty-path");
    fs::create_dir_all(&empty).unwrap();
    let r = s.amux_with(&["doctor"], &[("PATH", empty.to_str().unwrap())], None);
    assert!(
        r.out
            .contains("--   qwen: binary \"qwen\" not installed (no accounts)"),
        "{}",
        r.out
    );
    assert!(r.out.contains("ok   fake: binary"), "{}", r.out);
    assert!(!r.out.contains("FAIL qwen"), "{}", r.out);
    assert!(
        r.out
            .contains("FAIL ghost: binary \"ghost-cli-amux-test\" not found"),
        "{}",
        r.out
    );
    assert!(!r.out.contains("--   ghost"), "{}", r.out);
    assert_ne!(r.code, 0);
    assert!(s.amux(&["providers"]).out.contains("qwen"));
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

/// How long `wait_ps` and `eventually` wait: generous for slow CI runners, and they
/// return as soon as the condition holds.
const WAIT: std::time::Duration = std::time::Duration::from_secs(15);

/// Polls `amux ps` until the run's row contains `want`.
fn wait_ps(s: &Sandbox, run: &str, want: &str) -> String {
    let deadline = std::time::Instant::now() + WAIT;
    while std::time::Instant::now() < deadline {
        let ps = s.amux(&["ps"]).out;
        if ps.lines().any(|l| l.starts_with(run) && l.contains(want)) {
            return ps;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    panic!("{run} never showed {want:?}:\n{}", s.amux(&["ps"]).out);
}

#[test]
fn ps_prints_no_terminal_escapes_from_a_runs_args_or_branch() {
    let s = Sandbox::new("escps");
    s.amux(&["account", "add", "fake", "one", "--no-login"]);
    let evil = "\x1b]0;pwned\x07\x1b[2Jhi\u{9b}31m";
    let r = s.amux(&["spawn", "--name", "e", "fake", evil]);
    assert_eq!(r.code, 0, "{}", r.err);
    wait_ps(&s, "e", "exited");
    let file = s.amux_home.join("runs/e/run.json");
    let mut run: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&file).unwrap()).unwrap();
    run["worktree"] = serde_json::json!({
        "repo": "/nonexistent", "path": "/nonexistent/e",
        "branch": "amux/e\x1b[31m\x1b]8;;http://x\x07"
    });
    fs::write(&file, run.to_string()).unwrap();
    let ps = s.amux(&["ps"]).out;
    assert!(ps.contains("hi"), "{ps}");
    for c in ['\x1b', '\x07', '\u{9b}'] {
        assert!(!ps.contains(c), "{ps:?}");
    }
    // JSON escapes them instead.
    let json = s.amux(&["ps", "--json"]).out;
    assert!(!json.contains('\x1b') && json.contains("\\u001b"), "{json}");
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

/// `amux peek` and `amux send` ask the host for its screen alone, so a run that never
/// stops printing doesn't hold them up until it goes quiet.
#[test]
fn peek_and_send_dont_wait_for_a_busy_run_to_go_quiet() {
    let s = Sandbox::new("busy");
    let bin = s.home.join("bin/busy-cli");
    write(
        &bin,
        "#!/bin/sh\nprintf '\\033[?2004h'\ni=0; while true; do i=$((i+1)); echo \"tick $i\"; sleep 0.02; done\n",
    );
    fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
    let cfg = s.amux_home.join("config.json");
    let mut c: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&cfg).unwrap()).unwrap();
    c["providers"][0]["bin"] = bin.to_str().unwrap().into();
    write(&cfg, &c.to_string());
    s.amux(&["account", "add", "fake", "one", "--no-login"]);
    assert_eq!(s.amux(&["spawn", "--name", "busy", "fake"]).code, 0);
    wait_ps(&s, "busy", "running");
    std::thread::sleep(std::time::Duration::from_millis(300));
    let t = std::time::Instant::now();
    let peek = s.amux(&["peek", "busy"]);
    assert!(peek.out.contains("tick"), "{}", peek.out);
    assert!(
        t.elapsed() < std::time::Duration::from_millis(1500),
        "peek took {:?}",
        t.elapsed()
    );
    // One line: no need to look at the screen. Several: a quick look for bracketed paste.
    for text in ["hello", "two\nlines"] {
        let t = std::time::Instant::now();
        assert_eq!(s.amux(&["send", "busy", text]).code, 0);
        assert!(
            t.elapsed() < std::time::Duration::from_millis(1500),
            "send took {:?}",
            t.elapsed()
        );
    }
    s.amux(&["stop", "busy"]);
}

/// A client that never reads (a frozen terminal, a stopped preview) doesn't hold up the
/// output to the others: the host queues output per client and drops one that falls
/// too far behind.
#[test]
fn a_client_that_never_reads_doesnt_stall_the_others() {
    use std::io::{Read, Write};
    let s = Sandbox::new("stuck");
    let bin = s.home.join("bin/stream-cli");
    let line = "x".repeat(1000);
    write(
        &bin,
        &format!(
            "#!/bin/sh\ni=0; while true; do i=$((i+1)); echo \"$i {line}\"; sleep 0.01; done\n"
        ),
    );
    fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
    let cfg = s.amux_home.join("config.json");
    let mut c: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&cfg).unwrap()).unwrap();
    c["providers"][0]["bin"] = bin.to_str().unwrap().into();
    write(&cfg, &c.to_string());
    s.amux(&["account", "add", "fake", "one", "--no-login"]);
    assert_eq!(s.amux(&["spawn", "--name", "st", "fake"]).code, 0);
    wait_ps(&s, "st", "running");
    let sock = s.amux_home.join("runs/st/sock");
    let watch = || {
        let mut c = std::os::unix::net::UnixStream::connect(&sock).unwrap();
        c.write_all(&[2, 0, 0, 0, 1, 1]).unwrap();
        c
    };
    let _stuck = watch();
    let mut reader = watch();
    reader
        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .unwrap();
    let mut buf = vec![0u8; 64 * 1024];
    let (mut last, mut gap, mut total) = (std::time::Instant::now(), std::time::Duration::ZERO, 0);
    let end = last + std::time::Duration::from_secs(6);
    while std::time::Instant::now() < end {
        let n = reader.read(&mut buf).unwrap();
        assert!(n > 0, "the host closed the reading client");
        total += n;
        gap = gap.max(last.elapsed());
        last = std::time::Instant::now();
    }
    // The stuck client's socket buffer fills within the first tens of KB; before, every
    // write to it then held up everyone for 2 seconds. Slow CI runners print less, so this
    // only checks that output kept coming; the gap is what matters.
    assert!(
        total > 50_000,
        "read only {total} bytes (longest gap {gap:?})"
    );
    assert!(
        gap < std::time::Duration::from_millis(500),
        "a {gap:?} stall"
    );
    s.amux(&["stop", "st"]);
}

/// `spawn --sandbox` with a fake container engine: it records its arguments, answers
/// `image inspect` and `rm`, and for `run` starts the fake CLI named after the image.
#[test]
fn runs_the_cli_in_a_sandbox_container() {
    let s = Sandbox::new("sandbox");
    let engine = s.home.join("bin/fake-engine");
    write(
        &engine,
        "#!/bin/sh\necho \"$*\" >> \"$HOME/engine.log\"\n\
         case \"$1\" in\n\
           image) [ \"$3\" = amux-test:1 ] && exit 0; exit 1;;\n\
           ps) echo c0ffee; exit 0;;\n\
           rm) exit 0;;\n\
           run) shift; while [ $# -gt 0 ]; do a=$1; shift; [ \"$a\" = amux-test:1 ] && break; done\n\
                PATH=\"$(dirname \"$0\"):$PATH\" exec \"$@\";;\n\
         esac\nexit 2\n",
    );
    fs::set_permissions(&engine, fs::Permissions::from_mode(0o755)).unwrap();
    let cfg = s.amux_home.join("config.json");
    let mut c: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&cfg).unwrap()).unwrap();
    c["providers"][0]["loginFiles"] = serde_json::json!(["auth.json"]);
    c["sandbox"] = serde_json::json!({ "engine": engine });
    write(&cfg, &c.to_string());
    s.amux(&["account", "add", "fake", "one", "--no-login"]);
    // Outside any repository, so only the folder itself is mounted.
    let proj = s.home.join("proj");
    fs::create_dir_all(&proj).unwrap();
    let here = |a: &[&str]| s.amux_with(a, &[], Some(&proj));

    // No image: refused, and nothing is left behind.
    let r = here(&["spawn", "--sandbox", "fake"]);
    assert_ne!(r.code, 0);
    assert!(r.err.contains("needs an image"), "{}", r.err);
    // An engine that isn't installed.
    let r = here(&[
        "spawn",
        "--sandbox=no-such-engine-amux",
        "--image",
        "x",
        "fake",
    ]);
    assert!(r.err.contains("isn't installed"), "{}", r.err);
    // An image that isn't on the machine is never pulled.
    let r = here(&["spawn", "--image", "other:2", "fake"]);
    assert!(r.err.contains("doesn't pull images"), "{}", r.err);
    // A login the container can't read: no token and no login file.
    let r = here(&["spawn", "--image", "amux-test:1", "fake"]);
    assert!(r.err.contains("amux account token fake@one"), "{}", r.err);
    assert!(s.amux(&["ps"]).out.contains("No runs"));

    write(&s.overlay("one").join("auth.json"), "{}");
    let r = here(&[
        "spawn",
        "--image",
        "amux-test:1",
        "--network",
        "off",
        "fake",
        "--model",
        "x",
    ]);
    assert_eq!(r.code, 0, "{}", r.err);
    assert!(
        r.err
            .contains("sandboxed in fake-engine (amux-test:1, no network)"),
        "{}",
        r.err
    );
    let ps = wait_ps(&s, "fake-1", "exited 0");
    assert!(ps.contains("fake@one [sandbox]"), "{ps}");
    let logs = s.amux(&["logs", "fake-1"]).out;
    assert!(logs.contains("ARG --model"), "{logs}");
    assert!(logs.contains("ENV AMUX_RUN=fake-1"), "{logs}");
    assert!(logs.contains("TTY yes"), "{logs}");

    let log = fs::read_to_string(s.home.join("engine.log")).unwrap();
    let run = log.lines().find(|l| l.starts_with("run ")).unwrap();
    let overlay = s.overlay("one");
    let (o, sh) = (overlay.display(), s.shared.display());
    let uid = String::from_utf8(Command::new("id").arg("-u").output().unwrap().stdout).unwrap();
    let saved: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(s.amux_home.join("runs/fake-1/run.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(saved["sandbox"]["image"], "amux-test:1");
    assert_eq!(saved["sandbox"]["network"], false);
    // The container's name and label carry this amux home's id, saved for revive and stop.
    let home = saved["sandbox"]["home"].as_str().unwrap();
    assert_eq!(home.len(), 8, "{saved}");
    assert_eq!(saved["sandbox"]["name"], format!("amux-{home}-fake-1"));
    for want in [
        format!(
            "run --rm -it --init --pull never --name amux-{home}-fake-1 \
             --label amux.run=fake-1 --label amux.home={home}"
        ),
        format!("--user {}:", uid.trim()),
        "--network none".into(),
        format!("-w {}", proj.display()),
        format!("-v {o}:{o}"),
        format!("-v {sh}:{sh}"),
        "-e FAKE_HOME -e AMUX_ACCOUNT".into(),
        "-e AMUX_RUN".into(),
        "-e HOME=/tmp".into(),
        "amux-test:1 fake-cli --model x".into(),
    ] {
        assert!(run.contains(&want), "{want:?} not in {run}");
    }
    // Only a container with this run's and this home's labels is removed, by its id.
    assert!(
        log.contains(&format!(
            "ps -a -q --filter label=amux.run=fake-1 --filter label=amux.home={home}"
        )),
        "{log}"
    );
    assert!(log.contains("rm -f c0ffee"), "{log}");
    assert!(!log.contains("rm -f amux-"), "{log}");

    // stop ends a sandboxed run like any other; --no-sandbox runs on the machine.
    let chat = [("FAKE_CHAT", "1")];
    let r = s.amux_with(
        &[
            "spawn",
            "--sandbox",
            "--image",
            "amux-test:1",
            "--name",
            "chat",
            "fake",
        ],
        &chat,
        Some(&proj),
    );
    assert_eq!(r.code, 0, "{}", r.err);
    wait_ps(&s, "chat", "running");
    let stop = s.amux(&["stop", "chat"]);
    assert!(stop.out.contains("Stopped chat"), "{}", stop.out);
    let r = here(&["spawn", "--no-sandbox", "--name", "plain", "fake"]);
    assert_eq!(r.code, 0, "{}", r.err);
    assert!(!r.err.contains("sandboxed"), "{}", r.err);
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

/// Polls until `f` holds, for up to `WAIT`.
fn eventually(what: &str, mut f: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + WAIT;
    while std::time::Instant::now() < deadline {
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
fn lists_and_drops_hunks_from_the_cli_and_the_ui() {
    let s = Sandbox::new("hunks");
    s.amux(&["account", "add", "fake", "one", "--no-login"]);
    let repo = s.home.join("repo");
    let original: String = (1..=20).map(|i| format!("line {i}\n")).collect();
    write(&repo.join("a.rs"), &original);
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
        &["spawn", "--name", "dr", "--worktree", "fake"],
        &[("FAKE_CHAT", "1")],
        Some(&repo),
    );
    assert_eq!(r.code, 0, "{}", r.err);
    wait_ps(&s, "dr", "running");
    let wt = s.amux_home.join("worktrees/repo-dr");
    let changed = original
        .replace("line 2\n", "line two\n")
        .replace("line 18\n", "line eighteen\nline 18b\n");
    write(&wt.join("a.rs"), &changed);
    write(&wt.join("new.txt"), "brand new\n");

    let list = s.amux(&["review", "dr", "--hunks"]);
    assert_eq!(list.code, 0, "{}", list.err);
    assert!(list.out.contains("a.rs\n    1  2 "), "{}", list.out);
    assert!(list.out.contains("    2  18-19 "), "{}", list.out);
    assert!(
        list.out.contains("new.txt  (new, untracked)"),
        "{}",
        list.out
    );
    assert!(s.amux(&["review", "dr", "--drop", "a.rs:3"]).code != 0);

    // The fake agent may still look busy: without --force that is refused.
    let mut d = s.amux(&["review", "dr", "--drop", "a.rs:2"]);
    if d.code != 0 {
        assert!(d.err.contains("once its turn ends"), "{}", d.err);
        d = s.amux(&["review", "dr", "--drop", "a.rs:2", "--force"]);
    }
    assert_eq!(d.code, 0, "{}", d.err);
    assert!(
        d.out.contains("Dropped 1 hunk in 1 file from dr."),
        "{}",
        d.out
    );
    assert!(d.out.contains("uncommitted"), "{}", d.out);
    let now = fs::read_to_string(wt.join("a.rs")).unwrap();
    assert!(
        now.contains("line two\n") && !now.contains("eighteen"),
        "{now}"
    );
    // The agent hears what was dropped.
    let mut seen = String::new();
    for _ in 0..100 {
        seen = s.amux(&["logs", "dr"]).out;
        if seen.contains("a.rs:18-19") {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(seen.contains("got I reviewed your changes"), "{seen}");
    assert!(seen.contains("a.rs:18-19"), "{seen}");
    s.amux(&["stop", "dr"]);
    wait_ps(&s, "dr", "exited");

    // In the UI: space marks the hunk under the cursor, D and y drop it.
    let ui = |keys: &str| {
        s.amux(&["ui", "--dump", "--size", "30x120", "--keys", keys])
            .out
    };
    let marked = ui("c ");
    assert!(marked.contains("1 hunk in 1 file to drop"), "{marked}");
    assert!(marked.contains("✗ drop"), "{marked}");
    let asked = ui("c D");
    assert!(
        asked.contains("Drop 1 hunk in 1 file from dr's worktree?"),
        "{asked}"
    );
    let dropped = ui("c Dy");
    assert!(
        dropped.contains("Dropped 1 hunk in 1 file from dr"),
        "{dropped}"
    );
    assert_eq!(fs::read_to_string(wt.join("a.rs")).unwrap(), original);
    // A new untracked file goes whole; with nothing left, the review closes.
    let last = ui("c Dy");
    assert!(last.contains("Dropped 1 hunk in 1 file from dr"), "{last}");
    assert!(!wt.join("new.txt").exists());
    assert!(s.amux(&["review", "dr"]).out.contains("no hunks to drop"));
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

/// A stand-in for ssh in `local`: runs the remote command as "the other machine"
/// (`remote`, a second sandbox), the way sshd runs it in the user's shell there.
fn fake_ssh(local: &Sandbox, remote: &Sandbox) -> PathBuf {
    let ssh = local.home.join("bin/ssh");
    write(
        &ssh,
        &format!(
            "#!/bin/sh\nfor a; do last=\"$a\"; done\nexport HOME={h} AMUX_HOME={a}\nexec sh -c \"$last\"\n",
            h = remote.home.display(),
            a = remote.amux_home.display()
        ),
    );
    fs::set_permissions(&ssh, fs::Permissions::from_mode(0o755)).unwrap();
    ssh
}

#[test]
fn runs_and_drives_agents_on_another_machine_over_ssh() {
    let s = Sandbox::new("remote-local");
    let r = Sandbox::new("remote-box");
    r.amux(&["account", "add", "fake", "one", "--no-login"]);
    let ssh = fake_ssh(&s, &r);
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

#[test]
fn reviews_comments_on_and_drops_hunks_of_a_run_on_another_machine() {
    let s = Sandbox::new("rreview-local");
    let r = Sandbox::new("rreview-box");
    r.amux(&["account", "add", "fake", "one", "--no-login"]);
    let ssh = fake_ssh(&s, &r);
    let env = [("AMUX_SSH", ssh.to_str().unwrap()), ("FAKE_CHAT", "1")];
    let bin = env!("CARGO_BIN_EXE_amux");
    assert_eq!(
        s.amux_with(
            &["machine", "add", "box", "me@box", "--amux", bin],
            &env,
            None
        )
        .code,
        0
    );
    // The repo is on the box.
    let repo = r.home.join("repo");
    let original: String = (1..=20).map(|i| format!("line {i}\n")).collect();
    write(&repo.join("a.rs"), &original);
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
    let on = format!("box:{}", repo.display());
    let sp = s.amux_with(
        &["spawn", "--on", &on, "--name", "w", "--worktree", "fake"],
        &env,
        None,
    );
    assert_eq!(sp.code, 0, "{}{}", sp.out, sp.err);
    wait_ps(&r, "w", "running");
    let wt = r.amux_home.join("worktrees/repo-w");
    let changed = original
        .replace("line 2\n", "line two\n")
        .replace("line 18\n", "line eighteen\n");
    write(&wt.join("a.rs"), &changed);
    write(&wt.join("new.txt"), "brand new\n");
    let ui = |keys: &str| {
        let o = s.amux_with(
            &[
                "ui", "--dump", "--size", "30x120", "--select", "box:w", "--keys", keys,
            ],
            &env,
            None,
        );
        format!("{}{}", o.out, o.err)
    };

    // c opens the review through amux on the box; ⏎ comments on the line there.
    let commented = ui("c\nkeep two\n");
    assert!(
        commented.contains("Review box:w · 2 files · 1 comment to send"),
        "{commented}"
    );
    assert!(commented.contains("+line two"), "{commented}");
    let there = r.amux(&["comments", "w"]).out;
    assert!(
        there.contains("1. a.rs:1  [pending]\n    > line 1\n    keep two"),
        "{there}"
    );
    // x deletes it there.
    let deleted = ui("cx");
    assert!(deleted.contains("Deleted 1 comment."), "{deleted}");
    assert!(r.amux(&["comments", "w"]).out.contains("No comments"));
    // `amux review` goes to the box too.
    let list = s.amux_with(&["review", "box:w", "--hunks"], &env, None);
    assert!(
        list.out.contains("new.txt  (new, untracked)"),
        "{}",
        list.out
    );
    // A drop of a diff that isn't the one reviewed is refused, changing nothing.
    let stale = s.amux_with(
        &[
            "review", "box:w", "--drop", "a.rs:1", "--expect", "0", "--force",
        ],
        &env,
        None,
    );
    assert!(
        stale.code != 0 && stale.err.contains("no longer the ones you reviewed"),
        "{}",
        stale.err
    );
    assert_eq!(fs::read_to_string(wt.join("a.rs")).unwrap(), changed);

    // Space marks the hunk under the cursor, D and y drop it on the box.
    assert_eq!(s.amux_with(&["stop", "box:w"], &env, None).code, 0);
    wait_ps(&r, "w", "exited");
    let asked = ui("c D");
    assert!(
        asked.contains("Drop 1 hunk in 1 file from box:w's worktree?"),
        "{asked}"
    );
    let dropped = ui("c Dy");
    assert!(
        dropped.contains("Dropped 1 hunk in 1 file from box:w"),
        "{dropped}"
    );
    // The review stays open, read again: one hunk fewer.
    assert!(dropped.contains("Review box:w · 2 files"), "{dropped}");
    let now = fs::read_to_string(wt.join("a.rs")).unwrap();
    assert!(
        now.contains("line 2\n") && now.contains("line eighteen\n"),
        "{now}"
    );
    // The undo patch is kept on the box.
    let undo = fs::read_dir(r.amux_home.join("runs/w/dropped"))
        .unwrap()
        .count();
    assert_eq!(undo, 1);

    // amux on the box from before remote reviews: its diff in the pager instead.
    let old = r.home.join("bin/old-amux");
    write(
        &old,
        &format!(
            "#!/bin/sh\nif [ \"$1\" = __review-data ]; then echo \"amux: Unknown command \\\"$1\\\". See \\`amux help\\`.\" >&2; exit 1; fi\nexec {bin} \"$@\"\n"
        ),
    );
    fs::set_permissions(&old, fs::Permissions::from_mode(0o755)).unwrap();
    let add = s.amux_with(
        &[
            "machine",
            "add",
            "old",
            "me@box",
            "--amux",
            old.to_str().unwrap(),
        ],
        &env,
        None,
    );
    assert_eq!(add.code, 0, "{}{}", add.out, add.err);
    let o = s.amux_with(
        &[
            "ui", "--dump", "--size", "30x120", "--select", "old:w", "--keys", "c",
        ],
        &env,
        None,
    );
    assert!(
        o.out.contains("amux on old is too old to review"),
        "{}",
        o.out
    );
    assert!(!o.out.contains("Review old:w"), "{}", o.out);
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
    // Two phones watching: they share one `amux events`, and both get each line.
    let open_events = || {
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
        es
    };
    let streams = [open_events(), open_events()];
    std::thread::sleep(std::time::Duration::from_millis(600));
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
    for mut es in streams {
        use std::io::Read;
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
    // But a folder mapped to the limited account never moves to another one: it stops
    // and says how to choose.
    let work = s.home.join("work");
    fs::create_dir_all(&work).unwrap();
    assert_eq!(
        s.amux_with(&["map", ".", "fake@one"], &[], Some(&work))
            .code,
        0
    );
    let r = s.amux_with(&["spawn", "--name", "mapped", "fake"], &[], Some(&work));
    assert_ne!(r.code, 0);
    assert!(
        r.err.contains("fake@one is limited")
            && r.err.contains("until ")
            && r.err.contains("`amux map` chose it for ~/work")
            && r.err.contains("amux spawn … fake@<name>")
            && !r.err.contains("using fake@two"),
        "{}",
        r.err
    );
    assert!(!s.amux(&["ps"]).out.contains("mapped"));
    s.amux(&["map", "--rm", work.to_str().unwrap()]);

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

    // The UI and `amux open` show the port once a dev server listens on it.
    let opened = s.home.join("opened");
    let browser = s.home.join("bin/browser");
    write(
        &browser,
        &format!("#!/bin/sh\necho \"$1\" > {}\n", opened.display()),
    );
    fs::set_permissions(&browser, fs::Permissions::from_mode(0o755)).unwrap();
    let env = [("BROWSER", browser.to_str().unwrap())];
    let r = s.amux_with(&["open", "p1"], &env, None);
    assert_eq!(r.code, 1);
    assert!(
        r.err.contains("Nothing is listening on port 41000"),
        "{}",
        r.err
    );
    // Something else on this machine may hold 41000; then there's nothing to check.
    if let Ok(server) = std::net::TcpListener::bind("127.0.0.1:41000") {
        let ui = s.amux(&["ui", "--dump", "--size", "24x140", "--select", "p1"]);
        assert!(ui.out.contains("localhost:41000 (o opens)"), "{}", ui.out);
        assert!(ui.out.contains("· :41000"), "{}", ui.out);
        assert!(!ui.out.contains(":41010"), "p2 serves nothing: {}", ui.out);
        let r = s.amux_with(&["open", "p1"], &env, None);
        assert_eq!(r.code, 0, "{}", r.err);
        assert_eq!(r.out, "http://localhost:41000\n");
        assert_eq!(
            fs::read_to_string(&opened).unwrap(),
            "http://localhost:41000\n"
        );
        drop(server);
    }

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

/// A stand-in push service: answers 410 (gone) for paths starting /gone, 201 otherwise,
/// and keeps each request's head and raw body.
fn push_service() -> (String, std::sync::mpsc::Receiver<(String, Vec<u8>)>) {
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
            let status = if head.starts_with("POST /gone") {
                "410 Gone"
            } else {
                "201 Created"
            };
            let _ = (&s).write_all(
                format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                    .as_bytes(),
            );
            let _ = tx.send((head, body));
        }
    });
    (addr, rx)
}

/// What a browser does with a push message: RFC 8291 decryption with the subscription's
/// private key and auth secret.
fn webpush_decrypt(
    ua: ring::agreement::EphemeralPrivateKey,
    auth: &[u8],
    body: &[u8],
) -> serde_json::Value {
    use ring::{aead, agreement, hkdf};
    struct Len(usize);
    impl hkdf::KeyType for Len {
        fn len(&self) -> usize {
            self.0
        }
    }
    let expand = |prk: &hkdf::Prk, info: &[u8], n: usize| {
        let mut out = vec![0u8; n];
        prk.expand(&[info], Len(n)).unwrap().fill(&mut out).unwrap();
        out
    };
    let ua_public = ua.compute_public_key().unwrap().as_ref().to_vec();
    let (salt, rs, idlen) = (&body[..16], &body[16..20], body[20] as usize);
    assert_eq!(rs, &4096u32.to_be_bytes());
    let as_public = &body[21..21 + idlen];
    let peer = agreement::UnparsedPublicKey::new(&agreement::ECDH_P256, as_public);
    let secret = agreement::agree_ephemeral(ua, &peer, |s| s.to_vec()).unwrap();
    let prk_key = hkdf::Salt::new(hkdf::HKDF_SHA256, auth).extract(&secret);
    let mut info = b"WebPush: info\0".to_vec();
    info.extend_from_slice(&ua_public);
    info.extend_from_slice(as_public);
    let ikm = expand(&prk_key, &info, 32);
    let prk = hkdf::Salt::new(hkdf::HKDF_SHA256, salt).extract(&ikm);
    let cek = expand(&prk, b"Content-Encoding: aes128gcm\0", 16);
    let nonce = expand(&prk, b"Content-Encoding: nonce\0", 12);
    let key = aead::LessSafeKey::new(aead::UnboundKey::new(&aead::AES_128_GCM, &cek).unwrap());
    let mut rec = body[21 + idlen..].to_vec();
    let plain = key
        .open_in_place(
            aead::Nonce::try_assume_unique_for_key(&nonce).unwrap(),
            aead::Aad::empty(),
            &mut rec,
        )
        .unwrap();
    assert_eq!(plain.last(), Some(&2), "the last record ends with 0x02");
    serde_json::from_slice(&plain[..plain.len() - 1]).unwrap()
}

#[test]
fn web_push_reaches_the_phone_app_encrypted() {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
    use base64::Engine;
    use ring::rand::SecureRandom;
    let s = Sandbox::new("webpush");
    s.amux(&["account", "add", "fake", "one", "--no-login"]);
    let (push_addr, rx) = push_service();
    let wait = || rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap();

    let out = s.amux(&["phone", "webpush"]);
    assert_eq!(out.code, 0, "{}", out.err);
    assert!(out.out.contains("Add to Home Screen"), "{}", out.out);
    let mode = fs::metadata(s.amux_home.join("webpush-key.json"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o077, 0, "the VAPID key is private");
    let out = s.amux(&["phone", "test"]);
    assert_ne!(out.code, 0);
    assert!(out.err.contains("no device"), "{}", out.err);

    // The app: its service worker is public, the key and subscriptions need the token.
    let (mut srv, printed) = start_serve(
        &s,
        &["--listen", "127.0.0.1:0"],
        &[("AMUX_WEBPUSH_LOCAL_HTTP", "1")],
    );
    let link = printed.lines().find(|l| l.contains("?t=")).unwrap().trim();
    let (base, token) = link.split_once("/?t=").unwrap();
    let addr = base.trim_start_matches("http://").to_string();
    let get = |path: &str, cookie: &str| {
        http(
            &addr,
            &format!("GET {path} HTTP/1.1\r\nHost: x\r\nCookie: {cookie}\r\n\r\n"),
        )
    };
    let cookie = format!("amux={token}");
    let sw = get("/sw.js", "");
    assert!(
        sw.starts_with("HTTP/1.1 200") && sw.contains("showNotification"),
        "{sw}"
    );
    assert!(get("/api/push", "").starts_with("HTTP/1.1 401"));
    let info = get("/api/push", &cookie);
    let info: serde_json::Value =
        serde_json::from_str(info.split_once("\r\n\r\n").unwrap().1).unwrap();
    let server_key = info["publicKey"].as_str().unwrap().to_string();
    assert_eq!(B64.decode(&server_key).unwrap().len(), 65);
    assert_eq!(info["service"], "webpush");

    let rng = ring::rand::SystemRandom::new();
    // A browser subscribing: a fresh P-256 key and auth secret, POSTed to amux serve.
    let subscribe = |path: &str, device: &str| {
        let ua = ring::agreement::EphemeralPrivateKey::generate(&ring::agreement::ECDH_P256, &rng)
            .unwrap();
        let mut auth = [0u8; 16];
        rng.fill(&mut auth).unwrap();
        let body = serde_json::json!({
            "endpoint": format!("{push_addr}{path}"),
            "keys": {
                "p256dh": B64.encode(ua.compute_public_key().unwrap().as_ref()),
                "auth": B64.encode(auth),
            },
            "device": device,
        })
        .to_string();
        let r = http(
            &addr,
            &format!(
                "POST /api/push HTTP/1.1\r\nHost: x\r\nCookie: {cookie}\r\nX-Amux: 1\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            ),
        );
        assert!(r.starts_with("HTTP/1.1 200"), "{r}");
        (ua, auth)
    };
    let (ua, auth) = subscribe("/push/phone", "iPhone");
    subscribe("/gone/old-laptop", "Mac");
    let subs = fs::read_to_string(s.amux_home.join("webpush-subs.json")).unwrap();
    assert!(subs.contains("/push/phone") && subs.contains("/gone/old-laptop"));
    let mode = fs::metadata(s.amux_home.join("webpush-subs.json"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o077, 0, "subscriptions are private");
    assert!(s.amux(&["phone"]).out.contains("Devices: 2 (iPhone, Mac)"));

    // `amux phone test`: one encrypted message to each device; the gone one is removed.
    let out = s.amux(&["phone", "test"]);
    assert_eq!(out.code, 0, "{}", out.err);
    let (mut a, b) = (wait(), wait());
    if !a.0.starts_with("POST /push/phone") {
        a = b;
    }
    let (head, body) = a;
    let lower = head.to_ascii_lowercase();
    for h in [
        "ttl: 43200",
        "urgency: high",
        "content-encoding: aes128gcm",
        "content-type: application/octet-stream",
    ] {
        assert!(lower.contains(h), "{h} in {head}");
    }
    let vapid = head
        .lines()
        .find_map(|l| l.strip_prefix("Authorization: vapid t="))
        .unwrap_or_else(|| panic!("{head}"));
    let (jwt, k) = vapid.trim().split_once(", k=").unwrap();
    assert_eq!(k, server_key);
    let parts: Vec<&str> = jwt.split('.').collect();
    let claims: serde_json::Value = serde_json::from_slice(&B64.decode(parts[1]).unwrap()).unwrap();
    assert_eq!(claims["aud"], push_addr.as_str());
    assert_eq!(claims["sub"], "https://github.com/quynb93/amux");
    ring::signature::UnparsedPublicKey::new(
        &ring::signature::ECDSA_P256_SHA256_FIXED,
        B64.decode(k).unwrap(),
    )
    .verify(
        format!("{}.{}", parts[0], parts[1]).as_bytes(),
        &B64.decode(parts[2]).unwrap(),
    )
    .unwrap();
    let msg = webpush_decrypt(ua, &auth, &body);
    assert_eq!(msg["title"], "amux: test");
    assert_eq!(msg["body"], "Notifications from amux reach this phone.");
    assert_eq!(msg["url"], format!("{base}/#run=test"));
    let subs = fs::read_to_string(s.amux_home.join("webpush-subs.json")).unwrap();
    assert!(
        subs.contains("/push/phone") && !subs.contains("/gone/"),
        "{subs}"
    );

    // A run that starts waiting while nobody watches it.
    let (ua, auth) = subscribe("/push/phone", "iPhone");
    let env = [("FAKE_CHAT", "1"), ("FAKE_ASK", "1")];
    assert_eq!(
        s.amux_with(&["spawn", "--name", "ask", "fake"], &env, None)
            .code,
        0
    );
    wait_ps(&s, "ask", "waiting");
    let (head, body) = wait();
    assert!(
        head.to_ascii_lowercase().contains("urgency: high"),
        "{head}"
    );
    let msg = webpush_decrypt(ua, &auth, &body);
    assert!(
        msg["body"]
            .as_str()
            .unwrap()
            .contains("Waiting for you: approve THIS?"),
        "{msg}"
    );
    assert_eq!(msg["url"], format!("{base}/#run=ask"));
    assert_eq!(msg["tag"], "ask");

    // Brief: only the run's name and event; an exit isn't urgent.
    let (ua, auth) = subscribe("/push/phone", "iPhone");
    assert_eq!(s.amux(&["phone", "brief", "on"]).code, 0);
    assert_eq!(s.amux(&["phone", "events", "exited"]).code, 0);
    s.amux(&["send", "ask", "quit"]);
    wait_ps(&s, "ask", "exited 7");
    let (head, body) = wait();
    assert!(
        head.to_ascii_lowercase().contains("urgency: normal"),
        "{head}"
    );
    let msg = webpush_decrypt(ua, &auth, &body);
    assert_eq!(msg["body"], "ask: exited", "{msg}");

    // Turning it off on the phone removes the device.
    let body = serde_json::json!({ "endpoint": format!("{push_addr}/push/phone") }).to_string();
    let r = http(
        &addr,
        &format!(
            "POST /api/push/remove HTTP/1.1\r\nHost: x\r\nCookie: {cookie}\r\nX-Amux: 1\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        ),
    );
    assert!(r.starts_with("HTTP/1.1 200"), "{r}");
    assert!(s.amux(&["phone"]).out.contains("No device"));
    let _ = srv.kill();
    let _ = srv.wait();
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
        if line.contains("keep it private") || line.contains("vouch for other sites") {
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
    assert!(
        out.contains("fingerprint") && out.contains("--tailscale"),
        "{out}"
    );
    assert!(out.contains("not a CA"), "{out}");
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
fn updates_prompt_patterns_without_a_release() {
    let s = Sandbox::new("patterns");
    let path = "/quynb93/amux/main/patterns.json".to_string();
    let good = file_server(vec![(
        path.clone(),
        br#"{"claude": {"waitingPatterns": ["Shall I go ahead"]}, "newcli": {"waitingPatterns": ["Go on?"]}}"#
            .to_vec(),
    )]);
    let bad = file_server(vec![(
        path.clone(),
        br#"{"claude": {"bin": "/tmp/not-claude"}}"#.to_vec(),
    )]);
    let saved = s.amux_home.join("patterns.json");
    let update =
        |url: &str| s.amux_with(&["update", "--patterns"], &[("AMUX_GITHUB_URL", url)], None);

    let r = update(&good);
    assert_eq!(r.code, 0, "{}", r.err);
    assert!(
        r.out
            .contains("Updated the prompt and limit patterns for claude"),
        "{}",
        r.out
    );
    assert!(r.out.contains("Skipped newcli"), "{}", r.out);
    let text = fs::read_to_string(&saved).unwrap();
    assert!(
        text.contains("Shall I go ahead") && !text.contains("newcli"),
        "{text}"
    );
    assert!(update(&good).out.contains("up to date"));

    // A file that doesn't check out is never saved.
    let r = update(&bad);
    assert_eq!(r.code, 1);
    assert!(
        r.err.contains("only waitingPatterns and limitPatterns"),
        "{}",
        r.err
    );
    assert_eq!(fs::read_to_string(&saved).unwrap(), text);

    // New runs use the fetched patterns; the rest of the built-in stays.
    let cfg = s.amux_home.join("config.json");
    let mut c: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&cfg).unwrap()).unwrap();
    let fake = c["providers"][0]["bin"].clone();
    c["providers"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!({"id": "claude", "bin": fake}));
    write(&cfg, &c.to_string());
    s.amux(&["account", "add", "claude", "w", "--no-login"]);
    let r = s.amux(&["spawn", "--name", "c", "claude"]);
    assert_eq!(r.code, 0, "{}", r.err);
    let run: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(s.amux_home.join("runs/c/run.json")).unwrap())
            .unwrap();
    assert_eq!(
        run["waitingPatterns"],
        serde_json::json!(["Shall I go ahead"])
    );
    assert!(run["limitPatterns"]
        .as_array()
        .unwrap()
        .contains(&"You've hit your limit".into()));

    // config.json beats the fetched file.
    c["providers"][1]["waitingPatterns"] = serde_json::json!(["My own prompt"]);
    write(&cfg, &c.to_string());
    s.amux(&["spawn", "--name", "d", "claude"]);
    let run: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(s.amux_home.join("runs/d/run.json")).unwrap())
            .unwrap();
    assert_eq!(run["waitingPatterns"], serde_json::json!(["My own prompt"]));
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
    // 12 has a run already (started by hand above), so only 13 would start.
    assert!(
        !dry.out.contains("issue-12") && dry.out.contains("Would start issue-13"),
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

/// Auto-approve reads the screen only when a prompt is likely, to save the cost of
/// rendering it for every chunk. A prompt drawn right after streaming output, with its
/// words apart (cursor moves instead of spaces, so no chunk holds "Do you want"), is
/// still answered once the output goes quiet.
#[test]
fn auto_approve_answers_a_prompt_drawn_mid_stream() {
    let s = Sandbox::new("allow2");
    let bin = s.home.join("bin/perm-cli");
    write(
        &bin,
        "#!/bin/sh\nstty -icanon -echo\ni=0; while [ $i -lt 50 ]; do i=$((i+1)); echo \"building $i\"; sleep 0.01; done\n\
         printf '%s\\n' '────────────────' ' Bash command' '╌╌╌╌╌╌╌╌╌╌' ' cargo test --all' '╌╌╌╌╌╌╌╌╌╌'\n\
         printf ' Do\\033[1Cyou\\033[1Cwant\\033[1Cto\\033[1Cproceed?\\r\\n ❯ 1.\\033[1CYes\\r\\n   2. No\\r\\n'\n\
         a=$(dd bs=1 count=1 2>/dev/null); echo \"answer $a\"; read x\n",
    );
    fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
    let cfg = s.amux_home.join("config.json");
    let mut c: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&cfg).unwrap()).unwrap();
    c["providers"][0]["bin"] = bin.to_str().unwrap().into();
    write(&cfg, &c.to_string());
    s.amux(&["account", "add", "fake", "one", "--no-login"]);
    assert_eq!(s.amux(&["allow", "bash: cargo test*"]).code, 0);
    assert_eq!(s.amux(&["spawn", "--name", "mid", "fake"]).code, 0);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let out = s.amux(&["peek", "mid"]).out;
        if out.contains("answer 1") {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "never answered:\n{out}"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let log = s.amux(&["allow", "--log", "mid"]).out;
    assert!(
        log.contains("approved") && log.contains("cargo test --all"),
        "{log}"
    );
    s.amux(&["stop", "mid"]);
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

/// Writes an account's usage numbers as amux records them, with `pct` of a 5-hour window
/// that resets in an hour.
fn write_usage(s: &Sandbox, account: &str, pct: f64) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    write(
        &s.amux_home.join(format!("usage/{account}.json")),
        &serde_json::json!({ "at": now, "source": "statusline",
            "fiveHour": { "usedPercent": pct, "resetsAt": now + 3_600_000 } })
        .to_string(),
    );
}

#[test]
fn records_claude_code_usage_through_the_status_line_and_restores_it() {
    let s = Sandbox::new("track");
    s.amux(&["account", "add", "fake", "one", "--no-login"]);
    let settings = s.home.join(".claude/settings.json");
    let before = serde_json::to_string_pretty(&serde_json::json!({
        "model": "opus",
        "statusLine": { "type": "command", "command": "echo mine; cat > \"$HOME/line-input\"", "padding": 1 },
        "permissions": { "allow": ["Bash(ls)"] }
    }))
    .unwrap()
        + "\n";
    write(&settings, &before);

    let on = s.amux(&["usage", "--track", "on"]);
    assert_eq!(on.code, 0, "{}", on.err);
    let after: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&settings).unwrap()).unwrap();
    let cmd = after["statusLine"]["command"].as_str().unwrap();
    assert!(cmd.ends_with("__statusline"), "{cmd}");
    assert_eq!(after["statusLine"]["padding"], 1);
    assert_eq!(after["permissions"]["allow"][0], "Bash(ls)");
    assert!(s
        .amux(&["usage", "--track", "on"])
        .out
        .contains("Already on"));

    // What Claude Code runs: the wrapper records the numbers, then shows your own line.
    let input = serde_json::json!({ "model": { "display_name": "Opus" }, "rate_limits": {
        "five_hour": { "used_percentage": 42.0, "resets_at": 4_000_000_000i64 },
        "seven_day": { "used_percentage": 12.5, "resets_at": 4_000_500_000i64 } } })
    .to_string();
    let mut child = Command::new(env!("CARGO_BIN_EXE_amux"))
        .arg("__statusline")
        .env("HOME", &s.home)
        .env("AMUX_HOME", &s.amux_home)
        .env("AMUX_ACCOUNT", "fake@one")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write as _;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout), "mine\n");
    assert_eq!(
        fs::read_to_string(s.home.join("line-input")).unwrap(),
        input
    );
    let u = s.amux(&["usage"]).out;
    assert!(u.contains("▓▓▓▓░░░░░░  42%") && u.contains(" 12% "), "{u}");
    let j: serde_json::Value = serde_json::from_str(&s.amux(&["usage", "--json"]).out).unwrap();
    assert_eq!(j[0]["quota"]["fiveHour"]["usedPercent"], 42.0);
    assert_eq!(j[0]["quota"]["sevenDay"]["resetsAt"], 4_000_500_000_000i64);

    let off = s.amux(&["usage", "--track", "off"]);
    assert_eq!(off.code, 0, "{}", off.err);
    assert_eq!(fs::read_to_string(&settings).unwrap(), before);
    assert!(s.amux(&["usage", "--track", "off"]).out.contains("Not on"));
}

#[test]
fn spawns_on_the_account_with_the_most_headroom() {
    let s = Sandbox::new("headroom");
    s.amux(&["account", "add", "fake", "one", "--no-login"]);
    s.amux(&["account", "add", "fake", "two", "--no-login"]);
    s.amux(&["account", "default", "fake@one"]);
    write_usage(&s, "fake@one", 95.0);
    write_usage(&s, "fake@two", 20.0);
    // Without switchAt, the default account is used as before.
    let r = s.amux(&["spawn", "--name", "a", "fake"]);
    assert!(r.err.contains("as fake@one"), "{}", r.err);
    let cfg = s.amux_home.join("config.json");
    let mut c: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&cfg).unwrap()).unwrap();
    c["limits"]["switchAt"] = 90.into();
    write(&cfg, &c.to_string());
    let r = s.amux(&["spawn", "--name", "b", "fake"]);
    assert!(
        r.err
            .contains("using fake@two, which has the most headroom (20% used; fake@one: 95% used)"),
        "{}",
        r.err
    );
    // Named accounts are used as named.
    let r = s.amux(&["spawn", "--name", "c", "fake@one"]);
    assert!(r.err.contains("as fake@one"), "{}", r.err);
}

#[test]
fn moves_a_run_before_the_limit_only_between_turns() {
    let s = Sandbox::new("switchat");
    // Reports its state (OSC 7501): working until told "done", then idle.
    let bin = s.home.join("bin/turn-cli");
    write(
        &bin,
        "#!/bin/sh\nst() { printf '\\033]7501;state=%s\\033\\\\' \"$1\"; }\necho \"ARGS $*\"\nst working\n\
         while read l; do echo \"got $l\"; [ \"$l\" = done ] && st idle; done\n",
    );
    fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
    let cfg = s.amux_home.join("config.json");
    let mut c: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&cfg).unwrap()).unwrap();
    c["providers"][0]["bin"] = bin.to_str().unwrap().into();
    c["providers"][0]["resumeArgs"] = serde_json::json!(["--resume", "{id}"]);
    c["providers"][0]["sessions"] = "claude".into();
    c["providers"][0]["sharedDirs"] = serde_json::json!(["projects"]);
    c["limits"]["switchAt"] = 90.into();
    c["limits"]["onLimit"] = "failover".into();
    write(&cfg, &c.to_string());
    s.amux(&["account", "add", "fake", "one", "--no-login"]);
    s.amux(&["account", "add", "fake", "two", "--no-login"]);
    s.amux(&["account", "default", "fake@one"]);
    let tick = [("AMUX_QUOTA_TICK_MS", "100")];
    let r = s.amux_with(&["spawn", "--name", "q", "fake@one"], &tick, None);
    assert_eq!(r.code, 0, "{}", r.err);
    wait_ps(&s, "q", "running");
    let read_run = || -> serde_json::Value {
        serde_json::from_str(&fs::read_to_string(s.amux_home.join("runs/q/run.json")).unwrap())
            .unwrap()
    };
    let mut run = read_run();
    for _ in 0..100 {
        if run["status"]["state"] == "working" {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
        run = read_run();
    }
    assert_eq!(run["status"]["state"], "working", "{run}");
    let cwd = run["cwd"].as_str().unwrap().to_string();
    let sid = "5e55a0b1-0000-4000-8000-000000000002";
    write(
        &s.shared.join(format!("projects/-x/{sid}.jsonl")),
        &format!(
            "{}\n",
            serde_json::json!({"type": "user", "cwd": cwd, "sessionId": sid,
                "message": {"role": "user", "content": "build the login page"}})
        ),
    );
    write_usage(&s, "fake@one", 93.0);
    // Mid-turn: it stays where it is.
    std::thread::sleep(std::time::Duration::from_millis(1000));
    assert!(!s.amux_home.join("runs/q-2").exists());
    // The turn ends: it moves, once, to the account with room.
    assert_eq!(s.amux(&["send", "q", "done"]).code, 0);
    let ps = wait_ps(&s, "q-2", "fake@two");
    assert!(ps.contains(&format!("--resume {sid}")), "{ps}");
    let mut log = String::new();
    for _ in 0..100 {
        log = s.amux(&["logs", "q-2"]).out;
        if log.contains("got The previous account was close to its usage limit") {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(log.contains("was close to its usage limit"), "{log}");
    let notes = fs::read_to_string(s.home.join("notes")).unwrap_or_default();
    assert!(
        notes.contains("limited q fake@one is at 93% of its 5-hour window")
            && notes.contains("moving q to fake@two"),
        "{notes}"
    );
    s.amux(&["stop", "q-2"]);
}

/// A stand-in for Linear's GraphQL API and Jira's REST API. `answer` gets the method,
/// path, lowercased headers and body, and returns the status and JSON body. Every request
/// is kept, in order.
type Answer = Box<dyn Fn(&str, &str, &str, &str) -> (u16, String) + Send>;
type Seen = std::sync::Arc<std::sync::Mutex<Vec<String>>>;

fn api_server(answer: Answer) -> (String, Seen) {
    use std::io::{BufRead, BufReader, Read, Write};
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = format!("http://{}", l.local_addr().unwrap());
    let seen: Seen = Default::default();
    let log = seen.clone();
    std::thread::spawn(move || {
        for s in l.incoming().flatten() {
            let mut r = BufReader::new(s.try_clone().unwrap());
            let mut first = String::new();
            let _ = r.read_line(&mut first);
            let mut head = String::new();
            let mut len = 0;
            loop {
                let mut line = String::new();
                if r.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                    break;
                }
                let line = line.to_ascii_lowercase();
                if let Some(v) = line.strip_prefix("content-length:") {
                    len = v.trim().parse().unwrap_or(0);
                }
                head.push_str(&line);
            }
            let mut body = vec![0; len];
            let _ = r.read_exact(&mut body);
            let body = String::from_utf8_lossy(&body).into_owned();
            let mut parts = first.split_whitespace();
            let (method, path) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
            let (code, out) = answer(method, path, &head, &body);
            log.lock()
                .unwrap()
                .push(format!("{method} {path}\n{head}\n{body}"));
            let _ = write!(
                &s,
                "HTTP/1.1 {code} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{out}",
                out.len()
            );
        }
    });
    (addr, seen)
}

/// Runs amux with `input` on its stdin.
fn amux_stdin(s: &Sandbox, args: &[&str], env: &[(&str, &str)], input: &str) -> Out {
    use std::io::Write;
    let mut child = Command::new(env!("CARGO_BIN_EXE_amux"))
        .args(args)
        .env("HOME", &s.home)
        .env("AMUX_HOME", &s.amux_home)
        .env("AMUX_NO_UPDATE_CHECK", "1")
        .envs(env.iter().copied())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let r = child.wait_with_output().unwrap();
    Out {
        code: r.status.code().unwrap_or(-1),
        out: String::from_utf8_lossy(&r.stdout).into(),
        err: String::from_utf8_lossy(&r.stderr).into(),
    }
}

#[test]
fn works_on_linear_and_jira_issues() {
    let s = Sandbox::new("trackers");
    s.amux(&["account", "add", "fake", "one", "--no-login"]);
    let repo = s.home.join("repo");
    fs::create_dir_all(&repo).unwrap();
    write(&repo.join("a.txt"), "x");
    let git = |args: &[&str]| {
        assert!(Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
            .args(args)
            .status()
            .unwrap()
            .success());
    };
    git(&["init", "-q"]);
    git(&["add", "."]);
    git(&["commit", "-qm", "init"]);
    let remote = s.home.join("remote.git");
    assert!(Command::new("git")
        .args(["init", "-q", "--bare"])
        .arg(&remote)
        .status()
        .unwrap()
        .success());
    git(&["remote", "add", "origin", remote.to_str().unwrap()]);
    // A stand-in for the GitHub CLI: one PR per branch, once created.
    let gh = s.home.join("bin/gh");
    write(
        &gh,
        &format!(
            "#!/bin/sh\necho \"$@\" >> {log}\ncase \"$1 $2\" in\n\
             'pr view') b=$(echo \"$3\" | tr / _); [ -f {st}-$b ] && echo '{{\"url\":\"https://github.com/o/r/pull/7\",\"number\":7,\"state\":\"OPEN\"}}' && exit 0; echo 'no pull requests found' >&2; exit 1;;\n\
             'pr create') b=$(echo \"$4\" | tr / _); touch {st}-$b;;\n\
             esac\n",
            log = s.home.join("gh-log").display(),
            st = s.home.join("gh-pr").display(),
        ),
    );
    fs::set_permissions(&gh, fs::Permissions::from_mode(0o755)).unwrap();

    // Linear: two open issues with comments; the key is lin_key.
    let (linear, linear_log) = api_server(Box::new(|_, _, head, body| {
        if !head.contains("authorization: lin_key\r\n") {
            return (
                401,
                r#"{"errors":[{"message":"Authentication required"}]}"#.into(),
            );
        }
        let issue = |n: u32, title: &str, created: &str| {
            format!(
                r#"{{"id":"uuid-{n}","identifier":"ENG-{n}","title":"{title}","description":"Steps for **{n}**.","url":"https://linear.app/acme/issue/ENG-{n}/x","createdAt":"{created}",
                "comments":{{"nodes":[{{"body":"Second.","createdAt":"2026-01-05T00:00:00Z","user":{{"name":"Bo"}}}},{{"body":"First.","createdAt":"2026-01-04T00:00:00Z","user":{{"name":"Ann"}}}}]}}}}"#
            )
        };
        let v: serde_json::Value = serde_json::from_str(body).unwrap();
        let q = v["query"].as_str().unwrap_or_default();
        let data = if q.contains("viewer") {
            r#"{"viewer":{"name":"Ann","email":"ann@acme.com"}}"#.to_string()
        } else if q.contains("commentCreate") {
            r#"{"commentCreate":{"success":true}}"#.to_string()
        } else if q.contains("issues(") {
            format!(
                r#"{{"issues":{{"nodes":[{},{}]}}}}"#,
                issue(2, "Add dark mode", "2026-01-03T00:00:00Z"),
                issue(1, "Login fails on Safari", "2026-01-01T00:00:00Z")
            )
        } else {
            match v["variables"]["id"].as_str() {
                Some("ENG-1") => {
                    format!(r#"{{"issue":{}}}"#, issue(1, "Login fails on Safari", ""))
                }
                Some("ENG-2") => format!(r#"{{"issue":{}}}"#, issue(2, "Add dark mode", "")),
                _ => {
                    return (
                        200,
                        r#"{"data":null,"errors":[{"message":"Entity not found: Issue"}]}"#.into(),
                    )
                }
            }
        };
        (200, format!(r#"{{"data":{data}}}"#))
    }));
    // Jira: PROJ-45 and PROJ-46, for me@x.com with the token jtok.
    let (jira, jira_log) = api_server(Box::new(|method, path, head, _| {
        if !head.contains("authorization: basic bwvaec5jb206anrvaw==\r\n") {
            return (401, String::new());
        }
        let adf = |t: &str| {
            format!(
                r#"{{"type":"doc","version":1,"content":[{{"type":"paragraph","content":[{{"type":"text","text":"{t}"}}]}},
                {{"type":"bulletList","content":[{{"type":"listItem","content":[{{"type":"paragraph","content":[{{"type":"text","text":"a point"}}]}}]}}]}}]}}"#
            )
        };
        let p = path.split('?').next().unwrap_or_default();
        match (method, p) {
            ("GET", "/rest/api/3/myself") => (
                200,
                r#"{"displayName":"Me","emailAddress":"me@x.com"}"#.into(),
            ),
            // Jira's ENG-1 has the same key as Linear's ENG-1, which has a run by then.
            ("GET", "/rest/api/3/search/jql") if path.contains("ENG") => (
                200,
                r#"{"issues":[{"key":"ENG-1","fields":{"summary":"Jira's own ENG-1"}}],"isLast":true}"#.into(),
            ),
            ("GET", "/rest/api/3/issue/ENG-1") => (
                200,
                r#"{"key":"ENG-1","fields":{"summary":"Jira's own ENG-1","description":null}}"#.into(),
            ),
            ("GET", "/rest/api/3/search/jql") => (
                200,
                r#"{"issues":[{"key":"PROJ-45","fields":{"summary":"Export to CSV"}},{"key":"PROJ-46","fields":{"summary":"Import CSV"}}],"isLast":true}"#.into(),
            ),
            ("GET", "/rest/api/3/issue/PROJ-45") | ("GET", "/rest/api/3/issue/PROJ-46") => {
                let key = &p[p.len() - 7..];
                (
                    200,
                    format!(
                        r#"{{"key":"{key}","fields":{{"summary":"Export to CSV","description":{}}}}}"#,
                        adf("Export the table."),
                    ),
                )
            }
            // Newest first, as asked with orderBy=-created.
            ("GET", "/rest/api/3/issue/PROJ-45/comment")
            | ("GET", "/rest/api/3/issue/PROJ-46/comment")
                if path.contains("orderBy=-created") =>
            {
                (
                    200,
                    format!(
                        r#"{{"startAt":0,"maxResults":50,"total":2,"comments":[{{"author":{{"displayName":"Bo"}},"body":{}}},{{"author":{{"displayName":"Ann"}},"body":"Plain text."}}]}}"#,
                        adf("Use commas.")
                    ),
                )
            }
            ("GET", "/rest/api/3/issue/ENG-1/comment") => (
                200,
                r#"{"startAt":0,"maxResults":50,"total":0,"comments":[]}"#.into(),
            ),
            ("POST", "/rest/api/3/issue/PROJ-45/comment") => (201, r#"{"id":"1"}"#.into()),
            _ => (
                404,
                r#"{"errorMessages":["Issue does not exist or you do not have permission to see it."]}"#
                    .into(),
            ),
        }
    }));
    let linear_url = format!("{linear}/graphql");
    let env = [
        ("AMUX_GH", gh.to_str().unwrap()),
        ("AMUX_KEYCHAIN", "0"),
        ("AMUX_LINEAR_URL", linear_url.as_str()),
    ];
    let run_json = |id: &str| -> serde_json::Value {
        serde_json::from_str(
            &fs::read_to_string(s.amux_home.join("runs").join(id).join("run.json")).unwrap(),
        )
        .unwrap()
    };

    let st = s.amux_with(&["tracker"], &env, None);
    assert!(st.out.contains("Linear  not set up"), "{}", st.out);
    let r = s.amux_with(
        &["spawn", "--issue", "linear:ENG-1", "fake"],
        &env,
        Some(&repo),
    );
    assert!(r.err.contains("No Linear API key"), "{}", r.err);

    // Sign in: a key Linear turns down is not stored.
    let bad = amux_stdin(&s, &["tracker", "login", "linear"], &env, "wrong\n");
    assert_ne!(bad.code, 0);
    assert!(bad.err.contains("401"), "{}", bad.err);
    let key_file = s.amux_home.join("trackers/linear.token");
    assert!(!key_file.exists());
    let ok = amux_stdin(&s, &["tracker", "login", "linear"], &env, "lin_key\n");
    assert_eq!(ok.code, 0, "{}", ok.err);
    assert!(ok.out.contains("Ann <ann@acme.com>"), "{}", ok.out);
    assert_eq!(fs::read_to_string(&key_file).unwrap(), "lin_key");
    assert_eq!(
        fs::metadata(&key_file).unwrap().permissions().mode() & 0o077,
        0
    );
    assert!(s
        .amux_with(&["tracker"], &env, None)
        .out
        .contains("Linear  a stored key"));

    // One issue: its own branch, its text and comments as the prompt, a PR that links it.
    let r = s.amux_with(
        &[
            "spawn",
            "--issue",
            "linear:eng-1",
            "--prompt",
            "Keep it small.",
            "fake",
        ],
        &env,
        Some(&repo),
    );
    assert_eq!(r.code, 0, "{}", r.err);
    let j = run_json("eng-1");
    assert_eq!(j["worktree"]["branch"], "amux/eng-1");
    assert_eq!(j["tracker"]["key"], "ENG-1");
    assert_eq!(j["tracker"]["id"], "uuid-1");
    let p = j["args"][0].as_str().unwrap();
    assert!(
        p.starts_with(
            "Work on Linear issue ENG-1: Login fails on Safari\nhttps://linear.app/acme/issue/ENG-1/x"
        ),
        "{p}"
    );
    assert!(
        p.contains("Steps for **1**.")
            && p.contains("Comments:\n- Ann: First.\n- Bo: Second.")
            && p.contains("Keep it small.")
            && p.contains("\"Fixes ENG-1\""),
        "{p}"
    );
    let pr = s.amux_with(&["pr", "eng-1"], &env, None);
    assert_eq!(pr.code, 0, "{}", pr.err);
    assert!(
        pr.err.contains("posted the PR's link on ENG-1"),
        "{}",
        pr.err
    );
    let log = fs::read_to_string(s.home.join("gh-log")).unwrap();
    assert!(
        log.contains("--body Fixes ENG-1\n\nStarted by amux from Linear issue ENG-1: https://linear.app/acme/issue/ENG-1/x."),
        "{log}"
    );
    let last = linear_log.lock().unwrap().last().cloned().unwrap();
    assert!(
        last.contains("commentCreate")
            && last.contains("\"issueId\":\"uuid-1\"")
            && last.contains("https://github.com/o/r/pull/7"),
        "{last}"
    );
    let r = s.amux_with(
        &["spawn", "--issue", "linear:ENG-9", "fake"],
        &env,
        Some(&repo),
    );
    assert!(r.err.contains("Entity not found"), "{}", r.err);
    assert!(!s.amux_home.join("runs/eng-9").exists());

    // Intake from Linear: needs a label or team; starts each new issue once.
    let r = s.amux_with(&["intake", "--from", "linear", "fake"], &env, Some(&repo));
    assert!(r.err.contains("Give --label, --team"), "{}", r.err);
    let linear_intake = [
        "intake", "--from", "linear", "--label", "agent", "--team", "ENG", "fake",
    ];
    let r = s.amux_with(&linear_intake, &env, Some(&repo));
    assert_eq!(r.code, 0, "{}{}", r.out, r.err);
    assert!(
        r.out.contains("Started eng-2: ENG-2 Add dark mode"),
        "{}",
        r.out
    );
    assert!(!r.out.contains("eng-1:"), "{}", r.out);
    let list = linear_log
        .lock()
        .unwrap()
        .iter()
        .find(|q| q.contains("issues("))
        .cloned()
        .unwrap();
    assert!(
        list.contains(r#""labels":{"name":{"eqIgnoreCase":"agent"}}"#)
            && list.contains(r#""team":{"key":{"eqIgnoreCase":"ENG"}}"#)
            // The oldest open issues (Linear: newest first with `first`, oldest with `last`).
            && list.contains("issues(filter: $filter, last: 100, before: $before, orderBy: createdAt)"),
        "{list}"
    );
    // An issue's newest comments.
    assert!(linear_log
        .lock()
        .unwrap()
        .iter()
        .any(|q| q.contains("comments(first: 50, orderBy: createdAt)")));
    let again = s.amux_with(&linear_intake, &env, Some(&repo));
    assert!(
        again
            .out
            .contains("No new open Linear issues labeled agent in ENG."),
        "{}",
        again.out
    );
    let seen = fs::read_to_string(s.amux_home.join("intake.json")).unwrap();
    assert!(
        seen.contains("linear:ENG-1") && seen.contains("linear:ENG-2"),
        "{seen}"
    );

    // The key from the environment works as well.
    s.amux_with(&["tracker", "logout", "linear"], &env, None);
    assert!(!key_file.exists());
    let mut env_key = env.to_vec();
    env_key.push(("LINEAR_API_KEY", "lin_key"));
    let r = s.amux_with(
        &[
            "spawn",
            "--issue",
            "https://linear.app/acme/issue/ENG-2/x",
            "--name",
            "again",
            "fake",
        ],
        &env_key,
        Some(&repo),
    );
    assert_eq!(r.code, 0, "{}", r.err);

    // Jira: sign in with the site and email, then the same.
    let login = ["tracker", "login", "jira", jira.as_str(), "me@x.com"];
    let bad = amux_stdin(&s, &login, &env, "nope\n");
    assert_ne!(bad.code, 0, "{}", bad.out);
    let ok = amux_stdin(&s, &login, &env, "jtok\n");
    assert_eq!(ok.code, 0, "{}", ok.err);
    assert!(ok.out.contains("Me <me@x.com>"), "{}", ok.out);
    let st = s.amux_with(&["tracker"], &env, None).out;
    assert!(
        st.contains(&format!("Jira    {jira} as me@x.com, with a stored token")),
        "{st}"
    );
    let r = s.amux_with(
        &["spawn", "--issue", "jira:PROJ-45", "fake"],
        &env,
        Some(&repo),
    );
    assert_eq!(r.code, 0, "{}", r.err);
    let j = run_json("proj-45");
    assert_eq!(j["worktree"]["branch"], "amux/proj-45");
    let p = j["args"][0].as_str().unwrap();
    assert!(
        p.starts_with(&format!(
            "Work on Jira issue PROJ-45: Export to CSV\n{jira}/browse/PROJ-45\n\nExport the table.\n\n- a point\n"
        )),
        "{p}"
    );
    // The newest comment last.
    assert!(
        p.contains("- Ann: Plain text.\n- Bo: Use commas.\n\n  - a point"),
        "{p}"
    );
    let pr = s.amux_with(&["pr", "proj-45"], &env, None);
    assert_eq!(pr.code, 0, "{}", pr.err);
    let log = fs::read_to_string(s.home.join("gh-log")).unwrap();
    assert!(
        log.contains("--body PROJ-45\n\nStarted by amux from Jira issue PROJ-45"),
        "{log}"
    );
    let last = jira_log.lock().unwrap().last().cloned().unwrap();
    assert!(
        last.starts_with("POST /rest/api/3/issue/PROJ-45/comment")
            && last.contains(r#""href":"https://github.com/o/r/pull/7""#),
        "{last}"
    );
    let r = s.amux_with(
        &["spawn", "--issue", "jira:PROJ-99", "fake"],
        &env,
        Some(&repo),
    );
    assert!(r.err.contains("Issue does not exist"), "{}", r.err);

    // Intake from Jira by label and project; the JQL amux builds goes to /search/jql.
    let r = s.amux_with(
        &[
            "intake",
            "--from",
            "jira",
            "--label",
            "agent",
            "--project",
            "PROJ",
            "--dry-run",
            "fake",
        ],
        &env,
        Some(&repo),
    );
    assert!(
        r.out.contains("Would start proj-46: PROJ-46 Import CSV"),
        "{}{}",
        r.out,
        r.err
    );
    let search = jira_log
        .lock()
        .unwrap()
        .iter()
        .find(|q| q.contains("/search/jql"))
        .cloned()
        .unwrap();
    assert!(
        search.contains("jql=project+%3D+%22PROJ%22+AND+labels+%3D+%22agent%22")
            || search.contains("jql=project%20%3D%20%22PROJ%22%20AND%20labels%20%3D%20%22agent%22"),
        "{search}"
    );
    // proj-45 already has a run: marked as taken, and not started again.
    let r = s.amux_with(
        &[
            "intake",
            "--from",
            "jira",
            "--jql",
            "project = PROJ",
            "fake",
        ],
        &env,
        Some(&repo),
    );
    assert_eq!(r.code, 0, "{}{}", r.out, r.err);
    assert!(r.out.contains("Started proj-46: PROJ-46"), "{}", r.out);
    assert!(!r.out.contains("proj-45:"), "{}", r.out);

    // Jira's ENG-1 is not Linear's ENG-1 (run eng-1): intake would start it as
    // jira-eng-1, on its own branch.
    let eng = [
        "intake",
        "--from",
        "jira",
        "--jql",
        "project = ENG",
        "--dry-run",
        "fake",
    ];
    let r = s.amux_with(&eng, &env, Some(&repo));
    assert!(
        r.out
            .contains("Would start jira-eng-1: ENG-1 Jira's own ENG-1"),
        "{}{}",
        r.out,
        r.err
    );
    let r = s.amux_with(
        &["spawn", "--issue", "jira:ENG-1", "fake"],
        &env,
        Some(&repo),
    );
    assert_eq!(r.code, 0, "{}", r.err);
    let j = run_json("jira-eng-1");
    assert_eq!(j["worktree"]["branch"], "amux/jira-eng-1");
    assert_eq!(j["tracker"]["tracker"], "jira");
    assert_eq!(run_json("eng-1")["tracker"]["tracker"], "linear");
    // Started by hand, so intake marks it as taken without starting it again.
    let r = s.amux_with(
        &["intake", "--from", "jira", "--jql", "project = ENG", "fake"],
        &env,
        Some(&repo),
    );
    assert_eq!(r.code, 0, "{}{}", r.out, r.err);
    assert!(!r.out.contains("Started"), "{}", r.out);
    let seen = fs::read_to_string(s.amux_home.join("intake.json")).unwrap();
    assert!(seen.contains("jira:ENG-1"), "{seen}");
    // A second spawn of the same issue is told its run exists.
    let r = s.amux_with(
        &["spawn", "--issue", "jira:ENG-1", "fake"],
        &env,
        Some(&repo),
    );
    assert!(r.err.contains("\"jira-eng-1\" already exists"), "{}", r.err);

    // The comment on the issue can be turned off.
    let mut cfg: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(s.amux_home.join("config.json")).unwrap())
            .unwrap();
    cfg["trackers"] = serde_json::json!({ "comment": false });
    write(&s.amux_home.join("config.json"), &cfg.to_string());
    let before = jira_log.lock().unwrap().len();
    let pr = s.amux_with(&["pr", "proj-46"], &env, None);
    assert_eq!(pr.code, 0, "{}", pr.err);
    assert!(!pr.err.contains("posted"), "{}", pr.err);
    assert_eq!(jira_log.lock().unwrap().len(), before);
    for r in [
        "eng-1",
        "eng-2",
        "again",
        "proj-45",
        "proj-46",
        "jira-eng-1",
    ] {
        s.amux(&["stop", r]);
    }
}

#[test]
fn state_hooks_install_and_remove_leave_your_settings_as_they_were() {
    let s = Sandbox::new("statehooks");
    let settings = s.home.join(".claude/settings.json");
    let mine = serde_json::json!({
        "model": "opus",
        "hooks": { "PreToolUse": [{ "matcher": "Bash", "hooks": [{ "type": "command", "command": "my-check" }] }] },
        "permissions": { "allow": ["Bash(npm test)"] }
    });
    write(&settings, &serde_json::to_string_pretty(&mine).unwrap());
    let read = || -> serde_json::Value {
        serde_json::from_str(&fs::read_to_string(&settings).unwrap()).unwrap()
    };

    let st = s.amux(&["state-hooks", "status"]).out;
    assert!(st.contains("claude: not installed"), "{st}");
    assert!(st.contains("gemini: no ~/.gemini yet"), "{st}");

    let r = s.amux(&["state-hooks", "install"]);
    assert_eq!(r.code, 0, "{}{}", r.out, r.err);
    assert!(r.out.contains("claude: installed 8 hooks"), "{}", r.out);
    assert!(r.out.contains("gemini: skipped"), "{}", r.out);
    let v = read();
    assert_eq!(v["hooks"]["PreToolUse"][0], mine["hooks"]["PreToolUse"][0]);
    let cmd = v["hooks"]["PermissionRequest"][0]["hooks"][0]["command"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(cmd.ends_with(" __hook claude PermissionRequest"), "{cmd}");
    assert_eq!(v["model"], "opus");
    // Twice is the same as once.
    s.amux(&["state-hooks", "install", "claude"]);
    assert_eq!(read(), v);
    assert!(s
        .amux(&["state-hooks"])
        .out
        .contains("claude: installed in ~/.claude/settings.json"));
    let doctor = s.amux(&["doctor"]).out;
    assert!(
        doctor.contains("note state hooks, claude: installed"),
        "{doctor}"
    );

    let r = s.amux(&["state-hooks", "remove"]);
    assert!(r.out.contains("claude: removed 8 hooks"), "{}", r.out);
    assert_eq!(read(), mine);
    assert!(s
        .amux(&["state-hooks", "remove"])
        .out
        .contains("claude: nothing to remove"));
    assert_ne!(s.amux(&["state-hooks", "install", "codex"]).code, 0);
}

#[test]
fn the_hook_does_nothing_outside_a_run() {
    use std::io::Write;
    let s = Sandbox::new("hooknoop");
    let names = || -> Vec<_> {
        let mut v: Vec<_> = fs::read_dir(&s.amux_home)
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        v.sort();
        v
    };
    let before = names();
    for run in [None, Some("no-such-run")] {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_amux"));
        cmd.args(["__hook", "claude", "PermissionRequest"])
            .env("HOME", &s.home)
            .env("AMUX_HOME", &s.amux_home)
            .env_remove("AMUX_RUN")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        if let Some(r) = run {
            cmd.env("AMUX_RUN", r);
        }
        let started = std::time::Instant::now();
        let mut child = cmd.spawn().unwrap();
        let _ = child
            .stdin
            .take()
            .unwrap()
            .write_all(br#"{"tool_name":"Bash","tool_input":{"command":"ls"}}"#);
        let out = child.wait_with_output().unwrap();
        assert_eq!(out.status.code(), Some(0));
        assert!(out.stdout.is_empty() && out.stderr.is_empty(), "{out:?}");
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
    }
    // No update check, no scheduler, no files.
    assert_eq!(before, names());
}

#[test]
fn a_run_shows_what_its_state_hooks_report() {
    let s = Sandbox::new("hookrun");
    s.amux(&["account", "add", "fake", "one", "--no-login"]);
    let r = s.amux_with(
        &["spawn", "--name", "hk", "fake"],
        &[("FAKE_HOOK", env!("CARGO_BIN_EXE_amux"))],
        None,
    );
    assert_eq!(r.code, 0, "{}", r.err);
    let run_json = || -> serde_json::Value {
        serde_json::from_str(&fs::read_to_string(s.amux_home.join("runs/hk/run.json")).unwrap())
            .unwrap()
    };

    // PermissionRequest: waiting, with the command it asks about. `ps` shows the hook's
    // blocked status at once; the reason is recorded once auto-approve has passed on it.
    wait_ps(&s, "hk", "waiting");
    eventually("the waiting reason", || {
        run_json()["waiting"]["reason"] == "approve Bash: cargo test"
    });
    assert_eq!(run_json()["waiting"]["source"], "hook");
    assert_eq!(run_json()["status"]["state"], "blocked");
    assert_eq!(run_json()["status"]["via"], "hook");
    let notes = s.home.join("notes");
    eventually("a notification", || {
        fs::read_to_string(&notes).is_ok_and(|t| t.contains("approve Bash: cargo test"))
    });
    let why = s.amux(&["explain", "hk"]).out;
    assert!(why.contains("a state hook"), "{why}");

    // Looking around isn't answering: moving through the menu leaves it waiting.
    assert_eq!(s.amux(&["send", "hk", "--key", "down"]).code, 0);
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert_eq!(run_json()["waiting"]["reason"], "approve Bash: cargo test");

    // Your answer ends the wait; the Stop hook then says the turn is done.
    assert_eq!(s.amux(&["send", "hk", "y"]).code, 0);
    eventually("idle", || run_json()["status"]["state"] == "idle");
    assert!(run_json().get("waiting").is_none());
    wait_ps(&s, "hk", "idle");
    s.amux(&["send", "hk", "bye"]);
    wait_ps(&s, "hk", "exited 4");
}

#[test]
fn auto_approved_prompts_a_hook_reported_never_notify() {
    let s = Sandbox::new("hookallow");
    s.amux(&["account", "add", "fake", "one", "--no-login"]);
    assert_eq!(s.amux(&["allow", "bash: cargo test*"]).code, 0);
    let r = s.amux_with(
        &["spawn", "--name", "hp", "fake"],
        &[("FAKE_HOOKPERM", env!("CARGO_BIN_EXE_amux"))],
        None,
    );
    assert_eq!(r.code, 0, "{}", r.err);
    let run_json = || -> serde_json::Value {
        serde_json::from_str(&fs::read_to_string(s.amux_home.join("runs/hp/run.json")).unwrap())
            .unwrap()
    };
    let notes = s.home.join("notes");
    let read_notes = || fs::read_to_string(&notes).unwrap_or_default();

    // cargo test: the hook reports the block before the prompt shows; auto-approve
    // answers it, and the permission notification after that is old news. git push
    // isn't allowed: that one waits for you, and you hear about it once.
    eventually("git push to wait", || {
        run_json()["waiting"]["reason"] == "approve Bash: git push origin main"
    });
    eventually("a notification", || read_notes().contains("git push"));
    std::thread::sleep(std::time::Duration::from_millis(300));
    let n = read_notes();
    assert!(!n.contains("cargo test"), "{n}");
    assert_eq!(n.matches("waiting hp").count(), 1, "{n}");
    assert!(s
        .amux(&["peek", "hp"])
        .out
        .contains("answer 1 to cargo test --all"));

    // You answer; the notification that follows doesn't bring the wait back.
    assert_eq!(s.amux(&["send", "hp", "--key", "2"]).code, 0);
    eventually("answered", || run_json().get("waiting").is_none());
    std::thread::sleep(std::time::Duration::from_millis(700));
    assert!(run_json().get("waiting").is_none(), "{}", run_json());
    assert_eq!(read_notes().matches("waiting hp").count(), 1);
    s.amux(&["send", "hp", "bye"]);
    wait_ps(&s, "hp", "exited 5");
}

/// What an agent runs is a descendant of its run's CLI: the run hosts check the process at
/// the other end of their socket, so an agent types only into its own workers.
#[test]
fn an_agent_types_only_into_its_own_workers() {
    let s = Sandbox::new("peer");
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
    let r = s.amux_with(&["spawn", "--name", "victim", "fake"], &[chat], Some(&repo));
    assert_eq!(r.code, 0, "{}", r.err);
    wait_ps(&s, "victim", "running");
    // What the lead's agent does in its shell: amux send, and raw frames on the socket.
    let raw = r#"import os, socket, sys, time
os.chdir(sys.argv[1])
s = socket.socket(socket.AF_UNIX)
s.connect("sock")
d = sys.argv[3].encode()
s.sendall(bytes([int(sys.argv[2])]) + len(d).to_bytes(4, "big") + d)
time.sleep(0.5)
"#;
    write(&s.home.join("raw.py"), raw);
    let script = format!(
        "A={amux}\n\
         $A send victim from-lead; echo \"send-victim=$?\"\n\
         $A spawn --name w fake; echo \"spawn-w=$?\"\n\
         $A send w from-lead; echo \"send-w=$?\"\n\
         $A send lead self; echo \"send-self=$?\"\n\
         V=\"$AMUX_HOME/runs/victim\"\n\
         python3 {raw} \"$V\" 0 \"$(printf 'raw-input\\r')\"\n\
         python3 {raw} \"$V\" 3 '{{\"state\":\"blocked\",\"kind\":\"permission\",\"message\":\"forged\"}}'\n\
         echo sh-done\n",
        amux = env!("CARGO_BIN_EXE_amux"),
        raw = s.home.join("raw.py").display(),
    );
    write(&s.home.join("lead.sh"), &script);
    let sh = s.home.join("lead.sh").display().to_string();
    let r = s.amux_with(
        &["spawn", "--name", "lead", "--workers", "1", "fake"],
        &[chat, ("FAKE_SH", &sh), ("FAKE_SH_IN", "lead")],
        Some(&repo),
    );
    assert_eq!(r.code, 0, "{}", r.err);
    let screen = wait_peek(&s, "lead", "sh-done");
    assert!(screen.contains("send-victim=1"), "{screen}");
    assert!(screen.contains("spawn-w=0"), "{screen}");
    assert!(screen.contains("send-w=0"), "{screen}");
    assert!(screen.contains("send-self=1"), "{screen}");
    wait_peek(&s, "w", "got from-lead");
    // You can still type into the victim; nothing from the lead got there.
    assert_eq!(s.amux(&["send", "victim", "from-you"]).code, 0);
    let v = wait_peek(&s, "victim", "got from-you");
    assert!(!v.contains("from-lead") && !v.contains("raw-input"), "{v}");
    let ps = s.amux(&["ps"]).out;
    let line = ps.lines().find(|l| l.starts_with("victim")).unwrap();
    assert!(
        !line.contains("forged") && !line.contains("waiting"),
        "{ps}"
    );
}

/// `amux mcp` as an MCP client starts it: JSON-RPC over its stdin and stdout.
struct Mcp {
    child: std::process::Child,
    stdin: std::process::ChildStdin,
    rx: std::sync::mpsc::Receiver<serde_json::Value>,
    next: i64,
}

impl Mcp {
    fn start(s: &Sandbox, env: &[(&str, &str)], dir: &Path) -> Mcp {
        use std::io::BufRead;
        use std::process::Stdio;
        let mut child = Command::new(env!("CARGO_BIN_EXE_amux"))
            .arg("mcp")
            .env("HOME", &s.home)
            .env("AMUX_HOME", &s.amux_home)
            .env("AMUX_NO_UPDATE_CHECK", "1")
            .env_remove("AMUX_RUN")
            .envs(env.iter().copied())
            .current_dir(dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                // Every line on stdout is one JSON-RPC message.
                let v: serde_json::Value = serde_json::from_str(&line)
                    .unwrap_or_else(|e| panic!("not JSON on stdout: {line:?}: {e}"));
                if tx.send(v).is_err() {
                    break;
                }
            }
        });
        Mcp {
            child,
            stdin,
            rx,
            next: 0,
        }
    }

    fn write(&mut self, v: serde_json::Value) {
        use std::io::Write;
        writeln!(self.stdin, "{v}").unwrap();
        self.stdin.flush().unwrap();
    }

    /// Sends a request and returns the whole response to it.
    fn request(&mut self, method: &str, params: serde_json::Value) -> serde_json::Value {
        self.next += 1;
        let id = self.next;
        self.write(
            serde_json::json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }),
        );
        loop {
            let v = self
                .rx
                .recv_timeout(std::time::Duration::from_secs(60))
                .unwrap_or_else(|_| panic!("no answer to {method}"));
            if v["id"] == id {
                return v;
            }
        }
    }

    /// Calls a tool: its text, whether it's an error, and its structured content.
    fn call(&mut self, name: &str, args: serde_json::Value) -> (String, bool, serde_json::Value) {
        let r = self.request(
            "tools/call",
            serde_json::json!({ "name": name, "arguments": args }),
        );
        let res = &r["result"];
        assert!(res.is_object(), "{r}");
        (
            res["content"][0]["text"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
            res["isError"] == true,
            res["structuredContent"].clone(),
        )
    }
}

impl Drop for Mcp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn an_agent_runs_a_team_through_the_mcp_server() {
    let s = Sandbox::new("mcp");
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
    let r = s.amux_with(
        &["spawn", "--name", "t", "--workers", "1", "fake"],
        &[chat],
        Some(&repo),
    );
    assert_eq!(r.code, 0, "{}", r.err);
    wait_ps(&s, "t", "running");

    // The lead's CLI starts the server: it inherits AMUX_RUN.
    let tick = ("AMUX_TEAM_TICK_MS", "100");
    let mut lead = Mcp::start(&s, &[chat, tick, ("AMUX_RUN", "t")], &repo);
    let init = lead.request(
        "initialize",
        serde_json::json!({ "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "test", "version": "1" } }),
    );
    assert_eq!(init["result"]["protocolVersion"], "2025-06-18", "{init}");
    assert_eq!(init["result"]["serverInfo"]["name"], "amux");
    lead.write(serde_json::json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));
    let tools = lead.request("tools/list", serde_json::json!({}));
    let names: Vec<&str> = tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    for t in [
        "spawn", "list", "wait", "peek", "send", "report", "diff", "verify", "land", "stop",
    ] {
        assert!(names.contains(&t), "{names:?}");
    }
    let unknown = lead.request(
        "tools/call",
        serde_json::json!({ "name": "nope", "arguments": {} }),
    );
    assert_eq!(unknown["error"]["code"], -32602, "{unknown}");
    let bad = lead.call(
        "spawn",
        serde_json::json!({ "account": "fake", "worktree": "yes" }),
    );
    assert!(bad.1 && bad.0.contains("boolean"), "{bad:?}");

    // spawn: a worker of t, in its own worktree, told how to report.
    let (text, err, st) = lead.call(
        "spawn",
        serde_json::json!({ "account": "fake", "name": "t-a", "worktree": true, "prompt": "do a part" }),
    );
    assert!(!err, "{text}");
    assert_eq!(st["run"], "t-a", "{text}");
    assert!(text.contains("started t-a"), "{text}");
    // t may have one worker at a time, as with `amux spawn`.
    let (text, err, _) = lead.call(
        "spawn",
        serde_json::json!({ "account": "fake", "name": "t-b" }),
    );
    assert!(err && text.contains("its limit"), "{text}");

    let (_, err, st) = lead.call("list", serde_json::json!({}));
    assert!(!err);
    let runs = st["runs"].as_array().unwrap();
    let a = runs.iter().find(|r| r["id"] == "t-a").expect("t-a listed");
    assert_eq!(a["lead"], "t");
    assert_eq!(a["branch"], "amux/t-a");
    let (_, _, st) = lead.call("list", serde_json::json!({ "lead": "t" }));
    assert_eq!(st["runs"].as_array().unwrap().len(), 1);

    let mut screen = String::new();
    eventually("the worker's prompt on its screen", || {
        screen = lead.call("peek", serde_json::json!({ "run": "t-a" })).0;
        screen.contains("do a part")
    });
    assert!(screen.contains("amux report"), "{screen}");

    let (text, err, _) = lead.call("send", serde_json::json!({ "run": "t-a", "text": "hello" }));
    assert!(!err, "{text}");
    eventually("the worker got the text", || {
        lead.call("peek", serde_json::json!({ "run": "t-a", "lines": 3 }))
            .0
            .contains("got hello")
    });

    // The worker's own server: it reports, and can't start workers.
    let mut worker = Mcp::start(&s, &[chat, ("AMUX_RUN", "t-a")], &repo);
    let (text, err, _) = worker.call("spawn", serde_json::json!({ "account": "fake" }));
    assert!(
        err && text.contains("workers don't start workers"),
        "{text}"
    );
    let (text, err, _) = worker.call(
        "report",
        serde_json::json!({ "text": "did part a; tests pass" }),
    );
    assert!(!err && text.contains("Reported to t"), "{text}");
    drop(worker);

    lead.call("send", serde_json::json!({ "run": "t-a", "text": "quit" }));
    wait_ps(&s, "t-a", "exited 7");
    let (text, err, st) = lead.call("wait", serde_json::json!({ "timeout": 20 }));
    assert!(!err, "{text}");
    assert_eq!(st["timedOut"], false);
    assert!(
        text.contains("== t-a ended") && text.contains("did part a; tests pass"),
        "{text}"
    );
    // Told once: there is nothing new, and no worker is left to wait for.
    let (text, err, _) = lead.call("wait", serde_json::json!({ "timeout": 1 }));
    assert!(!err && text.contains("Every worker has ended"), "{text}");
    let (text, _, _) = lead.call("wait", serde_json::json!({ "runs": ["t-a"], "timeout": 5 }));
    assert!(text.contains("== t-a ended"), "{text}");
    let (text, err, _) = lead.call("diff", serde_json::json!({ "run": "t-a" }));
    assert!(!err && text.contains("No changes"), "{text}");

    // Inside a run, send and stop work only on that run's own workers.
    let (text, err, _) = lead.call("stop", serde_json::json!({ "run": "t" }));
    assert!(err && text.contains("own workers"), "{text}");
    let (text, err, _) = lead.call("send", serde_json::json!({ "run": "t", "text": "x" }));
    assert!(err && text.contains("own workers"), "{text}");
    let mut you = Mcp::start(&s, &[], &repo);
    let (text, err, _) = you.call("stop", serde_json::json!({ "run": "t" }));
    assert!(!err && text.contains("Stopped t"), "{text}");
    let (text, err, _) = you.call("stop", serde_json::json!({ "run": "t" }));
    assert!(err && text.contains("not running"), "{text}");
    drop(you);

    // A 2026-07-28 client: no handshake, the version on every request.
    let meta = serde_json::json!({
        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientCapabilities": {}
    });
    let d = lead.request("server/discover", serde_json::json!({ "_meta": meta }));
    assert_eq!(d["result"]["supportedVersions"][0], "2026-07-28", "{d}");
    let c = lead.request(
        "tools/call",
        serde_json::json!({ "name": "list", "arguments": {}, "_meta": meta }),
    );
    assert_eq!(c["result"]["resultType"], "complete", "{c}");
}

#[test]
fn mcp_install_and_remove_leave_your_config_as_it_was() {
    let s = Sandbox::new("mcpinstall");
    let claude_json = s.home.join(".claude.json");
    let mine = serde_json::json!({
        "hasCompletedOnboarding": true,
        "mcpServers": { "gh": { "type": "stdio", "command": "gh-mcp", "args": [] } }
    });
    write(&claude_json, &serde_json::to_string_pretty(&mine).unwrap());
    let r = s.amux(&["account", "add", "claude", "w", "--no-login"]);
    assert_eq!(r.code, 0, "{}{}", r.out, r.err);
    let overlay_json = s.amux_home.join("accounts/claude/w/.claude/.claude.json");
    assert!(overlay_json.exists(), "seeded from ~/.claude.json");
    let seeded = fs::read_to_string(&overlay_json).unwrap();
    let codex = s.home.join(".codex/config.toml");
    let codex_mine = "model = \"o3\"\n\n[mcp_servers.gh]\ncommand = \"gh-mcp\"\n";
    write(&codex, codex_mine);
    let gemini = s.home.join(".gemini/settings.json");
    write(&gemini, "{\n  \"theme\": \"dark\"\n}\n");
    let json = |p: &Path| -> serde_json::Value {
        serde_json::from_str(&fs::read_to_string(p).unwrap()).unwrap()
    };

    let st = s.amux(&["mcp", "--status"]).out;
    assert!(st.contains("claude (claude@w): not installed"), "{st}");
    assert!(st.contains("codex: not installed"), "{st}");

    let r = s.amux(&["mcp", "--install"]);
    assert_eq!(r.code, 0, "{}{}", r.out, r.err);
    for f in [&claude_json, &overlay_json] {
        let v = json(f);
        assert_eq!(
            v["mcpServers"]["amux"]["args"],
            serde_json::json!(["mcp"]),
            "{v}"
        );
        assert_eq!(v["mcpServers"]["gh"], mine["mcpServers"]["gh"]);
        assert_eq!(v["hasCompletedOnboarding"], true);
    }
    let me = env!("CARGO_BIN_EXE_amux");
    assert_eq!(json(&claude_json)["mcpServers"]["amux"]["command"], me);
    let toml = fs::read_to_string(&codex).unwrap();
    assert!(toml.starts_with(codex_mine), "{toml}");
    assert!(
        toml.contains("[mcp_servers.amux]") && toml.contains("\"AMUX_RUN\""),
        "{toml}"
    );
    assert_eq!(
        json(&gemini)["mcpServers"]["amux"]["args"],
        serde_json::json!(["mcp"])
    );
    assert_eq!(json(&gemini)["theme"], "dark");
    // Twice is the same as once.
    let once = (
        fs::read_to_string(&codex).unwrap(),
        json(&gemini),
        json(&overlay_json),
    );
    s.amux(&["mcp", "--install"]);
    assert_eq!(
        once,
        (
            fs::read_to_string(&codex).unwrap(),
            json(&gemini),
            json(&overlay_json)
        )
    );
    let st = s.amux(&["mcp", "--status"]).out;
    assert!(
        st.contains("codex: installed in ~/.codex/config.toml"),
        "{st}"
    );
    let doctor = s.amux(&["doctor"]).out;
    assert!(
        doctor.contains("note mcp server, gemini: installed"),
        "{doctor}"
    );

    let r = s.amux(&["mcp", "--remove", "codex"]);
    assert!(r.out.contains("codex: removed"), "{}", r.out);
    assert_eq!(fs::read_to_string(&codex).unwrap(), codex_mine);
    assert!(
        json(&gemini)["mcpServers"]["amux"].is_object(),
        "only codex"
    );
    let r = s.amux(&["mcp", "--remove"]);
    assert_eq!(r.code, 0, "{}{}", r.out, r.err);
    assert_eq!(json(&claude_json), mine);
    assert_eq!(
        json(&overlay_json),
        serde_json::from_str::<serde_json::Value>(&seeded).unwrap()
    );
    assert_eq!(json(&gemini), serde_json::json!({ "theme": "dark" }));
    assert!(s
        .amux(&["mcp", "--remove"])
        .out
        .contains("nothing to remove"));

    // Another server called amux is left alone.
    write(
        &gemini,
        "{\"mcpServers\":{\"amux\":{\"command\":\"npx\",\"args\":[\"x\"]}}}",
    );
    let r = s.amux(&["mcp", "--install", "gemini"]);
    assert_ne!(r.code, 0);
    assert_eq!(json(&gemini)["mcpServers"]["amux"]["command"], "npx");
    assert_ne!(s.amux(&["mcp", "--install", "qwen"]).code, 0);
}
