//! Notifications on your phone, for when you're away from the computer: a run needs you,
//! stalled, hit a usage limit, finished, or its tests or CI came back. Sent through
//! ntfy (ntfy.sh or your own server), Pushover, or any webhook (Slack and Discord take
//! the same one). Set up with `amux phone`; kept in ~/.amux/phone.json (0600).
//!
//! Each message is sent by a short-lived `amux __push` process, so a slow network never
//! holds up a run, and a command that exits right after an event still gets it out.
//! The same run and event are sent at most once a minute.

use crate::store::{read_json, write_json};
use crate::{paths, time};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::process::{Command, Stdio};

/// Every event a phone can be told about.
pub const EVENTS: [&str; 8] = [
    "waiting",
    "stalled",
    "limited",
    "exited",
    "verify",
    "checks",
    "schedule",
    "autopilot",
];

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Phone {
    /// "ntfy", "pushover" or "webhook".
    pub service: String,
    /// ntfy: the topic's URL (https://ntfy.sh/<topic>). webhook: where to POST.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub url: String,
    /// Pushover's user key, or an access token for a protected ntfy topic.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub user: String,
    /// Pushover's application token.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub token: String,
    /// Where `amux serve` is reached from the phone; a tap opens the run there.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub open: String,
    /// Which events to send; empty means all of them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub events: Vec<String>,
    /// Leave out what the agent said (titles and run names only), for a server you
    /// don't run yourself.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub brief: bool,
}

pub fn file() -> std::path::PathBuf {
    paths::AMUX_HOME.join("phone.json")
}

pub fn load() -> Option<Phone> {
    read_json::<Option<Phone>>(&file()).ok().flatten()
}

pub fn save(p: &Phone) -> Result<()> {
    crate::fsx::mkdir_private(&paths::AMUX_HOME)?;
    write_json(&file(), p)
}

impl Phone {
    pub fn wants(&self, event: &str) -> bool {
        self.events.is_empty() || self.events.iter().any(|e| e == event)
    }

    /// The link a tap opens: the run in the phone app.
    fn link(&self, run: &str) -> Option<String> {
        let base = if self.open.is_empty() {
            read_json::<serde_json::Value>(&paths::AMUX_HOME.join("serve.json"))
                .ok()?
                .get("url")?
                .as_str()?
                .to_string()
        } else {
            self.open.clone()
        };
        let base = base.trim_end_matches('/');
        Some(format!("{base}/#run={}", url_escape(run)))
    }
}

fn url_escape(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

/// Events worth a buzz. Tests and CI only once they're done, and only a change.
pub fn worth_sending(event: &str, message: &str) -> bool {
    match event {
        "verify" => ["passed", "failed", "error"]
            .iter()
            .any(|s| message.contains(&format!(" {s}"))),
        "checks" => message.starts_with("passing") || message.starts_with("failing"),
        _ => true,
    }
}

/// Sends a notification about `run` to the phone, if one is set up and wants `event`.
/// Returns at once; the sending happens in its own process.
pub fn send(event: &str, run: &str, title: &str, message: &str) {
    let Some(p) = load() else { return };
    if !p.wants(event) || !worth_sending(event, message) {
        return;
    }
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    use std::os::unix::process::CommandExt;
    let mut c = Command::new(exe);
    c.args(["__push", event, run, title, message])
        .env("AMUX_NO_UPDATE_CHECK", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // Its own session, so it outlives a command (or terminal) that ends right away.
    unsafe {
        c.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    if let Ok(mut child) = c.spawn() {
        std::thread::spawn(move || child.wait());
    }
}

/// `amux __push <event> <run> <title> <message>`.
pub fn main(args: &[String]) -> i32 {
    let [event, run, title, message] = args else {
        return 2;
    };
    let Some(p) = load() else { return 0 };
    if !first_in_a_minute(run, event) {
        return 0;
    }
    match deliver(&p, event, run, title, message) {
        Ok(()) => 0,
        Err(e) => {
            let line = format!("{} {run} {event}: {e:#}\n", time::now_ms());
            let _ = crate::fsx::append_private(&paths::AMUX_HOME.join("phone.log"), &line);
            1
        }
    }
}

/// Records that `run`/`event` is being sent; false if it already was in the last minute.
fn first_in_a_minute(run: &str, event: &str) -> bool {
    let f = paths::AMUX_HOME.join("phone-sent.json");
    let mut sent: std::collections::BTreeMap<String, i64> = read_json(&f).unwrap_or_default();
    let now = time::now_ms();
    sent.retain(|_, at| now - *at < 60_000);
    let key = format!("{run}/{event}");
    if sent.contains_key(&key) {
        return false;
    }
    sent.insert(key, now);
    let _ = write_json(&f, &sent);
    true
}

fn priority(event: &str, message: &str) -> u8 {
    match event {
        "waiting" | "stalled" | "limited" => 4,
        "verify" | "checks" if message.contains("fail") || message.contains("error") => 4,
        _ => 3,
    }
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(15))
        .build()
}

/// Sends one notification and waits for the answer.
pub fn deliver(p: &Phone, event: &str, run: &str, title: &str, message: &str) -> Result<()> {
    let message = if p.brief {
        format!("{run}: {event}")
    } else {
        message.to_string()
    };
    let link = p.link(run);
    let res = match p.service.as_str() {
        "ntfy" => {
            let (server, topic) = p
                .url
                .trim_end_matches('/')
                .rsplit_once('/')
                .context("the ntfy URL should look like https://ntfy.sh/<topic>")?;
            let mut body = serde_json::json!({
                "topic": topic,
                "title": title,
                "message": message,
                "priority": priority(event, &message),
                "tags": [match event {
                    "waiting" => "raising_hand",
                    "stalled" => "hourglass",
                    "limited" => "no_entry",
                    "exited" => "checkered_flag",
                    _ if message.contains("fail") || message.contains("error") => "x",
                    _ => "white_check_mark",
                }],
            });
            if let Some(l) = &link {
                body["click"] = l.clone().into();
            }
            let mut req = agent().post(&format!("{server}/"));
            if !p.user.is_empty() {
                req = req.set("Authorization", &format!("Bearer {}", p.user));
            }
            req.set("Content-Type", "application/json")
                .send_string(&body.to_string())
        }
        "pushover" => {
            let base = std::env::var("AMUX_PUSHOVER_URL")
                .unwrap_or_else(|_| "https://api.pushover.net/1/messages.json".into());
            let prio = if priority(event, &message) > 3 {
                "1"
            } else {
                "0"
            };
            let mut form = vec![
                ("token", p.token.as_str()),
                ("user", p.user.as_str()),
                ("title", title),
                ("message", message.as_str()),
                ("priority", prio),
            ];
            if let Some(l) = &link {
                form.push(("url", l));
                form.push(("url_title", "Open in amux"));
            }
            agent().post(&base).send_form(&form)
        }
        "webhook" => {
            let line = match &link {
                Some(l) => format!("{title}: {message} ({l})"),
                None => format!("{title}: {message}"),
            };
            let body = serde_json::json!({
                "run": run,
                "event": event,
                "title": title,
                "message": message,
                "url": link,
                // What Slack and Discord webhooks show.
                "text": line,
                "content": line,
            });
            agent()
                .post(&p.url)
                .set("Content-Type", "application/json")
                .send_string(&body.to_string())
        }
        other => bail!("unknown phone service \"{other}\""),
    };
    match res {
        Ok(_) => Ok(()),
        Err(ureq::Error::Status(code, r)) => {
            let body = r.into_string().unwrap_or_default();
            bail!("{code} {}", crate::util::one_line(&body, 200))
        }
        Err(e) => Err(e.into()),
    }
}

/// A topic name nobody else will guess: anyone who knows it can read the messages.
pub fn new_topic() -> Result<String> {
    use std::io::Read;
    let mut bytes = [0u8; 15];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .context("could not read /dev/urandom")?;
    const ABC: &[u8] = b"abcdefghijkmnpqrstuvwxyz23456789";
    Ok(format!(
        "amux-{}",
        bytes
            .iter()
            .map(|b| ABC[(*b as usize) % ABC.len()] as char)
            .collect::<String>()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sends_only_finished_results_and_links_to_the_run() {
        assert!(worth_sending("waiting", "anything"));
        assert!(worth_sending("verify", "test failed: cargo test"));
        assert!(worth_sending("verify", "push passed: x"));
        assert!(!worth_sending("verify", "test running: cargo test"));
        assert!(worth_sending("checks", "failing https://x/pr/1"));
        assert!(!worth_sending("checks", "pending https://x/pr/1"));
        let p = Phone {
            open: "http://100.1.2.3:7878/".into(),
            events: vec!["waiting".into()],
            ..Default::default()
        };
        assert_eq!(
            p.link("box:w").unwrap(),
            "http://100.1.2.3:7878/#run=box%3Aw"
        );
        assert!(p.wants("waiting") && !p.wants("exited"));
        assert!(Phone::default().wants("exited"));
        let t = new_topic().unwrap();
        assert!(t.starts_with("amux-") && t.len() == 20 && t != new_topic().unwrap());
        assert_eq!(priority("checks", "failing x"), 4);
        assert_eq!(priority("exited", "Exited with code 0"), 3);
    }
}
