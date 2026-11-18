//! Issues as work for runs: `amux spawn --issue <ref>` turns one into a run's first
//! prompt, and `amux intake` starts a run for each new issue with a label. GitHub issues
//! come through the GitHub CLI; Linear and Jira issues through their APIs (trackers.rs).
//! Which issues `intake` has started is kept in `~/.amux/intake.json`, per repo, so a run
//! that was removed is not started again.

use super::github::gh_out;
use super::trackers::{self, Tracked};
use crate::paths;
use crate::store::{read_json, write_json};
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;

/// Where an issue lives.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Source {
    #[default]
    GitHub,
    Linear,
    Jira,
}

impl Source {
    pub fn name(self) -> &'static str {
        match self {
            Source::GitHub => "GitHub",
            Source::Linear => "Linear",
            Source::Jira => "Jira",
        }
    }

    /// As in `--issue linear:ENG-1` and `intake --from linear`.
    pub fn tag(self) -> &'static str {
        match self {
            Source::GitHub => "github",
            Source::Linear => "linear",
            Source::Jira => "jira",
        }
    }

    pub fn from_tag(s: &str) -> Option<Source> {
        match s.to_ascii_lowercase().as_str() {
            "github" | "gh" => Some(Source::GitHub),
            "linear" => Some(Source::Linear),
            "jira" => Some(Source::Jira),
            _ => None,
        }
    }
}

/// A comment on an issue, for the prompt.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Comment {
    pub author: String,
    pub body: String,
}

#[derive(Deserialize, Debug, Clone, Default)]
pub struct Issue {
    /// GitHub's issue number; 0 for Linear and Jira.
    #[serde(default)]
    pub number: u64,
    pub title: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub url: String,
    #[serde(skip)]
    pub source: Source,
    /// Linear's identifier or Jira's key (ENG-123), upper case.
    #[serde(skip)]
    pub key: String,
    /// Linear's internal id, which comments are posted to.
    #[serde(skip)]
    pub id: String,
    #[serde(skip)]
    pub comments: Vec<Comment>,
}

impl Issue {
    /// How it's named in messages: #12, ENG-123.
    pub fn label(&self) -> String {
        match self.source {
            Source::GitHub => format!("#{}", self.number),
            _ => self.key.clone(),
        }
    }

    /// The run that works on it, and so its branch (amux/<name>): issue-12, eng-123.
    pub fn run_name(&self) -> String {
        match self.source {
            Source::GitHub => format!("issue-{}", self.number),
            _ => self.key.to_ascii_lowercase(),
        }
    }

    /// What `amux spawn --issue` takes for it.
    pub fn spawn_ref(&self) -> String {
        match self.source {
            Source::GitHub => self.number.to_string(),
            s => format!("{}:{}", s.tag(), self.key),
        }
    }

    /// How intake.json remembers it: GitHub's number, or "linear:ENG-123".
    fn intake_id(&self) -> Value {
        match self.source {
            Source::GitHub => self.number.into(),
            _ => self.spawn_ref().into(),
        }
    }

    /// What the run keeps about a Linear or Jira issue (run.json `tracker`).
    pub fn tracked(&self) -> Option<Tracked> {
        (self.source != Source::GitHub).then(|| Tracked {
            tracker: self.source.tag().into(),
            key: self.key.clone(),
            url: self.url.clone(),
            id: self.id.clone(),
        })
    }

    pub fn github_number(&self) -> Option<u64> {
        (self.source == Source::GitHub).then_some(self.number)
    }
}

/// What `--issue` was given.
#[derive(Debug, PartialEq, Eq)]
pub enum Ref {
    /// A number (12, #12) or an issue's URL: what `gh issue view` accepts.
    GitHub(String),
    Linear(String),
    Jira(String),
}

const REF_HELP: &str = "--issue takes a GitHub issue number (12 or #12) or URL, linear:ENG-123, jira:PROJ-45, or a Linear or Jira issue URL";

/// `ENG-123`, `eng-123` → `ENG-123`: a team or project key and a number.
pub fn tracker_key(k: &str) -> Option<String> {
    let (team, n) = k.split_once('-')?;
    let ok = team.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
        && team.len() <= 32
        && team.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !n.is_empty()
        && n.len() <= 12
        && n.bytes().all(|b| b.is_ascii_digit());
    ok.then(|| format!("{}-{n}", team.to_ascii_uppercase()))
}

/// Reads an `--issue` value. Nothing that reads as an option gets through.
pub fn parse_ref(r: &str) -> Option<Ref> {
    let n = r.strip_prefix('#').unwrap_or(r);
    if !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) {
        return Some(Ref::GitHub(n.to_string()));
    }
    if let Some((tag, key)) = r.split_once(':') {
        match tag.to_ascii_lowercase().as_str() {
            "linear" => return tracker_key(key).map(Ref::Linear),
            "jira" => return tracker_key(key).map(Ref::Jira),
            _ => {}
        }
    }
    let rest = r.strip_prefix("https://")?;
    let host = rest.split('/').next().unwrap_or_default();
    // https://linear.app/<workspace>/issue/ENG-123/<slug>
    if host == "linear.app" {
        let key = rest
            .split("/issue/")
            .nth(1)?
            .split(['/', '?', '#'])
            .next()?;
        return tracker_key(key).map(Ref::Linear);
    }
    // https://<site>/browse/PROJ-45
    if let Some(after) = rest.split("/browse/").nth(1) {
        return tracker_key(after.split(['/', '?', '#']).next()?).map(Ref::Jira);
    }
    // ?selectedIssue=PROJ-45 on a Jira board
    if host.ends_with(".atlassian.net") {
        let key = rest.split("selectedIssue=").nth(1)?.split('&').next()?;
        return tracker_key(key).map(Ref::Jira);
    }
    rest.contains("/issues/")
        .then(|| Ref::GitHub(r.to_string()))
}

const FIELDS: &str = "number,title,body,url";

/// The issue `r` names, from GitHub (`gh`, in `dir`'s repo), Linear or Jira.
pub fn view(dir: &Path, r: &str) -> Result<Issue> {
    match parse_ref(r) {
        None => bail!("{REF_HELP}, not \"{r}\"."),
        Some(Ref::Linear(k)) => trackers::linear_issue(&k),
        Some(Ref::Jira(k)) => trackers::jira_issue(&k),
        Some(Ref::GitHub(r)) => {
            let out = gh_out(dir, &["issue", "view", &r, "--json", FIELDS], &[])?;
            serde_json::from_str(&out).context("unexpected output from gh issue view")
        }
    }
}

/// Open GitHub issues, oldest first, with `label` if given.
pub fn open(dir: &Path, label: Option<&str>, limit: usize) -> Result<Vec<Issue>> {
    let limit = limit.max(1).to_string();
    let mut args = vec![
        "issue", "list", "--state", "open", "--json", FIELDS, "--limit", &limit,
    ];
    if let Some(l) = label {
        args.extend(["--label", l]);
    }
    let out = gh_out(dir, &args, &[])?;
    let mut v: Vec<Issue> = if out.trim().is_empty() {
        vec![]
    } else {
        serde_json::from_str(&out).context("unexpected output from gh issue list")?
    };
    v.sort_by_key(|i| i.number);
    Ok(v)
}

/// Text cut to `max` characters, marked when cut.
fn cut(s: &str, max: usize) -> String {
    if s.chars().count() > max {
        s.chars().take(max).collect::<String>() + "\n[…]"
    } else {
        s.to_string()
    }
}

/// The first prompt of a run that works on `issue`, with the caller's own instructions.
pub fn prompt(issue: &Issue, extra: Option<&str>) -> String {
    let body = cut(issue.body.trim(), 8000);
    let mut p = format!(
        "Work on {} issue {}: {}\n",
        issue.source.name(),
        issue.label(),
        issue.title.trim()
    );
    if !issue.url.is_empty() {
        p.push_str(&format!("{}\n", issue.url));
    }
    if !body.is_empty() {
        p.push_str(&format!("\n{body}\n"));
    }
    // The newest comments, oldest first, within a budget.
    let mut budget = 4000usize;
    let mut shown = vec![];
    for c in issue.comments.iter().rev() {
        let text = cut(c.body.trim(), 2000);
        if text.is_empty() {
            continue;
        }
        // Later lines of a comment are indented under it; blank lines stay blank.
        let text = text
            .lines()
            .enumerate()
            .map(|(i, l)| match (i, l.trim().is_empty()) {
                (_, true) => String::new(),
                (0, _) => l.to_string(),
                _ => format!("  {l}"),
            })
            .collect::<Vec<_>>()
            .join("\n");
        let line = format!("- {}: {text}", c.author.trim());
        if line.len() > budget {
            break;
        }
        budget -= line.len();
        shown.push(line);
    }
    if !shown.is_empty() {
        shown.reverse();
        p.push_str(&format!("\nComments:\n{}\n", shown.join("\n")));
    }
    if let Some(x) = extra.map(str::trim).filter(|x| !x.is_empty()) {
        p.push_str(&format!("\n{x}\n"));
    }
    let how = match issue.source {
        Source::GitHub => format!("a message that ends with \"Fixes #{}\"", issue.number),
        Source::Linear => format!("a message that ends with \"Fixes {}\"", issue.key),
        Source::Jira => format!("a message that starts with \"{}\"", issue.key),
    };
    p.push_str(&format!(
        "\nMake the change on this branch, with tests where they fit, and commit it with \
         {how}. If the issue is unclear or can't be done, say why instead of guessing."
    ));
    p
}

fn seen_file() -> std::path::PathBuf {
    paths::AMUX_HOME.join("intake.json")
}

/// The repo an intake belongs to: its top folder.
pub fn repo_key(dir: &Path) -> String {
    crate::git::git(dir, &["rev-parse", "--show-toplevel"])
        .unwrap_or_else(|_| dir.to_string_lossy().into_owned())
}

/// intake.json: per repo, GitHub issue numbers and "linear:ENG-1" / "jira:PROJ-2" keys.
type Seen = BTreeMap<String, Vec<Value>>;

pub fn taken(repo: &str, issue: &Issue) -> bool {
    let id = issue.intake_id();
    read_json::<Seen>(&seen_file())
        .unwrap_or_default()
        .get(repo)
        .is_some_and(|v| v.contains(&id))
}

pub fn mark_taken(repo: &str, issue: &Issue) -> Result<()> {
    let mut all: Seen = read_json(&seen_file()).unwrap_or_default();
    let v = all.entry(repo.to_string()).or_default();
    let id = issue.intake_id();
    if !v.contains(&id) {
        v.push(id);
    }
    write_json(&seen_file(), &all)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_ref(r: &str) -> bool {
        parse_ref(r).is_some()
    }

    #[test]
    fn issue_refs() {
        for ok in ["12", "#12", "https://github.com/o/r/issues/12"] {
            assert!(valid_ref(ok), "{ok}");
            assert!(matches!(parse_ref(ok), Some(Ref::GitHub(_))), "{ok}");
        }
        for bad in [
            "",
            "#",
            "-12",
            "--web",
            "12a",
            "http://x/issues/1",
            "linear:",
            "linear:ENG",
            "linear:ENG-",
            "linear:-1",
            "jira:1-2",
            "jira:PROJ-4x",
            "jira:--help",
            "gitlab:ENG-1",
            "ENG-123",
            "https://linear.app/acme/project/x",
        ] {
            assert!(!valid_ref(bad), "{bad}");
        }
        let lin = |k: &str| Some(Ref::Linear(k.into()));
        let jira = |k: &str| Some(Ref::Jira(k.into()));
        assert_eq!(parse_ref("linear:ENG-123"), lin("ENG-123"));
        assert_eq!(parse_ref("Linear:eng-7"), lin("ENG-7"));
        assert_eq!(
            parse_ref("https://linear.app/acme/issue/ENG-123/fix-the-crash"),
            lin("ENG-123")
        );
        assert_eq!(parse_ref("jira:PROJ-45"), jira("PROJ-45"));
        assert_eq!(parse_ref("jira:ab_2-9"), jira("AB_2-9"));
        assert_eq!(
            parse_ref("https://acme.atlassian.net/browse/PROJ-45"),
            jira("PROJ-45")
        );
        assert_eq!(
            parse_ref("https://jira.acme.com/browse/OPS-3?focused=1"),
            jira("OPS-3")
        );
        assert_eq!(
            parse_ref(
                "https://acme.atlassian.net/jira/software/projects/P/boards/1?selectedIssue=P-9"
            ),
            jira("P-9")
        );
    }

    #[test]
    fn writes_the_prompt() {
        let i = Issue {
            number: 7,
            title: "Crash on empty input".into(),
            body: "Steps: run it with no args.".into(),
            url: "https://github.com/o/r/issues/7".into(),
            ..Default::default()
        };
        assert_eq!((i.run_name(), i.label()), ("issue-7".into(), "#7".into()));
        let p = prompt(&i, Some("Keep it small."));
        assert!(p.starts_with("Work on GitHub issue #7: Crash on empty input\nhttps://"));
        assert!(p.contains("Steps: run it") && p.contains("Keep it small."));
        assert!(p.contains("\"Fixes #7\""));
        assert!(!p.contains("Comments:"));
        assert_eq!(i.intake_id(), Value::from(7));
        assert!(i.tracked().is_none());
    }

    #[test]
    fn writes_the_prompt_for_linear_and_jira() {
        let mut i = Issue {
            title: "Login fails".into(),
            body: "It **fails**.".into(),
            url: "https://linear.app/acme/issue/ENG-123/login-fails".into(),
            source: Source::Linear,
            key: "ENG-123".into(),
            id: "uuid-1".into(),
            comments: vec![
                Comment {
                    author: "Ann".into(),
                    body: "Only on Safari.\nSince 2.1.".into(),
                },
                Comment {
                    author: "Bo".into(),
                    body: "  ".into(),
                },
            ],
            ..Default::default()
        };
        assert_eq!(i.run_name(), "eng-123");
        assert_eq!(i.spawn_ref(), "linear:ENG-123");
        assert_eq!(i.intake_id(), Value::from("linear:ENG-123"));
        let p = prompt(&i, None);
        assert!(
            p.starts_with("Work on Linear issue ENG-123: Login fails\nhttps://linear.app/"),
            "{p}"
        );
        assert!(
            p.contains("\nComments:\n- Ann: Only on Safari.\n  Since 2.1.\n"),
            "{p}"
        );
        assert!(!p.contains("- Bo"));
        assert!(p.contains("\"Fixes ENG-123\""));
        let t = i.tracked().unwrap();
        assert_eq!((t.tracker.as_str(), t.id.as_str()), ("linear", "uuid-1"));
        i.source = Source::Jira;
        i.key = "PROJ-45".into();
        let p = prompt(&i, None);
        assert!(p.starts_with("Work on Jira issue PROJ-45: "));
        assert!(p.contains("starts with \"PROJ-45\""));
        assert_eq!(i.run_name(), "proj-45");
    }
}
