//! `amux attach`: connects the terminal to a run's host. Keystrokes go to the CLI and its
//! output comes back; Ctrl-] detaches and leaves the run going.

use super::{load, run_dir, size_payload, term_size, write_frame, Run, INPUT, RESIZE};
use anyhow::{anyhow, Result};
use std::io::{self, IsTerminal, Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

/// Ctrl-]
pub const DETACH_KEY: u8 = 0x1d;
/// Ctrl-] as the kitty keyboard protocol sends it, once a CLI has turned that on: plain,
/// and with the event type that "report event types" adds.
const DETACH_KITTY: [&[u8]; 2] = [b"\x1b[93;5u", b"\x1b[93;5:1u"];

/// Where the detach key starts in `input`, if it is there.
pub fn find_detach(input: &[u8]) -> Option<usize> {
    let plain = input.iter().position(|&b| b == DETACH_KEY);
    let kitty = DETACH_KITTY
        .iter()
        .filter_map(|k| input.windows(k.len()).position(|w| w == *k));
    plain.into_iter().chain(kitty).min()
}

/// Puts the terminal in raw mode until dropped.
struct RawMode(libc::termios);

impl RawMode {
    fn enable() -> io::Result<Self> {
        unsafe {
            let mut t: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(0, &mut t) != 0 {
                return Err(io::Error::last_os_error());
            }
            let orig = t;
            libc::cfmakeraw(&mut t);
            if libc::tcsetattr(0, libc::TCSANOW, &t) != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(RawMode(orig))
        }
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        unsafe { libc::tcsetattr(0, libc::TCSANOW, &self.0) };
    }
}

/// Undoes terminal modes a full-screen CLI may have left on when we detach mid-screen:
/// alternate screen, hidden cursor, mouse reporting, bracketed paste, focus events, and
/// the kitty keyboard protocol and modifyOtherKeys (terminals without them ignore these).
pub const RESET_MODES: &str = "\x1b[?1049l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?2004l\x1b[?1004l\x1b[<u\x1b[>4;0m\x1b[0m";

enum Event {
    Detached,
    Ended,
}

pub fn attach(run: &Run) -> Result<i32> {
    // The host binds by relative name (see host.rs), so connect the same way.
    std::env::set_current_dir(run_dir(&run.id))?;
    let stream =
        UnixStream::connect("sock").map_err(|_| anyhow!("Run {} is not running.", run.id))?;
    let tty = io::stdin().is_terminal() && io::stdout().is_terminal();
    let size = term_size().unwrap_or((24, 80));
    let writer = Arc::new(Mutex::new(stream.try_clone()?));
    write_frame(
        &mut *writer.lock().unwrap(),
        RESIZE,
        &size_payload(size.0, size.1),
    )?;

    if tty {
        eprintln!("amux: attached to {} (Ctrl-] to detach)", run.id);
    }
    let raw = if tty { Some(RawMode::enable()?) } else { None };
    let (tx, rx) = mpsc::channel::<Event>();

    {
        let tx = tx.clone();
        let mut stream = stream;
        std::thread::spawn(move || {
            let mut out = io::stdout();
            let mut buf = [0u8; 16 * 1024];
            loop {
                match stream.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if out.write_all(&buf[..n]).and_then(|_| out.flush()).is_err() {
                            break;
                        }
                    }
                }
            }
            let _ = tx.send(Event::Ended);
        });
    }
    {
        let tx = tx.clone();
        let writer = writer.clone();
        std::thread::spawn(move || {
            let mut input = io::stdin();
            let mut buf = [0u8; 4096];
            loop {
                let n = match input.read(&mut buf) {
                    Ok(0) | Err(_) => return, // stdin closed: keep showing output
                    Ok(n) => n,
                };
                let mut chunk = &buf[..n];
                let cut = if tty { find_detach(chunk) } else { None };
                let detach = cut.is_some();
                if let Some(i) = cut {
                    chunk = &chunk[..i];
                }
                if !chunk.is_empty() {
                    let mut w = writer.lock().unwrap_or_else(|e| e.into_inner());
                    if write_frame(&mut *w, INPUT, chunk).is_err() {
                        return;
                    }
                }
                if detach {
                    let _ = tx.send(Event::Detached);
                    return;
                }
            }
        });
    }
    if tty {
        let writer = writer.clone();
        std::thread::spawn(move || {
            let mut last = size;
            loop {
                std::thread::sleep(Duration::from_millis(200));
                match term_size() {
                    Some(now) if now != last => {
                        last = now;
                        let mut w = writer.lock().unwrap_or_else(|e| e.into_inner());
                        if write_frame(&mut *w, RESIZE, &size_payload(now.0, now.1)).is_err() {
                            return;
                        }
                    }
                    _ => {}
                }
            }
        });
    }
    drop(tx);

    let event = rx.recv().unwrap_or(Event::Ended);
    drop(raw);
    if tty {
        let mut out = io::stdout();
        let _ = out.write_all(format!("{RESET_MODES}\r\n").as_bytes());
        let _ = out.flush();
    }
    match event {
        Event::Detached => {
            eprintln!(
                "amux: detached from {0}. It keeps running; `amux attach {0}` to return.",
                run.id
            );
            Ok(0)
        }
        Event::Ended => {
            // The host records the exit code just before closing the socket.
            let deadline = Instant::now() + Duration::from_secs(2);
            let mut latest = load(&run.id).ok();
            while latest
                .as_ref()
                .is_some_and(|r| r.exit_code.is_none() && r.error.is_none())
                && Instant::now() < deadline
            {
                std::thread::sleep(Duration::from_millis(50));
                latest = load(&run.id).ok();
            }
            let code = latest.as_ref().and_then(|r| r.exit_code).unwrap_or(1);
            eprintln!("amux: {} exited with code {code}", run.id);
            Ok(code)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_detach_key_plain_or_kitty_encoded() {
        assert_eq!(find_detach(b"ls\r"), None);
        assert_eq!(find_detach(b"ab\x1dcd"), Some(2));
        assert_eq!(find_detach(b"x\x1b[93;5u"), Some(1));
        assert_eq!(find_detach(b"\x1b[93;5:1u\x1d"), Some(0));
        // Other kitty keys are not it.
        assert_eq!(find_detach(b"\x1b[93;3u\x1b[97;5u"), None);
    }
}
