//! `amux __run-host <id>`: owns a run's pseudo-terminal. It starts the CLI on the
//! terminal's slave side, copies everything the CLI prints to output.log and to attached
//! clients, and passes client keystrokes and window sizes back to the CLI. It exits when
//! the CLI does, after recording the exit code.

use super::detect::{Change, Detector};
use super::notify::Notifier;
use super::{load, parse_size, read_frame, run_dir, save, Run, Waiting, INPUT, RESIZE, WATCH};
use crate::{fsx, time};
use anyhow::{anyhow, Result};
use std::collections::VecDeque;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

/// Output kept in memory and replayed to a client when it attaches.
const RECENT: usize = 256 * 1024;
const LOG_ROTATE: u64 = 4 * 1024 * 1024;

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
#[derive(Default)]
struct Hub {
    clients: Vec<Client>,
    recent: VecDeque<u8>,
    trimmed: bool,
    detector: Detector,
    notifier: Notifier,
    /// The host's copy of run.json, which it alone writes while the run is alive.
    run: Run,
}

impl Hub {
    /// Whether a terminal is attached, so you already see what the CLI shows.
    fn attached(&self) -> bool {
        self.clients
            .iter()
            .any(|c| !c.watching.load(Ordering::Relaxed))
    }

    fn title(&self) -> String {
        format!("amux: {} ({})", self.run.id, self.run.account)
    }

    /// Records a change in whether the CLI is waiting for you, and notifies you when it
    /// starts waiting and no terminal is attached to see it.
    fn apply(&mut self, change: Option<Change>) {
        match change {
            None => return,
            Some(Change::Waiting(reason)) => {
                if !self.attached() {
                    let msg = format!("Waiting for you: {reason}");
                    self.notifier
                        .send(&self.run.id, "waiting", &self.title(), &msg);
                }
                self.run.waiting = Some(Waiting {
                    reason,
                    since: time::now_ms(),
                });
            }
            Some(Change::Cleared) => self.run.waiting = None,
        }
        let _ = save(&self.run);
    }
}

fn host(id: &str) -> Result<i32> {
    let mut run = load(id)?;
    let dir = run_dir(id);
    // Bind by relative name: Unix socket paths are limited to about 100 bytes, and
    // AMUX_HOME can be longer than that. The CLI gets its own cwd below.
    std::env::set_current_dir(&dir)?;
    let _ = fs::remove_file("sock");
    let listener = UnixListener::bind("sock")?;

    let (master, slave) = openpty(run.rows.max(1), run.cols.max(1))?;
    record_size(run.rows.max(1), run.cols.max(1));
    let mut cmd = Command::new(&run.bin);
    cmd.args(&run.args)
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
            anyhow!("\"{}\" not found on PATH.", run.bin)
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
        detector: Detector::new(&run.waiting_patterns),
        notifier: Notifier::from_config(),
        run: run.clone(),
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
    {
        let (master, hub) = (master.clone(), hub.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                serve(stream, &hub, &master, pid);
            }
        });
    }

    let status = child.wait()?;
    let code = status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(0));
    // Let the last output reach the log and clients. A process the CLI left behind can
    // keep the terminal open, so don't wait for that.
    let _ = done_rx.recv_timeout(Duration::from_millis(500));
    let mut h = hub.lock().unwrap_or_else(|e| e.into_inner());
    h.run.exit_code = Some(code);
    h.run.ended_at = Some(time::now_ms());
    h.run.waiting = None;
    save(&h.run)?;
    let _ = fs::remove_file("sock");
    if !h.attached() {
        let msg = format!("Exited with code {code}");
        h.notifier.send(&h.run.id, "exited", &h.title(), &msg);
    }
    for c in &h.clients {
        let _ = c.stream.shutdown(std::net::Shutdown::Both);
    }
    Ok(code)
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
        h.recent.extend(chunk);
        if h.recent.len() > RECENT {
            let over = h.recent.len() - RECENT;
            h.recent.drain(..over);
            h.trimmed = true;
        }
        // A client that can't keep up (write timeout) or has gone is dropped.
        h.clients.retain_mut(|c| c.stream.write_all(chunk).is_ok());
        let change = h.detector.feed(chunk);
        h.apply(change);
    }
}

/// What a new client sees first: the recent output, starting at a line boundary when
/// older output was dropped, so it doesn't begin in the middle of an escape sequence.
fn replay(h: &Hub) -> Vec<u8> {
    let data: Vec<u8> = h.recent.iter().copied().collect();
    if !h.trimmed {
        return data;
    }
    match data.iter().position(|&b| b == b'\n') {
        Some(i) => data[i + 1..].to_vec(),
        None => vec![],
    }
}

fn serve(stream: UnixStream, hub: &Arc<Mutex<Hub>>, master: &Arc<File>, pid: u32) {
    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
    let Ok(mut out) = stream.try_clone() else {
        return;
    };
    let watching = Arc::new(AtomicBool::new(false));
    {
        let mut h = hub.lock().unwrap_or_else(|e| e.into_inner());
        if out.write_all(&replay(&h)).is_err() {
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
        while let Ok((kind, data)) = read_frame(&mut stream) {
            match kind {
                INPUT => {
                    if (&*master).write_all(&data).is_err() {
                        break;
                    }
                    let mut h = hub.lock().unwrap_or_else(|e| e.into_inner());
                    let change = h.detector.answered();
                    h.apply(change);
                }
                RESIZE => {
                    if let Some((rows, cols)) = parse_size(&data) {
                        set_size(master.as_raw_fd(), rows.max(1), cols.max(1));
                        record_size(rows.max(1), cols.max(1));
                        // Also when the size didn't change, so the CLI redraws for the
                        // client that just attached.
                        unsafe { libc::killpg(pid as libc::pid_t, libc::SIGWINCH) };
                    }
                }
                WATCH => watching.store(data.first() == Some(&1), Ordering::Relaxed),
                _ => {}
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replays_from_a_line_boundary_once_trimmed() {
        let mut h = Hub::default();
        h.recent.extend(b"\x1b[3partial\nline two\n");
        assert_eq!(replay(&h), b"\x1b[3partial\nline two\n");
        h.trimmed = true;
        assert_eq!(replay(&h), b"line two\n");
    }
}
