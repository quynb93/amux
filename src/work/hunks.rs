//! Keeping or dropping single hunks of a run's changes, like `git add -p` in reverse: the
//! hunks picked are reverse-applied to the run's working tree (`git apply -R`), and a new
//! untracked file picked whole is deleted.
//!
//! Nothing is dropped unless all of it still applies: the patch is the diff the person
//! reviewed, so if the agent changed those lines since, `git apply --check` refuses and
//! nothing changes. Each drop is also saved as a patch in `runs/<id>/dropped/`, and
//! `git apply <patch>` puts it back.

use super::review::{self, Changes, UNTRACKED};
use crate::git::git;
use crate::runs::{self, state, Run};
use anyhow::{anyhow, bail, Context, Result};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// One hunk of a run's diff, or a whole new untracked file.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Hunk {
    pub file: String,
    /// Its number within the file, from 1, as `amux review --hunks` lists it.
    pub number: usize,
    /// `@@ -a,b +c,d @@ …`, or empty for an untracked file.
    pub header: String,
    pub old_start: u32,
    pub old_lines: u32,
    pub new_start: u32,
    pub new_lines: u32,
    pub added: usize,
    pub removed: usize,
    /// The changed lines in the new file, without the context around them. With no
    /// added lines, both are the line the removed ones were before.
    pub first: u32,
    pub last: u32,
    /// A new file git doesn't track yet: dropping it deletes it.
    pub untracked: bool,
    /// The untracked file's fingerprint when it was read.
    #[serde(skip)]
    print: Option<u64>,
    /// The file's `diff --git` header, as a patch needs it.
    #[serde(skip)]
    file_header: String,
    /// The `@@` line and the hunk's lines, exactly as git printed them.
    #[serde(skip)]
    body: String,
}

impl Hunk {
    /// Where it is, for people and for the agent: `src/a.rs:10-12`, `src/a.rs:10 (removed
    /// lines)`, or `new.txt (new file)`.
    pub fn place(&self) -> String {
        if self.untracked {
            return format!("{} (new file)", self.file);
        }
        match (self.added, self.first == self.last) {
            (0, _) => format!("{}:{} (removed lines)", self.file, self.first.max(1)),
            (_, true) => format!("{}:{}", self.file, self.first),
            _ => format!("{}:{}-{}", self.file, self.first, self.last),
        }
    }
}

/// `-a[,b] +c[,d] @@ …` → (a, b, c, d). A missing length is 1.
fn ranges(h: &str) -> (u32, u32, u32, u32) {
    let mut nums = h.split_whitespace();
    let mut range = |sign: char| -> (u32, u32) {
        let r = nums
            .next()
            .and_then(|s| s.strip_prefix(sign))
            .unwrap_or("0");
        let (start, len) = r.split_once(',').unwrap_or((r, "1"));
        (start.parse().unwrap_or(0), len.parse().unwrap_or(1))
    };
    let (a, b) = range('-');
    let (c, d) = range('+');
    (a, b, c, d)
}

/// The hunks of a diff from `review::changes`, in the order `review::parse_diff` numbers
/// them (`DiffLine::hunk`).
pub fn parse(changes: &Changes) -> Vec<Hunk> {
    let mut out: Vec<Hunk> = Vec::new();
    let mut file = String::new();
    let mut header = String::new();
    let (mut in_hunk, mut untracked, mut current) = (false, false, None::<usize>);
    let mut in_file = 0;
    // The new file's line number at this point of the hunk.
    let mut line = 0u32;
    for raw in changes.text.split_inclusive('\n') {
        let l = raw.trim_end_matches('\n');
        if let Some(rest) = l.strip_prefix("diff --git ") {
            (in_hunk, untracked, current, in_file) = (false, false, None, 0);
            file = rest.rsplit_once(" b/").map_or(rest, |(_, b)| b).to_string();
            header = raw.to_string();
            continue;
        }
        if !in_hunk {
            if l == UNTRACKED {
                untracked = true;
                let print = changes
                    .untracked
                    .iter()
                    .find(|(f, _)| *f == file)
                    .and_then(|(_, p)| *p);
                out.push(Hunk {
                    file: file.clone(),
                    number: 1,
                    header: String::new(),
                    old_start: 0,
                    old_lines: 0,
                    new_start: 0,
                    new_lines: 0,
                    added: 0,
                    removed: 0,
                    first: 1,
                    last: 1,
                    untracked: true,
                    print,
                    file_header: String::new(),
                    body: String::new(),
                });
                continue;
            }
            if let Some(p) = l.strip_prefix("+++ ") {
                if let Some(b) = p.strip_prefix("b/") {
                    file = b.to_string();
                }
                header.push_str(raw);
                continue;
            }
            // A drop changes content only: the mode stays as it is, and the blob ids
            // aren't needed.
            if ["index ", "old mode ", "new mode "]
                .iter()
                .any(|p| l.starts_with(p))
            {
                continue;
            }
            if !l.starts_with("@@ ") {
                header.push_str(raw);
                continue;
            }
        }
        if let Some(h) = l.strip_prefix("@@ ") {
            in_hunk = true;
            let (a, b, c, d) = ranges(h);
            if untracked {
                if let Some(u) = out.last_mut() {
                    (u.new_start, u.new_lines) = (c, d);
                }
                continue;
            }
            in_file += 1;
            out.push(Hunk {
                file: file.clone(),
                number: in_file,
                header: l.to_string(),
                old_start: a,
                old_lines: b,
                new_start: c,
                new_lines: d,
                added: 0,
                removed: 0,
                first: 0,
                last: 0,
                untracked: false,
                print: None,
                file_header: header.clone(),
                body: raw.to_string(),
            });
            current = Some(out.len() - 1);
            line = c;
            continue;
        }
        if !in_hunk {
            continue;
        }
        let Some(h) = (if untracked {
            out.last_mut()
        } else {
            current.map(|i| &mut out[i])
        }) else {
            continue;
        };
        if untracked {
            if l.starts_with('+') {
                h.added += 1;
                h.last = h.added as u32;
            }
            continue;
        }
        h.body.push_str(raw);
        match l.as_bytes().first() {
            Some(b'+') => {
                if h.added == 0 {
                    h.first = line;
                }
                h.added += 1;
                h.last = line;
                line += 1;
            }
            Some(b'-') => {
                if h.added == 0 && h.removed == 0 {
                    (h.first, h.last) = (line, line);
                }
                h.removed += 1;
            }
            Some(b'\\') => {}
            _ => line += 1,
        }
    }
    out
}

/// The patch of the tracked hunks picked, one header per file.
fn patch(picked: &[&Hunk]) -> String {
    let mut files: Vec<&str> = Vec::new();
    for h in picked.iter().filter(|h| !h.untracked) {
        if !files.contains(&h.file.as_str()) {
            files.push(&h.file);
        }
    }
    let mut out = String::new();
    for f in files {
        let mut mine: Vec<&&Hunk> = picked
            .iter()
            .filter(|h| !h.untracked && h.file == f)
            .collect();
        mine.sort_by_key(|h| h.number);
        mine.dedup_by_key(|h| h.number);
        out.push_str(&mine[0].file_header);
        for h in mine {
            out.push_str(&h.body);
        }
    }
    out
}

/// What a drop did.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Dropped {
    pub hunks: usize,
    pub files: usize,
    /// The places dropped, as `Hunk::place` says them.
    pub places: Vec<String>,
    /// The commit that holds the drop, when amux made one.
    pub commit: Option<String>,
    /// Why there is no commit although the worktree was clean (a hook refused, say).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit_error: Option<String>,
    /// The patch that puts the dropped changes back: `git -C <repo> apply <patch>`.
    pub undo: PathBuf,
    /// The checkout's top folder, where the undo patch applies.
    pub repo: PathBuf,
}

impl Dropped {
    /// "Dropped 3 hunks in 2 files"
    pub fn summary(&self) -> String {
        counted(self.hunks, self.files)
    }

    /// The note for the agent, so it doesn't redo what was dropped.
    pub fn note(&self) -> String {
        let mut m = String::from(
            "I reviewed your changes and dropped these from the working tree. Don't add them back:\n",
        );
        for p in &self.places {
            m.push_str(&format!("- {p}\n"));
        }
        m.push_str(&match &self.commit {
            Some(c) => format!(
                "I committed that as {c} \"{}\".",
                commit_message(self.hunks)
            ),
            None => {
                "The files are changed but not committed: include this in your next commit.".into()
            }
        });
        m
    }
}

/// "3 hunks in 2 files", "1 hunk in 1 file".
pub fn counted(hunks: usize, files: usize) -> String {
    format!(
        "{hunks} hunk{} in {files} file{}",
        if hunks == 1 { "" } else { "s" },
        if files == 1 { "" } else { "s" }
    )
}

fn commit_message(hunks: usize) -> String {
    format!(
        "Drop {hunks} hunk{} (amux review)",
        if hunks == 1 { "" } else { "s" }
    )
}

/// `git diff` exits 1 when there are differences, which `--no-index` always has here.
fn new_file_patch(top: &Path, rel: &str) -> Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(top)
        .args([
            "diff",
            "--no-index",
            "--binary",
            "--no-color",
            "--no-ext-diff",
            "--",
            "/dev/null",
            rel,
        ])
        .stdin(Stdio::null())
        .output()
        .context("could not run git")?;
    if out.status.code() != Some(1) {
        bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Reverse-applies the hunks picked to the working tree of `changes.dir`. A new
/// untracked file goes whole. Refuses, changing nothing, unless every hunk still applies
/// and every untracked file is as it was read. With `commit` and a worktree with nothing
/// else uncommitted, the drop is committed so the branch stays clean. The patch that
/// undoes it is written in `save_dir`.
pub fn drop_hunks(
    changes: &Changes,
    picked: &[&Hunk],
    commit: bool,
    save_dir: &Path,
) -> Result<Dropped> {
    if picked.is_empty() {
        bail!("No hunks picked to drop.");
    }
    let dir = &changes.dir;
    let top = PathBuf::from(git(dir, &["rev-parse", "--show-toplevel"])?);
    let top = top.canonicalize().unwrap_or(top);
    let here = dir.canonicalize().unwrap_or(dir.clone());
    let mut text = patch(picked);
    for h in picked.iter().filter(|h| h.untracked) {
        let path = dir.join(&h.file);
        if h.print.is_none() || review::fingerprint(&path) != h.print {
            bail!(
                "{} changed since you reviewed it, so nothing was dropped. Look again and retry.",
                h.file
            );
        }
        // Untracked paths are relative to the run's folder; the patch is from the top.
        let rel = here
            .join(&h.file)
            .strip_prefix(&top)
            .map(|p| p.to_string_lossy().into_owned())
            .map_err(|_| anyhow!("{} is outside the checkout.", h.file))?;
        text.push_str(&new_file_patch(&top, &rel)?);
    }
    std::fs::create_dir_all(save_dir)?;
    let undo = save_dir.join(format!("{}.patch", crate::time::now_ms()));
    std::fs::write(&undo, &text)?;
    let undo_s = undo.to_string_lossy().into_owned();
    if let Err(e) = git(
        &top,
        &["apply", "-R", "--check", "--whitespace=nowarn", &undo_s],
    ) {
        let _ = std::fs::remove_file(&undo);
        bail!(
            "Nothing was dropped: the files no longer match the diff you reviewed (the agent may have changed them since). Look again and retry.\n{e:#}"
        );
    }
    let clean = commit
        && git(&top, &["status", "--porcelain", "--untracked-files=all"])
            .is_ok_and(|s| s.is_empty());
    git(&top, &["apply", "-R", "--whitespace=nowarn", &undo_s])
        .context("git apply failed after its check passed")?;
    let mut files: Vec<&str> = picked.iter().map(|h| h.file.as_str()).collect();
    files.sort_unstable();
    files.dedup();
    let mut done = Dropped {
        hunks: picked.len(),
        files: files.len(),
        places: picked.iter().map(|h| h.place()).collect(),
        commit: None,
        commit_error: None,
        undo,
        repo: top.clone(),
    };
    // Only tracked files have anything to commit: an untracked file just went away.
    let tracked: Vec<&str> = picked
        .iter()
        .filter(|h| !h.untracked)
        .map(|h| h.file.as_str())
        .collect();
    if clean && !tracked.is_empty() {
        let mut add = vec!["add", "-A", "--"];
        add.extend(&tracked);
        let made = git(&top, &add).and_then(|_| {
            git(&top, &["commit", "-q", "-m", &commit_message(picked.len())])?;
            git(&top, &["rev-parse", "--short", "HEAD"])
        });
        match made {
            Ok(sha) => done.commit = Some(sha),
            Err(e) => done.commit_error = Some(format!("{e:#}")),
        }
    }
    Ok(done)
}

/// Why a drop should wait: the run's agent is in the middle of a turn and may be
/// editing these very files.
pub fn busy(run: &Run) -> Option<String> {
    let fresh = runs::load(&run.id).unwrap_or_else(|_| run.clone());
    let (kind, label) = state::run_kind(&fresh, crate::time::now_ms());
    matches!(kind, state::Kind::Working | state::Kind::Starting).then(|| {
        format!(
            "{} is {label}: its agent may be editing these files. Drop hunks once its turn ends.",
            run.id
        )
    })
}

/// Drops hunks from a run, after checking it isn't mid-turn (unless `force`). Commits
/// the drop when the run has its own worktree and nothing else was uncommitted there.
pub fn drop_from_run(
    run: &Run,
    changes: &Changes,
    picked: &[&Hunk],
    force: bool,
) -> Result<Dropped> {
    if !force {
        if let Some(why) = busy(run) {
            bail!("{why}");
        }
    }
    drop_hunks(
        changes,
        picked,
        run.worktree.is_some(),
        &runs::run_dir(&run.id).join("dropped"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{write, TempDir};

    fn sh(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// A repo whose a.txt has twenty lines, committed; returns the base commit.
    fn repo(t: &TempDir) -> (PathBuf, String) {
        let dir = t.path().join("repo");
        let lines: String = (1..=20).map(|i| format!("line {i}\n")).collect();
        write(&dir.join("a.txt"), &lines);
        sh(&dir, &["init", "-q"]);
        // drop_hunks commits with the repo's own identity.
        sh(&dir, &["config", "user.name", "t"]);
        sh(&dir, &["config", "user.email", "t@example.com"]);
        sh(&dir, &["add", "."]);
        sh(&dir, &["commit", "-qm", "init"]);
        let base = sh(&dir, &["rev-parse", "HEAD"]).trim().to_string();
        (dir, base)
    }

    /// Changes line 2 and line 18 of a.txt: two hunks far apart.
    fn two_hunks(dir: &Path) {
        let s = std::fs::read_to_string(dir.join("a.txt")).unwrap();
        let s = s
            .replace("line 2\n", "line two\n")
            .replace("line 18\n", "line eighteen\nline 18b\n");
        std::fs::write(dir.join("a.txt"), s).unwrap();
    }

    #[test]
    fn lists_hunks_numbered_per_file_and_matches_the_view() {
        let t = TempDir::new();
        let (dir, base) = repo(&t);
        two_hunks(&dir);
        write(&dir.join("new.txt"), "hello\nworld\n");
        let c = review::changes_in(&dir, &base).unwrap();
        let h = parse(&c);
        assert_eq!(h.len(), 3, "{h:?}");
        assert_eq!((h[0].file.as_str(), h[0].number), ("a.txt", 1));
        assert_eq!((h[0].added, h[0].removed), (1, 1));
        assert_eq!(h[0].place(), "a.txt:2");
        assert_eq!((h[1].number, h[1].added, h[1].removed), (2, 2, 1));
        assert!(h[2].untracked && h[2].file == "new.txt" && h[2].added == 2);
        assert_eq!(h[2].place(), "new.txt (new file)");
        // The diff lines point at the same hunks.
        let lines = review::parse_diff(&c.text);
        let at = |t: &str| lines.iter().find(|l| l.text == t).unwrap().hunk;
        assert_eq!(at("+line two"), Some(0));
        assert_eq!(at("+line 18b"), Some(1));
        assert_eq!(at("+world"), Some(2));
        assert_eq!(at(UNTRACKED), Some(2));
    }

    #[test]
    fn drops_one_of_two_hunks_and_commits_when_clean() {
        let t = TempDir::new();
        let (dir, base) = repo(&t);
        two_hunks(&dir);
        sh(&dir, &["commit", "-qam", "agent work"]);
        let c = review::changes_in(&dir, &base).unwrap();
        let h = parse(&c);
        let d = drop_hunks(&c, &[&h[1]], true, &t.path().join("saved")).unwrap();
        let now = std::fs::read_to_string(dir.join("a.txt")).unwrap();
        assert!(now.contains("line two\n"), "{now}");
        assert!(
            now.contains("line 18\nline 19") && !now.contains("eighteen"),
            "{now}"
        );
        assert_eq!((d.hunks, d.files), (1, 1));
        assert!(d.commit.is_some(), "{d:?}");
        assert_eq!(
            sh(&dir, &["log", "-1", "--format=%s"]).trim(),
            "Drop 1 hunk (amux review)"
        );
        assert!(sh(&dir, &["status", "--porcelain"]).is_empty());
        assert!(d.note().contains("- a.txt:18-19\n"), "{}", d.note());
        // The saved patch puts it back.
        sh(&dir, &["apply", &d.undo.to_string_lossy()]);
        assert!(std::fs::read_to_string(dir.join("a.txt"))
            .unwrap()
            .contains("eighteen"));
    }

    #[test]
    fn leaves_the_drop_uncommitted_next_to_other_uncommitted_work() {
        let t = TempDir::new();
        let (dir, base) = repo(&t);
        two_hunks(&dir);
        let c = review::changes_in(&dir, &base).unwrap();
        let h = parse(&c);
        let d = drop_hunks(&c, &[&h[0]], true, &t.path().join("saved")).unwrap();
        assert_eq!(d.commit, None);
        let now = std::fs::read_to_string(dir.join("a.txt")).unwrap();
        assert!(
            now.contains("line 2\n") && now.contains("eighteen"),
            "{now}"
        );
        assert!(d.note().contains("not committed"));
    }

    #[test]
    fn dropping_an_untracked_file_deletes_it() {
        let t = TempDir::new();
        let (dir, base) = repo(&t);
        write(&dir.join("sub/new.bin"), "x\0y");
        write(&dir.join("keep.txt"), "keep\n");
        let c = review::changes_in(&dir, &base).unwrap();
        let h = parse(&c);
        let new = h.iter().find(|h| h.file == "sub/new.bin").unwrap();
        let d = drop_hunks(&c, &[new], true, &t.path().join("saved")).unwrap();
        assert!(!dir.join("sub/new.bin").exists());
        assert!(dir.join("keep.txt").exists());
        assert_eq!(d.commit, None);
        // Undo brings it back, byte for byte.
        sh(&dir, &["apply", &d.undo.to_string_lossy()]);
        assert_eq!(std::fs::read(dir.join("sub/new.bin")).unwrap(), b"x\0y");
    }

    #[test]
    fn refuses_hunks_that_no_longer_apply_and_changes_nothing() {
        let t = TempDir::new();
        let (dir, base) = repo(&t);
        two_hunks(&dir);
        write(&dir.join("new.txt"), "hello\n");
        let c = review::changes_in(&dir, &base).unwrap();
        let h = parse(&c);
        // The agent edits the second hunk's lines after the review was read.
        let s = std::fs::read_to_string(dir.join("a.txt")).unwrap();
        std::fs::write(dir.join("a.txt"), s.replace("line 18b", "line 18c")).unwrap();
        let before = std::fs::read_to_string(dir.join("a.txt")).unwrap();
        let saved = t.path().join("saved");
        let e = drop_hunks(&c, &[&h[0], &h[1]], true, &saved).unwrap_err();
        assert!(format!("{e:#}").contains("no longer match"), "{e:#}");
        // All or nothing: the first hunk, which still applies, is kept too.
        assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), before);
        assert_eq!(std::fs::read_dir(&saved).unwrap().count(), 0);
        // An untracked file that changed since is refused the same way.
        write(&dir.join("new.txt"), "hello again\n");
        let e = drop_hunks(&c, &[&h[2]], true, &saved).unwrap_err();
        assert!(format!("{e:#}").contains("changed since"), "{e:#}");
        assert!(dir.join("new.txt").exists());
    }

    #[test]
    fn reads_hunk_ranges() {
        assert_eq!(ranges("-10,3 +10,4 @@ fn x"), (10, 3, 10, 4));
        assert_eq!(ranges("-1 +1 @@"), (1, 1, 1, 1));
        assert_eq!(ranges("-0,0 +1,2 @@"), (0, 0, 1, 2));
    }
}
