//! Update check in the style of Claude Code and Codex: commands never wait on the
//! network. Each run prints a one-line notice from a cached result and refreshes
//! that cache in a detached background process at most once a day.

use crate::{fsx, paths, time};
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

fn read_cache() -> Option<Cache> {
    serde_json::from_slice(&std::fs::read(paths::AMUX_HOME.join("update-check.json")).ok()?).ok()
}

fn write_cache(cache: &Cache) {
    // Best-effort.
    let _ = fsx::mkdir_private(&paths::AMUX_HOME);
    if let Ok(text) = serde_json::to_string(cache) {
        let _ = fsx::write_private(
            &paths::AMUX_HOME.join("update-check.json"),
            format!("{text}\n").as_bytes(),
        );
    }
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
        if let Ok(exe) = std::env::current_exe() {
            let mut cmd = Command::new(exe);
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
            let _ = cmd.spawn();
        }
    }
}

/// The command that upgrades this copy of amux, based on where it is installed:
/// Homebrew keeps formulae under a Cellar directory; everything else gets the release package.
pub fn upgrade_command(install_path: &str, latest: &str) -> String {
    let p = install_path.replace('\\', "/");
    if p.contains("/Cellar/amux/") {
        return "brew upgrade amux".into();
    }
    format!("npm install -g https://github.com/{REPO}/releases/download/v{latest}/amux.tgz")
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
        assert_eq!(
            upgrade_command("/opt/homebrew/Cellar/amux/0.1.1/bin/amux", "0.2.0"),
            "brew upgrade amux"
        );
        assert_eq!(
            upgrade_command(
                "/home/linuxbrew/.linuxbrew/Cellar/amux/0.1.1/bin/amux",
                "0.2.0"
            ),
            "brew upgrade amux"
        );
        assert_eq!(
            upgrade_command("/usr/local/lib/node_modules/@quynb/amux/bin/amux", "0.2.0"),
            "npm install -g https://github.com/quynb93/amux/releases/download/v0.2.0/amux.tgz"
        );
    }
}
