//! A run's screen as the CLI drew it. The run host keeps a terminal emulator fed with
//! everything the CLI prints, so it can hand a new client the exact current screen
//! instead of replaying a slice of raw output. Replaying a slice goes wrong because
//! full-screen CLIs redraw with relative cursor moves, and a slice can start anywhere.
//! When the CLI exits, the host saves the final screen as `screen.ans`.

use super::{Run, State};
use std::fs::File;
use std::io::Read;
use std::num::NonZeroU16;
use std::time::{Duration, Instant};

pub type Parser = vt100::Parser;

/// Escape sequences that recreate the screen on a terminal of the same size: clear it,
/// draw every cell with its attributes, put the cursor back, and restore the input modes
/// the CLI turned on (bracketed paste, mouse reporting, application keys).
pub fn snapshot(p: &Parser) -> Vec<u8> {
    let screen = p.screen();
    let mut out = Vec::new();
    if screen.alternate_screen() {
        out.extend_from_slice(b"\x1b[?1049h");
    }
    out.extend_from_slice(screen.state_formatted().as_bytes());
    out
}

pub fn new_parser(rows: u16, cols: u16, scrollback: usize) -> Parser {
    let nz = |n: u16| NonZeroU16::new(n).unwrap_or(NonZeroU16::MIN);
    vt100::Parser::new(nz(rows), nz(cols), scrollback)
}

/// The size the screen was last drawn at: the host's `size` file, else the start size.
pub fn last_size(run: &Run) -> (u16, u16) {
    std::fs::read_to_string(run.dir().join("size"))
        .ok()
        .and_then(|s| {
            let mut it = s.split_whitespace().map(|n| n.parse::<u16>().ok());
            match (it.next()??, it.next()??) {
                (r, c) if r > 0 && c > 0 => Some((r, c)),
                _ => None,
            }
        })
        .unwrap_or(if run.rows > 0 && run.cols > 0 {
            (run.rows, run.cols)
        } else {
            (24, 80)
        })
}

/// What draws the final screen of a run that has ended, and the size to draw it at.
/// That is the host's saved `screen.ans` when there is one. Runs from older hosts don't
/// have it, so their whole log is replayed from the start, at the size the run started
/// with: that redraws correctly unless the log was rotated or the terminal resized.
pub fn ended(run: &Run) -> ((u16, u16), Vec<u8>) {
    if let Ok(saved) = std::fs::read(run.dir().join("screen.ans")) {
        return (last_size(run), saved);
    }
    let size = if run.rows > 0 && run.cols > 0 {
        (run.rows, run.cols)
    } else {
        last_size(run)
    };
    let mut out = Vec::new();
    for f in run.log_files() {
        if let Ok(mut file) = File::open(&f) {
            let _ = file.read_to_end(&mut out);
        }
    }
    (size, out)
}

/// The run's screen now: from its host while it runs, else its final screen.
pub fn capture(run: &Run) -> Parser {
    if matches!(run.state(), State::Running | State::Starting) {
        if let Some(p) = live(run) {
            return p;
        }
    }
    let ((rows, cols), bytes) = ended(run);
    let mut p = new_parser(rows, cols, 0);
    p.process(&bytes);
    p
}

/// Connects as a watcher and draws what the host sends first: its current screen.
fn live(run: &Run) -> Option<Parser> {
    let stream = crate::ui::preview::connect(&run.id).ok()?;
    let mut w = &stream;
    super::write_frame(&mut w, super::WATCH, &[1]).ok()?;
    let (rows, cols) = last_size(run);
    let mut p = new_parser(rows, cols, 0);
    // The snapshot comes at once; stop when the connection goes quiet.
    stream
        .set_read_timeout(Some(Duration::from_millis(150)))
        .ok()?;
    let deadline = Instant::now() + Duration::from_secs(1);
    let mut buf = [0u8; 64 * 1024];
    let mut r = &stream;
    while Instant::now() < deadline {
        match r.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => p.process(&buf[..n]),
        }
    }
    Some(p)
}

/// The screen as plain text, without trailing blank lines or trailing spaces.
pub fn text(p: &Parser) -> Vec<String> {
    let mut lines: Vec<String> = p
        .screen()
        .contents()
        .lines()
        .map(|l| l.trim_end().to_string())
        .collect();
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_snapshot_recreates_a_screen_drawn_with_relative_moves() {
        // Draw, then redraw the last two lines in place the way TUIs do.
        let mut live = new_parser(6, 30, 0);
        live.process(b"header\r\nworking...\r\nstep 1\r\n");
        live.process(b"\x1b[2A\r\x1b[Kdone.\r\n\x1b[Kstep 2\r\n");
        let mut copy = new_parser(6, 30, 0);
        copy.process(b"old junk that must be cleared\r\n");
        copy.process(&snapshot(&live));
        assert_eq!(text(&copy), ["header", "done.", "step 2"]);
        assert_eq!(
            copy.screen().cursor_position(),
            live.screen().cursor_position()
        );
    }

    #[test]
    fn a_snapshot_keeps_the_alternate_screen_and_input_modes() {
        let mut live = new_parser(5, 20, 0);
        live.process(b"\x1b[?1049h\x1b[?2004hfull screen");
        let mut copy = new_parser(5, 20, 0);
        copy.process(&snapshot(&live));
        assert!(copy.screen().alternate_screen());
        assert!(copy.screen().bracketed_paste());
        assert_eq!(text(&copy), ["full screen"]);
    }
}
