//! `amux note`, `amux notes` and `amux broadcast`: what parallel runs on one repo share.
//! See work/notes.rs.

use crate::runs::{self, State};
use crate::util::need;
use crate::work::notes;
use anyhow::{bail, Result};
use std::path::PathBuf;

/// The repo to act on: `-C <dir>` or the current folder.
fn dir_of(argv: &[String]) -> Result<(PathBuf, Vec<String>)> {
    let mut dir = std::env::current_dir()?;
    let mut rest = vec![];
    let mut it = argv.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-C" | "--repo" => dir = crate::paths::expand_home(need(it.next(), "-C <folder>")?),
            _ => rest.push(a.clone()),
        }
    }
    if notes::git_dir(&dir).is_none() {
        bail!(
            "{} is not in a git repository.",
            crate::paths::tildify(&dir)
        );
    }
    Ok((dir, rest))
}

pub fn note_cmd(argv: &[String]) -> Result<i32> {
    let (dir, words) = dir_of(argv)?;
    if words.is_empty() {
        bail!("Usage: amux note <text...> [-C <repo>]");
    }
    let n = notes::add(&dir, &words.join(" "))?;
    println!("Noted ({n} for this repo). New runs here start with the notes; `amux broadcast` tells running ones.");
    Ok(0)
}

pub fn notes_cmd(argv: &[String]) -> Result<i32> {
    let (dir, rest) = dir_of(argv)?;
    if rest.first().map(String::as_str) == Some("--rm") {
        let n: usize = need(rest.get(1), "amux notes --rm <n>")?.parse()?;
        let gone = notes::remove(&dir, n)?;
        println!("Removed: {}", gone.text);
        return Ok(0);
    }
    let all = notes::list(&dir);
    if all.is_empty() {
        println!("No notes for this repo. Add one with `amux note <text>`.");
    }
    for (i, n) in all.iter().enumerate() {
        println!("{:>2}. {}", i + 1, n.text);
    }
    Ok(0)
}

/// `amux broadcast <text>`: the message to every live run working on this repo, and with
/// `--note`, kept for new runs too.
pub fn broadcast_cmd(argv: &[String]) -> Result<i32> {
    let (dir, rest) = dir_of(argv)?;
    let keep = rest.iter().any(|a| a == "--note");
    let words: Vec<&String> = rest.iter().filter(|a| *a != "--note").collect();
    if words.is_empty() {
        bail!("Usage: amux broadcast <text...> [--note] [-C <repo>]");
    }
    let text = words
        .iter()
        .map(|s| s.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    let repo = notes::git_dir(&dir);
    let mut sent = vec![];
    for r in runs::list() {
        if r.state() != State::Running {
            continue;
        }
        let here = std::path::Path::new(&r.cwd);
        if here.is_dir()
            && notes::git_dir(here) == repo
            && runs::client::send_text(&r, &text, true).is_ok()
        {
            sent.push(r.id);
        }
    }
    if keep {
        notes::add(&dir, &text)?;
    }
    if sent.is_empty() {
        println!(
            "No run is working on this repo{}.",
            if keep { "; noted for new ones" } else { "" }
        );
    } else {
        println!(
            "Sent to {}{}.",
            sent.join(", "),
            if keep { ", and noted for new runs" } else { "" }
        );
    }
    Ok(0)
}
