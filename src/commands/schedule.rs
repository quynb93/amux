//! `amux schedule`: amux commands that run by themselves at set times, such as a morning
//! triage run or an hourly `amux intake`.
//!
//! Schedules live in `~/.amux/schedules.json`. One background process, `amux __scheduler`,
//! starts each when it is due, in the folder it was added from, and logs its output to
//! `~/.amux/schedules/<name>.log`. Any amux command starts that process when there are
//! schedules and it isn't running (so do `amux revive --all` at login and `amux schedule`),
//! and it exits once there are none. A schedule missed while the machine slept runs once
//! when it wakes.

use crate::paths::{self, tildify};
use crate::store::{read_json, valid_name, write_json};
use crate::util::{ago, table};
use crate::{fsx, time};
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::File;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

const USAGE: &str = "amux schedule [list] | add <name> \"<when>\" <amux command...> | rm <name> | run <name> | pause <name> | resume <name> | log <name>";

const WHEN_HELP: &str = "When: \"every 30m\", \"hourly\", \"daily 9:00\", \"weekdays 9:00\", \"mondays 9:00\", or cron (\"0 9 * * 1-5\"), in local time.";

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct Schedule {
    pub name: String,
    /// As given: cron, or one of the friendly forms.
    pub when: String,
    /// The amux command, without `amux`.
    pub args: Vec<String>,
    /// Where it runs: the folder it was added from.
    pub dir: String,
    pub created_at: i64,
    #[serde(default)]
    pub paused: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_code: Option<i32>,
}

// ---- when ----------------------------------------------------------------------------

/// When a schedule is due: every so often, or on a cron calendar in local time.
#[derive(Debug, Clone, PartialEq)]
pub enum When {
    Every(u64),
    Cron(Cron),
}

/// The five cron fields as bit sets: minute, hour, day of month, month, day of week.
#[derive(Debug, Clone, PartialEq)]
pub struct Cron {
    min: u64,
    hour: u64,
    mday: u64,
    mon: u64,
    wday: u64,
    /// Whether day of month and day of week were given (cron matches either then).
    mday_any: bool,
    wday_any: bool,
}

fn field(s: &str, lo: u32, hi: u32) -> Option<u64> {
    let mut bits = 0u64;
    for part in s.split(',') {
        let (range, step) = match part.split_once('/') {
            Some((r, st)) => (r, st.parse::<u32>().ok().filter(|n| *n > 0)?),
            None => (part, 1),
        };
        let (a, b) = if range == "*" {
            (lo, hi)
        } else if let Some((a, b)) = range.split_once('-') {
            (a.parse().ok()?, b.parse().ok()?)
        } else {
            let a: u32 = range.parse().ok()?;
            // "5/15" means from 5 to the end, every 15.
            (a, if part.contains('/') { hi } else { a })
        };
        if a < lo || b > hi || a > b {
            return None;
        }
        let mut v = a;
        while v <= b {
            bits |= 1 << v;
            v += step;
        }
    }
    Some(bits)
}

fn hhmm(s: &str) -> Option<(u32, u32)> {
    let (h, m) = s.split_once(':').unwrap_or((s, "0"));
    let (h, m) = (h.parse().ok()?, m.parse().ok()?);
    (h < 24 && m < 60).then_some((h, m))
}

pub fn parse_when(s: &str) -> Option<When> {
    let s = s.trim().to_ascii_lowercase();
    let words: Vec<&str> = s.split_whitespace().collect();
    let cron = |c: String| parse_cron(&c).map(When::Cron);
    match words.as_slice() {
        ["every", d] => crate::work::guards::parse_duration(d)
            .filter(|ms| *ms >= 1000)
            .map(When::Every),
        ["hourly"] => cron("0 * * * *".into()),
        [day, t] => {
            let (h, m) = hhmm(t)?;
            let dow = match *day {
                "daily" => "*",
                "weekdays" => "1-5",
                "weekends" => "0,6",
                d => {
                    let days = ["sun", "mon", "tue", "wed", "thu", "fri", "sat"];
                    let i = days.iter().position(|x| d.starts_with(x))?;
                    return cron(format!("{m} {h} * * {i}"));
                }
            };
            cron(format!("{m} {h} * * {dow}"))
        }
        _ => cron(s.clone()),
    }
}

fn parse_cron(s: &str) -> Option<Cron> {
    let f: Vec<&str> = s.split_whitespace().collect();
    let [mi, h, md, mo, wd] = f.as_slice() else {
        return None;
    };
    let mut wday = field(wd, 0, 7)?;
    // 7 is Sunday too.
    if wday & (1 << 7) != 0 {
        wday = (wday | 1) & !(1 << 7);
    }
    Some(Cron {
        min: field(mi, 0, 59)?,
        hour: field(h, 0, 23)?,
        mday: field(md, 1, 31)?,
        mon: field(mo, 1, 12)?,
        wday,
        mday_any: *md == "*",
        wday_any: *wd == "*",
    })
}

/// Local time at `ms`: minute, hour, day of month, month (1-12), day of week (0 = Sunday).
fn local(ms: i64) -> (u32, u32, u32, u32, u32) {
    unsafe {
        let secs = (ms.div_euclid(1000)) as libc::time_t;
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&secs, &mut tm);
        (
            tm.tm_min as u32,
            tm.tm_hour as u32,
            tm.tm_mday as u32,
            tm.tm_mon as u32 + 1,
            tm.tm_wday as u32,
        )
    }
}

impl Cron {
    fn day_ok(&self, mday: u32, mon: u32, wday: u32) -> bool {
        let has = |bits: u64, v: u32| bits & (1 << v) != 0;
        let (d, w) = (has(self.mday, mday), has(self.wday, wday));
        let day = match (self.mday_any, self.wday_any) {
            (false, false) => d || w,
            _ => d && w,
        };
        day && has(self.mon, mon)
    }

    /// The first minute after `after` that matches, within a year and a day.
    fn next(&self, after: i64) -> Option<i64> {
        const MIN: i64 = 60_000;
        let mut t = after.div_euclid(MIN) * MIN + MIN;
        let end = after + 367 * 86_400_000;
        while t <= end {
            let (mi, h, md, mo, wd) = local(t);
            if !self.day_ok(md, mo, wd) || self.hour & (1 << h) == 0 {
                // Skip to the next hour (zones with half-hour offsets still land on :00 local).
                t += (60 - mi as i64) * MIN;
                continue;
            }
            if self.min & (1 << mi) != 0 {
                return Some(t);
            }
            t += MIN;
        }
        None
    }
}

impl When {
    /// When it is next due, after it last ran (or was added) at `since`.
    pub fn next(&self, since: i64) -> Option<i64> {
        match self {
            When::Every(ms) => Some(since + *ms as i64),
            When::Cron(c) => c.next(since),
        }
    }
}

impl Schedule {
    fn when(&self) -> Option<When> {
        parse_when(&self.when)
    }

    pub fn next_at(&self) -> Option<i64> {
        self.when()?.next(self.last_at.unwrap_or(self.created_at))
    }
}

// ---- storage -------------------------------------------------------------------------

fn file() -> PathBuf {
    paths::AMUX_HOME.join("schedules.json")
}

fn log_file(name: &str) -> PathBuf {
    paths::AMUX_HOME
        .join("schedules")
        .join(format!("{name}.log"))
}

pub fn load() -> Vec<Schedule> {
    read_json(&file()).unwrap_or_default()
}

fn lock(name: &str) -> Result<File> {
    fsx::mkdir_private(&paths::AMUX_HOME)?;
    let f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(paths::AMUX_HOME.join(name))?;
    Ok(f)
}

/// Read, change and write the schedules, one writer at a time.
fn edit<T>(f: impl FnOnce(&mut Vec<Schedule>) -> Result<T>) -> Result<T> {
    let l = lock("schedules.lock")?;
    unsafe { libc::flock(l.as_raw_fd(), libc::LOCK_EX) };
    let mut all: Vec<Schedule> = read_json(&file())?;
    let out = f(&mut all)?;
    write_json(&file(), &all)?;
    Ok(out)
}

// ---- the command ---------------------------------------------------------------------

pub fn schedule_cmd(argv: &[String]) -> Result<i32> {
    let name = || -> Result<&String> {
        argv.get(1)
            .ok_or_else(|| anyhow!("Which schedule? Usage: {USAGE}"))
    };
    match argv.first().map(String::as_str) {
        None | Some("list" | "ls") => list(argv.iter().any(|a| a == "--json")),
        Some("--json") => list(true),
        Some("add") => add(argv.get(1..).unwrap_or_default()),
        Some("rm" | "remove") => {
            let n = name()?.clone();
            edit(|all| {
                let before = all.len();
                all.retain(|s| s.name != n);
                if all.len() == before {
                    bail!("No schedule \"{n}\". `amux schedule` lists them.");
                }
                Ok(())
            })?;
            let _ = std::fs::remove_file(log_file(&n));
            println!("Removed schedule {n}.");
            Ok(0)
        }
        Some(p @ ("pause" | "resume")) => {
            let n = name()?.clone();
            let pause = p == "pause";
            edit(|all| {
                let s = all
                    .iter_mut()
                    .find(|s| s.name == n)
                    .ok_or_else(|| anyhow!("No schedule \"{n}\"."))?;
                s.paused = pause;
                if !pause {
                    // Count from now, so resuming doesn't fire what was missed while paused.
                    s.last_at = Some(time::now_ms());
                }
                Ok(())
            })?;
            println!("{} {n}.", if pause { "Paused" } else { "Resumed" });
            ensure_running();
            Ok(0)
        }
        Some("run") => {
            let n = name()?;
            let s = load()
                .into_iter()
                .find(|s| &s.name == n)
                .ok_or_else(|| anyhow!("No schedule \"{n}\"."))?;
            println!("Running {n}: amux {}", shown(&s.args));
            let code = fire(&s, true)?.wait()?.code().unwrap_or(1);
            record(&s.name, time::now_ms(), Some(code));
            Ok(code)
        }
        Some("log") => {
            let n = name()?;
            match std::fs::read_to_string(log_file(n)) {
                Ok(t) => print!("{}", tail(&t, 200)),
                Err(_) => println!("{n} has not run yet."),
            }
            Ok(0)
        }
        Some(other) => bail!("Unknown: amux schedule {other}. Usage: {USAGE}"),
    }
}

fn tail(t: &str, lines: usize) -> String {
    let v: Vec<&str> = t.lines().collect();
    let start = v.len().saturating_sub(lines);
    v[start..].iter().map(|l| format!("{l}\n")).collect()
}

fn add(argv: &[String]) -> Result<i32> {
    let (Some(name), Some(when)) = (argv.first(), argv.get(1)) else {
        bail!("Usage: amux schedule add <name> \"<when>\" <amux command...>\n{WHEN_HELP}");
    };
    if !valid_name(name) {
        bail!("Schedule names use letters, digits, '.', '_' and '-'.");
    }
    if parse_when(when).is_none() {
        bail!("Can't read \"{when}\" as a time. {WHEN_HELP}");
    }
    let mut args: Vec<String> = argv.get(2..).unwrap_or_default().to_vec();
    if args.first().map(String::as_str) == Some("amux") {
        args.remove(0);
    }
    match args.first().map(String::as_str) {
        None => bail!(
            "What should it run? For example:\n  amux schedule add {name} \"{when}\" spawn --prompt \"…\" claude"
        ),
        Some("schedule" | "run" | "attach" | "ui" | "serve") => {
            bail!("`amux {}` needs a terminal or never ends, so it can't be scheduled.", args[0])
        }
        _ => {}
    }
    let dir = std::env::current_dir()?;
    let s = Schedule {
        name: name.clone(),
        when: when.clone(),
        args,
        dir: dir.to_string_lossy().into_owned(),
        created_at: time::now_ms(),
        ..Default::default()
    };
    let next = s.next_at();
    edit(|all| {
        if all.iter().any(|x| x.name == s.name) {
            bail!(
                "There is a schedule named {} already. `amux schedule rm {}` first.",
                s.name,
                s.name
            );
        }
        all.push(s.clone());
        Ok(())
    })?;
    println!(
        "Scheduled {}: amux {}\n  in {}, next {}",
        s.name,
        shown(&s.args),
        tildify(&dir),
        next.map_or("never".into(), describe_at)
    );
    if ensure_running() {
        println!("  The scheduler runs in the background while you're logged in. `amux revive --at-login` brings it back after a restart.");
    }
    Ok(0)
}

/// "in 5m (09:00)" style.
/// The command as you'd type it.
fn shown(args: &[String]) -> String {
    args.iter()
        .map(|a| {
            if !a.is_empty()
                && a.chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_.,:/@=+%^".contains(c))
            {
                a.clone()
            } else {
                crate::util::sh_quote(a)
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn describe_at(at: i64) -> String {
    let now = time::now_ms();
    let (mi, h, _, _, wd) = local(at);
    // A day name when it's not within the next day.
    let day = if at - now > 20 * 3_600_000 {
        format!(
            "{} ",
            ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"][wd as usize % 7]
        )
    } else {
        String::new()
    };
    let rel = if at <= now {
        "now".to_string()
    } else {
        format!("in {}", ago(now, at).trim_end_matches(" ago"))
    };
    format!("{rel} ({day}{h:02}:{mi:02})")
}

fn list(json: bool) -> Result<i32> {
    let all = load();
    if json {
        let v: Vec<serde_json::Value> = all
            .iter()
            .map(|s| {
                let mut j = serde_json::to_value(s).unwrap_or_default();
                j["nextAt"] = serde_json::json!(s.next_at());
                j
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&v)?);
        return Ok(0);
    }
    if all.is_empty() {
        println!("No schedules. {USAGE}\n{WHEN_HELP}");
        return Ok(0);
    }
    let now = time::now_ms();
    let rows: Vec<Vec<String>> = all
        .iter()
        .map(|s| {
            let next = if s.paused {
                "paused".into()
            } else {
                s.next_at().map_or("never".into(), describe_at)
            };
            let last = match (s.last_at, s.last_code) {
                (None, _) => "-".into(),
                (Some(t), Some(0)) => format!("ok {}", ago(t, now)),
                (Some(t), Some(c)) => format!("exit {c} {}", ago(t, now)),
                (Some(t), None) => format!("running {}", ago(t, now)),
            };
            vec![
                s.name.clone(),
                s.when.clone(),
                next,
                last,
                format!("amux {}", shown(&s.args)),
            ]
        })
        .collect();
    println!(
        "{}",
        table(&["NAME", "WHEN", "NEXT", "LAST", "COMMAND"], &rows).trim_end()
    );
    ensure_running();
    if !running() {
        println!(
            "The scheduler isn't running: {}",
            tildify(&paths::AMUX_HOME.join("scheduler.log"))
        );
    }
    Ok(0)
}

// ---- running -------------------------------------------------------------------------

/// Starts a schedule's command, its output appended to its log.
fn fire(s: &Schedule, foreground: bool) -> Result<Child> {
    let exe = exe_path().context("can't tell where amux is")?;
    let log = log_file(&s.name);
    if let Some(d) = log.parent() {
        fsx::mkdir_private(d)?;
    }
    let mut out = fsx::open_append(&log)?;
    use std::io::Write;
    let _ = writeln!(
        out,
        "\n== {} amux {}",
        time::iso(time::now_ms()),
        shown(&s.args)
    );
    let mut c = Command::new(exe);
    c.args(&s.args)
        .current_dir(&s.dir)
        .env("AMUX_SCHEDULE", &s.name)
        .env("AMUX_NO_UPDATE_CHECK", "1")
        .stdin(Stdio::null());
    if foreground {
        c.stdout(Stdio::inherit()).stderr(Stdio::inherit());
    } else {
        c.stdout(out.try_clone()?).stderr(out);
    }
    c.spawn()
        .with_context(|| format!("could not start {}", s.name))
}

fn record(name: &str, at: i64, code: Option<i32>) {
    let _ = edit(|all| {
        if let Some(s) = all.iter_mut().find(|s| s.name == name) {
            s.last_at = Some(at);
            s.last_code = code;
        }
        Ok(())
    });
}

/// The amux binary, as it was when this process started. On Linux, current_exe() of a
/// process whose binary was replaced (`amux update`) names a deleted file.
fn exe_path() -> Option<PathBuf> {
    static EXE: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    EXE.get_or_init(|| std::env::current_exe().ok()).clone()
}

fn try_lock(f: &File) -> bool {
    unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0 }
}

/// Whether a scheduler holds its lock now.
pub fn running() -> bool {
    match lock("scheduler.lock") {
        Ok(f) => !try_lock(&f),
        Err(_) => false,
    }
}

/// Starts the scheduler if there are schedules and none is running. Returns true when it
/// started one.
pub fn ensure_running() -> bool {
    if !file().exists() || load().iter().all(|s| s.paused) || running() {
        return false;
    }
    let Some(exe) = exe_path() else { return false };
    let Ok(log) = fsx::open_append(&paths::AMUX_HOME.join("scheduler.log")) else {
        return false;
    };
    use std::os::unix::process::CommandExt;
    let mut c = Command::new(exe);
    c.arg("__scheduler")
        .env("AMUX_NO_UPDATE_CHECK", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log);
    unsafe {
        c.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    match c.spawn() {
        Ok(mut child) => {
            std::thread::spawn(move || child.wait());
            true
        }
        Err(_) => false,
    }
}

fn mtime(p: &Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(p).ok()?.modified().ok()
}

/// `amux __scheduler`: starts schedules when they're due, until there are none.
pub fn main() -> i32 {
    let Ok(l) = lock("scheduler.lock") else {
        return 1;
    };
    if !try_lock(&l) {
        return 0;
    }
    let exe = exe_path();
    let built = exe.as_deref().and_then(mtime);
    let tick = std::env::var("AMUX_SCHEDULE_TICK_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(20_000);
    let mut going: HashMap<String, Child> = HashMap::new();
    eprintln!(
        "{} scheduler {} started",
        time::iso(time::now_ms()),
        std::process::id()
    );
    loop {
        // Finished commands: their exit code, and a buzz if one failed.
        going.retain(|name, child| match child.try_wait() {
            Ok(Some(st)) => {
                let code = st.code().unwrap_or(1);
                let at = load()
                    .iter()
                    .find(|s| &s.name == name)
                    .and_then(|s| s.last_at);
                record(name, at.unwrap_or_else(time::now_ms), Some(code));
                if code != 0 {
                    crate::runs::push::send(
                        "schedule",
                        name,
                        &format!("amux: schedule {name} failed"),
                        &format!("exit {code}. amux schedule log {name}"),
                    );
                }
                false
            }
            Ok(None) => true,
            Err(_) => false,
        });
        let all = load();
        if all.is_empty() && going.is_empty() {
            eprintln!(
                "{} no schedules left; scheduler exits",
                time::iso(time::now_ms())
            );
            return 0;
        }
        let now = time::now_ms();
        for s in &all {
            if s.paused || going.contains_key(&s.name) {
                continue;
            }
            if s.next_at().is_some_and(|t| t <= now) {
                match fire(s, false) {
                    Ok(child) => {
                        record(&s.name, now, None);
                        going.insert(s.name.clone(), child);
                    }
                    Err(e) => {
                        record(&s.name, now, Some(127));
                        eprintln!("{} {}: {e:#}", time::iso(now), s.name);
                    }
                }
            }
        }
        // amux was updated: carry on as the new version once nothing is going.
        if going.is_empty() && exe.as_deref().and_then(mtime) != built {
            if let Some(e) = &exe {
                drop(l);
                use std::os::unix::process::CommandExt;
                let err = Command::new(e).arg("__scheduler").exec();
                eprintln!("could not restart the scheduler: {err}");
                return 1;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(tick));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_when() {
        assert_eq!(parse_when("every 30m"), Some(When::Every(1_800_000)));
        assert!(parse_when("every 0s").is_none());
        for ok in [
            "hourly",
            "daily 9:00",
            "weekdays 09:30",
            "Mondays 8",
            "*/15 * * * *",
            "0 9 * * 1-5",
            "0 0 1,15 * *",
            "5/10 * * * 7",
        ] {
            assert!(parse_when(ok).is_some(), "{ok}");
        }
        for bad in [
            "",
            "soon",
            "daily 25:00",
            "60 * * * *",
            "* * * *",
            "0 9 * * 8",
            "*/0 * * * *",
            "fooday 9:00",
        ] {
            assert!(parse_when(bad).is_none(), "{bad}");
        }
        let When::Cron(c) = parse_when("0 9 * * 7").unwrap() else {
            panic!()
        };
        assert_eq!(c.wday, 1, "7 is Sunday");
    }

    #[test]
    fn finds_the_next_minute() {
        let When::Cron(c) = parse_when("*/15 * * * *").unwrap() else {
            panic!()
        };
        let t = c.next(time::now_ms()).unwrap();
        let (mi, ..) = local(t);
        assert_eq!(mi % 15, 0);
        assert!(t > time::now_ms() && t - time::now_ms() <= 15 * 60_000);
        // A weekday at 9:00 is within the next four days, at 9:00.
        let When::Cron(w) = parse_when("weekdays 9:00").unwrap() else {
            panic!()
        };
        let t = w.next(time::now_ms()).unwrap();
        let (mi, h, _, _, wd) = local(t);
        assert_eq!((mi, h), (0, 9));
        assert!((1..=5).contains(&wd));
        assert!(t - time::now_ms() <= 4 * 86_400_000);
        // Day of month or day of week, when both are given.
        let When::Cron(e) = parse_when("0 0 1 * 1").unwrap() else {
            panic!()
        };
        assert!(e.day_ok(1, 3, 3) && e.day_ok(9, 3, 1) && !e.day_ok(9, 3, 3));
    }
}
