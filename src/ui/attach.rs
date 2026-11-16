//! Attaching in place: the UI hands the whole terminal to the selected run and takes it
//! back on Ctrl-]. It reuses the preview's connection, so there is no reconnect and no
//! replay: the emulator's current screen is painted at once, then output streams through.

use super::preview::{lock, Preview};
use crate::runs::attach::{find_detach, RESET_MODES};
use crate::runs::{term_size, write_frame, INPUT};
use std::io::{self, Write};

pub enum Outcome {
    /// Ctrl-]: back to the UI, the run keeps going.
    Detached,
    /// The run ended while attached.
    Ended,
}

/// Hands the terminal back to the UI however attaching ends, errors included: stops
/// passing output through, marks the connection as watching again, and resets modes.
struct Restore<'a>(&'a Preview);

impl Drop for Restore<'_> {
    fn drop(&mut self) {
        lock(&self.0.shared).passthrough = false;
        self.0.set_watching(true);
        let mut out = io::stdout().lock();
        let _ = out.write_all(RESET_MODES.as_bytes());
        let _ = out.flush();
    }
}

/// Runs the attached session. The caller has left the UI's alternate screen; raw mode is
/// still on from the UI.
pub fn attach_in_place(p: &Preview) -> io::Result<Outcome> {
    let Some(conn) = &p.conn else {
        return Ok(Outcome::Ended);
    };
    let _restore = Restore(p);
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

    loop {
        if lock(&p.shared).ended {
            return Ok(Outcome::Ended);
        }
        if let Some(now) = term_size().filter(|s| *s != size) {
            size = now;
            p.resize(size.0, size.1);
        }
        let mut buf = [0u8; 4096];
        let n = match read_stdin(&mut buf, 100)? {
            Some(0) => return Ok(Outcome::Ended),
            Some(n) => n,
            None => continue,
        };
        let input = &buf[..n];
        let cut = find_detach(input);
        let chunk = &input[..cut.unwrap_or(n)];
        if !chunk.is_empty() {
            let mut w = conn;
            if write_frame(&mut w, INPUT, chunk).is_err() {
                return Ok(Outcome::Ended);
            }
        }
        if cut.is_some() {
            return Ok(Outcome::Detached);
        }
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
