//! Attaching in place: the UI hands the whole terminal to the selected run and takes it
//! back on Ctrl-]. It reuses the preview's connection, so there is no reconnect and no
//! replay: the emulator's current screen is painted at once, then output streams through.

use super::preview::{lock, Preview};
use crate::runs::attach::{DETACH_KEY, RESET_MODES};
use crate::runs::{term_size, write_frame, INPUT};
use std::io::{self, Write};

pub enum Outcome {
    /// Ctrl-]: back to the UI, the run keeps going.
    Detached,
    /// The run ended while attached.
    Ended,
}

/// Modes some CLIs turn on that `RESET_MODES` doesn't cover: the kitty keyboard protocol
/// and xterm's modifyOtherKeys. Left on, the UI would get keys it can't read.
const RESET_KEYBOARD: &str = "\x1b[<u\x1b[>4;0m";

/// Runs the attached session. The caller has left the UI's alternate screen; raw mode is
/// still on from the UI.
pub fn attach_in_place(p: &Preview) -> io::Result<Outcome> {
    let Some(conn) = &p.conn else {
        return Ok(Outcome::Ended);
    };
    {
        let mut s = p.lock();
        if s.ended {
            return Ok(Outcome::Ended);
        }
        // Paint what the run shows now, under the lock so no output is lost or doubled
        // between the picture and the stream.
        let mut out = io::stdout().lock();
        out.write_all(b"\x1b[0m\x1b[H\x1b[2J")?;
        out.write_all(s.parser.screen().state_formatted().as_bytes())?;
        out.flush()?;
        s.passthrough = true;
    }
    p.set_watching(false);
    let mut size = term_size().unwrap_or((24, 80));
    // Fits the CLI to this terminal; it redraws (SIGWINCH) even if the size is the same.
    p.resize(size.0, size.1);

    let outcome = loop {
        if lock(&p.shared).ended {
            break Outcome::Ended;
        }
        if let Some(now) = term_size().filter(|s| *s != size) {
            size = now;
            p.resize(size.0, size.1);
        }
        let mut buf = [0u8; 4096];
        let n = match read_stdin(&mut buf, 100)? {
            Some(0) => break Outcome::Ended,
            Some(n) => n,
            None => continue,
        };
        let (chunk, detach) = split_detach(&buf[..n]);
        if !chunk.is_empty() {
            let mut w = conn;
            if write_frame(&mut w, INPUT, chunk).is_err() {
                break Outcome::Ended;
            }
        }
        if detach {
            break Outcome::Detached;
        }
    };

    lock(&p.shared).passthrough = false;
    p.set_watching(true);
    let mut out = io::stdout().lock();
    out.write_all(RESET_MODES.as_bytes())?;
    out.write_all(RESET_KEYBOARD.as_bytes())?;
    out.flush()?;
    Ok(outcome)
}

/// The input before the detach key, and whether the key was pressed.
pub fn split_detach(input: &[u8]) -> (&[u8], bool) {
    match input.iter().position(|&b| b == DETACH_KEY) {
        Some(i) => (&input[..i], true),
        None => (input, false),
    }
}

/// Reads stdin if it has input within `timeout_ms`. None on timeout.
fn read_stdin(buf: &mut [u8], timeout_ms: i32) -> io::Result<Option<usize>> {
    let mut fds = libc::pollfd {
        fd: 0,
        events: libc::POLLIN,
        revents: 0,
    };
    let r = unsafe { libc::poll(&mut fds, 1, timeout_ms) };
    if r < 0 {
        let e = io::Error::last_os_error();
        return if e.kind() == io::ErrorKind::Interrupted {
            Ok(None)
        } else {
            Err(e)
        };
    }
    if r == 0 {
        return Ok(None);
    }
    // Straight from the descriptor: std's buffered Stdin could hold bytes poll can't see.
    let n = unsafe { libc::read(0, buf.as_mut_ptr().cast(), buf.len()) };
    if n < 0 {
        let e = io::Error::last_os_error();
        return match e.kind() {
            io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock => Ok(None),
            _ => Err(e),
        };
    }
    Ok(Some(n as usize))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stops_input_at_the_detach_key() {
        assert_eq!(split_detach(b"ls\r"), (&b"ls\r"[..], false));
        assert_eq!(split_detach(b"ab\x1dcd"), (&b"ab"[..], true));
        assert_eq!(split_detach(b"\x1d"), (&b""[..], true));
    }
}
