//! Auto-approve: answers a CLI's permission prompt by itself when your rules allow what it
//! asks, so an unattended run doesn't sit on "Do you want to proceed?" for `cargo test`.
//!
//! Rules live in `~/.amux/approve.json`: `{ "allow": [...], "deny": [...] }`, each
//! `<kind>[: <pattern>]`. Kinds: `bash` (a shell command), `edit` (creating or changing a
//! file), or the first word of the prompt's title for anything else (`fetch`). Patterns use
//! `*` for any text. Deny rules win. A file edit is only approved inside the run's folder.
//! A shell command is split on `&&`, `||`, `;` and `|`, and every part must be allowed;
//! command substitution, redirection (other than to /dev/null or 2>&1) and multi-line
//! commands are never approved. What was approved is logged in `runs/<id>/approvals.jsonl`.
//!
//! The prompt is read from the run's screen. Claude Code's layout is known: a title such
//! as "Bash command" or "Create file", what it wants between dashed rules, the question
//! "Do you want to …?", and a menu whose first choice, "1. Yes", is chosen with the key `1`.

use crate::paths;
use crate::store::{read_json, write_json};
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Rules {
    #[serde(default)]
    pub allow: Vec<String>,
    #[serde(default)]
    pub deny: Vec<String>,
}

pub fn file() -> std::path::PathBuf {
    paths::AMUX_HOME.join("approve.json")
}

pub fn load() -> Rules {
    read_json(&file()).unwrap_or_default()
}

pub fn save(r: &Rules) -> Result<()> {
    write_json(&file(), r)
}

/// A rule as written: `bash: cargo test*`, `edit`, `edit: src/*`.
pub fn parse_rule(s: &str) -> Result<(String, String)> {
    let (kind, pat) = match s.split_once(':') {
        Some((k, p)) => (k, p.trim()),
        None => (s, ""),
    };
    let kind = kind.trim().to_ascii_lowercase();
    if kind.is_empty()
        || !kind
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        bail!("A rule is <kind>[: <pattern>], such as \"bash: cargo test*\" or \"edit\", not \"{s}\".");
    }
    Ok((kind, pat.to_string()))
}

/// Shell-style `*` (any text, spaces and slashes included) and `?` (one character).
pub fn glob(pat: &str, s: &str) -> bool {
    let (p, t): (Vec<char>, Vec<char>) = (pat.chars().collect(), s.chars().collect());
    let (mut i, mut j, mut star, mut mark) = (0, 0, None, 0);
    while j < t.len() {
        if i < p.len() && (p[i] == '?' || p[i] == t[j]) {
            i += 1;
            j += 1;
        } else if i < p.len() && p[i] == '*' {
            star = Some(i);
            mark = j;
            i += 1;
        } else if let Some(si) = star {
            i = si + 1;
            mark += 1;
            j = mark;
        } else {
            return false;
        }
    }
    p[i..].iter().all(|c| *c == '*')
}

/// What a permission prompt asks for.
#[derive(Debug, Clone, PartialEq)]
pub struct Ask {
    /// bash, edit, or the title's first word.
    pub kind: String,
    /// The command, or the file; empty when the prompt names neither.
    pub subject: String,
    /// The keys that choose "Yes".
    pub yes: &'static str,
}

impl Ask {
    pub fn label(&self) -> String {
        if self.subject.is_empty() {
            self.kind.clone()
        } else {
            format!("{}: {}", self.kind, self.subject)
        }
    }
}

fn is_rule_line(l: &str, c: char) -> bool {
    let t = l.trim();
    t.chars().count() >= 8 && t.chars().all(|x| x == c)
}

/// Finds a permission prompt on the screen (its rows, top to bottom).
pub fn read_prompt(rows: &[String]) -> Option<Ask> {
    // The question, with "1. Yes" chosen just below it.
    let q = rows
        .iter()
        .rposition(|l| l.trim_start().starts_with("Do you want to"))?;
    let yes = rows.get(q + 1)?.trim();
    if !(yes.starts_with("❯ 1. Yes") || yes.starts_with("> 1. Yes")) {
        return None;
    }
    // The prompt starts after the last full-width rule above the question.
    let top = rows[..q].iter().rposition(|l| is_rule_line(l, '─'))? + 1;
    let block: Vec<&str> = rows[top..q].iter().map(|l| l.trim()).collect();
    let title = *block.iter().find(|l| !l.is_empty())?;
    let ti = block.iter().position(|l| *l == title)?;
    let lower = title.to_ascii_lowercase();
    let (kind, subject) = if lower == "bash command" {
        // Between the first two dashed rules.
        let d: Vec<usize> = block
            .iter()
            .enumerate()
            .filter(|(_, l)| is_rule_line(l, '╌'))
            .map(|(i, _)| i)
            .take(2)
            .collect();
        let [a, b] = d[..] else { return None };
        let cmd: Vec<&str> = block[a + 1..b]
            .iter()
            .copied()
            .filter(|l| !l.is_empty())
            .collect();
        // A command the screen shows on several lines is never approved: it can't be told
        // apart from one line wrapped.
        if cmd.len() != 1 {
            return Some(Ask {
                kind: "bash".into(),
                subject: cmd.join("\n"),
                yes: "1",
            });
        }
        ("bash".to_string(), cmd[0].to_string())
    } else if lower.ends_with(" file") || lower == "edit" || lower == "write" {
        let path = block.get(ti + 1).copied().unwrap_or("").to_string();
        ("edit".to_string(), path)
    } else {
        let w = lower.split_whitespace().next().unwrap_or("").to_string();
        (w, String::new())
    };
    Some(Ask {
        kind,
        subject,
        yes: "1",
    })
}

/// Shell commands in a line: split on `&&`, `||`, `;` and `|`. None when the line does
/// anything that can't be judged part by part.
fn commands(line: &str) -> Option<Vec<String>> {
    if line.contains('\n')
        || line.contains('`')
        || line.contains("$(")
        || line.contains("<(")
        || line.contains(">(")
    {
        return None;
    }
    // Harmless redirections, then any other is out.
    let mut l = line.to_string();
    for ok in [
        "2>&1",
        "2>/dev/null",
        ">/dev/null",
        "> /dev/null",
        "2> /dev/null",
    ] {
        l = l.replace(ok, " ");
    }
    if l.contains('>') || l.contains('<') {
        return None;
    }
    let l = l.replace("&&", ";").replace("||", ";").replace('|', ";");
    if l.contains('&') {
        return None;
    }
    let parts: Vec<String> = l
        .split(';')
        .map(|p| p.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|p| !p.is_empty())
        .collect();
    (!parts.is_empty()).then_some(parts)
}

/// Why amux may (or may not) answer `ask` by itself.
#[derive(Debug, PartialEq)]
pub enum Verdict {
    /// Allowed, by these rules.
    Allow(Vec<String>),
    /// A deny rule matched.
    Deny(String),
    /// No rule says; you decide.
    Ask(String),
}

fn matching<'a>(rules: &'a [String], kind: &str, subject: &str) -> Option<&'a String> {
    rules.iter().find(|r| match parse_rule(r) {
        Ok((k, p)) => k == kind && (p.is_empty() || glob(&p, subject)),
        Err(_) => false,
    })
}

pub fn judge(rules: &Rules, ask: &Ask, cwd: &Path) -> Verdict {
    if let Some(d) = matching(&rules.deny, &ask.kind, &ask.subject) {
        return Verdict::Deny(d.clone());
    }
    match ask.kind.as_str() {
        "bash" => {
            let Some(parts) = commands(&ask.subject) else {
                return Verdict::Ask("it substitutes, redirects or spans lines".into());
            };
            let mut by = vec![];
            for p in &parts {
                if let Some(d) = matching(&rules.deny, "bash", p) {
                    return Verdict::Deny(d.clone());
                }
                match matching(&rules.allow, "bash", p) {
                    Some(r) => by.push(r.clone()),
                    None => return Verdict::Ask(format!("no rule allows `{p}`")),
                }
            }
            by.dedup();
            Verdict::Allow(by)
        }
        "edit" => {
            if !inside(&ask.subject, cwd) {
                return Verdict::Ask("the file is outside the run's folder".into());
            }
            match matching(&rules.allow, "edit", &ask.subject) {
                Some(r) => Verdict::Allow(vec![r.clone()]),
                None => Verdict::Ask("no rule allows it".into()),
            }
        }
        k => match matching(&rules.allow, k, &ask.subject) {
            Some(r) => Verdict::Allow(vec![r.clone()]),
            None => Verdict::Ask("no rule allows it".into()),
        },
    }
}

/// A file path as the prompt shows it, inside `cwd`.
fn inside(path: &str, cwd: &Path) -> bool {
    if path.is_empty() || path.split('/').any(|c| c == "..") || path.starts_with('~') {
        return false;
    }
    let p = Path::new(path);
    if p.is_relative() {
        return true;
    }
    let cwd = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    p.starts_with(&cwd)
}

/// A sensible start: read-only git and file commands, builds, tests and linters, and edits
/// inside the run's folder. Pushing, deleting recursively and sudo always ask.
pub fn starter() -> Rules {
    let allow = [
        "git status*",
        "git diff*",
        "git log*",
        "git show*",
        "git branch",
        "git add *",
        "git commit *",
        "ls",
        "ls *",
        "pwd",
        "cat *",
        "head *",
        "tail *",
        "wc *",
        "grep *",
        "rg *",
        "cargo build*",
        "cargo test*",
        "cargo check*",
        "cargo clippy*",
        "cargo fmt*",
        "npm test*",
        "npm run test*",
        "npm run lint*",
        "npm run build*",
        "pnpm test*",
        "yarn test*",
        "pytest*",
        "python3 -m pytest*",
        "python3 -m unittest*",
        "go build*",
        "go test*",
        "go vet*",
        "make test*",
    ];
    let mut r = Rules {
        allow: allow.iter().map(|c| format!("bash: {c}")).collect(),
        deny: [
            "*git push*",
            "*rm -rf*",
            "*sudo*",
            "*--force*",
            "*--no-verify*",
        ]
        .iter()
        .map(|c| format!("bash: {c}"))
        .collect(),
    };
    r.allow.push("edit".into());
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen(s: &str) -> Vec<String> {
        s.lines().map(str::to_string).collect()
    }

    const BASH: &str = "⏺ Running 1 shell command…
────────────────────────────────────────
 Bash command
 Tip: auto mode handles these prompts for you
 Run unit tests quietly
╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌
 python3 -m unittest -q
╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌
 This command requires approval

 Do you want to proceed?
 ❯ 1. Yes
   2. Yes, and don’t ask again for: python3 *
   4. No

 Esc to cancel · Tab to amend";

    const CREATE: &str = "⏺ Write(note.txt)
────────────────────────────────────────
 Create file
 note.txt
╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌
  1 hi
╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌
 Do you want to create note.txt?
 ❯ 1. Yes
   3. No";

    #[test]
    fn reads_claude_codes_prompts() {
        let a = read_prompt(&screen(BASH)).unwrap();
        assert_eq!(
            (a.kind.as_str(), a.subject.as_str()),
            ("bash", "python3 -m unittest -q")
        );
        let a = read_prompt(&screen(CREATE)).unwrap();
        assert_eq!((a.kind.as_str(), a.subject.as_str()), ("edit", "note.txt"));
        // Not a permission prompt, or "Yes" not chosen.
        assert!(read_prompt(&screen("  Teach auto mode?\n  ❯ 1. Yes\n    2. Not now")).is_none());
        let moved = BASH
            .replace("❯ 1. Yes", "  1. Yes")
            .replace("   4. No", " ❯ 4. No");
        assert!(read_prompt(&screen(&moved)).is_none());
    }

    #[test]
    fn globs() {
        assert!(glob("cargo test*", "cargo test --all"));
        assert!(glob("cargo test*", "cargo test"));
        // A pattern alone would match a chained command; `commands` splits it first.
        assert!(glob("cargo test*", "cargo test; rm x"));
        assert!(glob("*rm -rf*", "cd x; rm -rf /"));
        assert!(!glob("git status", "git status --short"));
        assert!(glob("a?c", "abc") && !glob("a?c", "ac"));
    }

    #[test]
    fn judges() {
        let r = starter();
        let cwd = Path::new("/w");
        let bash = |c: &str| Ask {
            kind: "bash".into(),
            subject: c.into(),
            yes: "1",
        };
        let edit = |p: &str| Ask {
            kind: "edit".into(),
            subject: p.into(),
            yes: "1",
        };
        assert!(matches!(
            judge(&r, &bash("cargo test --all 2>&1"), cwd),
            Verdict::Allow(_)
        ));
        assert!(matches!(
            judge(&r, &bash("git status && git diff --stat | head -5"), cwd),
            Verdict::Allow(_)
        ));
        assert!(matches!(
            judge(&r, &bash("cargo test; curl x"), cwd),
            Verdict::Ask(_)
        ));
        assert!(matches!(
            judge(&r, &bash("git push origin main"), cwd),
            Verdict::Deny(_)
        ));
        assert!(matches!(
            judge(&r, &bash("cargo test && rm -rf /"), cwd),
            Verdict::Deny(_)
        ));
        assert!(matches!(
            judge(&r, &bash("cat $(echo /etc/passwd)"), cwd),
            Verdict::Ask(_)
        ));
        assert!(matches!(
            judge(&r, &bash("cat a > b"), cwd),
            Verdict::Ask(_)
        ));
        assert!(matches!(
            judge(&r, &bash("cargo test &"), cwd),
            Verdict::Ask(_)
        ));
        assert!(matches!(
            judge(&r, &bash("cargo test\nrm x"), cwd),
            Verdict::Ask(_)
        ));
        assert!(matches!(
            judge(&r, &edit("src/a.rs"), cwd),
            Verdict::Allow(_)
        ));
        assert!(matches!(
            judge(&r, &edit("/w/src/a.rs"), cwd),
            Verdict::Allow(_)
        ));
        assert!(matches!(judge(&r, &edit("../x"), cwd), Verdict::Ask(_)));
        assert!(matches!(
            judge(&r, &edit("/etc/hosts"), cwd),
            Verdict::Ask(_)
        ));
        assert!(matches!(judge(&r, &edit("~/.zshrc"), cwd), Verdict::Ask(_)));
        let fetch = Ask {
            kind: "fetch".into(),
            subject: String::new(),
            yes: "1",
        };
        assert!(matches!(judge(&r, &fetch, cwd), Verdict::Ask(_)));
        assert!(
            parse_rule("bash: x").is_ok()
                && parse_rule(": x").is_err()
                && parse_rule("a b").is_err()
        );
    }
}
