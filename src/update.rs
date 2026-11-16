//! Update check in the style of Claude Code and Codex: commands never wait on the
//! network. Each run prints a one-line notice from a cached result and refreshes
//! that cache in a detached background process at most once a day.

use crate::store::{read_json, write_json};
use crate::{paths, time};
use serde::{Deserialize, Serialize};
use std::io::IsTerminal;
use std::process::{Command, Stdio};

pub const REPO: &str = "quynb93/amux";
const DAY_MS: i64 = 24 * 60 * 60 * 1000;

pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[derive(Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct Cache {
    checked_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    latest: Option<String>,
}

fn releases_url() -> String {
    format!("https://github.com/{REPO}/releases/latest")
}

/// True when version `a` is newer than `b`. Compares "1.2.3" numerically; a leading
/// "v" and any "-pre" suffix are ignored.
pub fn is_newer(a: &str, b: &str) -> bool {
    let parse = |v: &str| -> Vec<u64> {
        let v = v.strip_prefix('v').unwrap_or(v);
        v.split('-')
            .next()
            .unwrap_or("")
            .split('.')
            .map(|n| n.parse().unwrap_or(0))
            .collect()
    };
    let (x, y) = (parse(a), parse(b));
    for i in 0..x.len().max(y.len()) {
        let (p, q) = (
            x.get(i).copied().unwrap_or(0),
            y.get(i).copied().unwrap_or(0),
        );
        if p != q {
            return p > q;
        }
    }
    false
}

/// A plain release version like `v1.2.3` or `1.2.3-beta.1`. The tag ends up in a
/// copy-paste upgrade command, so anything else is ignored.
pub fn is_version_tag(tag: &str) -> bool {
    let t = tag.strip_prefix('v').unwrap_or(tag);
    let (core, pre) = match t.split_once('-') {
        Some((c, p)) => (c, Some(p)),
        None => (t, None),
    };
    let core_ok = !core.is_empty()
        && core
            .split('.')
            .all(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()));
    let pre_ok = pre.is_none_or(|p| {
        !p.is_empty()
            && p.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
    });
    core_ok && pre_ok
}

fn cache_file() -> std::path::PathBuf {
    paths::AMUX_HOME.join("update-check.json")
}

fn read_cache() -> Option<Cache> {
    let mut cache: Cache = read_json::<Option<Cache>>(&cache_file()).ok()??;
    // The cached version is printed to the terminal, so hold it to the same rule as a fresh one.
    cache.latest = cache.latest.filter(|v| is_version_tag(v));
    Some(cache)
}

fn write_cache(cache: &Cache) {
    // Best-effort. Atomic, so a concurrent run never reads a truncated cache.
    let _ = write_json(&cache_file(), cache);
}

fn disabled() -> bool {
    let set = |k: &str| std::env::var_os(k).is_some_and(|v| !v.is_empty());
    set("AMUX_NO_UPDATE_CHECK") || set("CI") || !std::io::stderr().is_terminal()
}

/// Asks GitHub for the latest release tag and caches it. Returns the version without "v".
pub fn fetch_latest() -> Option<String> {
    let latest = ureq::get(&format!(
        "https://api.github.com/repos/{REPO}/releases/latest"
    ))
    .set("Accept", "application/vnd.github+json")
    .set("User-Agent", &format!("amux/{}", version()))
    .timeout(std::time::Duration::from_secs(5))
    .call()
    .ok()
    .and_then(|r| r.into_string().ok())
    .and_then(|body| serde_json::from_str::<serde_json::Value>(&body).ok())
    .and_then(|v| v.get("tag_name")?.as_str().map(str::to_string))
    .filter(|tag| is_version_tag(tag))
    .map(|tag| tag.trim_start_matches('v').to_string());
    // Offline or blocked: record the attempt so the next try is tomorrow, and keep the last known version.
    write_cache(&Cache {
        checked_at: time::now_ms(),
        latest: latest
            .clone()
            .or_else(|| read_cache().and_then(|c| c.latest)),
    });
    latest
}

/// Called on every run: prints the cached notice and starts a background refresh when the cache is stale.
pub fn check_for_updates() {
    if disabled() {
        return;
    }
    let cache = read_cache();
    if let Some(latest) = cache.as_ref().and_then(|c| c.latest.as_deref()) {
        if is_newer(latest, version()) {
            eprintln!(
                "\x1b[33mUpdate available: amux {} → {latest}\x1b[0m  Run `amux update` or see {}",
                version(),
                releases_url()
            );
        }
    }
    if cache
        .as_ref()
        .is_none_or(|c| time::now_ms() - c.checked_at > DAY_MS)
    {
        // Stamp the attempt first so runs started before the refresh finishes don't each spawn one.
        write_cache(&Cache {
            checked_at: time::now_ms(),
            latest: cache.and_then(|c| c.latest),
        });
        // amux usually execs the CLI next, and the CLI would inherit the refresh as a child it
        // never reaps. So start it through a short-lived intermediate (`--detach`) that exits
        // right away, leaving the refresh to be reparented and reaped by init.
        if cfg!(unix) {
            let _ = background_cmd().map(|mut c| c.arg(DETACH).status());
        } else {
            spawn_refresh();
        }
    }
}

const DETACH: &str = "--detach";

/// `amux __update-check [--detach]`: the background refresh, or (with --detach) the
/// short-lived step that starts it and exits.
pub fn internal_cmd(args: &[String]) -> i32 {
    if args.first().map(String::as_str) == Some(DETACH) {
        spawn_refresh();
    } else {
        fetch_latest();
    }
    0
}

/// `amux __update-check` with no terminal and its own process group.
fn background_cmd() -> Option<Command> {
    let mut cmd = Command::new(std::env::current_exe().ok()?);
    cmd.arg("__update-check")
        .env("AMUX_NO_UPDATE_CHECK", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Its own process group, so Ctrl-C in the terminal doesn't reach it.
        cmd.process_group(0);
    }
    Some(cmd)
}

/// Starts the background refresh without waiting for it.
fn spawn_refresh() {
    let _ = background_cmd().map(|mut c| c.spawn());
}

/// The command that upgrades this copy of amux, based on where its binary lives:
/// Homebrew keeps formulae under a Cellar directory, npm under node_modules, and
/// anything else came from the install script (or a manual download).
pub fn upgrade_command(install_path: &str, latest: &str) -> String {
    let p = install_path.replace('\\', "/");
    if p.contains("/Cellar/amux/") {
        "brew upgrade amux".into()
    } else if p.contains("/node_modules/") {
        format!("npm install -g @quynb/amux@{latest}")
    } else {
        format!("curl -fsSL https://github.com/{REPO}/releases/latest/download/install.sh | sh")
    }
}

/// `amux update`: checks now and prints how to upgrade.
pub fn update_cmd() -> i32 {
    let Some(latest) = fetch_latest() else {
        println!(
            "amux {}. Could not read the latest release from {}.",
            version(),
            releases_url()
        );
        return 1;
    };
    if !is_newer(&latest, version()) {
        println!("amux {} is up to date.", version());
        return 0;
    }
    let exe = std::env::current_exe()
        .and_then(|p| p.canonicalize())
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    println!("Update available: amux {} → {latest}", version());
    println!("  {}", upgrade_command(&exe, &latest));
    println!("  Release notes: {}", releases_url());
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_versions() {
        assert!(is_newer("0.2.0", "0.1.0"));
        assert!(is_newer("0.10.0", "0.9.9"));
        assert!(!is_newer("v1.0.0", "1.0.0"));
        assert!(!is_newer("1.0.0", "1.0.1"));
        assert!(is_newer("1.1", "1.0.9"));
        assert!(is_newer("1.0.0-beta.1", "0.9.0"));
    }

    #[test]
    fn accepts_only_plain_version_tags() {
        for ok in ["v1.2.3", "1.2.3", "1.2.3-beta.1", "v10"] {
            assert!(is_version_tag(ok), "{ok}");
        }
        for bad in [
            "",
            "v",
            "1..2",
            "1.2.3; rm -rf ~",
            "1.2.3-",
            "latest",
            "1.2.3-a b",
        ] {
            assert!(!is_version_tag(bad), "{bad}");
        }
    }

    #[test]
    fn picks_the_upgrade_command() {
        let brew = "brew upgrade amux";
        assert_eq!(
            upgrade_command("/opt/homebrew/Cellar/amux/0.2.0/bin/amux", "0.3.0"),
            brew
        );
        assert_eq!(
            upgrade_command(
                "/home/linuxbrew/.linuxbrew/Cellar/amux/0.2.0/bin/amux",
                "0.3.0"
            ),
            brew
        );
        assert_eq!(
            upgrade_command(
                "/usr/local/lib/node_modules/@quynb/amux/native/x86_64-apple-darwin/amux",
                "0.3.0"
            ),
            "npm install -g @quynb/amux@0.3.0"
        );
        assert_eq!(
            upgrade_command("/home/me/.local/bin/amux", "0.3.0"),
            "curl -fsSL https://github.com/quynb93/amux/releases/latest/download/install.sh | sh"
        );
    }
}
