//! `amux phone`: where notifications go when you're away from the computer.

use crate::runs::push::{self, Phone, EVENTS};
use crate::runs::webpush;
use crate::{paths, util};
use anyhow::{bail, Result};

const USAGE: &str = "amux phone [webpush | ntfy [<topic>|<url>] [--token <t>] | pushover <user-key> [--token <app-token>] | webhook <url>
            | open <url>|off | events <event,...>|all | brief on|off | test | devices [rm <n>|all] | off]";

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
        Some("webpush") => {
            webpush::public_key()?;
            push::save(&keep(Phone {
                service: "webpush".into(),
                ..Default::default()
            }))?;
            println!("Notifications go straight to the phone app (Web Push), with no other app.");
            println!("  1. Serve the app over HTTPS: `amux serve --tailscale`, or `--tls` with a");
            println!("     certificate the phone trusts. Push needs HTTPS.");
            println!(
                "  2. Open it on the phone. iPhone and iPad: Share → Add to Home Screen first"
            );
            println!("     (iOS 16.4 or later), then open amux from the home screen.");
            println!("  3. Monitor → Notifications → Turn on, then run `amux phone test`.");
            println!("{}", devices_line());
            println!(
                "Messages are encrypted for the phone: its push service (Apple, Google or Mozilla)"
            );
            println!("sees when one is sent, not what it says.");
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
        Some("devices") => devices(&positional),
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

/// `amux phone devices [rm <n>|all]`: the browsers that turned on Web Push, or forget one
/// (it can turn notifications on again in the app) or all of them.
fn devices(positional: &[&String]) -> Result<i32> {
    let subs = webpush::load_subs();
    match positional.first().map(|s| s.as_str()) {
        None => {
            if subs.is_empty() {
                println!("No device has turned notifications on.");
            }
            let now = crate::time::now_ms();
            for (i, s) in subs.iter().enumerate() {
                let host = url::Url::parse(&s.endpoint)
                    .ok()
                    .and_then(|u| u.host_str().map(str::to_string))
                    .unwrap_or_default();
                let name = if s.device.is_empty() {
                    "browser"
                } else {
                    &s.device
                };
                let added = if s.added > 0 {
                    format!(", added {}", crate::util::ago(s.added, now))
                } else {
                    String::new()
                };
                println!(
                    "{:>2}. {}  ({}{added})",
                    i + 1,
                    crate::util::clean(name, false),
                    crate::util::clean(&host, false)
                );
            }
            Ok(0)
        }
        Some("rm") => match positional.get(1).map(|s| s.as_str()) {
            Some("all") => {
                let n = webpush::remove_all()?;
                println!("Forgot {n} device{}.", if n == 1 { "" } else { "s" });
                Ok(0)
            }
            Some(n) => {
                let i: usize = n
                    .parse()
                    .ok()
                    .filter(|i| *i >= 1 && *i <= subs.len())
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "No device {n}: `amux phone devices` lists them (1 to {}).",
                            subs.len()
                        )
                    })?;
                webpush::remove(&subs[i - 1].endpoint)?;
                println!("Forgot device {i}. It can turn notifications on again in the app.");
                Ok(0)
            }
            None => bail!("Usage: amux phone devices rm <n>|all"),
        },
        Some(_) => bail!("Usage: amux phone devices [rm <n>|all]"),
    }
}

fn need(p: Option<Phone>) -> Result<Phone> {
    p.ok_or_else(|| {
        anyhow::anyhow!("Set up a phone first: `amux phone webpush` (or ntfy, pushover, webhook).")
    })
}

fn devices_line() -> String {
    let subs = webpush::load_subs();
    if subs.is_empty() {
        return "No device has turned notifications on yet.".into();
    }
    let names: Vec<&str> = subs
        .iter()
        .map(|s| {
            if s.device.is_empty() {
                "browser"
            } else {
                s.device.as_str()
            }
        })
        .collect();
    format!("Devices: {} ({})", subs.len(), names.join(", "))
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
        println!(
            "No phone set up. `amux phone webpush` sends to the phone app itself (it needs HTTPS);"
        );
        println!("`amux phone ntfy` works anywhere (free, no account).");
        println!("Usage: {USAGE}");
        return Ok(0);
    };
    let to = match p.service.as_str() {
        "pushover" => "Pushover".to_string(),
        "webpush" => format!("the phone app (Web Push). {}", devices_line()),
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
