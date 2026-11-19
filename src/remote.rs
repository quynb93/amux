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

    /// The remote command that runs amux with `args`, optionally in `dir`: `amux __argv
    /// <payload>`, with the arguments as base64url JSON, which every login shell (sh,
    /// fish, tcsh…) passes on unchanged. When amux there is too old for `__argv`, the
    /// arguments are quoted for a POSIX shell instead, if its login shell is one.
    pub fn command_line(&self, args: &[String], dir: Option<&str>) -> Result<String> {
        if self.speaks_argv() {
            return Ok(self.argv_line(args, dir));
        }
        match self.login_shell() {
            Some(sh) if posix_shell(&sh) => Ok(self.legacy_line(args, dir)),
            sh => bail!(
                "amux on {} is too old to take arguments safely through its login shell ({}): update amux there.",
                self.name,
                sh.as_deref().unwrap_or("unknown")
            ),
        }
    }

    /// `amux __argv <base64url JSON {args, cwd}>`: only letters, digits, `-` and `_`
    /// after the program, so no shell does anything to it.
    pub fn argv_line(&self, args: &[String], dir: Option<&str>) -> String {
        use base64::Engine;
        let payload = serde_json::json!({ "args": args, "cwd": dir });
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload.to_string());
        format!(
            "{} __argv {b64}",
            sh_quote(self.amux.as_deref().unwrap_or("amux"))
        )
    }

    /// Whether amux on the machine has `__argv` (0.11.15 and later): asked once, and
    /// remembered in ~/.amux/remote-caps.json once it has. That it hasn't is remembered
    /// for an hour, so every command to an older amux doesn't ask again first.
    fn speaks_argv(&self) -> bool {
        let key = format!(
            "{} {} {}",
            self.name,
            self.target,
            self.amux.as_deref().unwrap_or("amux")
        );
        let file = paths::AMUX_HOME.join("remote-caps.json");
        let mut caps: serde_json::Map<String, serde_json::Value> =
            read_json(&file).unwrap_or_default();
        let now = crate::time::now_ms();
        match caps.get(&key) {
            Some(v) if v["argv"].as_bool() == Some(true) => return true,
            Some(v)
                if v["argv"].as_bool() == Some(false)
                    && v["at"]
                        .as_i64()
                        .is_some_and(|at| (0..3_600_000).contains(&(now - at))) =>
            {
                return false
            }
            _ => {}
        }
        let probe = self
            .ssh(false)
            .arg(self.argv_line(&["--version".into()], None))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output();
        match probe {
            Ok(o) if o.status.success() && !o.stdout.is_empty() => {
                caps.insert(key, serde_json::json!({ "argv": true }));
                let _ = write_json(&file, &caps);
                true
            }
            // ssh itself failed (255) or couldn't run: say nothing here, the command
            // will fail the same way and say why.
            Ok(o) if o.status.code() == Some(255) => true,
            Err(_) => true,
            Ok(_) => {
                caps.insert(key, serde_json::json!({ "argv": false, "at": now }));
                let _ = write_json(&file, &caps);
                false
            }
        }
    }

    /// The machine's login shell, from `$SHELL` there (every shell expands that).
    fn login_shell(&self) -> Option<String> {
        let o = self
            .ssh(false)
            .arg("echo $SHELL")
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()
            .filter(|o| o.status.success())?;
        let sh = String::from_utf8_lossy(&o.stdout).trim().to_string();
        (!sh.is_empty()).then_some(sh)
    }

    /// The command for amux before 0.11.15, quoted for a POSIX shell.
    fn legacy_line(&self, args: &[String], dir: Option<&str>) -> String {
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
            .arg(self.command_line(args, dir)?)
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
            .arg(self.command_line(args, None)?)
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
            .arg(
                self.command_line(&["__pipe".into(), run.into()], None)
                    .map_err(std::io::Error::other)?,
            )
            .stdin(Stdio::from(std::os::fd::OwnedFd::from(theirs.try_clone()?)))
            .stdout(Stdio::from(std::os::fd::OwnedFd::from(theirs)))
            .stderr(Stdio::null())
            .spawn()?;
        std::thread::spawn(move || child.wait());
        Ok(mine)
    }
}

/// sh, bash, zsh and the like, by the path's last part.
fn posix_shell(path: &str) -> bool {
    let name = path
        .rsplit('/')
        .next()
        .unwrap_or(path)
        .trim_start_matches('-');
    matches!(
        name,
        "sh" | "bash" | "zsh" | "dash" | "ksh" | "mksh" | "ash" | "yash" | "busybox"
    )
}

/// `amux __argv <payload>` on the machine: decodes the arguments (and the folder to run
/// in) that `Machine::argv_line` sent, and runs amux with them.
pub fn argv_main(rest: &[String]) -> i32 {
    use base64::Engine;
    use std::os::unix::process::CommandExt;
    let payload = rest
        .first()
        .and_then(|b| {
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(b.trim_end_matches('='))
                .ok()
        })
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok());
    let Some(p) = payload else {
        eprintln!("amux: __argv: unreadable arguments");
        return 2;
    };
    let Some(args) = p["args"].as_array().and_then(|a| {
        a.iter()
            .map(|v| v.as_str().map(str::to_string))
            .collect::<Option<Vec<String>>>()
    }) else {
        eprintln!("amux: __argv: unreadable arguments");
        return 2;
    };
    if let Some(d) = p["cwd"].as_str() {
        let dir = match d.strip_prefix("~/") {
            Some(rest) => paths::HOME.join(rest),
            None if d == "~" => paths::HOME.clone(),
            None => PathBuf::from(d),
        };
        if let Err(e) = std::env::set_current_dir(&dir) {
            eprintln!("amux: {}: {e}", dir.display());
            return 1;
        }
    }
    let Ok(exe) = std::env::current_exe() else {
        return 1;
    };
    let err = Command::new(exe)
        .args(&args)
        .env("AMUX_NO_UPDATE_CHECK", "1")
        .exec();
    eprintln!("amux: {err}");
    1
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

/// Commands whose run arguments may name a run on another machine: those that take one
/// run (its first positional argument), and those that take several.
const ONE_RUN: &[&str] = &[
    "attach", "logs", "peek", "stop", "explain", "send", "diff", "verify", "pr", "checks",
    "comment", "comments", "failover", "review",
];
const RUNS: &[&str] = &["rm", "wait", "events", "overlap", "land"];

/// Options of `cmd` that take a value, which is never a run. Per command: the same
/// letter means different things (`-t` is `--timeout` for `send`, `--test` for `verify`).
fn value_options(cmd: &str) -> &'static [&'static str] {
    match cmd {
        "send" => &["--key", "-k", "--timeout", "-t"],
        "wait" => &["--until", "-u", "--timeout", "-t"],
        "verify" => &["--command", "-c"],
        "pr" => &["--base", "-B"],
        "comments" => &["--rm"],
        "review" => &["--drop", "--expect"],
        "failover" => &["--to"],
        "overlap" => &["--into"],
        "land" => &["--into", "--command"],
        _ => &[],
    }
}

/// The positions in `rest` that name runs for `cmd`: positional arguments only (not an
/// option's value, nothing after `--`), the first one only for a one-run command.
fn run_positions(cmd: &str, rest: &[String]) -> Vec<usize> {
    let many = RUNS.contains(&cmd);
    if !many && !ONE_RUN.contains(&cmd) {
        return vec![];
    }
    let takes_value = value_options(cmd);
    let mut out = vec![];
    let mut i = 0;
    while i < rest.len() {
        let a = rest[i].as_str();
        if a == "--" {
            break;
        }
        if a.starts_with('-') {
            if takes_value.contains(&a) {
                i += 1;
            }
        } else {
            out.push(i);
            if !many {
                break;
            }
        }
        i += 1;
    }
    out
}

/// If this command is about a run on another machine, runs it there and returns its exit
/// code. All the runs named must be on the same machine.
pub fn forward(cmd: &str, rest: &[String]) -> Option<Result<i32>> {
    let mut machine: Option<Machine> = None;
    let mut args: Vec<String> = std::iter::once(cmd.to_string())
        .chain(rest.iter().cloned())
        .collect();
    for i in run_positions(cmd, rest) {
        let Some((m, run)) = split(&rest[i]).filter(|(_, r)| crate::store::valid_name(r)) else {
            continue;
        };
        if machine.as_ref().is_some_and(|x| x.name != m.name) {
            return Some(Err(anyhow!("Name runs on one machine at a time.")));
        }
        machine = Some(m);
        args[i + 1] = run;
    }
    let m = machine?;
    // On the other machine the command comes from sshd, so its host can't tell an agent
    // from you. From inside a run, typing into another machine's run is refused here
    // (as the MCP tools do): an agent types only into its own workers, which are local.
    if matches!(cmd, "send" | "attach") {
        if let crate::runs::peer::From::Run(r) =
            crate::runs::peer::Peer::of_pid(std::process::id()).from()
        {
            return Some(Err(anyhow!(
                "{r} can't type into a run on {}: from inside a run, amux sends only to that run's own workers.",
                m.name
            )));
        }
    }
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
        let line = m.legacy_line(
            &["send".into(), "r1".into(), "it's $HOME; rm -rf ~".into()],
            None,
        );
        assert_eq!(
            line,
            "AMUX_NO_UPDATE_CHECK=1 amux send r1 'it'\\''s $HOME; rm -rf ~'"
        );
        let line = m.legacy_line(&["ps".into()], Some("~/src/my app"));
        assert_eq!(
            line,
            "cd \"$HOME\"/'src/my app' && AMUX_NO_UPDATE_CHECK=1 amux ps"
        );
        assert!(valid_target("me@host.example.com") && valid_target("box"));
        assert!(!valid_target("-oProxyCommand=x") && !valid_target("a b") && !valid_target("a;b"));

        // What any shell gets: the program, `__argv` and base64url, nothing to expand.
        let args: Vec<String> = ["send", "r1", "--", "it's $HOME; `x` %s\n!!\\ 'q' \"d\""]
            .map(String::from)
            .to_vec();
        let line = m.argv_line(&args, Some("~/my dir"));
        let b64 = line.strip_prefix("amux __argv ").unwrap();
        assert!(b64
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
        use base64::Engine;
        let v: serde_json::Value = serde_json::from_slice(
            &base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(b64)
                .unwrap(),
        )
        .unwrap();
        assert_eq!(v["args"], serde_json::json!(args));
        assert_eq!(v["cwd"], "~/my dir");
        assert!(posix_shell("/bin/bash") && posix_shell("/usr/bin/zsh") && posix_shell("-sh"));
        assert!(!posix_shell("/usr/local/bin/fish") && !posix_shell("/bin/tcsh"));
    }

    #[test]
    fn finds_the_run_arguments_of_a_command() {
        let v = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        // The message after the run, or after --, is never a run.
        assert_eq!(run_positions("send", &v(&["gpu:w", "see gpu:x"])), [0]);
        assert_eq!(run_positions("send", &v(&["w", "--", "gpu:x"])), [0]);
        assert_eq!(run_positions("send", &v(&["--key", "gpu:x", "w"])), [2]);
        assert_eq!(
            run_positions("comment", &v(&["gpu:w", "a.rs:3", "box:y"])),
            [0]
        );
        assert_eq!(
            run_positions("land", &v(&["a", "--into", "gpu:main", "b"])),
            [0, 3]
        );
        assert_eq!(run_positions("rm", &v(&["a", "b", "--", "c"])), [0, 1]);
        assert!(run_positions("ps", &v(&["gpu:w"])).is_empty());
        assert!(run_positions("send", &v(&["--", "gpu:w"])).is_empty());
        // `-t` is a flag for verify (--test), and `-c`/`-B` take values.
        assert_eq!(run_positions("verify", &v(&["-t", "gpu:w"])), [1]);
        assert_eq!(
            run_positions("verify", &v(&["-c", "make test", "gpu:w"])),
            [2]
        );
        assert_eq!(run_positions("pr", &v(&["-B", "dev", "gpu:w"])), [2]);
    }
}
