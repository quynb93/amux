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

/// The window title the CLI last set. The screen itself doesn't hold it, so a snapshot
/// would otherwise lose it (the UI shows it on the preview's border).
#[derive(Default)]
pub struct Title(pub String);

impl vt100::Callbacks for Title {
    fn set_window_title(&mut self, _: &mut vt100::Screen, title: &[u8]) {
        self.0 = String::from_utf8_lossy(title).into_owned();
    }
}

pub type Parser = vt100::Parser<Title>;

/// Escape sequences that recreate the screen on a terminal of the same size: set the
/// window title, clear the screen, draw every cell with its attributes, put the cursor
/// back, and restore the input modes the CLI turned on (bracketed paste, mouse
/// reporting, application keys).
pub fn snapshot(p: &Parser) -> Vec<u8> {
    let screen = p.screen();
    let mut out = Vec::new();
    let title: String = p
        .callbacks()
        .0
        .chars()
        .filter(|c| !c.is_control())
        .collect();
    if !title.is_empty() {
        out.extend_from_slice(format!("\x1b]0;{title}\x07").as_bytes());
    }
    if screen.alternate_screen() {
        out.extend_from_slice(b"\x1b[?1049h");
    }
    out.extend_from_slice(screen.state_formatted().as_bytes());
    out
}

/// How much of the latest output the host keeps for `unfinished`: enough for any escape
/// sequence a CLI sends mid-stream, such as a window title.
pub const TAIL: usize = 4096;

/// The end of `out` that starts an escape sequence or UTF-8 character but doesn't finish
/// it, or nothing. A snapshot shows the screen as of the end of `out`; a client that gets
/// only the snapshot and then the next chunk would read the rest of a cut-off sequence
/// as text (and miss the cursor move it was).
pub fn unfinished(out: &[u8]) -> &[u8] {
    // A cut-off UTF-8 character: up to three bytes after its lead byte.
    let utf8 = {
        let n = out.len();
        let mut cut = None;
        for back in 1..=n.min(3) {
            let b = out[n - back];
            if b & 0xC0 == 0x80 {
                continue;
            }
            let need = match b {
                0xC0..=0xDF => 2,
                0xE0..=0xEF => 3,
                0xF0..=0xF7 => 4,
                _ => 0,
            };
            if need > back {
                cut = Some(n - back);
            }
            break;
        }
        cut
    };
    let Some(esc) = out.iter().rposition(|&b| b == 0x1b) else {
        return utf8.map_or(&[], |i| &out[i..]);
    };
    let seq = &out[esc..];
    let done = match seq.get(1) {
        None => false,
        // CSI: parameters and intermediates, then one final byte.
        Some(b'[') => seq[2..].iter().any(|b| (0x40..=0x7e).contains(b)),
        // OSC, DCS and other strings: until BEL or ESC \ (whose ESC would be the last one).
        Some(b']' | b'P' | b'X' | b'^' | b'_') => seq[2..].contains(&0x07),
        // Character sets and the like: ESC, intermediates (0x20–0x2F), one final byte.
        Some(_) => seq[1..].iter().any(|b| (0x30..=0x7e).contains(b)),
    };
    if !done {
        return seq;
    }
    utf8.map_or(&[], |i| &out[i..])
}

/// A terminal dimension, at least 1.
pub fn nz(n: u16) -> NonZeroU16 {
    NonZeroU16::new(n).unwrap_or(NonZeroU16::MIN)
}

pub fn new_parser(rows: u16, cols: u16, scrollback: usize) -> Parser {
    vt100::Parser::new_with_callbacks(nz(rows), nz(cols), scrollback, Title::default())
}

/// The size the screen was last drawn at: the host's `size` file, else the start size.
pub fn last_size(run: &Run) -> (u16, u16) {
    super::client::run_size(run).0
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
    let stream = super::client::connect(&run.id).ok()?;
    let mut w = &stream;
    super::write_frame(&mut w, super::WATCH, &[1]).ok()?;
    let (rows, cols) = last_size(run);
    let mut p = new_parser(rows, cols, 0);
    // The host sends the snapshot first, though it may be held up for a moment (it writes
    // to every client under the same lock); then stop when the connection goes quiet.
    stream.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut buf = [0u8; 64 * 1024];
    let mut r = &stream;
    while Instant::now() < deadline {
        match r.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => p.process(&buf[..n]),
        }
        stream
            .set_read_timeout(Some(Duration::from_millis(150)))
            .ok()?;
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
    fn finds_what_a_chunk_cut_off() {
        assert_eq!(unfinished(b"text"), b"");
        assert_eq!(unfinished(b"x\x1b[2K\r\x1b[1"), b"\x1b[1");
        assert_eq!(unfinished(b"x\x1b[1A\r"), b"");
        assert_eq!(unfinished(b"x\x1b"), b"\x1b");
        assert_eq!(unfinished(b"x\x1b]0;tit"), b"\x1b]0;tit");
        assert_eq!(unfinished(b"x\x1b]0;title\x07"), b"");
        assert_eq!(unfinished(b"x\x1b(B"), b"");
        assert_eq!(unfinished(b"x\x1b("), b"\x1b(");
        assert_eq!(unfinished("é".as_bytes()), b"");
        assert_eq!(unfinished(&"aé".as_bytes()[..2]), &"é".as_bytes()[..1]);
        assert_eq!(unfinished(&"a→".as_bytes()[..3]), &"→".as_bytes()[..2]);
    }

    #[test]
    fn a_snapshot_plus_the_cut_off_start_draws_the_same_screen() {
        // The case that broke: a cursor-up cut after "ESC[1".
        let all: &[u8] = b"top\r\nline 1\r\n  sub\x1b[1A\rdone\r\n";
        for cut in 0..all.len() {
            let (a, b) = all.split_at(cut);
            let mut host = new_parser(6, 20, 0);
            host.process(a);
            let mut client = new_parser(6, 20, 0);
            client.process(&snapshot(&host));
            client.process(unfinished(a));
            client.process(b);
            let mut whole = new_parser(6, 20, 0);
            whole.process(all);
            assert_eq!(text(&client), text(&whole), "cut at {cut}");
        }
    }

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

    #[test]
    fn a_snapshot_keeps_the_window_title() {
        let mut live = new_parser(3, 20, 0);
        live.process(b"\x1b]0;my task\x07hi");
        let mut copy = new_parser(3, 20, 0);
        copy.process(&snapshot(&live));
        assert_eq!(copy.callbacks().0, "my task");
        assert_eq!(text(&copy), ["hi"]);
    }
}
