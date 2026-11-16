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
             for k in FAKE_HOME FAKE_BASE_URL FAKE_TELEMETRY AMUX_ACCOUNT FAKE_TOKEN; do eval \"v=\\${$k-<unset>}\"; echo \"ENV $k=$v\"; done\n",
        );
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
        write(&shared.join("settings.json"), "{}");
        let config = serde_json::json!({ "providers": [{
            "id": "fake", "label": "Fake CLI", "bin": bin, "sharedHome": shared, "homeEnv": "FAKE_HOME",
            "privateEntries": ["auth.json"], "sharedDirs": ["sessions"], "baseUrlEnv": "FAKE_BASE_URL",
            "tokenEnv": "FAKE_TOKEN", "privacyEnv": { "FAKE_TELEMETRY": "0" }
        }]});
        write(&amux_home.join("config.json"), &config.to_string());
        Sandbox {
            home,
            amux_home,
            shared,
        }
    }

    fn amux(&self, args: &[&str]) -> Out {
        let r = Command::new(env!("CARGO_BIN_EXE_amux"))
            .args(args)
            .env("HOME", &self.home)
            .env("AMUX_HOME", &self.amux_home)
            .env("AMUX_NO_UPDATE_CHECK", "1")
            .env("FAKE_TOKEN", "inherited-from-shell")
            .output()
            .unwrap();
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
