//! `amux comment` and `amux comments`: review comments on a run's changes from the CLI.
//! `amux ui` writes the same comments from its review view (`c`).

use crate::runs;
use crate::util::need;
use crate::work::review::{self, Anchor};
use anyhow::{anyhow, bail, Result};

const COMMENT_USAGE: &str = "amux comment <run> <file>:<line> [--removed] <note...>";

/// `file:line` → the file and line, splitting at the last colon so paths may hold colons.
fn parse_anchor(s: &str, removed: bool) -> Result<Anchor> {
    let (file, line) = s
        .rsplit_once(':')
        .ok_or_else(|| anyhow!("Say where: <file>:<line>, such as src/main.rs:42."))?;
    let line: u32 = line
        .parse()
        .ok()
        .filter(|n| *n > 0)
        .ok_or_else(|| anyhow!("\"{line}\" is not a line number."))?;
    if file.is_empty() || file.starts_with('/') || file.split('/').any(|p| p == "..") {
        bail!("Give the file's path inside the run's folder, such as src/main.rs.");
    }
    Ok(Anchor {
        file: file.trim_start_matches("./").to_string(),
        line,
        removed,
    })
}

pub fn comment_cmd(argv: &[String]) -> Result<i32> {
    let removed = argv.iter().any(|a| a == "--removed");
    let rest: Vec<&String> = argv.iter().filter(|a| *a != "--removed").collect();
    let run = runs::find(need(rest.first().copied(), COMMENT_USAGE)?)?;
    let at = parse_anchor(need(rest.get(1).copied(), COMMENT_USAGE)?, removed)?;
    let note = rest[2..]
        .iter()
        .map(|s| s.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    let code = review::line_text(&run, &at);
    let n = review::add(&run.id, at.clone(), &code, &note)?;
    println!(
        "Comment on {at} saved. {n} waiting to be sent: `amux comments {} --send`.",
        run.id
    );
    Ok(0)
}

const COMMENTS_USAGE: &str = "amux comments <run> [--send | --rm <n> | --clear]";

pub fn comments_cmd(argv: &[String]) -> Result<i32> {
    let mut send = false;
    let mut clear = false;
    let mut json = false;
    let mut rm = None;
    let mut id = None;
    let mut it = argv.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--send" | "-s" => send = true,
            "--clear" => clear = true,
            "--json" => json = true,
            "--rm" => {
                let n = it.next().and_then(|n| n.parse::<usize>().ok());
                rm = Some(
                    n.filter(|n| *n > 0)
                        .ok_or_else(|| anyhow!("--rm takes a comment's number."))?,
                );
            }
            f if f.starts_with('-') => bail!("Unknown option {f}. Usage: {COMMENTS_USAGE}"),
            _ => id = Some(a.clone()),
        }
    }
    let run = runs::find(need(id.as_ref(), COMMENTS_USAGE)?)?;
    if send {
        let n = review::send(&run)?;
        println!(
            "Sent {n} comment{} to {}.",
            if n == 1 { "" } else { "s" },
            run.id
        );
        return Ok(0);
    }
    let mut all = review::load(&run.id);
    if json {
        println!("{}", serde_json::to_string_pretty(&all)?);
        return Ok(0);
    }
    if clear {
        all.retain(|c| c.sent_at.is_some());
        review::save(&run.id, &all)?;
        println!("Removed the comments waiting to be sent.");
        return Ok(0);
    }
    if let Some(n) = rm {
        if n > all.len() {
            bail!("{} has {} comment(s).", run.id, all.len());
        }
        let c = all.remove(n - 1);
        review::save(&run.id, &all)?;
        println!("Removed the comment on {}.", c.at);
        return Ok(0);
    }
    if all.is_empty() {
        println!(
            "No comments on {0}. Add one with `amux comment {0} <file>:<line> <note>`, or press c in amux ui.",
            run.id
        );
        return Ok(0);
    }
    for (i, c) in all.iter().enumerate() {
        let state = if c.sent_at.is_some() {
            "sent"
        } else {
            "pending"
        };
        println!("{:>2}. {}  [{state}]", i + 1, c.at);
        if !c.code.trim().is_empty() {
            println!("    > {}", crate::util::one_line(c.code.trim(), 100));
        }
        for l in c.note.lines() {
            println!("    {l}");
        }
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_anchors_and_refuses_paths_outside() {
        let a = parse_anchor("src/a:b.rs:12", false).unwrap();
        assert_eq!((a.file.as_str(), a.line), ("src/a:b.rs", 12));
        assert_eq!(parse_anchor("./x.rs:1", true).unwrap().file, "x.rs");
        assert!(parse_anchor("x.rs", false).is_err());
        assert!(parse_anchor("x.rs:0", false).is_err());
        assert!(parse_anchor("../etc/passwd:1", false).is_err());
        assert!(parse_anchor("/etc/passwd:1", false).is_err());
    }
}
