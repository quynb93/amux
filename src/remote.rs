//! Other machines, over SSH. A machine is a name for an SSH destination where amux is
//! installed too. Runs there are named `<machine>:<run>`, and every amux command that
//! takes a run works on them: amux runs the same command on the machine and passes its
//! terminal, output and exit code through.
//!
//! One SSH connection per machine is kept open for ten minutes and reused (OpenSSH's
//! ControlMaster), so the UI can look at remote runs every couple of seconds cheaply.
//! amux keeps no keys or passwords: SSH uses your own config, agent and known hosts.

use crate::store::{read_json, write_json};
use crate::{paths, util::need};
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Command, Stdio};

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Machine {
    pub name: String,
    /// What `ssh` connects to: `host`, `user@host`, or a Host from ~/.ssh/config.
    pub target: String,
    /// amux on the machine, when it isn't `amux` on its PATH.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub amux: Option<String>,
}

#[derive(Serialize, Deserialize, Default)]
struct MachinesFile {
    #[serde(default)]
    machines: Vec<Machine>,
}

fn machines_file() -> PathBuf {
    paths::AMUX_HOME.join("machines.json")
}

pub fn list() -> Vec<Machine> {
    read_json::<MachinesFile>(&machines_file())
        .map(|f| f.machines)
        .unwrap_or_default()
}

fn save(all: Vec<Machine>) -> Result<()> {
    write_json(&machines_file(), &MachinesFile { machines: all })
}

pub fn valid_name(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= 32
        && n.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// An SSH destination that can't be read as an option or carry anything else.
fn valid_target(t: &str) -> bool {
    !t.is_empty()
        && !t.starts_with('-')
        && t.chars()
            .all(|c| c.is_ascii_alphanumeric() || "@._-:[]".contains(c))
}

pub fn find(name: &str) -> Option<Machine> {
    list().into_iter().find(|m| m.name == name)
}

/// `gpu:claude-1` → the machine and the run's name there, if `gpu` is a machine.
pub fn split(r: &str) -> Option<(Machine, String)> {
    let (m, run) = r.split_once(':')?;
    if run.is_empty() {
        return None;
    }
    Some((find(m)?, run.to_string()))
}

pub fn sh_quote(s: &str) -> String {
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "@%+=:,./_-".contains(c))
    {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', r"'\''"))
}

impl Machine {
    /// `ssh` with the shared connection. `tty`: give the remote command a terminal.
    pub fn ssh(&self, tty: bool) -> Command {
        let bin = std::env::var_os("AMUX_SSH")
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| "ssh".into());
        let mut c = Command::new(bin);
        if let Some(dir) = control_dir() {
            c.arg("-o")
                .arg("ControlMaster=auto")
                .arg("-o")
                .arg(format!("ControlPath={}/amux-%C", dir.display()))
                .arg("-o")
                .arg("ControlPersist=10m");
        }
        c.arg(if tty { "-t" } else { "-T" });
        if !tty {
            // Never stop to ask for a password in the background; fail instead.
            c.arg("-o").arg("BatchMode=yes");
        }
        c.arg("--").arg(&self.target);
        c
    }

    /// The remote shell command that runs amux with `args`, optionally in `dir`.
    pub fn command_line(&self, args: &[String], dir: Option<&str>) -> String {
        let amux = self.amux.as_deref().unwrap_or("amux");
        let mut s = String::new();
        if let Some(d) = dir {
            // `~/x` must stay unquoted to expand; the rest is quoted.
            match d.strip_prefix("~/") {
                Some(rest) => s.push_str(&format!("cd \"$HOME\"/{} && ", sh_quote(rest))),
                None if d == "~" => s.push_str("cd && "),
                None => s.push_str(&format!("cd {} && ", sh_quote(d))),
            }
        }
        s.push_str("AMUX_NO_UPDATE_CHECK=1 ");
        s.push_str(&sh_quote(amux));
        for a in args {
            s.push(' ');
            s.push_str(&sh_quote(a));
        }
        s
    }

    /// Runs amux on the machine with this terminal, and returns its exit code.
    pub fn run(&self, args: &[String], dir: Option<&str>) -> Result<i32> {
        use std::io::IsTerminal;
        let tty = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
        let status = self
            .ssh(tty)
            .arg(self.command_line(args, dir))
            .status()
            .context("could not run ssh")?;
        // ssh's own failures (can't connect) are 255.
        if status.code() == Some(255) {
            eprintln!("amux: could not reach {} ({}).", self.name, self.target);
        }
        Ok(status.code().unwrap_or(1))
    }

    /// Runs amux on the machine without a terminal and returns what it printed.
    pub fn output(&self, args: &[String]) -> Result<String> {
        let out = self
            .ssh(false)
            .arg(self.command_line(args, None))
            .stdin(Stdio::null())
            .output()
            .context("could not run ssh")?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            let why = err
                .lines()
                .rfind(|l| !l.trim().is_empty())
                .unwrap_or("ssh failed");
            bail!("{}: {}", self.name, why.trim());
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// A socket that reaches a run's host on the machine, through `amux __pipe` over SSH,
    /// so previews, typing and attaching work as they do for local runs.
    pub fn connect(&self, run: &str) -> std::io::Result<UnixStream> {
        let (mine, theirs) = UnixStream::pair()?;
        let mut child = self
            .ssh(false)
            .arg(self.command_line(&["__pipe".into(), run.into()], None))
            .stdin(Stdio::from(std::os::fd::OwnedFd::from(theirs.try_clone()?)))
            .stdout(Stdio::from(std::os::fd::OwnedFd::from(theirs)))
            .stderr(Stdio::null())
            .spawn()?;
        std::thread::spawn(move || child.wait());
        Ok(mine)
    }
}

/// Where SSH keeps the shared connections: ~/.ssh, which is private and has a short path
/// (socket paths are limited to about 100 bytes).
fn control_dir() -> Option<PathBuf> {
    let d = paths::HOME.join(".ssh");
    (d.is_dir() && d.as_os_str().len() < 60).then_some(d)
}

/// Runs on the other machines, as their `ps --json` says, named `<machine>:<run>`. A
/// machine that can't be reached is reported on stderr and left out.
pub fn runs(quiet: bool) -> Vec<serde_json::Value> {
    let machines = list();
    let handles: Vec<_> = machines
        .into_iter()
        .map(|m| {
            std::thread::spawn(move || {
                let out = m.output(&["ps".into(), "--json".into(), "--local".into()]);
                (m, out)
            })
        })
        .collect();
    let mut all = vec![];
    for h in handles {
        let Ok((m, out)) = h.join() else { continue };
        match out.and_then(|o| Ok(serde_json::from_str::<Vec<serde_json::Value>>(&o)?)) {
            Ok(list) => {
                for mut v in list {
                    let id = format!("{}:{}", m.name, v["id"].as_str().unwrap_or_default());
                    v["id"] = id.clone().into();
                    v["machine"] = m.name.clone().into();
                    if let Some(run) = v.get_mut("run") {
                        run["id"] = id.into();
                    }
                    all.push(v);
                }
            }
            Err(e) if !quiet => eprintln!("amux: {e:#}"),
            Err(_) => {}
        }
    }
    all
}

/// `amux __pipe <run>` on the machine: copies stdin to the run's socket and the socket to
/// stdout, for `Machine::connect`.
pub fn pipe_main(args: &[String]) -> i32 {
    use std::io::{Read, Write};
    let Some(id) = args.first() else {
        return 2;
    };
    let Ok(sock) = crate::runs::client::connect(id) else {
        return 1;
    };
    let Ok(mut to_sock) = sock.try_clone() else {
        return 1;
    };
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin().lock();
        let mut buf = [0u8; 16 * 1024];
        while let Ok(n) = stdin.read(&mut buf) {
            if n == 0 || to_sock.write_all(&buf[..n]).is_err() {
                break;
            }
        }
        let _ = to_sock.shutdown(std::net::Shutdown::Write);
    });
    let mut from = sock;
    let mut out = std::io::stdout().lock();
    let mut buf = [0u8; 16 * 1024];
    loop {
        match from.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if out.write_all(&buf[..n]).and_then(|_| out.flush()).is_err() {
                    break;
                }
            }
        }
    }
    0
}

const USAGE: &str =
    "amux machine add <name> <ssh-target> [--amux <path>] | list | rm <name> | check <name>";

pub fn machine_cmd(argv: &[String]) -> Result<i32> {
    match argv.first().map(String::as_str) {
        None | Some("list" | "ls") => {
            let all = list();
            if all.is_empty() {
                println!("No machines. Add one with `amux machine add <name> <user@host>`.");
            }
            for m in all {
                println!("{:<12} {}", m.name, m.target);
            }
            Ok(0)
        }
        Some("add") => {
            let name = need(argv.get(1), USAGE)?;
            let target = need(argv.get(2), USAGE)?;
            if !valid_name(name) {
                bail!("Machine names use letters, digits, '-' and '_'.");
            }
            if !valid_target(target) {
                bail!("\"{target}\" doesn't look like an SSH destination (user@host).");
            }
            let amux = match argv.get(3).map(String::as_str) {
                Some("--amux") => Some(need(argv.get(4), USAGE)?.to_string()),
                Some(a) => bail!("Unknown argument {a}. Usage: {USAGE}"),
                None => None,
            };
            let mut all = list();
            if all.iter().any(|m| m.name == name) {
                bail!("A machine named \"{name}\" exists. `amux machine rm {name}` first.");
            }
            let m = Machine {
                name: name.to_string(),
                target: target.to_string(),
                amux,
            };
            all.push(m.clone());
            save(all)?;
            println!("Added {name}. Checking it…");
            check(&m)
        }
        Some("rm") => {
            let name = need(argv.get(1), USAGE)?;
            let mut all = list();
            let n = all.len();
            all.retain(|m| m.name != name);
            if all.len() == n {
                bail!("No machine named \"{name}\".");
            }
            save(all)?;
            println!("Removed {name}. Its runs keep going there.");
            Ok(0)
        }
        Some("check") => {
            let m = find(need(argv.get(1), USAGE)?).ok_or_else(|| anyhow!("No such machine."))?;
            check(&m)
        }
        Some(a) => bail!("Unknown argument {a}. Usage: {USAGE}"),
    }
}

fn check(m: &Machine) -> Result<i32> {
    match m.output(&["--version".into()]) {
        Ok(v) => {
            println!("{}: amux {} over ssh {}", m.name, v.trim(), m.target);
            Ok(0)
        }
        Err(e) => {
            println!(
                "{e:#}\nMake sure `ssh {}` works without a password prompt, and that amux is installed there (or pass --amux <path>).",
                m.target
            );
            Ok(1)
        }
    }
}

/// Commands whose run arguments may name a run on another machine.
const RUN_COMMANDS: &[&str] = &[
    "attach", "logs", "peek", "stop", "rm", "explain", "send", "wait", "diff", "verify", "pr",
    "checks", "comment", "comments", "events", "overlap", "land", "failover", "review",
];

/// If this command is about a run on another machine, runs it there and returns its exit
/// code. All the runs named must be on the same machine.
pub fn forward(cmd: &str, rest: &[String]) -> Option<Result<i32>> {
    if !RUN_COMMANDS.contains(&cmd) {
        return None;
    }
    let mut machine: Option<Machine> = None;
    let mut args = vec![cmd.to_string()];
    for a in rest {
        match split(a).filter(|_| !a.starts_with('-')) {
            Some((m, run)) => {
                if machine.as_ref().is_some_and(|x| x.name != m.name) {
                    return Some(Err(anyhow!("Name runs on one machine at a time.")));
                }
                machine = Some(m);
                args.push(run);
            }
            None => args.push(a.clone()),
        }
    }
    let m = machine?;
    Some(m.run(&args, None))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_for_the_remote_shell() {
        let m = Machine {
            name: "gpu".into(),
            target: "me@gpu".into(),
            amux: None,
        };
        let line = m.command_line(
            &["send".into(), "r1".into(), "it's $HOME; rm -rf ~".into()],
            None,
        );
        assert_eq!(
            line,
            "AMUX_NO_UPDATE_CHECK=1 amux send r1 'it'\\''s $HOME; rm -rf ~'"
        );
        let line = m.command_line(&["ps".into()], Some("~/src/my app"));
        assert_eq!(
            line,
            "cd \"$HOME\"/'src/my app' && AMUX_NO_UPDATE_CHECK=1 amux ps"
        );
        assert!(valid_target("me@host.example.com") && valid_target("box"));
        assert!(!valid_target("-oProxyCommand=x") && !valid_target("a b") && !valid_target("a;b"));
    }
}
