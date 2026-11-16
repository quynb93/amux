//! Extending amux without changing it.
//!
//! Plugins, the way git does it: an executable `~/.amux/plugins/<name>` or `amux-<name>`
//! on PATH becomes `amux <name> [args]`, and `/plugin <name>` in `amux ui`. A comment
//! line `# amux: <what it does>` in the first lines describes it in listings.
//!
//! Hooks: an executable `~/.amux/hooks/on-<event>` runs whenever that happens, for every
//! run: `waiting`, `exited`, `verify` (a test run or push finished) and `checks` (a PR's
//! CI result). Hooks are for automation; notifications (notify.rs) are for people.
//!
//! Both get `AMUX_BIN` (this amux), `AMUX_HOME` and, where there is one, `AMUX_RUN`,
//! plus `AMUX_EVENT` and `AMUX_MESSAGE` for hooks. A plugin or hook file must belong to
//! you and not be writable by others, or amux won't run it.

use crate::paths;
use anyhow::{bail, Result};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub fn plugins_dir() -> PathBuf {
    paths::AMUX_HOME.join("plugins")
}

pub fn hooks_dir() -> PathBuf {
    paths::AMUX_HOME.join("hooks")
}

/// An executable you own that no one else can change.
fn trusted_exe(p: &Path) -> bool {
    let Ok(m) = std::fs::metadata(p) else {
        return false;
    };
    m.is_file()
        && m.permissions().mode() & 0o111 != 0
        && m.permissions().mode() & 0o022 == 0
        && m.uid() == unsafe { libc::getuid() }
}

pub fn valid_name(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= 64
        && n.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

#[derive(Debug, Clone, PartialEq)]
pub struct Plugin {
    pub name: String,
    pub path: PathBuf,
    pub about: String,
}

fn about(p: &Path) -> String {
    let Ok(text) = std::fs::read(p) else {
        return String::new();
    };
    let head = String::from_utf8_lossy(&text[..text.len().min(2048)]).into_owned();
    head.lines()
        .take(10)
        .find_map(|l| {
            l.trim_start_matches(['#', '/', ' '])
                .strip_prefix("amux:")
                .map(|s| s.trim().to_string())
        })
        .unwrap_or_default()
}

/// Every plugin: amux's folder first, then `amux-*` on PATH. The first of a name wins.
pub fn list() -> Vec<Plugin> {
    let mut out: Vec<Plugin> = vec![];
    let mut add = |name: String, path: PathBuf| {
        if valid_name(&name) && trusted_exe(&path) && !out.iter().any(|p| p.name == name) {
            let about = about(&path);
            out.push(Plugin { name, path, about });
        }
    };
    if let Ok(rd) = std::fs::read_dir(plugins_dir()) {
        let mut v: Vec<_> = rd.flatten().map(|e| e.path()).collect();
        v.sort();
        for p in v {
            let name = p
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            add(name, p);
        }
    }
    for dir in std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()) {
        if let Ok(rd) = std::fs::read_dir(&dir) {
            let mut v: Vec<_> = rd.flatten().map(|e| e.path()).collect();
            v.sort();
            for p in v {
                let file = p
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned();
                if let Some(name) = file.strip_prefix("amux-") {
                    add(name.to_string(), p);
                }
            }
        }
    }
    out
}

pub fn find(name: &str) -> Option<Plugin> {
    list().into_iter().find(|p| p.name == name)
}

fn base_env(cmd: &mut Command) {
    if let Ok(exe) = std::env::current_exe() {
        cmd.env("AMUX_BIN", exe);
    }
    cmd.env("AMUX_HOME", &*paths::AMUX_HOME);
}

/// Runs a plugin in the foreground with the terminal, and returns its exit code.
pub fn run(p: &Plugin, args: &[String], run_id: Option<&str>) -> Result<i32> {
    let mut cmd = Command::new(&p.path);
    cmd.args(args);
    base_env(&mut cmd);
    if let Some(r) = run_id {
        cmd.env("AMUX_RUN", r);
    }
    let status = cmd
        .status()
        .map_err(|e| anyhow::anyhow!("could not run plugin {}: {e}", p.name))?;
    Ok(status.code().unwrap_or(1))
}

/// Runs `~/.amux/hooks/on-<event>` if there is one, without waiting for it.
pub fn hook(event: &str, run_id: &str, message: &str) {
    let path = hooks_dir().join(format!("on-{event}"));
    if !trusted_exe(&path) {
        return;
    }
    let mut cmd = Command::new(&path);
    base_env(&mut cmd);
    cmd.env("AMUX_EVENT", event)
        .env("AMUX_RUN", run_id)
        .env("AMUX_MESSAGE", message)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Ok(mut child) = cmd.spawn() {
        std::thread::spawn(move || child.wait());
    }
}

/// A starting point for a new plugin, in amux's plugin folder.
pub fn create(name: &str) -> Result<PathBuf> {
    if !valid_name(name) {
        bail!("Plugin names use letters, digits, '-' and '_'.");
    }
    let dir = plugins_dir();
    crate::fsx::mkdir_private(&dir)?;
    let path = dir.join(name);
    if path.exists() {
        bail!("{} already exists.", paths::tildify(&path));
    }
    let script = format!(
        r#"#!/bin/sh
# amux: describe what {name} does here
# Runs as `amux {name} [args]`, or /plugin {name} in amux ui (with AMUX_RUN set to the
# selected run). $AMUX_BIN is amux itself: drive runs with it, for example:
set -eu
run="${{AMUX_RUN:-${{1:-}}}}"
if [ -z "$run" ]; then
  "$AMUX_BIN" ps
  exit 0
fi
"$AMUX_BIN" peek "$run" | tail -5
"#
    );
    std::fs::write(&path, script)?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{write, TempDir};

    #[test]
    fn reads_the_description_and_trusts_only_private_executables() {
        let t = TempDir::new();
        let p = t.path().join("hello");
        write(&p, "#!/bin/sh\n# amux: says hello\necho hi\n");
        assert_eq!(about(&p), "says hello");
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(!trusted_exe(&p), "not executable");
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(!trusted_exe(&p), "anyone can change it");
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(trusted_exe(&p));
        assert!(!valid_name("../x") && valid_name("pr-summary"));
    }
}
