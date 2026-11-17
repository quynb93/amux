//! `amux revive --at-login`: a login item that runs `amux revive --all` after each restart,
//! so runs that were going when the machine went down come back by themselves. A
//! LaunchAgent on macOS, a systemd user unit on Linux. It carries this shell's PATH, since
//! login items start with a bare one and the agent CLIs live in places like ~/.local/bin.

use crate::paths;
use anyhow::{Context, Result};
use std::path::PathBuf;
use std::process::{Command, Stdio};

const LABEL: &str = "dev.amux.revive";

fn exe() -> Result<String> {
    Ok(std::env::current_exe()
        .context("can't tell where amux is")?
        .to_string_lossy()
        .into_owned())
}

/// What the login item runs: wait for the network a little, then revive everything lost.
fn args(exe: &str) -> Vec<String> {
    ["revive", "--all", "--quiet", "--after", "30s"]
        .iter()
        .fold(vec![exe.to_string()], |mut v, a| {
            v.push(a.to_string());
            v
        })
}

fn env_vars() -> Vec<(String, String)> {
    let mut v = vec![(
        "PATH".to_string(),
        std::env::var("PATH").unwrap_or_default(),
    )];
    if let Ok(h) = std::env::var("AMUX_HOME") {
        v.push(("AMUX_HOME".into(), h));
    }
    v
}

fn xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn plist_path() -> PathBuf {
    paths::HOME.join(format!("Library/LaunchAgents/{LABEL}.plist"))
}

fn unit_path() -> PathBuf {
    paths::HOME.join(".config/systemd/user/amux-revive.service")
}

pub fn plist(exe: &str) -> String {
    let log = xml(&paths::AMUX_HOME.join("revive.log").to_string_lossy());
    let prog: String = args(exe)
        .iter()
        .map(|a| format!("\n    <string>{}</string>", xml(a)))
        .collect();
    let env: String = env_vars()
        .iter()
        .map(|(k, v)| format!("\n    <key>{}</key><string>{}</string>", xml(k), xml(v)))
        .collect();
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{LABEL}</string>
  <key>ProgramArguments</key>
  <array>{prog}
  </array>
  <key>RunAtLoad</key><true/>
  <key>EnvironmentVariables</key>
  <dict>{env}
  </dict>
  <key>StandardOutPath</key><string>{log}</string>
  <key>StandardErrorPath</key><string>{log}</string>
</dict>
</plist>
"#
    )
}

/// A systemd value with spaces or quotes, quoted.
fn sd_quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

pub fn unit(exe: &str) -> String {
    let env: String = env_vars()
        .iter()
        .map(|(k, v)| format!("Environment={}\n", sd_quote(&format!("{k}={v}"))))
        .collect();
    let exec: Vec<String> = args(exe).iter().map(|a| sd_quote(a)).collect();
    format!(
        "[Unit]\nDescription=Resume amux runs lost in a restart\nAfter=network-online.target\n\n\
         [Service]\nType=oneshot\n{env}ExecStart={}\n\n[Install]\nWantedBy=default.target\n",
        exec.join(" ")
    )
}

pub fn install() -> Result<i32> {
    let exe = exe()?;
    if cfg!(target_os = "macos") {
        let p = plist_path();
        std::fs::create_dir_all(p.parent().unwrap_or(&p))?;
        std::fs::write(&p, plist(&exe))?;
        println!(
            "Installed {}. After each restart, once you log in, amux revives the runs that were going (log: {}).",
            paths::tildify(&p),
            paths::tildify(&paths::AMUX_HOME.join("revive.log"))
        );
    } else {
        let p = unit_path();
        std::fs::create_dir_all(p.parent().unwrap_or(&p))?;
        std::fs::write(&p, unit(&exe))?;
        let enabled = Command::new("systemctl")
            .args(["--user", "enable", "amux-revive.service"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        println!("Installed {}.", paths::tildify(&p));
        if !enabled {
            println!("Enable it with: systemctl --user enable amux-revive.service");
        }
    }
    println!("It carries this shell's PATH, so the CLIs are found; run --at-login again after changing PATH. `amux revive --no-at-login` removes it.");
    Ok(0)
}

pub fn uninstall() -> Result<i32> {
    let p = if cfg!(target_os = "macos") {
        plist_path()
    } else {
        let _ = Command::new("systemctl")
            .args(["--user", "disable", "amux-revive.service"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        unit_path()
    };
    match std::fs::remove_file(&p) {
        Ok(()) => println!("Removed {}.", paths::tildify(&p)),
        Err(_) => println!("No login item was installed."),
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_items_run_revive_and_escape_their_values() {
        let p = plist("/Apps/a&b/amux");
        assert!(p.contains("<string>/Apps/a&amp;b/amux</string>"));
        assert!(p.contains("<string>revive</string>\n    <string>--all</string>"));
        assert!(p.contains("<key>RunAtLoad</key><true/>"));
        let u = unit("/opt/my amux/amux");
        assert!(u.contains("ExecStart=\"/opt/my amux/amux\" \"revive\" \"--all\""));
        assert!(u.contains("Environment=\"PATH="));
    }
}
