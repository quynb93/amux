//! Reviewing what a run changed: its diff, read line by line, and comments on lines that
//! go back to the agent as one message, the way a reviewer comments on a pull request.
//!
//! Comments are kept in `runs/<id>/review.json` until they are sent, then marked sent, so
//! a review can be written over time and from the CLI or `amux ui` alike.

use crate::git::git_raw;
use crate::runs::{self, Run};
use crate::store::{read_json, write_json};
use crate::time;
use anyhow::{anyhow, bail, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// The largest new file shown whole in a review; bigger ones show a note instead.
const MAX_NEW_FILE: u64 = 200 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineKind {
    /// `diff --git …` and the a/ b/ lines: which file follows.
    File,
    /// `@@ -a,b +c,d @@`.
    Hunk,
    Added,
    Removed,
    Context,
    /// Anything else git prints (mode changes, "Binary files differ", notes).
    Meta,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DiffLine {
    pub kind: LineKind,
    pub text: String,
    /// The file the line belongs to (its new path).
    pub file: String,
    /// Its line number in the new file (added and context lines).
    pub new: Option<u32>,
    /// Its line number in the old file (removed lines).
    pub old: Option<u32>,
    /// The hunk it is part of: an index into `hunks::parse` of the same diff. A new
    /// untracked file is one hunk from its first note on.
    pub hunk: Option<usize>,
}

impl DiffLine {
    /// Where a comment on this line points: `file:line`, or `file:line (removed)`.
    pub fn anchor(&self) -> Option<Anchor> {
        match (self.new, self.old) {
            (Some(n), _) => Some(Anchor {
                file: self.file.clone(),
                line: n,
                removed: false,
            }),
            (None, Some(o)) => Some(Anchor {
                file: self.file.clone(),
                line: o,
                removed: true,
            }),
            _ => None,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Anchor {
    pub file: String,
    pub line: u32,
    /// The line was removed: `line` is its number in the old file.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub removed: bool,
}

impl std::fmt::Display for Anchor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.file, self.line)?;
        if self.removed {
            write!(f, " (removed line)")?;
        }
        Ok(())
    }
}

/// Reads a unified diff (`git diff`) into lines that know their file and line numbers.
pub fn parse_diff(text: &str) -> Vec<DiffLine> {
    let mut out = Vec::new();
    let mut file = String::new();
    let (mut old, mut new) = (0u32, 0u32);
    let mut in_hunk = false;
    // Hunks so far, the one being read, and whether this file is new and untracked.
    let (mut hunks, mut hunk, mut untracked) = (0usize, None, false);
    for raw in text.lines() {
        let line = |kind, new: Option<u32>, old: Option<u32>, file: &str, hunk| DiffLine {
            kind,
            text: raw.to_string(),
            file: file.to_string(),
            new,
            old,
            hunk,
        };
        if let Some(rest) = raw.strip_prefix("diff --git ") {
            in_hunk = false;
            untracked = false;
            hunk = None;
            // "a/x b/x": the new path is after " b/". Paths with spaces are rare; the
            // "+++ b/" line below corrects it when present.
            file = rest.rsplit_once(" b/").map_or(rest, |(_, b)| b).to_string();
            out.push(line(LineKind::File, None, None, &file, hunk));
            continue;
        }
        if !in_hunk {
            if let Some(p) = raw.strip_prefix("+++ ") {
                if let Some(b) = p.strip_prefix("b/") {
                    file = b.to_string();
                }
                continue;
            }
            if raw.starts_with("--- ") || raw.starts_with("index ") {
                continue;
            }
            if raw == UNTRACKED {
                untracked = true;
                hunk = Some(hunks);
                hunks += 1;
            }
        }
        if let Some(h) = raw.strip_prefix("@@ ") {
            if !untracked {
                hunk = Some(hunks);
                hunks += 1;
            }
            (old, _, new, _) = hunk_ranges(h);
            in_hunk = true;
            out.push(line(LineKind::Hunk, None, None, &file, hunk));
            continue;
        }
        if in_hunk {
            match raw.as_bytes().first() {
                Some(b'+') => {
                    out.push(line(LineKind::Added, Some(new), None, &file, hunk));
                    new += 1;
                    continue;
                }
                Some(b'-') => {
                    out.push(line(LineKind::Removed, None, Some(old), &file, hunk));
                    old += 1;
                    continue;
                }
                Some(b' ') | None => {
                    out.push(line(LineKind::Context, Some(new), None, &file, hunk));
                    old += 1;
                    new += 1;
                    continue;
                }
                // "\ No newline at end of file"
                _ => {}
            }
        }
        out.push(line(LineKind::Meta, None, None, &file, hunk));
    }
    out
}

/// `-a[,b] +c[,d] @@ …` (a hunk header after its `@@ `) → (a, b, c, d). A missing length
/// is 1.
pub fn hunk_ranges(h: &str) -> (u32, u32, u32, u32) {
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

/// The note under `diff --git` that marks a new file git doesn't track yet.
pub const UNTRACKED: &str = "new file (not yet added to git)";

/// The folder a run's changes are in and what to compare them with.
pub fn diff_target(run: &Run) -> Result<(PathBuf, String)> {
    match &run.worktree {
        Some(wt) => {
            let base = crate::git::base_of(run)
                .ok_or_else(|| anyhow!("Can't tell where {}'s branch started.", run.id))?;
            Ok((PathBuf::from(&wt.path), base))
        }
        None => Ok((PathBuf::from(&run.cwd), "HEAD".into())),
    }
}

/// A run's changes as a diff, read once so the lines shown and the hunks that can be
/// dropped come from the same text.
#[derive(Clone)]
pub struct Changes {
    /// The folder the diff was made in.
    pub dir: PathBuf,
    pub text: String,
    /// New untracked files (paths as in the diff) and a fingerprint of each as it was
    /// read, so a drop can tell whether it changed since.
    pub untracked: Vec<(String, Option<u64>)>,
}

impl Changes {
    /// A digest of the diff and of the untracked files as they were read, the same on
    /// every machine and amux version (FNV-1a), so a drop asked for from another machine
    /// can tell it is about the diff that was reviewed.
    pub fn digest(&self) -> String {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        let mut eat = |b: &[u8]| {
            for x in b {
                h ^= u64::from(*x);
                h = h.wrapping_mul(0x0000_0100_0000_01b3);
            }
        };
        eat(self.text.as_bytes());
        for (f, print) in &self.untracked {
            eat(b"\0");
            eat(f.as_bytes());
            eat(&print.unwrap_or(0).to_le_bytes());
            eat(&[u8::from(print.is_some())]);
        }
        format!("{h:016x}")
    }

    /// What a review on another machine needs: `amux __review-data <run>` prints it.
    pub fn data(&self, comments: Vec<Comment>) -> ReviewData {
        ReviewData {
            text: self.text.clone(),
            untracked: self.untracked.clone(),
            digest: self.digest(),
            comments,
        }
    }
}

/// A run's changes and comments as amux on the run's machine read them, for reviewing a
/// run on another machine in `amux ui`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ReviewData {
    pub text: String,
    #[serde(default)]
    pub untracked: Vec<(String, Option<u64>)>,
    /// `Changes::digest` there: a drop passes it back (`amux review --expect`), and amux
    /// there refuses if the diff changed since.
    pub digest: String,
    #[serde(default)]
    pub comments: Vec<Comment>,
}

impl ReviewData {
    /// The changes, for parsing into lines and hunks here. The folder is on the other
    /// machine, so there is none.
    pub fn changes(&self) -> Changes {
        Changes {
            dir: PathBuf::new(),
            text: self.text.clone(),
            untracked: self.untracked.clone(),
        }
    }
}

/// Everything the run changed against where it started, committed or not, with new
/// untracked files shown as added, ready to review.
pub fn changes(run: &Run) -> Result<Changes> {
    let (dir, base) = diff_target(run)?;
    if !dir.is_dir() {
        bail!(
            "{}'s folder {} is gone.",
            run.id,
            crate::paths::tildify(&dir)
        );
    }
    changes_in(&dir, &base)
}

/// The diff of `dir` against `base`. Renames show as a deletion and a new file, so every
/// hunk can be dropped on its own.
pub fn changes_in(dir: &Path, base: &str) -> Result<Changes> {
    let mut text = git_raw(
        dir,
        &[
            // Paths as they are, not octal-quoted: the parser and `--drop` match them.
            "-c",
            "core.quotePath=false",
            "diff",
            "--no-color",
            "--no-ext-diff",
            "--no-renames",
            // a/ and b/ whatever diff.noprefix says: the parser and `git apply` need them.
            "--src-prefix=a/",
            "--dst-prefix=b/",
            base,
            "--",
        ],
    )?;
    let list = git_raw(dir, &["ls-files", "--others", "--exclude-standard", "-z"])?;
    let mut untracked = Vec::new();
    for f in list.split('\0').filter(|f| !f.is_empty()) {
        let (diff, print) = new_file_diff(dir, f);
        text.push_str(&diff);
        untracked.push((f.to_string(), print));
    }
    Ok(Changes {
        dir: dir.to_path_buf(),
        text,
        untracked,
    })
}

/// A fingerprint of a file's content (or a symlink's target), to notice a change. A file
/// too large to show is fingerprinted by its size and modification time, not read whole.
pub fn fingerprint(path: &Path) -> Option<u64> {
    let m = std::fs::symlink_metadata(path).ok()?;
    if m.file_type().is_symlink() {
        Some(hash(&std::fs::read_link(path).ok()?))
    } else if m.is_file() && m.len() > MAX_NEW_FILE {
        Some(hash(&(m.len(), m.modified().ok())))
    } else if m.is_file() {
        Some(hash(&std::fs::read(path).ok()?))
    } else {
        None
    }
}

fn hash<T: std::hash::Hash + ?Sized>(v: &T) -> u64 {
    use std::hash::Hasher;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    v.hash(&mut h);
    h.finish()
}

/// A file git doesn't track yet, as the diff that would add it, and its fingerprint.
fn new_file_diff(dir: &Path, f: &str) -> (String, Option<u64>) {
    let mut out = format!("diff --git a/{f} b/{f}\n{UNTRACKED}\n");
    let path = dir.join(f);
    // Not through symlinks: a link could point anywhere on the machine.
    match std::fs::symlink_metadata(&path) {
        Ok(m) if m.is_file() && m.len() > MAX_NEW_FILE => {
            out.push_str(&format!("({} KB, too large to show)\n", m.len() / 1024));
        }
        Ok(m) if m.is_file() => match std::fs::read(&path) {
            Ok(b) => {
                if b.contains(&0) {
                    out.push_str("(binary file)\n");
                } else {
                    let s = String::from_utf8_lossy(&b);
                    let n = s.lines().count();
                    out.push_str(&format!("+++ b/{f}\n@@ -0,0 +1,{n} @@\n"));
                    for l in s.lines() {
                        out.push('+');
                        out.push_str(l);
                        out.push('\n');
                    }
                }
                // The bytes shown are the bytes fingerprinted, as `fingerprint` does.
                return (out, Some(hash(&b)));
            }
            Err(_) => out.push_str("(unreadable)\n"),
        },
        _ => {}
    }
    (out, fingerprint(&path))
}

/// One review comment.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Comment {
    #[serde(flatten)]
    pub at: Anchor,
    /// The line it was written on, as the diff showed it then.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub code: String,
    pub note: String,
    pub created_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sent_at: Option<i64>,
}

#[derive(Serialize, Deserialize, Default, Debug)]
struct ReviewFile {
    #[serde(default)]
    comments: Vec<Comment>,
}

fn review_file(id: &str) -> PathBuf {
    runs::run_dir(id).join("review.json")
}

pub fn load(id: &str) -> Vec<Comment> {
    read_json::<ReviewFile>(&review_file(id))
        .map(|f| f.comments)
        .unwrap_or_default()
}

pub fn save(id: &str, comments: &[Comment]) -> Result<()> {
    write_json(
        &review_file(id),
        &ReviewFile {
            comments: comments.to_vec(),
        },
    )
}

pub fn pending(comments: &[Comment]) -> usize {
    comments.iter().filter(|c| c.sent_at.is_none()).count()
}

/// Adds a comment and returns how many are waiting to be sent.
pub fn add(id: &str, at: Anchor, code: &str, note: &str) -> Result<usize> {
    let note = note.trim();
    if note.is_empty() {
        bail!("A comment needs some text.");
    }
    let mut all = load(id);
    all.push(Comment {
        at,
        code: code.trim_end().to_string(),
        note: note.to_string(),
        created_at: time::now_ms(),
        sent_at: None,
    });
    save(id, &all)?;
    Ok(pending(&all))
}

/// The message that carries the pending comments to the agent.
pub fn message(comments: &[Comment]) -> Option<String> {
    let todo: Vec<&Comment> = comments.iter().filter(|c| c.sent_at.is_none()).collect();
    if todo.is_empty() {
        return None;
    }
    let mut m = format!(
        "Review of your changes: {} comment{}.\n",
        todo.len(),
        if todo.len() == 1 { "" } else { "s" }
    );
    for (i, c) in todo.iter().enumerate() {
        m.push_str(&format!("\n{}. {}\n", i + 1, c.at));
        let code = c.code.trim();
        if !code.is_empty() {
            m.push_str(&format!("   > {}\n", crate::util::one_line(code, 200)));
        }
        for l in c.note.lines() {
            m.push_str(&format!("   {l}\n"));
        }
    }
    m.push_str("\nPlease address each comment, then tell me what you changed.");
    Some(m)
}

/// Sends the pending comments to the run's agent and marks them sent. Returns how many.
pub fn send(run: &Run) -> Result<usize> {
    let mut all = load(&run.id);
    let Some(msg) = message(&all) else {
        bail!("{} has no comments waiting to be sent.", run.id);
    };
    crate::runs::client::send_text(run, &msg, true)?;
    let now = time::now_ms();
    let mut n = 0;
    for c in all.iter_mut().filter(|c| c.sent_at.is_none()) {
        c.sent_at = Some(now);
        n += 1;
    }
    save(&run.id, &all)?;
    Ok(n)
}

/// The text of a line in the run's folder, for a comment made from the CLI.
pub fn line_text(run: &Run, at: &Anchor) -> String {
    if at.removed {
        return String::new();
    }
    let Ok((dir, _)) = diff_target(run) else {
        return String::new();
    };
    // Only files inside the run's folder.
    let path = dir.join(&at.file);
    let inside = path
        .canonicalize()
        .ok()
        .zip(dir.canonicalize().ok())
        .is_some_and(|(p, d)| p.starts_with(d));
    if !inside {
        return String::new();
    }
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| {
            s.lines()
                .nth(at.line.saturating_sub(1) as usize)
                .map(str::to_string)
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIFF: &str = "diff --git a/src/a.rs b/src/a.rs
index 1..2 100644
--- a/src/a.rs
+++ b/src/a.rs
@@ -10,3 +10,4 @@ fn main() {
 let a = 1;
-let b = 2;
+let b = 3;
+let c = 4;
 done();
\\ No newline at end of file
diff --git a/new.txt b/new.txt
new file mode 100644
--- /dev/null
+++ b/new.txt
@@ -0,0 +1 @@
+hello
";

    #[test]
    fn parses_files_and_line_numbers() {
        let l = parse_diff(DIFF);
        let find = |t: &str| l.iter().find(|d| d.text == t).unwrap();
        assert_eq!(l[0].kind, LineKind::File);
        assert_eq!(l[0].file, "src/a.rs");
        assert_eq!(find(" let a = 1;").new, Some(10));
        let removed = find("-let b = 2;");
        assert_eq!(
            (removed.kind, removed.old, removed.new),
            (LineKind::Removed, Some(11), None)
        );
        assert_eq!(find("+let b = 3;").new, Some(11));
        assert_eq!(find("+let c = 4;").new, Some(12));
        assert_eq!(find(" done();").new, Some(13));
        assert_eq!(find("\\ No newline at end of file").kind, LineKind::Meta);
        let hello = find("+hello");
        assert_eq!((hello.file.as_str(), hello.new), ("new.txt", Some(1)));
        assert_eq!(
            removed.anchor().unwrap().to_string(),
            "src/a.rs:11 (removed line)"
        );
    }

    #[test]
    fn review_data_round_trips_and_its_digest_notices_changes() {
        let changes = Changes {
            dir: PathBuf::from("/somewhere"),
            text: format!(
                "{DIFF}diff --git a/n.txt b/n.txt\n{UNTRACKED}\n+++ b/n.txt\n@@ -0,0 +1,1 @@\n+x\n"
            ),
            untracked: vec![("n.txt".into(), Some(42))],
        };
        let note = Comment {
            at: Anchor {
                file: "src/a.rs".into(),
                line: 11,
                removed: true,
            },
            code: "-let b = 2;".into(),
            note: "keep it".into(),
            created_at: 1,
            sent_at: None,
        };
        let data = changes.data(vec![note]);
        let json = serde_json::to_string(&data).unwrap();
        let back: ReviewData = serde_json::from_str(&json).unwrap();
        assert_eq!(back, data);
        // The same lines and hunks as reading the diff here.
        let there = back.changes();
        assert_eq!(parse_diff(&there.text), parse_diff(&changes.text));
        let hunks = |c: &Changes| {
            crate::work::hunks::parse(c)
                .iter()
                .map(|h| (h.file.clone(), h.number, h.untracked, h.place()))
                .collect::<Vec<_>>()
        };
        assert_eq!(hunks(&there), hunks(&changes));
        assert_eq!(hunks(&there).len(), 3);
        // The digest is stable, and any change to the diff or a new file changes it.
        assert_eq!(there.digest(), changes.digest());
        assert_eq!(changes.digest().len(), 16);
        let edited = Changes {
            text: changes.text.replace("+let c = 4;", "+let c = 5;"),
            ..changes.clone()
        };
        assert_ne!(edited.digest(), changes.digest());
        let touched = Changes {
            untracked: vec![("n.txt".into(), Some(43))],
            ..changes.clone()
        };
        assert_ne!(touched.digest(), changes.digest());
        // An older amux's output without comments still reads.
        let bare: ReviewData = serde_json::from_str(r#"{"text": "", "digest": "0"}"#).unwrap();
        assert!(bare.comments.is_empty() && bare.untracked.is_empty());
    }

    #[test]
    fn builds_one_message_from_pending_comments() {
        let c = |line, note: &str, sent| Comment {
            at: Anchor {
                file: "a.rs".into(),
                line,
                removed: false,
            },
            code: "+let b = 3;".into(),
            note: note.into(),
            created_at: 0,
            sent_at: sent,
        };
        assert_eq!(message(&[c(1, "old", Some(1))]), None);
        let m = message(&[c(1, "old", Some(1)), c(11, "Why 3?\nUse a constant.", None)]).unwrap();
        assert!(m.starts_with("Review of your changes: 1 comment."), "{m}");
        assert!(
            m.contains("1. a.rs:11\n   > +let b = 3;\n   Why 3?\n   Use a constant.\n"),
            "{m}"
        );
        assert!(!m.contains("old"));
    }
}
