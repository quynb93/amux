//! Linear and Jira: issues to work on (`amux spawn --issue linear:ENG-123`, `amux intake
//! --from linear|jira`), and a comment with the pull request's link when it opens.
//!
//! Linear is asked through its GraphQL API (https://api.linear.app/graphql) with a
//! personal API key; Jira Cloud through REST v3 with an email and an API token (basic
//! auth). Keys come from the environment (LINEAR_API_KEY; JIRA_API_TOKEN, JIRA_URL,
//! JIRA_EMAIL) or from `amux tracker login`, which keeps the key in the Keychain (a 0600
//! file in ~/.amux/trackers/ on Linux) and Jira's site and email in ~/.amux/trackers.json.

use super::issues::{Comment, Issue, Source};
use crate::paths;
use crate::secrets;
use crate::store::{read_json, write_json};
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::PathBuf;

/// A run's Linear or Jira issue, kept in run.json.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct Tracked {
    /// linear or jira.
    pub tracker: String,
    /// ENG-123, PROJ-45.
    pub key: String,
    #[serde(default)]
    pub url: String,
    /// Linear's internal id for the issue.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub id: String,
}

impl Tracked {
    pub fn source(&self) -> Option<Source> {
        Source::from_tag(&self.tracker).filter(|s| *s != Source::GitHub)
    }

    /// What the pull request's body says about it. Linear links a PR whose body says
    /// "Fixes ENG-123" (with its GitHub integration); Jira links one that names the key.
    pub fn pr_body(&self) -> String {
        let name = self.source().map_or("", |s| s.name());
        let first = match self.source() {
            Some(Source::Linear) => format!("Fixes {}", self.key),
            _ => self.key.clone(),
        };
        let link = if self.url.is_empty() {
            String::new()
        } else {
            format!(": {}", self.url)
        };
        format!(
            "{first}\n\nStarted by amux from {name} issue {}{link}.",
            self.key
        )
    }
}

// ---- Credentials ----

/// Jira's site and email, from `amux tracker login jira` (no secrets).
#[derive(Serialize, Deserialize, Default, Debug)]
pub struct Saved {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jira: Option<JiraSite>,
}

#[derive(Serialize, Deserialize, Default, Debug, Clone)]
pub struct JiraSite {
    pub url: String,
    pub email: String,
}

fn saved_file() -> PathBuf {
    paths::AMUX_HOME.join("trackers.json")
}

pub fn load_saved() -> Saved {
    read_json(&saved_file()).unwrap_or_default()
}

pub fn save_saved(s: &Saved) -> Result<()> {
    write_json(&saved_file(), s)
}

fn secret_name(source: Source) -> String {
    format!("tracker:{}", source.tag())
}

fn secret_file(source: Source) -> PathBuf {
    paths::AMUX_HOME
        .join("trackers")
        .join(format!("{}.token", source.tag()))
}

pub fn store_key(source: Source, key: &str) -> Result<()> {
    secrets::set_secret(&secret_name(source), &secret_file(source), key)
}

pub fn stored_key(source: Source) -> Option<String> {
    secrets::get_secret(&secret_name(source), &secret_file(source)).filter(|k| !k.is_empty())
}

pub fn forget_key(source: Source) {
    secrets::delete_secret(&secret_name(source), &secret_file(source))
}

/// An environment variable, unless unset or blank.
pub fn env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// Linear's API key: LINEAR_API_KEY, or the stored one.
pub fn linear_key() -> Result<String> {
    env("LINEAR_API_KEY")
        .or_else(|| stored_key(Source::Linear))
        .ok_or_else(|| {
            anyhow!("No Linear API key: run `amux tracker login linear`, or set LINEAR_API_KEY.")
        })
}

/// How to reach Jira: site, email and API token.
pub struct Jira {
    pub url: String,
    pub email: String,
    pub token: String,
}

/// Jira's site, email and token: JIRA_URL, JIRA_EMAIL, JIRA_API_TOKEN, or the stored ones.
pub fn jira() -> Result<Jira> {
    let saved = load_saved().jira.unwrap_or_default();
    // JIRA_URL may be given bare (acme.atlassian.net), as `tracker login` takes it.
    let url = env("JIRA_URL").map_or(saved.url, |u| site_url(&u).unwrap_or(u));
    let email = env("JIRA_EMAIL").unwrap_or(saved.email);
    let token = env("JIRA_API_TOKEN").or_else(|| stored_key(Source::Jira));
    match token {
        Some(token) if !url.is_empty() && !email.is_empty() => Ok(Jira {
            url: url.trim_end_matches('/').to_string(),
            email,
            token,
        }),
        _ => bail!(
            "Jira isn't set up: run `amux tracker login jira <site-url> <email>`, or set JIRA_URL, JIRA_EMAIL and JIRA_API_TOKEN."
        ),
    }
}

/// `acme.atlassian.net` or `https://acme.atlassian.net/` → `https://acme.atlassian.net`.
pub fn site_url(s: &str) -> Result<String> {
    let s = s.trim().trim_end_matches('/');
    let s = if s.contains("://") {
        s.to_string()
    } else {
        format!("https://{s}")
    };
    let u = url::Url::parse(&s).map_err(|_| anyhow!("\"{s}\" is not a site URL."))?;
    if !matches!(u.scheme(), "https" | "http") || u.host_str().is_none() {
        bail!("\"{s}\" is not a site URL.");
    }
    Ok(s)
}

// ---- HTTP ----

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(20))
        .user_agent(&format!("amux/{}", env!("CARGO_PKG_VERSION")))
        .build()
}

/// Sends a request and reads a JSON answer, with a readable error for a failed one.
fn send(service: &str, req: ureq::Request, body: Option<&Value>) -> Result<Value> {
    let req = req.set("Accept", "application/json");
    let res = match body {
        Some(b) => req
            .set("Content-Type", "application/json")
            .send_string(&b.to_string()),
        None => req.call(),
    };
    match res {
        Ok(r) => {
            let text = r.into_string()?;
            if text.trim().is_empty() {
                return Ok(Value::Null);
            }
            serde_json::from_str(&text).with_context(|| format!("unexpected answer from {service}"))
        }
        Err(ureq::Error::Status(code, r)) => {
            let text = r.into_string().unwrap_or_default();
            let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
            let why = error_message(&v).unwrap_or_else(|| crate::util::one_line(&text, 200));
            match code {
                401 => bail!("{service} turned down the credentials (401). {why}"),
                403 => bail!("{service} says you may not do that (403). {why}"),
                404 => bail!("{service} has no such issue, or you can't see it (404). {why}"),
                _ => bail!("{service} answered {code}. {why}"),
            }
        }
        Err(e) => Err(anyhow!("could not reach {service}: {e}")),
    }
}

/// The first error message in a Linear (`errors[].message`) or Jira (`errorMessages`,
/// `errors`) answer.
fn error_message(v: &Value) -> Option<String> {
    if let Some(m) = v["errors"][0]["message"].as_str() {
        return Some(m.to_string());
    }
    if let Some(m) = v["errorMessages"][0].as_str() {
        return Some(m.to_string());
    }
    v["errors"]
        .as_object()
        .and_then(|o| o.values().next())
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn base64(bytes: &[u8]) -> String {
    const ABC: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ABC[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

// ---- Linear ----

fn linear_url() -> String {
    env("AMUX_LINEAR_URL").unwrap_or_else(|| "https://api.linear.app/graphql".into())
}

/// One GraphQL request to Linear; returns `data`.
fn linear(key: &str, query: &str, vars: Value) -> Result<Value> {
    let req = agent().post(&linear_url()).set("Authorization", key);
    let v = send(
        "Linear",
        req,
        Some(&json!({ "query": query, "variables": vars })),
    )?;
    if let Some(m) = error_message(&v) {
        bail!("Linear: {m}");
    }
    Ok(v["data"].clone())
}

fn s(v: &Value) -> String {
    v.as_str().unwrap_or_default().to_string()
}

/// Who the key belongs to: checks a key before it's stored.
pub fn linear_viewer(key: &str) -> Result<String> {
    let d = linear(key, "query { viewer { name email } }", json!({}))?;
    let v = &d["viewer"];
    Ok(format!("{} <{}>", s(&v["name"]), s(&v["email"])))
}

/// Linear's connections are newest first with `first` and oldest first with `last`, by
/// `orderBy` (createdAt unless told): the issue's 50 newest comments, sorted back below.
const LINEAR_ISSUE: &str = "query($id: String!) { issue(id: $id) { id identifier title description url comments(first: 50, orderBy: createdAt) { nodes { body createdAt user { name } } } } }";

pub fn linear_issue(key: &str) -> Result<Issue> {
    let d = linear(&linear_key()?, LINEAR_ISSUE, json!({ "id": key }))?;
    let i = &d["issue"];
    if i.is_null() {
        bail!("Linear has no issue {key}, or you can't see it.");
    }
    let mut comments: Vec<(String, Comment)> = i["comments"]["nodes"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|c| {
            (
                s(&c["createdAt"]),
                Comment {
                    author: s(&c["user"]["name"]),
                    body: s(&c["body"]),
                },
            )
        })
        .collect();
    comments.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(Issue {
        title: s(&i["title"]),
        body: s(&i["description"]),
        url: s(&i["url"]),
        source: Source::Linear,
        key: s(&i["identifier"]),
        id: s(&i["id"]),
        comments: comments.into_iter().map(|c| c.1).collect(),
        ..Default::default()
    })
}

/// The 100 oldest matching issues, oldest first (`last` on a createdAt connection), as
/// Jira's `ORDER BY created ASC`: intake starts the issues that have waited longest.
const LINEAR_OPEN: &str = "query($filter: IssueFilter) { issues(filter: $filter, last: 100, orderBy: createdAt) { nodes { id identifier title url createdAt } } }";

/// Linear's IssueFilter for open issues with the label, in the team.
fn linear_filter(label: Option<&str>, team: Option<&str>) -> Value {
    let mut f = json!({ "state": { "type": { "nin": ["completed", "canceled"] } } });
    if let Some(l) = label {
        f["labels"] = json!({ "name": { "eqIgnoreCase": l } });
    }
    if let Some(t) = team {
        f["team"] = json!({ "key": { "eqIgnoreCase": t } });
    }
    f
}

/// Open Linear issues (not completed or canceled) with the label and in the team,
/// oldest first.
pub fn linear_open(label: Option<&str>, team: Option<&str>) -> Result<Vec<Issue>> {
    let d = linear(
        &linear_key()?,
        LINEAR_OPEN,
        json!({ "filter": linear_filter(label, team) }),
    )?;
    let mut v: Vec<(String, Issue)> = d["issues"]["nodes"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|i| {
            (
                s(&i["createdAt"]),
                Issue {
                    title: s(&i["title"]),
                    url: s(&i["url"]),
                    source: Source::Linear,
                    key: s(&i["identifier"]),
                    id: s(&i["id"]),
                    ..Default::default()
                },
            )
        })
        .filter(|(_, i)| !i.key.is_empty())
        .collect();
    v.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(v.into_iter().map(|x| x.1).collect())
}

fn linear_comment(t: &Tracked, body: &str) -> Result<()> {
    let key = linear_key()?;
    // Comments go to the issue's internal id; runs from before it was kept look it up.
    let id = if t.id.is_empty() {
        let d = linear(
            &key,
            "query($id: String!) { issue(id: $id) { id } }",
            json!({ "id": t.key }),
        )?;
        s(&d["issue"]["id"])
    } else {
        t.id.clone()
    };
    let d = linear(
        &key,
        "mutation($input: CommentCreateInput!) { commentCreate(input: $input) { success } }",
        json!({ "input": { "issueId": id, "body": body } }),
    )?;
    if d["commentCreate"]["success"].as_bool() != Some(true) {
        bail!("Linear didn't add the comment.");
    }
    Ok(())
}

// ---- Jira ----

fn jira_req(j: &Jira, method: &str, path: &str) -> ureq::Request {
    let auth = format!(
        "Basic {}",
        base64(format!("{}:{}", j.email, j.token).as_bytes())
    );
    agent()
        .request(method, &format!("{}{path}", j.url))
        .set("Authorization", &auth)
}

/// Who the credentials belong to: checks them before they're stored.
pub fn jira_myself(j: &Jira) -> Result<String> {
    let v = send("Jira", jira_req(j, "GET", "/rest/api/3/myself"), None)?;
    Ok(format!(
        "{} <{}>",
        s(&v["displayName"]),
        s(&v["emailAddress"])
    ))
}

/// A Jira field that is ADF (Cloud, v3) or plain text (older servers).
fn jira_text(v: &Value) -> String {
    match v {
        Value::String(t) => t.clone(),
        Value::Null => String::new(),
        v => adf_to_text(v),
    }
}

pub fn jira_issue(key: &str) -> Result<Issue> {
    let j = jira()?;
    let req = jira_req(&j, "GET", &format!("/rest/api/3/issue/{key}"))
        .query("fields", "summary,description");
    let v = send("Jira", req, None)?;
    let f = &v["fields"];
    let key = Some(s(&v["key"]))
        .filter(|k| !k.is_empty())
        .unwrap_or(key.into());
    // The issue's `comment` field is a page of the oldest ones; Get comments can start
    // from the newest. Its 50 newest, put back oldest first.
    let req = jira_req(&j, "GET", &format!("/rest/api/3/issue/{key}/comment"))
        .query("orderBy", "-created")
        .query("maxResults", "50");
    let c = send("Jira", req, None)?;
    let mut comments: Vec<Comment> = c["comments"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|c| Comment {
            author: s(&c["author"]["displayName"]),
            body: jira_text(&c["body"]),
        })
        .collect();
    comments.reverse();
    Ok(Issue {
        title: s(&f["summary"]),
        body: jira_text(&f["description"]),
        url: format!("{}/browse/{key}", j.url),
        source: Source::Jira,
        key,
        comments,
        ..Default::default()
    })
}

/// A JQL string literal.
fn jql_quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// The query `intake --from jira` runs: `--jql` as given, or open issues with the label
/// in the project, oldest first.
pub fn jira_query(jql: Option<&str>, label: Option<&str>, project: Option<&str>) -> String {
    if let Some(q) = jql {
        return q.to_string();
    }
    let mut parts = vec![];
    if let Some(p) = project {
        parts.push(format!("project = {}", jql_quote(p)));
    }
    if let Some(l) = label {
        parts.push(format!("labels = {}", jql_quote(l)));
    }
    parts.push("statusCategory != Done".into());
    format!("{} ORDER BY created ASC", parts.join(" AND "))
}

/// The issues a JQL query finds, in its order (up to 100).
pub fn jira_open(jql: &str) -> Result<Vec<Issue>> {
    let j = jira()?;
    let req = jira_req(&j, "GET", "/rest/api/3/search/jql")
        .query("jql", jql)
        .query("fields", "summary")
        .query("maxResults", "100");
    let v = send("Jira", req, None)?;
    Ok(v["issues"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|i| {
            let key = s(&i["key"]);
            (!key.is_empty()).then(|| Issue {
                title: s(&i["fields"]["summary"]),
                url: format!("{}/browse/{key}", j.url),
                source: Source::Jira,
                key,
                ..Default::default()
            })
        })
        .collect())
}

fn jira_comment(t: &Tracked, text: &str, link: &str) -> Result<()> {
    let j = jira()?;
    let body = json!({ "body": { "type": "doc", "version": 1, "content": [{
        "type": "paragraph",
        "content": [
            { "type": "text", "text": text },
            { "type": "text", "text": link, "marks": [{ "type": "link", "attrs": { "href": link } }] },
        ],
    }]}});
    let req = jira_req(&j, "POST", &format!("/rest/api/3/issue/{}/comment", t.key));
    send("Jira", req, Some(&body))?;
    Ok(())
}

// ---- Back on the issue ----

/// Whether amux comments on the issue when its PR opens: `"trackers": { "comment": false }`
/// in config.json turns it off.
pub fn comment_enabled() -> bool {
    crate::store::load_config()
        .ok()
        .and_then(|c| c.extra.get("trackers")?.get("comment")?.as_bool())
        .unwrap_or(true)
}

/// Comments on the run's Linear or Jira issue with its pull request's link.
pub fn comment_pr(t: &Tracked, pr_url: &str) -> Result<()> {
    let text = "amux opened a pull request for this issue: ";
    match t.source() {
        Some(Source::Linear) => linear_comment(t, &format!("{text}{pr_url}")),
        Some(Source::Jira) => jira_comment(t, text, pr_url),
        _ => bail!("unknown tracker \"{}\"", t.tracker),
    }
}

// ---- Atlassian Document Format ----

/// Jira's rich text (ADF) as plain text: paragraphs, headings, lists, code blocks,
/// quotes, tables, links and mentions. Anything else keeps its text.
pub fn adf_to_text(doc: &Value) -> String {
    let out = blocks(
        doc["content"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default(),
    );
    out.trim().to_string()
}

fn children(n: &Value) -> &[Value] {
    n["content"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
}

fn blocks(nodes: &[Value]) -> String {
    nodes
        .iter()
        .map(block)
        .filter(|b| !b.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Indents every line after the first by `n` spaces.
fn hang(text: &str, n: usize) -> String {
    text.replace('\n', &format!("\n{}", " ".repeat(n)))
}

fn block(n: &Value) -> String {
    match n["type"].as_str().unwrap_or_default() {
        "paragraph" => inline(children(n)),
        "heading" => {
            let level = n["attrs"]["level"].as_u64().unwrap_or(1).clamp(1, 6) as usize;
            format!("{} {}", "#".repeat(level), inline(children(n)))
        }
        "bulletList" | "orderedList" => {
            let ordered = n["type"] == "orderedList";
            let mut i = n["attrs"]["order"].as_u64().unwrap_or(1);
            children(n)
                .iter()
                .map(|item| {
                    let mark = if ordered {
                        i += 1;
                        format!("{}. ", i - 1)
                    } else {
                        "- ".into()
                    };
                    // Items of a list are tight: one line each, sublists under them.
                    let body = children(item)
                        .iter()
                        .map(block)
                        .filter(|b| !b.trim().is_empty())
                        .collect::<Vec<_>>()
                        .join("\n");
                    format!("{mark}{}", hang(&body, mark.len()))
                })
                .collect::<Vec<_>>()
                .join("\n")
        }
        "codeBlock" => {
            let lang = n["attrs"]["language"].as_str().unwrap_or_default();
            let code: String = children(n).iter().map(|t| s(&t["text"])).collect();
            format!("```{lang}\n{}\n```", code.trim_end_matches('\n'))
        }
        "blockquote" => blocks(children(n))
            .lines()
            .map(|l| format!("> {l}").trim_end().to_string())
            .collect::<Vec<_>>()
            .join("\n"),
        "rule" => "---".into(),
        "table" => children(n)
            .iter()
            .map(|row| {
                children(row)
                    .iter()
                    .map(|cell| blocks(children(cell)).replace('\n', " "))
                    .collect::<Vec<_>>()
                    .join(" | ")
            })
            .collect::<Vec<_>>()
            .join("\n"),
        "mediaSingle" | "mediaGroup" | "media" => "[attachment]".into(),
        "expand" | "nestedExpand" => {
            let title = n["attrs"]["title"].as_str().unwrap_or_default();
            let body = blocks(children(n));
            if title.is_empty() {
                body
            } else {
                format!("{title}\n{body}")
            }
        }
        _ => {
            let kids = children(n);
            // Unknown blocks (panels, layouts…) hold blocks; unknown inline nodes hold text.
            if kids.iter().any(is_block) {
                blocks(kids)
            } else {
                inline(std::slice::from_ref(n))
            }
        }
    }
}

fn is_block(n: &Value) -> bool {
    !matches!(
        n["type"].as_str().unwrap_or_default(),
        "text" | "hardBreak" | "mention" | "emoji" | "inlineCard" | "status" | "date"
    )
}

fn inline(nodes: &[Value]) -> String {
    let mut out = String::new();
    for n in nodes {
        let a = &n["attrs"];
        match n["type"].as_str().unwrap_or_default() {
            "text" => {
                let mut t = s(&n["text"]);
                let marks = n["marks"].as_array().map(Vec::as_slice).unwrap_or_default();
                for m in marks {
                    match m["type"].as_str().unwrap_or_default() {
                        "code" => t = format!("`{t}`"),
                        "link" => {
                            let href = s(&m["attrs"]["href"]);
                            if !href.is_empty() && href != t {
                                t = format!("{t} ({href})");
                            }
                        }
                        _ => {}
                    }
                }
                out.push_str(&t);
            }
            "hardBreak" => out.push('\n'),
            "mention" | "status" => out.push_str(&s(&a["text"])),
            "emoji" => {
                let t = s(&a["text"]);
                out.push_str(if t.is_empty() {
                    a["shortName"].as_str().unwrap_or_default()
                } else {
                    &t
                });
            }
            "inlineCard" | "blockCard" | "embedCard" => out.push_str(&s(&a["url"])),
            "date" => out.push_str(&s(&a["timestamp"])),
            _ => out.push_str(&inline(children(n))),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_basic_auth() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"me@x.com:tok"), "bWVAeC5jb206dG9r");
    }

    #[test]
    fn turns_adf_into_text() {
        let doc: Value = serde_json::from_str(
            r#"{"type":"doc","version":1,"content":[
              {"type":"heading","attrs":{"level":2},"content":[{"type":"text","text":"Steps"}]},
              {"type":"paragraph","content":[
                {"type":"text","text":"Run "},{"type":"text","text":"amux ps","marks":[{"type":"code"}]},
                {"type":"text","text":" as "},{"type":"mention","attrs":{"id":"1","text":"@Ann"}},
                {"type":"hardBreak"},{"type":"text","text":"see "},
                {"type":"text","text":"the docs","marks":[{"type":"link","attrs":{"href":"https://x.dev/d"}}]}]},
              {"type":"orderedList","attrs":{"order":3},"content":[
                {"type":"listItem","content":[{"type":"paragraph","content":[{"type":"text","text":"one"}]},
                  {"type":"bulletList","content":[{"type":"listItem","content":[{"type":"paragraph","content":[{"type":"text","text":"nested"}]}]}]}]},
                {"type":"listItem","content":[{"type":"paragraph","content":[{"type":"text","text":"two"}]}]}]},
              {"type":"codeBlock","attrs":{"language":"sh"},"content":[{"type":"text","text":"cargo test\nexit 1\n"}]},
              {"type":"blockquote","content":[{"type":"paragraph","content":[{"type":"text","text":"quoted"}]}]},
              {"type":"panel","attrs":{"panelType":"info"},"content":[{"type":"paragraph","content":[{"type":"text","text":"in a panel "},{"type":"emoji","attrs":{"shortName":":ok:"}}]}]},
              {"type":"rule"},
              {"type":"table","content":[{"type":"tableRow","content":[
                {"type":"tableHeader","content":[{"type":"paragraph","content":[{"type":"text","text":"a"}]}]},
                {"type":"tableCell","content":[{"type":"paragraph","content":[{"type":"text","text":"b"}]}]}]}]},
              {"type":"mediaSingle","content":[{"type":"media","attrs":{"id":"x"}}]},
              {"type":"paragraph","content":[]}
            ]}"#,
        )
        .unwrap();
        assert_eq!(
            adf_to_text(&doc),
            "## Steps\n\n\
             Run `amux ps` as @Ann\nsee the docs (https://x.dev/d)\n\n\
             3. one\n   - nested\n4. two\n\n\
             ```sh\ncargo test\nexit 1\n```\n\n\
             > quoted\n\n\
             in a panel :ok:\n\n\
             ---\n\n\
             a | b\n\n\
             [attachment]"
        );
        assert_eq!(adf_to_text(&Value::Null), "");
        assert_eq!(jira_text(&json!("plain")), "plain");
    }

    #[test]
    fn builds_queries_and_pr_bodies() {
        assert_eq!(
            jira_query(None, Some("agent \"ready\""), Some("PROJ")),
            "project = \"PROJ\" AND labels = \"agent \\\"ready\\\"\" AND statusCategory != Done ORDER BY created ASC"
        );
        assert_eq!(
            jira_query(Some("assignee = currentUser()"), Some("x"), None),
            "assignee = currentUser()"
        );
        let f = linear_filter(Some("agent"), Some("ENG"));
        assert_eq!(f["labels"]["name"]["eqIgnoreCase"], "agent");
        assert_eq!(f["team"]["key"]["eqIgnoreCase"], "ENG");
        assert_eq!(f["state"]["type"]["nin"][1], "canceled");
        let t = Tracked {
            tracker: "linear".into(),
            key: "ENG-1".into(),
            url: "https://linear.app/a/issue/ENG-1".into(),
            id: String::new(),
        };
        assert_eq!(
            t.pr_body(),
            "Fixes ENG-1\n\nStarted by amux from Linear issue ENG-1: https://linear.app/a/issue/ENG-1."
        );
        let t = Tracked {
            tracker: "jira".into(),
            key: "P-2".into(),
            ..t
        };
        assert!(t
            .pr_body()
            .starts_with("P-2\n\nStarted by amux from Jira issue P-2: "));
        assert_eq!(
            site_url("acme.atlassian.net/").unwrap(),
            "https://acme.atlassian.net"
        );
        assert!(site_url("ftp://x").is_err());
        assert_eq!(
            error_message(&json!({"errorMessages": ["Issue does not exist"]})).as_deref(),
            Some("Issue does not exist")
        );
        assert_eq!(
            error_message(&json!({"errors": [{"message": "bad key"}]})).as_deref(),
            Some("bad key")
        );
    }
}
