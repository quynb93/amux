//! What parallel runs on one repo should share.
//!
//! - Notes: decisions every agent on the repo should follow ("use zod, not yup", "the API
//!   is frozen until Friday"). Kept in the repo's own git folder (`.git/amux/notes.jsonl`),
//!   so every worktree sees them and they are never committed. Each new run's prompt
//!   starts with them.
//! - Isolation: each worktree run gets its own ports (`PORT`, `AMUX_PORT`: a block of
//!   ten) and docker-compose project name, so dev servers don't collide; and untracked
//!   files a worktree lacks, like `.env`, are copied in from the main checkout.

use crate::git::git;
use crate::store::load_config;
use crate::time;
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Note {
    pub text: String,
    pub at: i64,
}

/// The repo's shared git folder (the same for all its worktrees), if `dir` is in a repo.
pub fn git_dir(dir: &Path) -> Option<PathBuf> {
    let d = git(
        dir,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
    .ok()?;
    Some(PathBuf::from(d))
}

fn notes_file(dir: &Path) -> Option<PathBuf> {
    Some(git_dir(dir)?.join("amux/notes.jsonl"))
}

pub fn list(dir: &Path) -> Vec<Note> {
    let Some(f) = notes_file(dir) else {
        return vec![];
    };
    std::fs::read_to_string(f)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

fn save(dir: &Path, notes: &[Note]) -> Result<()> {
    let Some(f) = notes_file(dir) else {
        bail!("{} is not in a git repository.", crate::paths::tildify(dir));
    };
    if let Some(p) = f.parent() {
        crate::fsx::mkdir_private(p)?;
    }
    let text: String = notes
        .iter()
        .map(|n| serde_json::to_string(n).unwrap_or_default() + "\n")
        .collect();
    crate::fsx::write_atomic(&f, text.as_bytes())?;
    Ok(())
}

pub fn add(dir: &Path, text: &str) -> Result<usize> {
    let text = text.trim();
    if text.is_empty() {
        bail!("A note needs some text.");
    }
    let mut all = list(dir);
    all.push(Note {
        text: text.to_string(),
        at: time::now_ms(),
    });
    save(dir, &all)?;
    Ok(all.len())
}

pub fn remove(dir: &Path, n: usize) -> Result<Note> {
    let mut all = list(dir);
    if n == 0 || n > all.len() {
        bail!("There are {} note(s).", all.len());
    }
    let gone = all.remove(n - 1);
    save(dir, &all)?;
    Ok(gone)
}

/// The notes as the start of a prompt, or nothing.
pub fn preamble(dir: &Path) -> String {
    let all = list(dir);
    if all.is_empty() {
        return String::new();
    }
    let mut s = String::from("Decisions for this repo that every agent here follows:\n");
    for n in &all {
        s.push_str(&format!("- {}\n", n.text));
    }
    s.push_str("\nYour task:\n");
    s
}

/// `"worktree": { "copy": [...] }` in config.json: untracked files to bring into new
/// worktrees. By default the usual env files.
pub fn files_to_copy() -> Vec<String> {
    load_config()
        .ok()
        .and_then(|c| {
            let v = c.extra.get("worktree")?.get("copy")?.as_array()?.clone();
            Some(
                v.iter()
                    .filter_map(|x| x.as_str().map(str::to_string))
                    .collect(),
            )
        })
        .unwrap_or_else(|| {
            [".env", ".env.local", ".env.development.local"]
                .iter()
                .map(|s| s.to_string())
                .collect()
        })
}

/// Copies those files from the main checkout into a new worktree, where they're missing.
/// Only plain relative paths inside the checkout; never through symlinks. Returns what
/// was copied.
pub fn copy_into(top: &Path, wt: &Path) -> Vec<String> {
    let mut out = vec![];
    for rel in files_to_copy() {
        let p = Path::new(&rel);
        if p.is_absolute()
            || p.components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            continue;
        }
        let (from, to) = (top.join(p), wt.join(p));
        let plain = std::fs::symlink_metadata(&from).is_ok_and(|m| m.is_file());
        // Nothing there, not even a symlink (which a copy would follow out of the tree).
        if plain && std::fs::symlink_metadata(&to).is_err() && copy_private(&from, &to, wt) {
            out.push(rel);
        }
    }
    out
}

/// Copies `from` to a new file `to` (mode 0600) inside `wt`: neither is followed if it is
/// a symlink, `to` must not exist, and its folder must be inside `wt` once resolved.
fn copy_private(from: &Path, to: &Path, wt: &Path) -> bool {
    use std::os::unix::fs::OpenOptionsExt;
    let Some(dir) = to.parent() else {
        return false;
    };
    let _ = std::fs::create_dir_all(dir);
    let inside = match (dir.canonicalize(), wt.canonicalize()) {
        (Ok(d), Ok(w)) => d.starts_with(w),
        _ => false,
    };
    if !inside {
        return false;
    }
    let Ok(mut src) = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(from)
    else {
        return false;
    };
    let Ok(mut dst) = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(to)
    else {
        return false;
    };
    if std::io::copy(&mut src, &mut dst).is_err() {
        drop(dst);
        let _ = std::fs::remove_file(to);
        return false;
    }
    true
}

/// Ports are handed out in blocks of ten from here.
const PORT_BASE: u16 = 41000;
const PORT_BLOCK: u16 = 10;

/// The first block of ports no live run has, for a new run. `"worktree": { "ports":
/// false }` turns port isolation off.
pub fn free_port_block(taken: &[u16]) -> Option<u16> {
    let on = load_config()
        .ok()
        .and_then(|c| c.extra.get("worktree")?.get("ports")?.as_bool())
        .unwrap_or(true);
    if !on {
        return None;
    }
    (0..200u16)
        .map(|i| PORT_BASE + i * PORT_BLOCK)
        .find(|p| !taken.contains(p))
}

/// Whether something, usually the run's dev server, accepts connections on `port` on this
/// machine. Tries 127.0.0.1 and then ::1, since some servers bind "localhost" to only one.
/// Each try waits at most 150 ms, so callers do this off the UI thread.
pub fn listening(port: u16) -> bool {
    use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream};
    let wait = std::time::Duration::from_millis(150);
    [
        SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
        SocketAddr::from((Ipv6Addr::LOCALHOST, port)),
    ]
    .iter()
    .any(|a| TcpStream::connect_timeout(a, wait).is_ok())
}

/// The address of a run's dev server.
pub fn port_url(port: u16) -> String {
    format!("http://localhost:{port}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{write, TempDir};

    #[test]
    fn tells_whether_a_port_is_listening() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        assert!(listening(port));
        // Not checked after closing it: another test running at the same time may get
        // the same port from the OS.
        drop(l);
        assert_eq!(port_url(41000), "http://localhost:41000");
    }

    #[test]
    fn notes_live_in_the_git_folder_and_open_prompts() {
        let t = TempDir::new();
        let d = t.path();
        assert!(std::process::Command::new("git")
            .arg("-C")
            .arg(d)
            .args(["init", "-q"])
            .status()
            .unwrap()
            .success());
        assert_eq!(preamble(d), "");
        add(d, "use zod, not yup").unwrap();
        add(d, "  the API is frozen ").unwrap();
        assert!(add(d, " ").is_err());
        assert!(d.join(".git/amux/notes.jsonl").exists());
        assert_eq!(
            preamble(d),
            "Decisions for this repo that every agent here follows:\n- use zod, not yup\n- the API is frozen\n\nYour task:\n"
        );
        assert_eq!(remove(d, 1).unwrap().text, "use zod, not yup");
        assert!(remove(d, 5).is_err());
    }

    #[test]
    fn copies_only_plain_files_inside_the_checkout() {
        let t = TempDir::new();
        let (top, wt) = (t.path().join("top"), t.path().join("wt"));
        write(&top.join(".env"), "SECRET=1\n");
        std::fs::create_dir_all(&wt).unwrap();
        std::os::unix::fs::symlink("/etc/hosts", top.join(".env.local")).unwrap();
        assert_eq!(copy_into(&top, &wt), [".env"]);
        assert!(!wt.join(".env.local").exists(), "symlinks aren't followed");
        assert!(copy_into(&top, &wt).is_empty(), "never overwrites");
        let mode = {
            use std::os::unix::fs::PermissionsExt;
            std::fs::metadata(wt.join(".env"))
                .unwrap()
                .permissions()
                .mode()
        };
        assert_eq!(mode & 0o777, 0o600);
        assert_eq!(free_port_block(&[41000, 41010]), Some(41020));

        // A symlink planted where the copy goes, even a dangling one, isn't written through.
        let wt2 = t.path().join("wt2");
        std::fs::create_dir_all(&wt2).unwrap();
        let outside = t.path().join("outside");
        std::os::unix::fs::symlink(&outside, wt2.join(".env")).unwrap();
        assert!(copy_into(&top, &wt2).is_empty());
        assert!(!outside.exists(), "nothing written through the link");
        // Nor through a folder that is a link out of the worktree.
        let wt3 = t.path().join("wt3");
        std::fs::create_dir_all(&wt3).unwrap();
        let elsewhere = t.path().join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, wt3.join("cfg")).unwrap();
        write(&top.join("cfg/.env"), "X=1\n");
        assert!(!copy_private(
            &top.join("cfg/.env"),
            &wt3.join("cfg/.env"),
            &wt3
        ));
        assert!(!elsewhere.join(".env").exists());
    }
}
