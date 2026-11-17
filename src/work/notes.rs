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
        if plain && !to.exists() {
            if let Some(d) = to.parent() {
                let _ = std::fs::create_dir_all(d);
            }
            if std::fs::copy(&from, &to).is_ok() {
                out.push(rel);
            }
        }
    }
    out
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{write, TempDir};

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
        assert_eq!(free_port_block(&[41000, 41010]), Some(41020));
    }
}
