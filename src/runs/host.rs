//! `amux __run-host <id>`: owns a run's pseudo-terminal. It starts the CLI on the
//! terminal's slave side, copies everything the CLI prints to output.log and to attached
//! clients, and passes client keystrokes and window sizes back to the CLI. It exits when
//! the CLI does, after recording the exit code.

use super::detect::{Change, Detector, Keys, Source};
use super::notify::Notifier;
use super::program::{Report, PROBE_REPLY};
use super::screen;
use super::{
    load, parse_size, read_frame, run_dir, save, ProgramStatus, Run, Waiting, INPUT, RESIZE, STATE,
    WATCH,
};
use crate::work::{approve, slots};
use crate::{fsx, time};
use anyhow::{anyhow, Result};
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

const LOG_ROTATE: u64 = 4 * 1024 * 1024;
/// How long a hook-reported permission block is held back for auto-approve to answer it
/// (see `Hub::apply`).
const GRACE: Duration = Duration::from_millis(1500);

pub fn main(args: &[String]) -> i32 {
    let Some(id) = args.first() else {
        return 2;
    };
    match host(id) {
        Ok(code) => code,
        Err(e) => {
            if let Ok(mut run) = load(id) {
                run.error = Some(format!("{e:#}"));
                run.ended_at = Some(time::now_ms());
                let _ = save(&run);
            }
            let _ = fs::remove_file(run_dir(id).join("sock"));
            1
        }
    }
}

fn cvt(r: libc::c_int) -> io::Result<libc::c_int> {
    if r < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(r)
    }
}

fn openpty(rows: u16, cols: u16) -> io::Result<(OwnedFd, OwnedFd)> {
    let (mut m, mut s): (RawFd, RawFd) = (-1, -1);
    let mut ws = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    unsafe {
        cvt(libc::openpty(
            &mut m,
            &mut s,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            // *mut on macOS, *const on Linux; a raw pointer fits both.
            std::ptr::addr_of_mut!(ws),
        ))?;
        let (m, s) = (OwnedFd::from_raw_fd(m), OwnedFd::from_raw_fd(s));
        // Neither end may leak into the CLI beyond its stdio.
        for fd in [m.as_raw_fd(), s.as_raw_fd()] {
            cvt(libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC))?;
        }
        Ok((m, s))
    }
}

fn set_size(fd: RawFd, rows: u16, cols: u16) {
    let ws = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    unsafe { libc::ioctl(fd, libc::TIOCSWINSZ as _, &ws) };
}

/// Publishes the terminal size as `size` ("rows cols") in the run folder (the host's
/// cwd), for clients that emulate the screen themselves, like `amux ui`'s preview.
fn record_size(rows: u16, cols: u16) {
    if fs::write("size.tmp", format!("{rows} {cols}\n")).is_ok() {
        let _ = fs::rename("size.tmp", "size");
    }
}

struct Client {
    stream: UnixStream,
    /// Set by a WATCH frame: a client that only shows the screen, like a preview in
    /// `amux ui`. It doesn't count as someone attached, so notifications still go out.
    watching: Arc<AtomicBool>,
}

/// Shared between the output pump and the clients.
struct Hub {
    clients: Vec<Client>,
    /// The CLI's screen, kept current, so a new client gets exactly what is on it.
    screen: screen::Parser,
    /// The end of the output so far, for an escape sequence or character the last chunk
    /// cut in two (see `screen::unfinished`).
    tail: Vec<u8>,
    /// When the CLI last printed something, for the stall watchdog.
    last_out: i64,
    detector: Detector,
    notifier: Notifier,
    /// The host's copy of run.json, which it alone writes while the run is alive.
    run: Run,
    /// The auto-approve rules, reread every few seconds, and when.
    rules: approve::Rules,
    rules_at: i64,
    /// The prompt last judged, so each is judged (and answered) once.
    judged: Option<String>,
    /// `amux failover` is running for this run (at its limit, or before it), so it is
    /// started once at a time. Cleared when that failover fails, so a later limit can
    /// still move the run.
    moving: Arc<AtomicBool>,
    /// That prompt is on screen and auto-approve didn't answer it: it waits for you.
    held: bool,
    /// When auto-approve last answered a prompt.
    auto_answered: Option<Instant>,
    /// A hook-reported block not announced yet, and since when (see `apply`).
    pending: Option<(String, Instant)>,
}

/// Starts `amux failover <run> [extra…]` in its own session: failover stops this run,
/// which ends this host. Clears `moving` when it can't start or fails (no session to
/// resume, no account left), so the run isn't stuck as "moving".
fn start_failover(id: &str, extra: &[&str], moving: Arc<AtomicBool>) {
    let Ok(exe) = std::env::current_exe() else {
        moving.store(false, Ordering::SeqCst);
        return;
    };
    let mut c = Command::new(exe);
    c.args(["failover", id])
        .args(extra)
        .env("AMUX_NO_UPDATE_CHECK", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    unsafe {
        c.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    match c.spawn() {
        Ok(mut child) => {
            std::thread::spawn(move || {
                if !child.wait().is_ok_and(|s| s.success()) {
                    moving.store(false, Ordering::SeqCst);
                }
            });
        }
        Err(_) => moving.store(false, Ordering::SeqCst),
    }
}

impl Default for Hub {
    fn default() -> Self {
        Hub {
            clients: Vec::new(),
            screen: screen::new_parser(24, 80, 0),
            tail: Vec::new(),
            last_out: 0,
            detector: Detector::default(),
            notifier: Notifier::default(),
            run: Run::default(),
            rules: approve::Rules::default(),
            rules_at: 0,
            judged: None,
            moving: Arc::new(AtomicBool::new(false)),
            held: false,
            auto_answered: None,
            pending: None,
        }
    }
}

impl Hub {
    /// Whether a terminal is attached, so you already see what the CLI shows.
    fn attached(&self) -> bool {
        self.clients
            .iter()
            .any(|c| !c.watching.load(Ordering::Relaxed))
    }

    /// Records the CLI's own report of its state (OSC 7501).
    fn set_status(&mut self, record: Option<Option<Report>>) {
        let Some(record) = record else {
            return;
        };
        let now = time::now_ms();
        let via = self.detector.hooks_only().then(|| "hook".to_string());
        let old = self.run.status.take();
        self.run.status = record.map(|r| ProgramStatus {
            since: match &old {
                Some(o) if o.state == r.state => o.since,
                _ => now,
            },
            state: r.state,
            kind: r.kind,
            message: r.message,
            via,
        });
        let _ = save(&self.run);
    }

    /// Records a state a state hook reported (`amux __hook`). OSC 7501 reports win.
    fn hook_report(&mut self, payload: &[u8]) {
        let Some(r) = super::program::from_hook(payload) else {
            return;
        };
        let change = self.detector.hook(r);
        let record = self.detector.take_record();
        self.set_status(record);
        self.apply(change);
    }

    /// The account hit its usage limit: the run waits (with the CLI's message), the
    /// account is marked limited until the reset, and, with `onLimit: failover`, the run
    /// moves to another account by itself.
    fn limited(&mut self, msg: &str) {
        let l = crate::work::limits::record(&self.run.account, msg, Some(&self.run.id));
        self.run.waiting = Some(Waiting {
            reason: l.reason.clone(),
            since: time::now_ms(),
            source: Some("limit".into()),
        });
        let _ = save(&self.run);
        crate::plugins::hook("limited", &self.run.id, &l.reason);
        if !self.attached() {
            let msg = format!("{} hit its usage limit: {}", self.run.account, l.reason);
            self.notifier
                .send(&self.run.id, "limited", &self.title(), &msg);
        }
        if crate::work::limits::on_limit() == "failover"
            && !self.moving.swap(true, Ordering::SeqCst)
        {
            start_failover(&self.run.id, &[], self.moving.clone());
        }
    }

    /// Whether auto-approve may answer prompts in this run.
    fn auto_approving(&mut self) -> bool {
        if self.run.auto_approve == Some(false) {
            return false;
        }
        let now = time::now_ms();
        if now.saturating_sub(self.rules_at) > 2000 {
            self.rules = approve::load();
            self.rules_at = now;
        }
        !self.rules.allow.is_empty()
    }

    /// Answers a permission prompt on the screen when the rules allow what it asks, and
    /// logs each prompt it judged. True when it answered.
    fn auto_approve(&mut self, mut master: &File) -> bool {
        if !self.auto_approving() {
            return false;
        }
        let now = time::now_ms();
        let Some(ask) = approve::read_prompt(&screen::text(&self.screen)) else {
            self.judged = None;
            self.held = false;
            return false;
        };
        let label = ask.label();
        if self.judged.as_deref() == Some(label.as_str()) {
            return false;
        }
        self.judged = Some(label.clone());
        let (action, why) = match approve::judge(&self.rules, &ask, Path::new(&self.run.cwd)) {
            approve::Verdict::Allow(by) => ("approved", by.join(", ")),
            approve::Verdict::Deny(rule) => ("denied", rule),
            approve::Verdict::Ask(why) => ("asked", why),
        };
        let approved = action == "approved" && master.write_all(ask.yes.as_bytes()).is_ok();
        self.held = !approved;
        if approved {
            self.auto_answered = Some(Instant::now());
        }
        let line = serde_json::json!({
            "at": now, "kind": ask.kind, "subject": ask.subject,
            "action": if action == "approved" && !approved { "failed" } else { action },
            "why": why,
        });
        let _ = fsx::append_private(
            &self.run.dir().join("approvals.jsonl"),
            &format!("{line}\n"),
        );
        if approved {
            crate::plugins::hook("approved", &self.run.id, &label);
        }
        approved
    }

    fn title(&self) -> String {
        format!("amux: {} ({})", self.run.id, self.run.account)
    }

    /// Records a change in whether the CLI is waiting for you, and notifies you when it
    /// starts waiting and no terminal is attached to see it.
    ///
    /// Claude Code runs its PermissionRequest hook before the prompt shows, so a permission
    /// block a hook reports may be one auto-approve is about to answer. While auto-approve
    /// is on, such a block is held back (`pending`) until the prompt shows and auto-approve
    /// doesn't answer it, or GRACE passes; answered meanwhile, it is never announced.
    fn apply(&mut self, change: Option<Change>) {
        match change {
            None => return,
            Some(Change::Waiting(reason)) => {
                if self.detector.source() == Source::Hook
                    && self.detector.blocked_kind() == Some("permission")
                    && !self.held
                    && self.auto_approving()
                {
                    self.pending = Some((reason, Instant::now()));
                    return;
                }
                self.announce(reason);
            }
            Some(Change::Cleared) => {
                self.pending = None;
                self.run.waiting = None;
            }
        }
        let _ = save(&self.run);
    }

    fn announce(&mut self, reason: String) {
        crate::plugins::hook("waiting", &self.run.id, &reason);
        if !self.attached() {
            let msg = format!("Waiting for you: {reason}");
            self.notifier
                .send(&self.run.id, "waiting", &self.title(), &msg);
        }
        self.run.waiting = Some(Waiting {
            reason,
            since: time::now_ms(),
            source: Some(self.detector.source().as_str().into()),
        });
    }

    /// Announces a held-back block once auto-approve has passed on its prompt, or GRACE
    /// has passed. A block that came in just after auto-approve answered, with no prompt
    /// on screen by now, was about that prompt: it is dropped as answered.
    fn settle(&mut self, now: Instant) {
        let Some((_, at)) = &self.pending else {
            return;
        };
        let at = *at;
        if !self.held && now.saturating_duration_since(at) < GRACE {
            return;
        }
        let Some((reason, _)) = self.pending.take() else {
            return;
        };
        let late = self
            .auto_answered
            .is_some_and(|t| at.saturating_duration_since(t) < GRACE);
        if !self.held && late && approve::read_prompt(&screen::text(&self.screen)).is_none() {
            let change = self.detector.answered();
            let record = self.detector.take_record();
            self.set_status(record);
            self.apply(change);
            return;
        }
        self.announce(reason);
        let _ = save(&self.run);
    }
}

/// Waits for a `runs` slot when `"limits"` caps how many runs are alive, or the machine is
/// too busy to start one more. The run shows as queued meanwhile. The slot is held until
/// the host exits, which is when the CLI has.
fn wait_for_slot(run: &mut Run) -> Result<slots::Slot> {
    let me = slots::Holder {
        pid: std::process::id(),
        run: Some(run.id.clone()),
        cmd: format!("{} {}", run.account, run.args.join(" ")),
        cwd: Some(crate::paths::tildify(std::path::Path::new(&run.cwd))),
        since: time::now_ms(),
    };
    let limit = slots::Limits::from_config().of(slots::RUNS);
    let wait = slots::Wait {
        timeout: None,
        pressure: true,
    };
    let slot = slots::acquire(slots::RUNS, &me, &wait, |b| {
        // Re-read: `amux stop` may have recorded something in between.
        if let Ok(mut r) = load(&run.id) {
            r.host_pid = Some(std::process::id());
            r.queued = Some(Waiting {
                reason: b.describe(slots::RUNS, limit),
                since: run.queued.as_ref().map_or_else(time::now_ms, |q| q.since),
                source: Some("slots".into()),
            });
            *run = r;
            let _ = save(run);
        }
    })?
    .ok_or_else(|| anyhow!("gave up waiting for a run slot"))?;
    Ok(slot)
}

fn host(id: &str) -> Result<i32> {
    let mut run = load(id)?;
    let _slot = wait_for_slot(&mut run)?;
    run.queued = None;
    let dir = run_dir(id);
    // Bind by relative name: Unix socket paths are limited to about 100 bytes, and
    // AMUX_HOME can be longer than that. The CLI gets its own cwd below.
    std::env::set_current_dir(&dir)?;
    let _ = fs::remove_file("sock");
    let listener = UnixListener::bind("sock")?;

    let (master, slave) = openpty(run.rows.max(1), run.cols.max(1))?;
    record_size(run.rows.max(1), run.cols.max(1));
    // A sandboxed run starts the container engine, which runs the CLI in the image.
    let argv = match &run.sandbox {
        Some(sb) => {
            // A container of the same name left over (a revived run) would block this one.
            sb.remove(id);
            sb.command(id, &run.cwd, &run.bin, &run.args)
        }
        None => [vec![run.bin.clone()], run.args.clone()].concat(),
    };
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..])
        .current_dir(&run.cwd)
        .env("AMUX_RUN", id)
        .stdin(Stdio::from(slave.try_clone()?))
        .stdout(Stdio::from(slave.try_clone()?))
        .stderr(Stdio::from(slave));
    unsafe {
        // A new session with the terminal as its controlling terminal, so the CLI gets
        // job control, Ctrl-C and window-size signals as it would in a real terminal.
        cmd.pre_exec(|| {
            cvt(libc::setsid())?;
            cvt(libc::ioctl(0, libc::TIOCSCTTY as _, 0))?;
            Ok(())
        });
    }
    let spawned = cmd.spawn();
    // `cmd` holds copies of the slave end; the host must not keep it open, or reading
    // the master would never report the CLI's exit.
    drop(cmd);
    let mut child = spawned.map_err(|e| {
        if e.kind() == io::ErrorKind::NotFound {
            anyhow!("\"{}\" not found on PATH.", argv[0])
        } else {
            e.into()
        }
    })?;
    let pid = child.id();
    run.host_pid = Some(std::process::id());
    run.pid = Some(pid);
    save(&run)?;

    let master = Arc::new(File::from(master));
    let hub = Arc::new(Mutex::new(Hub {
        detector: Detector::new(&run.waiting_patterns).with_limits(&run.limit_patterns),
        notifier: Notifier::from_config(),
        run: run.clone(),
        screen: screen::new_parser(run.rows.max(1), run.cols.max(1), 0),
        ..Default::default()
    }));
    let (done_tx, done_rx) = mpsc::channel::<()>();
    {
        let (master, hub) = (master.clone(), hub.clone());
        let log = dir.join("output.log");
        std::thread::spawn(move || {
            pump(&master, &hub, log);
            let _ = done_tx.send(());
        });
    }
    // State hooks' connections, in the order they came, read off the accept thread so a
    // slow one never holds up a client attaching. It also announces held-back blocks.
    let (hooks_tx, hooks_rx) = mpsc::channel::<UnixStream>();
    {
        let hub = hub.clone();
        std::thread::spawn(move || hook_reader(&hooks_rx, &hub));
    }
    {
        let (master, hub) = (master.clone(), hub.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                serve(stream, &hub, &master, pid, &hooks_tx);
            }
        });
    }

    {
        let (master, hub) = (master.clone(), hub.clone());
        let guard = run
            .guard
            .clone()
            .unwrap_or_else(|| crate::work::guards::Guard::from_config(None, None));
        let (cwd, started) = (PathBuf::from(&run.cwd), run.started_at);
        std::thread::spawn(move || watchdog(&hub, &master, guard, &cwd, started, pid));
    }
    {
        let hub = hub.clone();
        std::thread::spawn(move || quota_watch(&hub));
    }

    let status = child.wait()?;
    // The engine's client can be killed while its container goes on: end that too.
    if let Some(sb) = &run.sandbox {
        sb.remove(id);
    }
    let code = status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(0));
    // Let the last output reach the log and clients. A process the CLI left behind can
    // keep the terminal open, so don't wait for that.
    let _ = done_rx.recv_timeout(Duration::from_millis(500));
    let mut h = hub.lock().unwrap_or_else(|e| e.into_inner());
    // The final screen, for previews and `amux peek` after the run has ended. Saved before
    // run.json says it ended, so no reader falls back to replaying the log in between.
    let _ = fsx::write_atomic(&dir.join("screen.ans"), &screen::snapshot(&h.screen));
    h.run.exit_code = Some(code);
    h.run.ended_at = Some(time::now_ms());
    h.run.waiting = None;
    let record = h.detector.exited();
    h.set_status(record);
    save(&h.run)?;
    let _ = fs::remove_file("sock");
    crate::plugins::hook("exited", &h.run.id, &code.to_string());
    if !h.attached() {
        let msg = format!("Exited with code {code}");
        h.notifier.send(&h.run.id, "exited", &h.title(), &msg);
    }
    for c in &h.clients {
        let _ = c.stream.shutdown(std::net::Shutdown::Both);
    }
    Ok(code)
}

/// Flags the run when it works without changing its files for too long, or runs past its
/// time limit, and acts on it as configured (see work/guards.rs). Ends with the run.
fn watchdog(
    hub: &Mutex<Hub>,
    master: &File,
    guard: crate::work::guards::Guard,
    cwd: &std::path::Path,
    started: i64,
    pid: u32,
) {
    use crate::work::guards::{fingerprint, Verdict, Watch};
    if guard.stall_ms == 0 && guard.max_ms == 0 {
        return;
    }
    // Tests make it tick faster.
    let tick = std::env::var("AMUX_GUARD_TICK_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(60_000u64);
    let action = guard.action.clone();
    let mut w = Watch::new(guard, time::now_ms());
    loop {
        std::thread::sleep(Duration::from_millis(tick));
        let now = time::now_ms();
        let working = {
            let h = lock(hub);
            if h.run.exit_code.is_some() {
                return;
            }
            // A hook's "working" isn't ended by an interrupted turn, so output decides then.
            let reported = h
                .run
                .status
                .as_ref()
                .filter(|s| s.via.is_none() || s.state != "working")
                .map(|s| s.state.as_str());
            match reported {
                Some("working") => true,
                Some(_) => false,
                // No reports: printing in the last seconds means busy.
                // Its own stall flag doesn't count as waiting on you.
                None => {
                    now - h.last_out < 10_000
                        && h.run
                            .waiting
                            .as_ref()
                            .is_none_or(|w| w.source.as_deref() == Some("stall"))
                }
            }
        };
        let print = fingerprint(cwd);
        let reason = match w.tick(now, working, print, started) {
            Verdict::Stalled(r) | Verdict::Overtime(r) => r,
            Verdict::Recovered => {
                let mut h = lock(hub);
                if h.run.waiting.as_ref().and_then(|w| w.source.as_deref()) == Some("stall") {
                    h.run.waiting = None;
                    let _ = save(&h.run);
                }
                continue;
            }
            Verdict::Fine => continue,
        };
        {
            let mut h = lock(hub);
            h.run.waiting = Some(Waiting {
                reason: reason.clone(),
                since: now,
                source: Some("stall".into()),
            });
            let _ = save(&h.run);
            crate::plugins::hook("stalled", &h.run.id, &reason);
            if !h.attached() {
                let title = h.title();
                let id = h.run.id.clone();
                h.notifier.send(&id, "stalled", &title, &reason);
            }
        }
        match action.as_str() {
            // Esc stops the agent's current turn in Claude Code and Codex.
            "interrupt" => {
                let _ = (&*master).write_all(b"\x1b");
            }
            "stop" => unsafe {
                libc::killpg(pid as libc::pid_t, libc::SIGTERM);
            },
            _ => {}
        }
    }
}

/// Watches the account's quota (work/quota.rs) while the run lives. For a Codex run it
/// also reads the numbers from the run's session file. Once the account passes
/// `limits.switchAt`, it tells you, and with `onLimit: failover` it moves the run to an
/// account under it, once, and only between turns: never mid-turn, and never while a
/// terminal is attached (you are working in it). It moves to an account under switchAt
/// only, so a session doesn't bounce back to an account that is just as full.
fn quota_watch(hub: &Mutex<Hub>) {
    use crate::work::{limits, quota};
    let tick = std::env::var("AMUX_QUOTA_TICK_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20_000u64);
    let account = lock(hub).run.account.clone();
    let provider = account.split('@').next().unwrap_or_default().to_string();
    let codex_home = crate::store::get_provider(&provider)
        .ok()
        .filter(|s| s.sessions.as_deref() == Some("codex"))
        .map(|s| s.shared_home);
    let mut seen = None;
    // Whether you were told the account is past switchAt, since it last was under it.
    let mut told = false;
    // A move before the limit was started; it is tried once. If it fails, the run stays
    // and keeps being watched (and can still fail over at the limit itself).
    let mut tried = false;
    loop {
        std::thread::sleep(Duration::from_millis(tick));
        let run = {
            let h = lock(hub);
            if h.run.exit_code.is_some() {
                return;
            }
            if h.moving.load(Ordering::SeqCst) {
                continue;
            }
            h.run.clone()
        };
        if let Some(home) = &codex_home {
            quota::refresh_codex(&run, home, &mut seen);
        }
        let Some(at) = quota::switch_at() else {
            continue;
        };
        let Some(q) = quota::load(&account) else {
            continue;
        };
        let Some(used) = q.used() else { continue };
        if used < at {
            told = false;
            continue;
        }
        let now = time::now_ms();
        let near = format!("{account} is at {}", quota::describe(&q, now));
        let failover = limits::on_limit() == "failover";
        let target = failover
            .then(|| quota::pick_switch(&provider, &account, at))
            .flatten();
        let h = lock(hub);
        let between = !h.attached()
            && quota::turn_boundary(
                h.run.status.as_ref(),
                h.run.waiting.as_ref(),
                h.last_out,
                now,
            );
        match target {
            Some(t) if between && !tried => {
                if h.moving.swap(true, Ordering::SeqCst) {
                    continue;
                }
                tried = true;
                let msg = format!("{near}; moving {} to {}", run.id, t.id());
                crate::plugins::hook("near-limit", &run.id, &msg);
                let title = h.title();
                h.notifier.send(&run.id, "limited", &title, &msg);
                let moving = h.moving.clone();
                drop(h);
                start_failover(&run.id, &["--to", &t.id(), "--near-limit"], moving);
            }
            // Wait for the turn to end.
            Some(_) => {}
            None if !told => {
                told = true;
                let msg = if failover {
                    format!("{near}, and no other {provider} account is under {at:.0}%")
                } else {
                    near
                };
                crate::plugins::hook("near-limit", &run.id, &msg);
                if !h.attached() {
                    let title = h.title();
                    h.notifier.send(&run.id, "limited", &title, &msg);
                }
            }
            None => {}
        }
    }
}

/// Appends to output.log, moving it to output.log.1 once it passes LOG_ROTATE.
struct Log {
    path: PathBuf,
    file: Option<File>,
    size: u64,
}

impl Log {
    fn open(path: PathBuf) -> Self {
        let file = fsx::open_append(&path).ok();
        let size = file
            .as_ref()
            .and_then(|f| f.metadata().ok())
            .map_or(0, |m| m.len());
        Log { path, file, size }
    }

    fn write(&mut self, d: &[u8]) {
        if self.size >= LOG_ROTATE {
            let _ = fs::rename(&self.path, self.path.with_extension("log.1"));
            self.file = fsx::open_append(&self.path).ok();
            self.size = 0;
        }
        if let Some(f) = &mut self.file {
            if f.write_all(d).is_ok() {
                self.size += d.len() as u64;
            }
        }
    }
}

fn pump(master: &File, hub: &Mutex<Hub>, log: PathBuf) {
    let mut log = Log::open(log);
    let mut buf = [0u8; 16 * 1024];
    loop {
        let n = match (&*master).read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            // EIO: the CLI and everything it started have closed the terminal.
            Err(_) => break,
        };
        let chunk = &buf[..n];
        log.write(chunk);
        let mut h = hub.lock().unwrap_or_else(|e| e.into_inner());
        h.screen.process(chunk);
        h.last_out = time::now_ms();
        h.tail.extend_from_slice(chunk);
        let keep = h.tail.len().saturating_sub(screen::TAIL);
        h.tail.drain(..keep);
        // A client that can't keep up (write timeout) or has gone is dropped.
        h.clients.retain_mut(|c| c.stream.write_all(chunk).is_ok());
        let change = h.detector.feed(chunk);
        // The CLI's own report first, so no reader sees "waiting" without the report that
        // caused it (each saves run.json).
        let record = h.detector.take_record();
        h.set_status(record);
        if h.auto_approve(master) {
            // Answered for you: not waiting after all, and no one needs to hear about it.
            let cleared = h.detector.answered();
            // A hook-reported block goes back to working: record that too.
            let record = h.detector.take_record();
            h.set_status(record);
            h.pending = None;
            if h.run.waiting.is_some() {
                h.apply(cleared);
            }
        } else {
            h.apply(change);
        }
        h.settle(Instant::now());
        if let Some(msg) = h.detector.take_limit() {
            h.limited(&msg);
        }
        // Tell the CLI that this terminal understands OSC 7501, so it reports its state.
        // Only live output gets here, never a replay, so each probe is answered once.
        let probed = h.detector.take_probe();
        drop(h);
        if probed {
            let _ = (&*master).write_all(PROBE_REPLY);
        }
    }
}

/// What a client's first frame says about it.
enum Lead {
    /// WATCH: a preview, or (false) a person.
    Watch(bool),
    /// A state hook's report: the client wants nothing else, not even the screen.
    State,
    Other,
}

/// Reads state hooks' reports, one connection at a time in the order they came, and
/// announces held-back blocks when their time is up (see `Hub::settle`).
fn hook_reader(rx: &mpsc::Receiver<UnixStream>, hub: &Mutex<Hub>) {
    loop {
        match rx.recv_timeout(Duration::from_millis(250)) {
            Ok(stream) => {
                let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
                if let Ok((STATE, data)) = read_frame(&mut &stream) {
                    lock(hub).hook_report(&data);
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }
        let mut h = lock(hub);
        if h.run.exit_code.is_some() {
            return;
        }
        h.settle(Instant::now());
    }
}

/// A client may send WATCH as its first frame. It is read before the replay, so a preview
/// never counts as attached, not even while the replay is being sent. A state hook sends
/// one STATE frame and goes. Clients that send something else first (`amux attach` sends
/// RESIZE) are left alone.
fn leading(stream: &UnixStream) -> Lead {
    let mut head = [0u8; 6];
    let _ = stream.set_read_timeout(Some(Duration::from_millis(20)));
    let n = unsafe {
        libc::recv(
            stream.as_raw_fd(),
            head.as_mut_ptr().cast(),
            head.len(),
            libc::MSG_PEEK,
        )
    };
    let head = &head[..n.max(0) as usize];
    let lead = if head.len() >= 5 && head[0] == STATE {
        // Its body is read off this thread (see `hook_reader`).
        Lead::State
    } else {
        match watch_frame(head) {
            Some(on) => {
                let mut frame = [0u8; 6];
                Lead::Watch((&*stream).read_exact(&mut frame).is_ok() && on)
            }
            None => Lead::Other,
        }
    };
    let _ = stream.set_read_timeout(None);
    lead
}

/// The value of a complete WATCH frame.
fn watch_frame(d: &[u8]) -> Option<bool> {
    match d {
        [WATCH, 0, 0, 0, 1, v] => Some(*v == 1),
        _ => None,
    }
}

fn lock(hub: &Mutex<Hub>) -> std::sync::MutexGuard<'_, Hub> {
    hub.lock().unwrap_or_else(|e| e.into_inner())
}

fn serve(
    stream: UnixStream,
    hub: &Arc<Mutex<Hub>>,
    master: &Arc<File>,
    pid: u32,
    hooks: &mpsc::Sender<UnixStream>,
) {
    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
    let watching = match leading(&stream) {
        Lead::State => {
            let _ = hooks.send(stream);
            return;
        }
        Lead::Watch(on) => on,
        Lead::Other => false,
    };
    let Ok(mut out) = stream.try_clone() else {
        return;
    };
    let watching = Arc::new(AtomicBool::new(watching));
    {
        let mut h = hub.lock().unwrap_or_else(|e| e.into_inner());
        // The current screen first, then live output from the same point on. The emulator
        // may be in the middle of an escape sequence or character the last chunk cut off;
        // the client needs that start too, or the rest of it reads as text.
        let mut first = screen::snapshot(&h.screen);
        first.extend_from_slice(screen::unfinished(&h.tail));
        if out.write_all(&first).is_err() {
            return;
        }
        h.clients.push(Client {
            stream: out,
            watching: watching.clone(),
        });
    }
    let (master, hub) = (master.clone(), hub.clone());
    std::thread::spawn(move || {
        let mut stream = stream;
        let mut keys = Keys::default();
        while let Ok((kind, data)) = read_frame(&mut stream) {
            match kind {
                INPUT => {
                    if (&*master).write_all(&data).is_err() {
                        break;
                    }
                    // Focus reports and moving through a menu don't answer anything.
                    if !keys.answers(&data) {
                        continue;
                    }
                    let mut h = lock(&hub);
                    let change = h.detector.answered();
                    // A hook-reported block goes back to working: record that too.
                    let record = h.detector.take_record();
                    h.set_status(record);
                    h.apply(change);
                }
                RESIZE => {
                    if let Some((rows, cols)) = parse_size(&data) {
                        let (rows, cols) = (rows.max(1), cols.max(1));
                        set_size(master.as_raw_fd(), rows, cols);
                        record_size(rows, cols);
                        lock(&hub)
                            .screen
                            .set_size(screen::nz(rows), screen::nz(cols));
                        // Also when the size didn't change, so the CLI redraws for the
                        // client that just attached.
                        unsafe { libc::killpg(pid as libc::pid_t, libc::SIGWINCH) };
                    }
                }
                WATCH => watching.store(data.first() == Some(&1), Ordering::Relaxed),
                // A state hook whose frame came too late for `leading`: it isn't a
                // person attached, so it mustn't hold back notifications.
                STATE => {
                    watching.store(true, Ordering::Relaxed);
                    lock(&hub).hook_report(&data);
                }
                _ => {}
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_clients_get_the_current_screen_not_a_slice_of_output() {
        let mut h = Hub {
            screen: screen::new_parser(4, 20, 0),
            ..Default::default()
        };
        h.screen.process(b"one\r\ntwo\r\n\x1b[2A\r\x1b[Kuno");
        let mut client = screen::new_parser(4, 20, 0);
        client.process(&screen::snapshot(&h.screen));
        assert_eq!(screen::text(&client), ["uno", "two"]);
    }

    #[test]
    fn recognizes_a_leading_watch_frame() {
        assert_eq!(watch_frame(&[WATCH, 0, 0, 0, 1, 1]), Some(true));
        assert_eq!(watch_frame(&[WATCH, 0, 0, 0, 1, 0]), Some(false));
        assert_eq!(watch_frame(&[RESIZE, 0, 0, 0, 4, 0]), None);
        assert_eq!(watch_frame(&[WATCH, 0, 0]), None);
    }
}
