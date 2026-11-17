//! `amux phone`: where notifications go when you're away from the computer.

use crate::runs::push::{self, Phone, EVENTS};
use crate::{paths, util};
use anyhow::{bail, Result};

const USAGE: &str = "amux phone [ntfy [<topic>|<url>] [--token <t>] | pushover <user-key> [--token <app-token>] | webhook <url>
            | open <url>|off | events <event,...>|all | brief on|off | test | off]";

pub fn phone_cmd(argv: &[String]) -> Result<i32> {
    let sub = argv.first().map(String::as_str);
    let rest = argv.get(1..).unwrap_or_default();
    let flag = |name: &str| {
        rest.iter()
            .position(|a| a == name)
            .and_then(|i| rest.get(i + 1))
            .cloned()
    };
    let positional: Vec<&String> = {
        let mut v = vec![];
        let mut it = rest.iter();
        while let Some(a) = it.next() {
            if a.starts_with("--") {
                it.next();
            } else {
                v.push(a);
            }
        }
        v
    };
    let current = push::load();
    let keep = |mut p: Phone| -> Phone {
        if let Some(c) = &current {
            p.open = c.open.clone();
            p.events = c.events.clone();
            p.brief = c.brief;
        }
        p
    };
    match sub {
        None | Some("status") => status(current.as_ref()),
        Some("ntfy") => {
            let url = match positional.first() {
                Some(u) if u.contains("://") => u.trim_end_matches('/').to_string(),
                Some(topic) => format!("https://ntfy.sh/{topic}"),
                None => format!("https://ntfy.sh/{}", push::new_topic()?),
            };
            let p = keep(Phone {
                service: "ntfy".into(),
                url: url.clone(),
                user: flag("--token").unwrap_or_default(),
                ..Default::default()
            });
            push::save(&p)?;
            let (server, topic) = url.rsplit_once('/').unwrap_or((&url, ""));
            println!("Notifications go to ntfy. On your phone:");
            println!("  1. Install the ntfy app (App Store / Play Store / F-Droid).");
            println!("  2. Subscribe to the topic {topic}");
            if server != "https://ntfy.sh" {
                println!("     on the server {server}");
            }
            println!("     (or open {url} in the phone's browser).");
            println!("  3. Run `amux phone test`.");
            println!("Anyone who knows the topic can read it: keep it private, or self-host ntfy.");
            println!("`amux phone brief on` leaves out what the agents say.");
            Ok(0)
        }
        Some("pushover") => {
            let Some(user) = positional.first() else {
                bail!("Usage: amux phone pushover <user-key> [--token <app-token>]");
            };
            let token = match flag("--token") {
                Some(t) => t,
                None => util::read_secret("Pushover application token: ")?,
            };
            if token.is_empty() {
                bail!("Pushover needs an application token (pushover.net/apps/build).");
            }
            push::save(&keep(Phone {
                service: "pushover".into(),
                user: user.to_string(),
                token,
                ..Default::default()
            }))?;
            println!("Notifications go to Pushover. Try it with `amux phone test`.");
            Ok(0)
        }
        Some("webhook") => {
            let Some(url) = positional.first() else {
                bail!("Usage: amux phone webhook <url>");
            };
            if !url.starts_with("https://") && !url.starts_with("http://") {
                bail!("A webhook is an http(s) URL.");
            }
            push::save(&keep(Phone {
                service: "webhook".into(),
                url: url.to_string(),
                ..Default::default()
            }))?;
            println!("Notifications are POSTed to that URL as JSON (Slack and Discord webhooks work as is).");
            Ok(0)
        }
        Some("open") => {
            let mut p = need(current)?;
            p.open = match positional.first().map(|s| s.as_str()) {
                Some("off") => String::new(),
                Some(u) if u.starts_with("http://") || u.starts_with("https://") => {
                    u.trim_end_matches('/').to_string()
                }
                _ => bail!("Usage: amux phone open <url-of-amux-serve>|off"),
            };
            push::save(&p)?;
            println!(
                "Tapping a notification opens the run at {}.",
                or_none(&p.open)
            );
            Ok(0)
        }
        Some("events") => {
            let mut p = need(current)?;
            let Some(list) = positional.first() else {
                bail!("Usage: amux phone events <{}>|all", EVENTS.join(","));
            };
            p.events = if list.as_str() == "all" {
                vec![]
            } else {
                let v: Vec<String> = list.split(',').map(|s| s.trim().to_string()).collect();
                if let Some(bad) = v.iter().find(|e| !EVENTS.contains(&e.as_str())) {
                    bail!("Unknown event {bad}. Events: {}", EVENTS.join(", "));
                }
                v
            };
            push::save(&p)?;
            status(Some(&p))
        }
        Some("brief") => {
            let mut p = need(current)?;
            p.brief = match positional.first().map(|s| s.as_str()) {
                Some("on") => true,
                Some("off") => false,
                _ => bail!("Usage: amux phone brief on|off"),
            };
            push::save(&p)?;
            status(Some(&p))
        }
        Some("test") => {
            let p = need(current)?;
            push::deliver(
                &p,
                "waiting",
                "test",
                "amux: test",
                "Notifications from amux reach this phone.",
            )?;
            println!("Sent. If nothing arrived, check the app's subscription or settings.");
            Ok(0)
        }
        Some("off") => {
            match std::fs::remove_file(push::file()) {
                Ok(()) => println!("Phone notifications are off."),
                Err(_) => println!("Phone notifications weren't on."),
            }
            Ok(0)
        }
        Some(_) => bail!("Usage: {USAGE}"),
    }
}

fn need(p: Option<Phone>) -> Result<Phone> {
    p.ok_or_else(|| {
        anyhow::anyhow!("Set up a phone first: `amux phone ntfy` (or pushover, webhook).")
    })
}

fn or_none(s: &str) -> &str {
    if s.is_empty() {
        "(not set)"
    } else {
        s
    }
}

fn status(p: Option<&Phone>) -> Result<i32> {
    let Some(p) = p else {
        println!("No phone set up. `amux phone ntfy` is the quickest (free, no account).");
        println!("Usage: {USAGE}");
        return Ok(0);
    };
    let to = match p.service.as_str() {
        "pushover" => "Pushover".to_string(),
        _ => format!("{} {}", p.service, p.url),
    };
    println!("Sending to: {to}");
    let events = if p.events.is_empty() {
        EVENTS.join(", ")
    } else {
        p.events.join(", ")
    };
    println!("Events:     {events}");
    let served = crate::store::read_json::<serde_json::Value>(&paths::AMUX_HOME.join("serve.json"))
        .ok()
        .and_then(|v| v.get("url")?.as_str().map(str::to_string));
    let open = if !p.open.is_empty() {
        p.open.clone()
    } else if let Some(u) = served {
        format!("{u} (from the last `amux serve`)")
    } else {
        "(not set: `amux phone open <url>`)".into()
    };
    println!("Taps open:  {open}");
    println!("Brief:      {}", if p.brief { "on" } else { "off" });
    Ok(0)
}
