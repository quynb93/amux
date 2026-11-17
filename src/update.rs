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
    let latest = ureq::get(&format!("{}/repos/{REPO}/releases/latest", github(true)))
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

/// GitHub, or a stand-in for tests (`AMUX_GITHUB_URL` replaces both github.com and its API).
fn github(api: bool) -> String {
    match std::env::var("AMUX_GITHUB_URL") {
        Ok(u) if !u.is_empty() => u.trim_end_matches('/').to_string(),
        _ if api => "https://api.github.com".into(),
        _ => "https://github.com".into(),
    }
}

/// The release archive built for this machine, as named by the release workflow.
fn target() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Some("aarch64-apple-darwin"),
        ("macos", "x86_64") => Some("x86_64-apple-darwin"),
        ("linux", "aarch64") => Some("aarch64-unknown-linux-musl"),
        ("linux", "x86_64") => Some("x86_64-unknown-linux-musl"),
        _ => None,
    }
}

fn fetch(url: &str) -> anyhow::Result<Vec<u8>> {
    use std::io::Read;
    let r = ureq::get(url)
        .set("User-Agent", &format!("amux/{}", version()))
        .timeout(std::time::Duration::from_secs(120))
        .call()
        .map_err(|e| anyhow::anyhow!("download failed: {url}: {e}"))?;
    let mut body = Vec::new();
    r.into_reader().take(200 << 20).read_to_end(&mut body)?;
    Ok(body)
}

/// Replaces the binary at `exe` with release `latest`: downloads the archive for this
/// machine, checks it against the published SHA-256, unpacks it next to `exe`, makes sure
/// it runs and reports `latest`, then renames it into place. A rename leaves running
/// amux processes on the old file, so background runs keep going.
fn self_replace(exe: &std::path::Path, latest: &str) -> anyhow::Result<()> {
    use anyhow::{bail, Context};
    use sha2::Digest;
    let Some(target) = target() else {
        bail!(
            "no release is built for this machine ({} {})",
            std::env::consts::OS,
            std::env::consts::ARCH
        );
    };
    let archive = format!("amux-{target}.tar.gz");
    let base = format!("{}/{REPO}/releases/download/v{latest}", github(false));
    println!("Downloading {archive}...");
    let bytes = fetch(&format!("{base}/{archive}"))?;
    let sums = String::from_utf8(fetch(&format!("{base}/{archive}.sha256"))?).unwrap_or_default();
    let expected = sums
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    let actual = format!("{:x}", sha2::Sha256::digest(&bytes));
    if expected != actual {
        bail!("checksum mismatch for {archive}; nothing was changed");
    }
    let dir = exe.parent().context("the amux binary has no directory")?;
    let tmp = dir.join(format!(".amux-update-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).with_context(|| {
        format!(
            "can't write to {}; rerun with permission to change it",
            dir.display()
        )
    })?;
    let result = (|| {
        std::fs::write(tmp.join(&archive), &bytes)?;
        let ok = Command::new("tar")
            .arg("-xzf")
            .arg(tmp.join(&archive))
            .arg("-C")
            .arg(&tmp)
            .status()
            .is_ok_and(|s| s.success());
        let new = tmp.join("amux");
        if !ok || !new.is_file() {
            bail!("could not unpack {archive}");
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&new, std::fs::Permissions::from_mode(0o755))?;
        }
        let said = Command::new(&new)
            .arg("--version")
            .env("AMUX_NO_UPDATE_CHECK", "1")
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default();
        if is_newer(latest, &said) || is_newer(&said, latest) || said.is_empty() {
            bail!("the downloaded amux reports {said:?}, not {latest}; nothing was changed");
        }
        std::fs::rename(&new, exe).with_context(|| format!("can't replace {}", exe.display()))?;
        Ok(())
    })();
    let _ = std::fs::remove_dir_all(&tmp);
    result
}

/// `amux update [--check]`: installs the latest release, the same way this copy was
/// installed. With `--check`, only says whether there is one and how to get it.
pub fn update_cmd(args: &[String]) -> i32 {
    let check = match args.first().map(String::as_str) {
        None => false,
        Some("--check") => true,
        Some(_) => {
            eprintln!("Usage: amux update [--check]");
            return 2;
        }
    };
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
    let exe = std::env::current_exe().and_then(|p| p.canonicalize());
    let path = exe
        .as_ref()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let how = upgrade_command(&path, &latest);
    println!("Update available: amux {} → {latest}", version());
    if check {
        println!("  {how}   (or `amux update`)");
        println!("  Release notes: {}", releases_url());
        return 0;
    }
    if path.replace('\\', "/").contains("/target/") {
        println!("This amux was built from source ({path}). Pull and rebuild it instead.");
        return 1;
    }
    let done = |ok: bool| {
        if ok {
            println!(
                "Updated amux {} → {latest}. Release notes: {}",
                version(),
                releases_url()
            );
            println!("Runs already going keep the old version until they restart.");
            0
        } else {
            eprintln!("Update failed. You can run it yourself:\n  {how}");
            1
        }
    };
    if !how.starts_with("curl ") {
        // Homebrew and npm own their copies, so let them do the upgrade.
        println!("Running: {how}");
        let mut parts = how.split_whitespace();
        let ok = Command::new(parts.next().unwrap_or_default())
            .args(parts)
            .status()
            .is_ok_and(|s| s.success());
        return done(ok);
    }
    match exe
        .map_err(anyhow::Error::from)
        .and_then(|e| self_replace(&e, &latest))
    {
        Ok(()) => done(true),
        Err(e) => {
            eprintln!("amux update: {e:#}");
            done(false)
        }
    }
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
