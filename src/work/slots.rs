//! Machine-wide slots for heavy work: test suites, builds, dev servers and agent runs.
//!
//! Several agents in several worktrees can each decide to run the full test suite or
//! start a dev server at the same moment, which is how a laptop runs out of memory. A
//! slot caps how many commands of one class run at once, across every run and terminal
//! on the machine, and holds new ones back while the machine is short of CPU or memory.
//!
//! On disk, in `~/.amux/slots/<class>/`:
//!   <n>.lock         slot n. Held with flock(2) by whoever runs in it, so the kernel frees
//!                    it when that process ends, however it ends. Its content says who.
//!   queue/<pid>      one file per waiter, so waiters are served first come, first served.
//!
//! Limits come from `"limits"` in `~/.amux/config.json`:
//!   { "limits": { "test": 2, "build": 2, "dev": 1, "runs": 4,
//!                 "maxLoad": 1.5, "minFreeMemMB": 2048 } }
//! Any other key with a number is the limit of a class of that name.

use crate::store::load_config;
use crate::{fsx, paths, time};
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// The exit code for "no slot in time": EX_TEMPFAIL, so scripts and agents can tell it
/// from a failing test and try later or let CI run it instead.
pub const EX_TEMPFAIL: i32 = 75;

/// The class that limits how many agent runs are alive at once.
pub const RUNS: &str = "runs";

#[derive(Clone, Debug, PartialEq)]
pub struct Limits {
    pub classes: Vec<(String, usize)>,
    /// One-minute load average per CPU above which new work waits.
    pub max_load: f64,
    /// Available memory below which new work waits.
    pub min_free_mb: u64,
}

impl Limits {
    pub fn from_config() -> Limits {
        let cfg = load_config().unwrap_or_default();
        Limits::parse(cfg.extra.get("limits"), cpus())
    }

    pub fn parse(v: Option<&Value>, cpus: usize) -> Limits {
        // A quarter of the CPUs for each heavy class: test runners and compilers use
        // every core they get, so two at once already fill a laptop.
        let heavy = (cpus / 4).max(1);
        let mut l = Limits {
            classes: vec![
                ("test".into(), heavy),
                ("build".into(), heavy),
                ("dev".into(), 2),
                // Agent runs are not limited unless you set one.
                (RUNS.into(), usize::MAX),
            ],
            max_load: 1.5,
            min_free_mb: 1024,
        };
        let Some(Value::Object(m)) = v else {
            return l;
        };
        for (k, v) in m {
            match (k.as_str(), v) {
                ("maxLoad", v) => {
                    if let Some(f) = v.as_f64().filter(|f| *f > 0.0) {
                        l.max_load = f;
                    }
                }
                ("minFreeMemMB", v) => {
                    if let Some(n) = v.as_u64() {
                        l.min_free_mb = n;
                    }
                }
                (class, v) if valid_class(class) => {
                    // 0 or null means no limit.
                    let n = if v.is_null() || v.as_u64() == Some(0) {
                        usize::MAX
                    } else if let Some(n) = v.as_u64() {
                        n.min(256) as usize
                    } else {
                        continue;
                    };
                    match l.classes.iter_mut().find(|(c, _)| c == class) {
                        Some(e) => e.1 = n,
                        None => l.classes.push((class.to_string(), n)),
                    }
                }
                _ => {}
            }
        }
        l
    }

    pub fn of(&self, class: &str) -> usize {
        self.classes
            .iter()
            .find(|(c, _)| c == class)
            .map_or((cpus() / 4).max(1), |e| e.1)
    }
}

pub fn valid_class(c: &str) -> bool {
    !c.is_empty()
        && c.len() <= 32
        && c.chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
}

pub fn cpus() -> usize {
    std::thread::available_parallelism().map_or(4, |n| n.get())
}

/// Who holds a slot or waits for one.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Holder {
    pub pid: u32,
    /// The run it belongs to (`AMUX_RUN`), if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<String>,
    pub cmd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    pub since: i64,
}

impl Holder {
    pub fn me(cmd: &str) -> Holder {
        Holder {
            pid: std::process::id(),
            run: std::env::var("AMUX_RUN").ok().filter(|r| !r.is_empty()),
            cmd: crate::util::one_line(cmd, 120),
            cwd: std::env::current_dir().ok().map(|d| paths::tildify(&d)),
            since: time::now_ms(),
        }
    }

    pub fn label(&self) -> String {
        match &self.run {
            Some(r) => format!("{r}: {}", self.cmd),
            None => format!("pid {}: {}", self.pid, self.cmd),
        }
    }
}

pub fn slots_dir() -> PathBuf {
    paths::AMUX_HOME.join("slots")
}

fn class_dir(root: &Path, class: &str) -> PathBuf {
    root.join(class)
}

/// A held slot. Dropping it (or the process ending) frees it.
pub struct Slot {
    file: File,
    pub index: usize,
}

impl Slot {
    /// Lets processes this one starts keep the slot after it exits, so a dev server that
    /// outlives `amux slot` still counts. The kernel frees it when the last one closes it.
    pub fn inherit(&self) {
        unsafe {
            let fd = self.file.as_raw_fd();
            let flags = libc::fcntl(fd, libc::F_GETFD);
            if flags >= 0 {
                libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC);
            }
        }
    }
}

fn open_lock(p: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(p)
}

fn try_flock(f: &File, op: libc::c_int) -> bool {
    unsafe { libc::flock(f.as_raw_fd(), op | libc::LOCK_NB) == 0 }
}

/// The holders of a class's slots now, by slot number. Holders of slots above the
/// current limit (it was lowered) count too.
pub fn holders(root: &Path, class: &str) -> Vec<(usize, Option<Holder>)> {
    let Ok(rd) = fs::read_dir(class_dir(root, class)) else {
        return vec![];
    };
    let mut out = vec![];
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let Some(n) = name.strip_suffix(".lock").and_then(|n| n.parse().ok()) else {
            continue;
        };
        let Ok(f) = open_lock(&e.path()) else {
            continue;
        };
        if try_flock(&f, libc::LOCK_SH) {
            unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_UN) };
            continue;
        }
        let mut s = String::new();
        let _ = (&f).take(8 * 1024).read_to_string(&mut s);
        out.push((n, serde_json::from_str(&s).ok()));
    }
    out.sort_by_key(|(n, _)| *n);
    out
}

/// Live waiters of a class, first come first. Files of waiters that have gone are removed.
pub fn waiters(root: &Path, class: &str) -> Vec<Holder> {
    let dir = class_dir(root, class).join("queue");
    let Ok(rd) = fs::read_dir(&dir) else {
        return vec![];
    };
    let mut out: Vec<Holder> =
        rd.flatten()
            .filter_map(|e| {
                let h: Option<Holder> = fs::read(e.path())
                    .ok()
                    .and_then(|b| serde_json::from_slice(&b).ok());
                match h {
                    Some(h) if crate::runs::alive(h.pid) => Some(h),
                    // Unreadable right after creation is normal; only drop it when it's old.
                    None if e.metadata().and_then(|m| m.modified()).is_ok_and(|t| {
                        t.elapsed().unwrap_or_default() < Duration::from_secs(5)
                    }) =>
                    {
                        None
                    }
                    _ => {
                        let _ = fs::remove_file(e.path());
                        None
                    }
                }
            })
            .collect();
    out.sort_by_key(|h| (h.since, h.pid));
    out
}

/// What the machine looks like right now.
#[derive(Clone, Copy, Debug, Default)]
pub struct Pressure {
    /// One-minute load average divided by the number of CPUs.
    pub load: f64,
    pub free_mb: Option<u64>,
}

impl Pressure {
    pub fn now() -> Pressure {
        let mut avg = [0f64; 3];
        let load = if unsafe { libc::getloadavg(avg.as_mut_ptr(), 3) } > 0 {
            avg[0] / cpus() as f64
        } else {
            0.0
        };
        Pressure {
            load,
            free_mb: free_mem_mb(),
        }
    }

    /// Why new work should wait, if it should.
    pub fn too_high(&self, l: &Limits) -> Option<String> {
        if self.load > l.max_load {
            return Some(format!(
                "the machine is busy (load {:.1} per CPU, limit {:.1})",
                self.load, l.max_load
            ));
        }
        match self.free_mb {
            Some(m) if m < l.min_free_mb => Some(format!(
                "memory is low ({m} MB free, limit {} MB)",
                l.min_free_mb
            )),
            _ => None,
        }
    }
}

#[cfg(target_os = "linux")]
fn free_mem_mb() -> Option<u64> {
    let s = fs::read_to_string("/proc/meminfo").ok()?;
    let line = s.lines().find(|l| l.starts_with("MemAvailable:"))?;
    let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kb / 1024)
}

#[cfg(target_os = "macos")]
fn free_mem_mb() -> Option<u64> {
    fn sysctl<T: Default + Copy>(name: &std::ffi::CStr) -> Option<T> {
        let mut v = T::default();
        let mut len = std::mem::size_of::<T>();
        let r = unsafe {
            libc::sysctlbyname(
                name.as_ptr(),
                (&mut v as *mut T).cast(),
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        (r == 0 && len == std::mem::size_of::<T>()).then_some(v)
    }
    // The kernel's own estimate of how much memory is free for use, in percent: what
    // Activity Monitor's memory pressure and `memory_pressure` are based on.
    let level: u32 = sysctl(c"kern.memorystatus_level")?;
    let total: u64 = sysctl(c"hw.memsize")?;
    Some(total / 100 * u64::from(level.min(100)) / (1024 * 1024))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn free_mem_mb() -> Option<u64> {
    None
}

/// How to wait for a slot.
pub struct Wait {
    /// Give up after this long; `None` waits as long as it takes.
    pub timeout: Option<Duration>,
    /// Wait for the machine to calm down too, not just for a free slot.
    pub pressure: bool,
}

/// Why a waiter is still waiting, for progress messages.
#[derive(Clone, Debug, PartialEq)]
pub enum Blocked {
    /// All slots are taken; these hold them.
    Full(Vec<Holder>),
    /// Others came first.
    Behind(usize),
    /// The machine is busy or short of memory.
    Pressure(String),
}

impl Blocked {
    pub fn describe(&self, class: &str, limit: usize) -> String {
        match self {
            Blocked::Full(h) => {
                let who: Vec<String> = h.iter().map(Holder::label).collect();
                let s = if limit == 1 { "" } else { "s" };
                if who.is_empty() {
                    format!("all {limit} {class} slot{s} are taken")
                } else {
                    format!("all {limit} {class} slot{s} are taken ({})", who.join("; "))
                }
            }
            Blocked::Behind(n) => format!("{n} earlier {class} job(s) are waiting first"),
            Blocked::Pressure(why) => why.clone(),
        }
    }
}

/// Removes the queue file when the waiter stops waiting, however it stops.
struct QueueEntry(PathBuf);

impl Drop for QueueEntry {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// Takes a slot of `class`, waiting in line for one. Calls `progress` whenever the reason
/// for waiting changes. Returns `None` if `w.timeout` passed first.
pub fn acquire(
    class: &str,
    me: &Holder,
    w: &Wait,
    progress: impl FnMut(&Blocked),
) -> Result<Option<Slot>> {
    acquire_in(&slots_dir(), class, me, w, Limits::from_config, progress)
}

fn acquire_in(
    root: &Path,
    class: &str,
    me: &Holder,
    w: &Wait,
    load_limits: impl Fn() -> Limits,
    mut progress: impl FnMut(&Blocked),
) -> Result<Option<Slot>> {
    if !valid_class(class) {
        bail!("A slot class uses letters, digits, '-' and '_' (got \"{class}\").");
    }
    let dir = class_dir(root, class);
    fsx::mkdir_private(&dir.join("queue"))?;
    let started = Instant::now();
    let mut queued: Option<QueueEntry> = None;
    let mut last: Option<Blocked> = None;
    let mut limits = load_limits();
    let mut reread = Instant::now();
    loop {
        // Pick up a changed limit while waiting, without reading config.json every turn.
        if reread.elapsed() > Duration::from_secs(5) {
            limits = load_limits();
            reread = Instant::now();
        }
        let limit = limits.of(class);
        let blocked = match try_take(
            root,
            class,
            me,
            &limits,
            limit,
            w.pressure,
            queued.is_some(),
        ) {
            Ok(slot) => return Ok(Some(slot)),
            Err(b) => b,
        };
        if queued.is_none() {
            let p = dir.join("queue").join(me.pid.to_string());
            fsx::write_atomic(&p, serde_json::to_string(me)?.as_bytes())?;
            queued = Some(QueueEntry(p));
        }
        if last.as_ref() != Some(&blocked) {
            progress(&blocked);
            last = Some(blocked);
        }
        if w.timeout.is_some_and(|t| started.elapsed() >= t) {
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// One attempt. `in_line`: this waiter already has a queue file, so it counts itself.
fn try_take(
    root: &Path,
    class: &str,
    me: &Holder,
    limits: &Limits,
    limit: usize,
    pressure: bool,
    in_line: bool,
) -> std::result::Result<Slot, Blocked> {
    let held = holders(root, class);
    if held.len() >= limit {
        return Err(Blocked::Full(
            held.into_iter().filter_map(|(_, h)| h).collect(),
        ));
    }
    // First come, first served: only as many waiters as there are free slots may take one.
    let line = waiters(root, class);
    let ahead = line
        .iter()
        .take_while(|h| !(in_line && h.pid == me.pid))
        .filter(|h| h.pid != me.pid)
        .count();
    let free = limit - held.len();
    if ahead >= free {
        return Err(Blocked::Behind(ahead));
    }
    // The machine's own state only holds work back while some of ours is already running,
    // so a machine that is busy with something else never blocks everything forever.
    if pressure && !held.is_empty() {
        if let Some(why) = Pressure::now().too_high(limits) {
            return Err(Blocked::Pressure(why));
        }
    }
    let dir = class_dir(root, class);
    for n in 0..limit.min(256) {
        let Ok(mut f) = open_lock(&dir.join(format!("{n}.lock"))) else {
            continue;
        };
        if try_flock(&f, libc::LOCK_EX) {
            let _ = f.set_len(0);
            let _ = f.rewind();
            let _ = f.write_all(serde_json::to_string(me).unwrap_or_default().as_bytes());
            return Ok(Slot { file: f, index: n });
        }
    }
    Err(Blocked::Full(
        holders(root, class)
            .into_iter()
            .filter_map(|(_, h)| h)
            .collect(),
    ))
}

/// Guesses the class of a shell command: "dev" for servers and watchers, "build" for
/// compilers and bundlers, and "test" for everything else.
pub fn guess_class(cmd: &str) -> &'static str {
    match heavy_class(cmd) {
        Some(c) => c,
        None => "test",
    }
}

/// The class of a command that is worth a slot, or `None` for light commands. Used by
/// `amux slot --check`, the agent hook that keeps unwrapped test runs out.
pub fn heavy_class(cmd: &str) -> Option<&'static str> {
    const DEV: &[&str] = &[
        "npm run dev",
        "npm start",
        "npm run start",
        "pnpm dev",
        "pnpm run dev",
        "pnpm start",
        "yarn dev",
        "yarn start",
        "bun dev",
        "bun run dev",
        "next dev",
        "vite dev",
        "nuxt dev",
        "astro dev",
        "rails server",
        "rails s",
        "manage.py runserver",
        "flask run",
        "uvicorn ",
        "docker compose up",
        "docker-compose up",
        "cargo watch",
        "cargo run",
        "expo start",
        "ng serve",
        "remix dev",
        "wrangler dev",
        "storybook dev",
    ];
    const BUILD: &[&str] = &[
        "cargo build",
        "cargo clippy",
        "npm run build",
        "pnpm build",
        "pnpm run build",
        "yarn build",
        "bun run build",
        "next build",
        "vite build",
        "tsc",
        "go build",
        "xcodebuild",
        "gradle build",
        "gradlew build",
        "gradlew assemble",
        "mvn package",
        "mvn install",
        "docker build",
        "make",
        "cmake --build",
        "swift build",
        "dotnet build",
    ];
    const TEST: &[&str] = &[
        "npm test",
        "npm run test",
        "npm t",
        "pnpm test",
        "pnpm run test",
        "yarn test",
        "bun test",
        "cargo test",
        "cargo nextest",
        "go test",
        "pytest",
        "python -m pytest",
        "jest",
        "vitest",
        "playwright test",
        "cypress run",
        "mvn test",
        "mvn verify",
        "gradle test",
        "gradlew test",
        "gradlew check",
        "rspec",
        "rake test",
        "rails test",
        "phpunit",
        "mix test",
        "swift test",
        "dotnet test",
        "ctest",
        "tox",
        "nox",
        "npm run e2e",
        "pnpm e2e",
        "yarn e2e",
        "npm run test:",
        "pnpm test:",
        "yarn test:",
    ];
    // Look at each command of a pipeline or list on its own, so `cd app && npm test`
    // counts and `echo npm test` mostly doesn't.
    let parts = cmd.split(['&', '|', ';', '\n', '(', ')']);
    let mut found: Option<&'static str> = None;
    for part in parts {
        let words: Vec<&str> = part
            .split_whitespace()
            // Leading variable assignments and wrappers don't change what runs.
            .skip_while(|w| {
                w.contains('=') && !w.starts_with('-')
                    || matches!(
                        *w,
                        "env" | "time" | "nice" | "exec" | "command" | "npx" | "bunx"
                    )
            })
            .collect();
        if words.first() == Some(&"amux") && words.get(1) == Some(&"slot") {
            // Already in a slot: nothing inside it needs another.
            return None;
        }
        let line = words.join(" ");
        let line = line.trim_start_matches("./");
        let starts = |pats: &[&str]| {
            pats.iter().any(|p| {
                line == p.trim_end()
                    || line.starts_with(p)
                        && (p.ends_with(' ')
                            || p.ends_with(':')
                            || line[p.len()..].starts_with([' ', '\t']))
            })
        };
        let class = if starts(DEV) {
            Some("dev")
        } else if starts(TEST) {
            Some("test")
        } else if starts(BUILD) {
            Some("build")
        } else {
            None
        };
        // A dev server wins over a build before it (`npm run build && npm start`).
        match (found, class) {
            (_, Some("dev")) => found = Some("dev"),
            (None, c) | (Some("build"), c @ Some("test")) => found = c.or(found),
            _ => {}
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;
    use serde_json::json;

    #[test]
    fn limits_default_from_cpus_and_read_from_config() {
        let l = Limits::parse(None, 12);
        assert_eq!(l.of("test"), 3);
        assert_eq!(l.of("dev"), 2);
        assert_eq!(l.of(RUNS), usize::MAX);
        let l = Limits::parse(
            Some(&json!({"test": 1, "runs": 4, "gpu": 1, "dev": 0,
            "maxLoad": 0.9, "minFreeMemMB": 4096, "bad class": 3})),
            2,
        );
        assert_eq!(l.of("test"), 1);
        assert_eq!(l.of("build"), 1);
        assert_eq!(l.of("runs"), 4);
        assert_eq!(l.of("gpu"), 1);
        assert_eq!(l.of("dev"), usize::MAX);
        assert_eq!((l.max_load, l.min_free_mb), (0.9, 4096));
        assert!(!l.classes.iter().any(|(c, _)| c == "bad class"));
    }

    #[test]
    fn classifies_heavy_commands() {
        assert_eq!(heavy_class("npm test"), Some("test"));
        assert_eq!(
            heavy_class("cd web && pnpm run test -- --watch=false"),
            Some("test")
        );
        assert_eq!(heavy_class("CI=1 cargo test --all"), Some("test"));
        assert_eq!(heavy_class("npx vitest run src"), Some("test"));
        assert_eq!(heavy_class("./gradlew test"), Some("test"));
        assert_eq!(heavy_class("npm run dev"), Some("dev"));
        assert_eq!(heavy_class("npm run build && npm start"), Some("dev"));
        assert_eq!(heavy_class("cargo build --release"), Some("build"));
        assert_eq!(heavy_class("make"), Some("build"));
        assert_eq!(heavy_class("cargo build && cargo test"), Some("test"));
        assert_eq!(heavy_class("git status"), None);
        assert_eq!(heavy_class("ls tests"), None);
        assert_eq!(heavy_class("makefile-lint"), None);
        assert_eq!(heavy_class("cat jest.config.js"), None);
        assert_eq!(heavy_class("amux slot -- npm test"), None);
        assert_eq!(guess_class("./run-e2e.sh"), "test");
    }

    #[test]
    fn slots_cap_holders_and_free_on_drop() {
        let tmp = TempDir::new();
        let root = tmp.path();
        let me = Holder::me("npm test");
        let w = Wait {
            timeout: Some(Duration::ZERO),
            pressure: false,
        };
        let limits = || Limits::parse(Some(&json!({"t1": 2})), 8);
        let take = |why: &mut Option<Blocked>| {
            acquire_in(root, "t1", &me, &w, limits, |b| *why = Some(b.clone())).unwrap()
        };
        let limit = 2;
        let mut held = vec![];
        for _ in 0..limit {
            held.push(take(&mut None).expect("a free slot"));
        }
        let mut why = None;
        assert!(take(&mut why).is_none());
        match why {
            Some(Blocked::Full(h)) => {
                assert_eq!(h.len(), limit);
                assert_eq!(h[0].cmd, "npm test");
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(
            waiters(root, "t1").is_empty(),
            "the queue file is removed after giving up"
        );
        held.pop();
        assert_eq!(holders(root, "t1").len(), 1);
        assert!(take(&mut None).is_some());
        assert!(acquire_in(root, "../x", &me, &w, limits, |_| {}).is_err());
    }
}
