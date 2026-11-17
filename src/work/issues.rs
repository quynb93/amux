//! GitHub issues as work for runs, through the GitHub CLI: `amux spawn --issue <n>` turns
//! one into a run's first prompt, and `amux intake` starts a run for each new issue with a
//! label. Which issues `intake` has started is kept in `~/.amux/intake.json`, per repo, so
//! a run that was removed is not started again.

use super::github::gh_out;
use crate::paths;
use crate::store::{read_json, write_json};
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Deserialize, Debug, Clone, Default)]
pub struct Issue {
    pub number: u64,
    pub title: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub url: String,
}

const FIELDS: &str = "number,title,body,url";

/// `12`, `#12`, or an issue's URL: what `gh issue view` accepts, and nothing that reads as
/// an option.
pub fn valid_ref(r: &str) -> bool {
    let n = r.strip_prefix('#').unwrap_or(r);
    (!n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        || (r.starts_with("https://") && r.contains("/issues/"))
}

pub fn view(dir: &Path, r: &str) -> Result<Issue> {
    if !valid_ref(r) {
        bail!("--issue takes an issue number (12 or #12) or its URL, not \"{r}\".");
    }
    let r = r.strip_prefix('#').unwrap_or(r);
    let out = gh_out(dir, &["issue", "view", r, "--json", FIELDS], &[])?;
    serde_json::from_str(&out).context("unexpected output from gh issue view")
}

/// Open issues, oldest first, with `label` if given.
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

/// The first prompt of a run that works on `issue`, with the caller's own instructions.
pub fn prompt(issue: &Issue, extra: Option<&str>) -> String {
    let mut body = issue.body.trim().to_string();
    if body.chars().count() > 8000 {
        body = body.chars().take(8000).collect::<String>() + "\n[…]";
    }
    let mut p = format!(
        "Work on GitHub issue #{}: {}\n",
        issue.number,
        issue.title.trim()
    );
    if !issue.url.is_empty() {
        p.push_str(&format!("{}\n", issue.url));
    }
    if !body.is_empty() {
        p.push_str(&format!("\n{body}\n"));
    }
    if let Some(x) = extra.map(str::trim).filter(|x| !x.is_empty()) {
        p.push_str(&format!("\n{x}\n"));
    }
    p.push_str(&format!(
        "\nMake the change on this branch, with tests where they fit, and commit it with a \
         message that ends with \"Fixes #{}\". If the issue is unclear or can't be done, say \
         why instead of guessing.",
        issue.number
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

pub fn taken(repo: &str) -> Vec<u64> {
    read_json::<BTreeMap<String, Vec<u64>>>(&seen_file())
        .unwrap_or_default()
        .remove(repo)
        .unwrap_or_default()
}

pub fn mark_taken(repo: &str, n: u64) -> Result<()> {
    let mut all: BTreeMap<String, Vec<u64>> = read_json(&seen_file()).unwrap_or_default();
    let v = all.entry(repo.to_string()).or_default();
    if !v.contains(&n) {
        v.push(n);
    }
    write_json(&seen_file(), &all)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issue_refs() {
        for ok in ["12", "#12", "https://github.com/o/r/issues/12"] {
            assert!(valid_ref(ok), "{ok}");
        }
        for bad in ["", "#", "-12", "--web", "12a", "http://x/issues/1"] {
            assert!(!valid_ref(bad), "{bad}");
        }
    }

    #[test]
    fn writes_the_prompt() {
        let i = Issue {
            number: 7,
            title: "Crash on empty input".into(),
            body: "Steps: run it with no args.".into(),
            url: "https://github.com/o/r/issues/7".into(),
        };
        let p = prompt(&i, Some("Keep it small."));
        assert!(p.starts_with("Work on GitHub issue #7: Crash on empty input\nhttps://"));
        assert!(p.contains("Steps: run it") && p.contains("Keep it small."));
        assert!(p.contains("\"Fixes #7\""));
    }
}
