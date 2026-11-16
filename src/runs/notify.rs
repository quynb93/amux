//! Notifications from background runs. By default a desktop notification (macOS
//! Notification Center, or `notify-send` on Linux). In ~/.amux/config.json:
//!
//!   "notify": false                         turn them off
//!   "notifyCommand": ["my-notifier", "--x"] run this instead, with AMUX_RUN, AMUX_EVENT,
//!                                           AMUX_TITLE and AMUX_MESSAGE in its environment

use crate::store::load_config;
use serde_json::Value;
use std::process::{Command, Stdio};

#[derive(Clone, Default)]
pub struct Notifier {
    enabled: bool,
    command: Option<Vec<String>>,
}

impl Notifier {
    pub fn from_config() -> Self {
        let Ok(cfg) = load_config() else {
            return Notifier {
                enabled: true,
                command: None,
            };
        };
        let enabled = !matches!(cfg.extra.get("notify"), Some(Value::Bool(false)));
        let command = cfg
            .extra
            .get("notifyCommand")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect::<Vec<_>>()
            })
            .filter(|c| !c.is_empty());
        Notifier { enabled, command }
    }

    /// Sends a notification without waiting for it. `event` is "waiting" or "exited".
    pub fn send(&self, run: &str, event: &str, title: &str, message: &str) {
        if !self.enabled {
            return;
        }
        let mut cmd = match &self.command {
            Some(c) => {
                let mut cmd = Command::new(&c[0]);
                cmd.args(&c[1..]);
                cmd
            }
            None => desktop(title, message),
        };
        cmd.env("AMUX_RUN", run)
            .env("AMUX_EVENT", event)
            .env("AMUX_TITLE", title)
            .env("AMUX_MESSAGE", message)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Ok(mut child) = cmd.spawn() {
            std::thread::spawn(move || child.wait());
        }
    }
}

fn desktop(title: &str, message: &str) -> Command {
    if cfg!(target_os = "macos") {
        // Text goes in as arguments, never into the script, so it can't inject AppleScript.
        let mut c = Command::new("osascript");
        c.args([
            "-e",
            "on run argv",
            "-e",
            "display notification (item 2 of argv) with title (item 1 of argv)",
            "-e",
            "end run",
            title,
            message,
        ]);
        c
    } else {
        let mut c = Command::new("notify-send");
        c.args(["--app-name=amux", "--", title, message]);
        c
    }
}
