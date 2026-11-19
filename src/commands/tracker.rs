//! `amux tracker`: sign in to Linear and Jira, so their issues can be runs
//! (`amux spawn --issue linear:ENG-123`, `amux intake --from jira`).

use crate::util::read_secret;
use crate::work::issues::Source;
use crate::work::trackers::{self, Jira, JiraSite};
use anyhow::{anyhow, bail, Result};

const USAGE: &str = "amux tracker [status] | login linear | login jira <site-url> <email> | logout linear|jira   (login: --no-check skips trying the key)";

fn source(v: Option<&String>) -> Result<Source> {
    v.and_then(|s| Source::from_tag(s))
        .filter(|s| *s != Source::GitHub)
        .ok_or_else(|| anyhow!("linear or jira? Usage: {USAGE}"))
}

pub fn tracker_cmd(argv: &[String]) -> Result<i32> {
    let check = !argv.iter().any(|a| a == "--no-check");
    let args: Vec<&String> = argv.iter().filter(|a| *a != "--no-check").collect();
    if let Some(f) = args.iter().find(|a| a.starts_with('-')) {
        bail!("Unknown option {f}. Usage: {USAGE}");
    }
    match args.first().map(|s| s.as_str()) {
        None | Some("status") => status(),
        Some("login") => match source(args.get(1).copied())? {
            Source::Linear => {
                if args.len() > 2 {
                    bail!("Usage: {USAGE}");
                }
                let key = read_secret(
                    "Paste a Linear personal API key (Linear: Settings → Security & access): ",
                )?;
                if key.is_empty() {
                    bail!("No key given; nothing stored.");
                }
                if check {
                    let who = trackers::linear_viewer(&key)?;
                    println!("Linear key works: {who}");
                }
                trackers::store_key(Source::Linear, &key)?;
                println!("Stored the Linear key. Try `amux spawn --issue linear:<KEY> <cli>`.");
                Ok(0)
            }
            _ => {
                let (Some(site), Some(email), None) = (args.get(2), args.get(3), args.get(4))
                else {
                    bail!("Usage: amux tracker login jira <site-url> <email>, e.g. amux tracker login jira acme.atlassian.net me@acme.com");
                };
                let url = trackers::site_url(site)?;
                if !email.contains('@') {
                    bail!("\"{email}\" is not an email address.");
                }
                let token = read_secret(
                    "Paste a Jira API token (id.atlassian.com → Security → API tokens): ",
                )?;
                if token.is_empty() {
                    bail!("No token given; nothing stored.");
                }
                if check {
                    let j = Jira {
                        url: url.clone(),
                        email: email.to_string(),
                        token: token.clone(),
                    };
                    println!("Jira token works: {}", trackers::jira_myself(&j)?);
                }
                trackers::store_key(Source::Jira, &token)?;
                let mut saved = trackers::load_saved();
                saved.jira = Some(JiraSite {
                    url: url.clone(),
                    email: email.to_string(),
                });
                trackers::save_saved(&saved)?;
                println!("Stored the Jira token for {email} on {url}. Try `amux spawn --issue jira:<KEY> <cli>`.");
                Ok(0)
            }
        },
        Some("logout") => {
            let s = source(args.get(1).copied())?;
            trackers::forget_key(s);
            if s == Source::Jira {
                let mut saved = trackers::load_saved();
                if saved.jira.take().is_some() {
                    trackers::save_saved(&saved)?;
                }
            }
            println!("Forgot the stored {} key.", s.name());
            Ok(0)
        }
        Some(other) => bail!("Unknown subcommand \"{other}\". Usage: {USAGE}"),
    }
}

/// Where each tracker's key comes from. Asks no service.
fn status() -> Result<i32> {
    let linear = if trackers::env("LINEAR_API_KEY").is_some() {
        "LINEAR_API_KEY from the environment".to_string()
    } else if trackers::stored_key(Source::Linear).is_some() {
        "a stored key".to_string()
    } else {
        "not set up (amux tracker login linear)".to_string()
    };
    println!("Linear  {linear}");
    let jira = match trackers::jira() {
        Ok(j) => {
            let from = if trackers::env("JIRA_API_TOKEN").is_some() {
                "JIRA_API_TOKEN"
            } else {
                "a stored token"
            };
            format!("{} as {}, with {from}", j.url, j.email)
        }
        Err(_) => "not set up (amux tracker login jira <site-url> <email>)".to_string(),
    };
    println!("Jira    {jira}");
    let comment = if trackers::comment_enabled() {
        "on"
    } else {
        "off (\"trackers\": { \"comment\": false } in config.json)"
    };
    println!("Comment the PR link on the issue: {comment}");
    Ok(0)
}
