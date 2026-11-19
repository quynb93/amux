//! `amux comment` and `amux comments`: review comments on a run's changes from the CLI.
//! `amux review`: list a run's hunks and drop some. `amux ui` does the same from its
//! review view (`c`).

use crate::runs;
use crate::util::need;
use crate::work::hunks::{self, Hunk};
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

const REVIEW_USAGE: &str =
    "amux review <run> [--hunks] [--drop <file>[:<n>]]... [--no-tell] [--force] [--json]";

/// `amux __review-data <run>`: the run's diff, untracked files and comments as JSON, for
/// `amux ui` on another machine to review this run (`ReviewView::open_remote`).
pub fn review_data_cmd(argv: &[String]) -> Result<i32> {
    let run = runs::find(need(argv.first(), "amux __review-data <run>")?)?;
    let changes = review::changes(&run)?;
    let data = changes.data(review::load(&run.id));
    println!("{}", serde_json::to_string(&data)?);
    Ok(0)
}

/// `--drop` arguments → indexes into `hunks`, in diff order. `file` is every hunk of the
/// file, `file:n` its n-th (as `--hunks` numbers them). Paths may hold colons.
fn pick(hunks: &[Hunk], specs: &[String]) -> Result<Vec<usize>> {
    let mut out = Vec::new();
    for s in specs {
        let s = s.trim_start_matches("./");
        let whole: Vec<usize> = (0..hunks.len()).filter(|i| hunks[*i].file == s).collect();
        if !whole.is_empty() {
            out.extend(whole);
            continue;
        }
        let found = s.rsplit_once(':').and_then(|(f, n)| {
            let n: usize = n.parse().ok()?;
            let mine: Vec<usize> = (0..hunks.len()).filter(|i| hunks[*i].file == f).collect();
            Some((f, n, mine))
        });
        match found {
            Some((f, n, mine)) if !mine.is_empty() => match mine.get(n.wrapping_sub(1)) {
                Some(i) => out.push(*i),
                None => bail!(
                    "{f} has {} hunk{}; `--hunks` lists them.",
                    mine.len(),
                    if mine.len() == 1 { "" } else { "s" }
                ),
            },
            _ => bail!("{s} is not a file this run changed; `--hunks` lists them."),
        }
    }
    out.sort_unstable();
    out.dedup();
    Ok(out)
}

pub fn review_cmd(argv: &[String]) -> Result<i32> {
    let (mut drops, mut tell, mut force, mut json, mut id) = (vec![], true, false, false, None);
    // The digest of the diff that was reviewed (`amux ui` on another machine): the drop
    // is refused unless the diff is still that one, so `--drop` names the same hunks.
    let mut expect = None;
    let mut it = argv.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--hunks" => {}
            "--drop" => drops.push(need(it.next(), REVIEW_USAGE)?.to_string()),
            "--expect" => expect = Some(need(it.next(), REVIEW_USAGE)?.to_string()),
            "--no-tell" => tell = false,
            "--force" => force = true,
            "--json" => json = true,
            f if f.starts_with('-') => bail!("Unknown option {f}. Usage: {REVIEW_USAGE}"),
            _ if id.is_some() => bail!("Usage: {REVIEW_USAGE}"),
            _ => id = Some(a.clone()),
        }
    }
    // A run on another machine was sent there before this (`remote::forward`).
    let run = runs::find(need(id.as_ref(), REVIEW_USAGE)?)?;
    let changes = review::changes(&run)?;
    let all = hunks::parse(&changes);
    if drops.is_empty() {
        if json {
            println!("{}", serde_json::to_string_pretty(&all)?);
        } else if all.is_empty() {
            println!("{} has no hunks to drop.", run.id);
        } else {
            print_hunks(&run.id, &all);
        }
        return Ok(0);
    }
    if expect.is_some_and(|d| d != changes.digest()) {
        bail!(
            "{}'s changes are no longer the ones you reviewed (the agent may have changed them since), so nothing was dropped. Look again and retry.",
            run.id
        );
    }
    let picked: Vec<&Hunk> = pick(&all, &drops)?.into_iter().map(|i| &all[i]).collect();
    let done = hunks::drop_from_run(&run, &changes, &picked, force)?;
    let mut told = None;
    if tell && run.state() == runs::State::Running {
        told = Some(
            crate::runs::client::send_text(&run, &done.note(), true).map_err(|e| format!("{e:#}")),
        );
    }
    if json {
        let mut v = serde_json::to_value(&done)?;
        v["told"] = serde_json::json!(matches!(told, Some(Ok(()))));
        println!("{}", serde_json::to_string_pretty(&v)?);
        return Ok(0);
    }
    println!("Dropped {} from {}.", done.summary(), run.id);
    match (&done.commit, &done.commit_error) {
        (Some(c), _) => println!("Committed as {c}."),
        (None, Some(e)) => println!("Not committed ({e}); the change is in the worktree."),
        (None, None) => println!(
            "The change is uncommitted in the run's folder; the agent's next commit takes it."
        ),
    }
    match told {
        Some(Ok(())) => println!("Told the agent what was dropped."),
        Some(Err(e)) => println!("Could not tell the agent: {e}"),
        None => {}
    }
    println!(
        "Undo: git -C {} apply {}",
        crate::remote::sh_quote(&done.repo.to_string_lossy()),
        crate::remote::sh_quote(&done.undo.to_string_lossy())
    );
    Ok(0)
}

fn print_hunks(id: &str, all: &[Hunk]) {
    let mut file = "";
    for h in all {
        if h.file != file {
            file = &h.file;
            println!(
                "{file}{}",
                if h.untracked {
                    "  (new, untracked)"
                } else {
                    ""
                }
            );
        }
        let what = if h.untracked {
            match h.added {
                0 => "the whole file".to_string(),
                n => format!("the whole file, {n} line{}", if n == 1 { "" } else { "s" }),
            }
        } else {
            format!("{}  +{} -{}", h.header, h.added, h.removed)
        };
        println!("  {:>3}  {:<10} {what}", h.number, place_lines(h));
    }
    println!(
        "\nDrop one with `amux review {id} --drop <file>:<n>`, or a whole file with `--drop <file>`."
    );
}

/// The changed lines alone: `18-19`, `2`, or empty for an untracked file.
fn place_lines(h: &Hunk) -> String {
    if h.untracked {
        return String::new();
    }
    h.place()
        .strip_prefix(&format!("{}:", h.file))
        .unwrap_or_default()
        .to_string()
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

    #[test]
    fn picks_hunks_by_file_and_number() {
        let t = crate::testutil::TempDir::new();
        let dir = t.path();
        let git = |args: &[&str]| {
            let ok = std::process::Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(args)
                .output()
                .unwrap()
                .status
                .success();
            assert!(ok, "git {args:?}");
        };
        let lines: String = (1..=20).map(|i| format!("{i}\n")).collect();
        crate::testutil::write(&dir.join("a:b.txt"), &lines);
        git(&["init", "-q"]);
        git(&["add", "."]);
        git(&[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@e",
            "commit",
            "-qm",
            "i",
        ]);
        let changed = lines.replace("2\n", "two\n").replace("19\n", "nineteen\n");
        crate::testutil::write(&dir.join("a:b.txt"), &changed);
        crate::testutil::write(&dir.join("new.txt"), "x\n");
        let all = hunks::parse(&review::changes_in(dir, "HEAD").unwrap());
        let p = |s: &[&str]| pick(&all, &s.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert_eq!(p(&["a:b.txt"]).unwrap(), vec![0, 1]);
        assert_eq!(
            p(&["a:b.txt:2", "./new.txt", "a:b.txt:2"]).unwrap(),
            vec![1, 2]
        );
        assert!(p(&["a:b.txt:3"])
            .unwrap_err()
            .to_string()
            .contains("has 2 hunks"));
        assert!(p(&["nope.txt"]).is_err());
        assert!(p(&["a:b.txt:0"]).is_err());
    }
}
