//! The live preview of a run: a terminal emulator (vt100) fed from the run host's socket,
//! exactly as an attached terminal would be. The preview is an ordinary host client that
//! never sends input, so it can't disturb the run. The same connection is reused when you
//! attach in place (see ui/attach.rs), which then paints the emulator's screen at once
//! instead of replaying raw output.

use crate::runs::{run_dir, size_payload, write_frame, Run, RESIZE, WATCH};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::num::NonZeroU16;
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex, MutexGuard};

/// Lines of scrollback kept per preview, for PgUp/PgDn.
const SCROLLBACK: usize = 2_000;
/// How much of output.log a finished run's preview replays.
const LOG_TAIL: u64 = 256 * 1024;

/// Things the emulator reports that the UI shows: the window title the CLI sets (Claude
/// Code and Codex put the task there) and bells (CLIs ring when they need you).
#[derive(Default)]
pub struct Signals {
    pub title: String,
    pub bells: u32,
}

impl vt100::Callbacks for Signals {
    fn audible_bell(&mut self, _: &mut vt100::Screen) {
        self.bells += 1;
    }
    fn set_window_title(&mut self, _: &mut vt100::Screen, title: &[u8]) {
        self.title = String::from_utf8_lossy(title).trim().to_string();
    }
}

pub type Parser = vt100::Parser<Signals>;

/// State shared with the reader thread.
pub struct Shared {
    pub parser: Parser,
    /// While attached in place, output also goes straight to the real terminal.
    pub passthrough: bool,
    /// The host closed the connection: the run ended.
    pub ended: bool,
    /// New output since the UI last drew.
    pub dirty: bool,
}

pub struct Preview {
    pub id: String,
    pub shared: Arc<Mutex<Shared>>,
    /// The socket, for input and resizes. None for a finished run read from its log.
    pub conn: Option<UnixStream>,
    /// Whether the size came from the host's `size` file (exact) or run.json (a guess).
    exact_size: bool,
}

fn nz(n: u16) -> NonZeroU16 {
    NonZeroU16::new(n).unwrap_or(NonZeroU16::MIN)
}

pub fn new_parser(rows: u16, cols: u16) -> Parser {
    vt100::Parser::new_with_callbacks(nz(rows), nz(cols), SCROLLBACK, Signals::default())
}

pub fn parse_size_file(s: &str) -> Option<(u16, u16)> {
    let mut it = s.split_whitespace().map(|n| n.parse::<u16>().ok());
    match (it.next()??, it.next()??) {
        (0, _) | (_, 0) => None,
        size => Some(size),
    }
}

/// The run's terminal size: what its host last published, else what it started with.
/// The bool says whether it is the published (exact) size.
pub fn run_size(run: &Run) -> ((u16, u16), bool) {
    if let Some(s) = std::fs::read_to_string(run.dir().join("size"))
        .ok()
        .and_then(|s| parse_size_file(&s))
    {
        return (s, true);
    }
    let guess = if run.rows > 0 && run.cols > 0 {
        (run.rows, run.cols)
    } else {
        (40, 120)
    };
    (guess, false)
}

/// Connects to a run's socket. Socket paths are limited to about 100 bytes, so a long one
/// is reached by relative name from the run folder, as `amux attach` does.
pub fn connect(id: &str) -> io::Result<UnixStream> {
    let dir = run_dir(id);
    let path = dir.join("sock");
    if path.as_os_str().len() < 100 {
        return UnixStream::connect(path);
    }
    let back = std::env::current_dir()?;
    std::env::set_current_dir(&dir)?;
    let r = UnixStream::connect("sock");
    std::env::set_current_dir(back)?;
    r
}

/// The last `max` bytes of the run's output, across the rotated log too.
fn log_tail(run: &Run, max: u64) -> Vec<u8> {
    let mut out = Vec::new();
    for f in run.log_files().iter().rev() {
        let need = max.saturating_sub(out.len() as u64);
        if need == 0 {
            break;
        }
        let Ok(mut file) = File::open(f) else {
            continue;
        };
        let len = file.metadata().map_or(0, |m| m.len());
        let _ = file.seek(SeekFrom::Start(len.saturating_sub(need)));
        let mut chunk = Vec::new();
        let _ = file.read_to_end(&mut chunk);
        chunk.extend_from_slice(&out);
        out = chunk;
    }
    out
}

impl Preview {
    /// A live preview of a running run. The host first replays its recent output, then
    /// streams; a reader thread feeds both into the emulator.
    pub fn connect(run: &Run) -> io::Result<Preview> {
        let stream = connect(&run.id)?;
        let ((rows, cols), exact_size) = run_size(run);
        let shared = Arc::new(Mutex::new(Shared {
            parser: new_parser(rows, cols),
            passthrough: false,
            ended: false,
            dirty: true,
        }));
        let mut reader = stream.try_clone()?;
        {
            let shared = shared.clone();
            std::thread::spawn(move || {
                let mut buf = [0u8; 16 * 1024];
                loop {
                    let n = match reader.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => n,
                    };
                    let mut s = lock(&shared);
                    s.parser.process(&buf[..n]);
                    s.dirty = true;
                    if s.passthrough {
                        let mut out = io::stdout().lock();
                        if out.write_all(&buf[..n]).and_then(|_| out.flush()).is_err() {
                            s.passthrough = false;
                        }
                    }
                }
                let mut s = lock(&shared);
                s.ended = true;
                s.dirty = true;
            });
        }
        let p = Preview {
            id: run.id.clone(),
            shared,
            conn: Some(stream),
            exact_size,
        };
        // Only watching: the host keeps notifying you while the preview is on screen.
        p.set_watching(true);
        if exact_size {
            // Same size as now, so nothing moves; the CLI just redraws, which repairs a
            // screen whose replayed output started mid-way.
            p.resize(rows, cols);
        }
        Ok(p)
    }

    /// A still picture of a finished run, from the end of its log.
    pub fn from_log(run: &Run) -> Preview {
        let ((rows, cols), exact_size) = run_size(run);
        let mut parser = new_parser(rows, cols);
        parser.process(&log_tail(run, LOG_TAIL));
        Preview {
            id: run.id.clone(),
            shared: Arc::new(Mutex::new(Shared {
                parser,
                passthrough: false,
                ended: true,
                dirty: true,
            })),
            conn: None,
            exact_size,
        }
    }

    pub fn lock(&self) -> MutexGuard<'_, Shared> {
        lock(&self.shared)
    }

    pub fn is_live(&self) -> bool {
        self.conn.is_some() && !self.lock().ended
    }

    /// Follows a size change made by another client (an `amux attach` elsewhere).
    pub fn follow_size(&mut self, run: &Run) {
        let ((rows, cols), exact) = run_size(run);
        if !exact && !self.exact_size {
            return;
        }
        self.exact_size = exact;
        let mut s = self.lock();
        let (r, c) = s.parser.screen().size();
        if (r.get(), c.get()) != (rows, cols) {
            s.parser.set_size(nz(rows), nz(cols));
            s.dirty = true;
        }
    }

    /// Tells the host whether this connection only watches (a preview) or has a person
    /// attached (in place), which decides whether the host notifies you.
    pub fn set_watching(&self, on: bool) {
        if let Some(conn) = &self.conn {
            let mut w = conn;
            let _ = write_frame(&mut w, WATCH, &[u8::from(on)]);
        }
    }

    /// Resizes the run's terminal (which makes the CLI redraw) and the emulator with it.
    pub fn resize(&self, rows: u16, cols: u16) {
        if let Some(conn) = &self.conn {
            let mut w = conn;
            let _ = write_frame(&mut w, RESIZE, &size_payload(rows, cols));
        }
        let mut s = self.lock();
        let (r, c) = s.parser.screen().size();
        if (r.get(), c.get()) != (rows, cols) {
            s.parser.set_size(nz(rows), nz(cols));
        }
    }
}

impl Drop for Preview {
    fn drop(&mut self) {
        // Ends the reader thread; the host forgets this client on its next write.
        if let Some(c) = &self.conn {
            let _ = c.shutdown(std::net::Shutdown::Both);
        }
    }
}

pub fn lock(m: &Mutex<Shared>) -> MutexGuard<'_, Shared> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Previews kept connected, most recently viewed last, so flipping between a few runs is
/// instant. Only these few hold a socket, however many runs there are.
pub struct Previews {
    list: Vec<Preview>,
    cap: usize,
}

impl Previews {
    pub fn new(cap: usize) -> Self {
        Previews {
            list: Vec::new(),
            cap,
        }
    }

    /// The preview for `run`, opened (or reopened, after the run ended) as needed.
    pub fn get(&mut self, run: &Run, live: bool) -> Option<&mut Preview> {
        let pos = self.list.iter().position(|p| p.id == run.id);
        let stale = pos.is_some_and(|i| {
            let p = &self.list[i];
            // A live preview whose run is now over is fine (it shows the last screen); a
            // log preview of a run that is now live is not.
            live && p.conn.is_none()
        });
        let i = match pos {
            Some(i) if !stale => i,
            _ => {
                if let Some(i) = pos {
                    self.list.remove(i);
                }
                let p = if live {
                    Preview::connect(run).unwrap_or_else(|_| Preview::from_log(run))
                } else {
                    Preview::from_log(run)
                };
                self.list.push(p);
                if self.list.len() > self.cap {
                    self.list.remove(0);
                }
                self.list.len() - 1
            }
        };
        let p = self.list.remove(i);
        self.list.push(p);
        self.list.last_mut()
    }

    pub fn find(&self, id: &str) -> Option<&Preview> {
        self.list.iter().find(|p| p.id == id)
    }

    pub fn forget(&mut self, id: &str) {
        self.list.retain(|p| p.id != id);
    }

    /// Whether any preview has unseen output, and clears the marks.
    pub fn take_dirty(&self) -> bool {
        let mut any = false;
        for p in &self.list {
            let mut s = p.lock();
            any |= std::mem::take(&mut s.dirty);
        }
        any
    }
}

/// The first screen row to show when only `height` of `total` rows fit: keep the bottom
/// of the content (or the cursor, if lower) in view, since that is where a CLI asks you
/// things.
pub fn window_start(total: u16, height: u16, bottom: u16) -> u16 {
    if total <= height {
        return 0;
    }
    let end = bottom.clamp(height, total);
    end - height
}

/// One past the last row with any content, or below the cursor when it is visible.
fn content_bottom(screen: &vt100::Screen) -> u16 {
    let (rows, cols) = screen.size();
    let mut bottom = 0;
    for r in (0..rows.get()).rev() {
        if (0..cols.get()).any(|c| screen.cell(r, c).is_some_and(|x| x.has_contents())) {
            bottom = r + 1;
            break;
        }
    }
    if !screen.hide_cursor() {
        bottom = bottom.max(screen.cursor_position().0 + 1);
    }
    bottom
}

fn color(c: vt100::Color) -> Color {
    match c {
        vt100::Color::Default => Color::Reset,
        vt100::Color::Idx(i) => Color::Indexed(i),
        vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}

fn cell_style(cell: &vt100::Cell) -> Style {
    let mut s = Style::default()
        .fg(color(cell.fgcolor()))
        .bg(color(cell.bgcolor()));
    let mut m = Modifier::empty();
    if cell.bold() {
        m |= Modifier::BOLD;
    }
    if cell.dim() {
        m |= Modifier::DIM;
    }
    if cell.italic() {
        m |= Modifier::ITALIC;
    }
    if cell.underline() {
        m |= Modifier::UNDERLINED;
    }
    if cell.inverse() {
        m |= Modifier::REVERSED;
    }
    s = s.add_modifier(m);
    s
}

/// The part of the screen that fits in `height` x `width`, as styled lines. When the
/// view is scrolled back (`Screen::scrollback() > 0`) the top of the view is shown;
/// otherwise the bottom of the content, with the cursor drawn.
pub fn screen_lines(screen: &vt100::Screen, height: u16, width: u16) -> Vec<Line<'static>> {
    let (rows, cols) = screen.size();
    let (rows, cols) = (rows.get(), cols.get());
    let start = if screen.scrollback() > 0 {
        0
    } else {
        window_start(rows, height, content_bottom(screen))
    };
    let cursor =
        (!screen.hide_cursor() && screen.scrollback() == 0).then(|| screen.cursor_position());
    let mut lines = Vec::new();
    for r in start..rows.min(start.saturating_add(height)) {
        let mut spans: Vec<Span<'static>> = Vec::new();
        let (mut text, mut style) = (String::new(), Style::default());
        for c in 0..cols.min(width) {
            let Some(cell) = screen.cell(r, c) else {
                continue;
            };
            if cell.is_wide_continuation() {
                continue;
            }
            let mut st = cell_style(cell);
            if cursor == Some((r, c)) {
                st = st.add_modifier(Modifier::REVERSED);
            }
            // A wide character that would be cut by the edge is left out.
            let sym = if cell.is_wide() && c + 1 >= width {
                " "
            } else if cell.has_contents() {
                cell.contents()
            } else {
                " "
            };
            if st != style && !text.is_empty() {
                spans.push(Span::styled(std::mem::take(&mut text), style));
            }
            style = st;
            text.push_str(sym);
        }
        if !text.is_empty() {
            spans.push(Span::styled(text, style));
        }
        lines.push(Line::from(spans));
    }
    lines
}

/// Plain text of the lines, trailing blanks trimmed.
#[cfg(test)]
pub fn plain(lines: &[Line]) -> Vec<String> {
    lines
        .iter()
        .map(|l| {
            l.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_size_file() {
        assert_eq!(parse_size_file("40 120\n"), Some((40, 120)));
        assert_eq!(parse_size_file("0 120"), None);
        assert_eq!(parse_size_file("40"), None);
        assert_eq!(parse_size_file("x y"), None);
    }

    #[test]
    fn window_keeps_the_bottom_of_the_content() {
        assert_eq!(window_start(10, 20, 3), 0);
        assert_eq!(window_start(50, 10, 50), 40);
        assert_eq!(window_start(50, 10, 30), 20);
        // Content shorter than the window starts at the top.
        assert_eq!(window_start(50, 10, 4), 0);
    }

    #[test]
    fn renders_the_screen_cropped_to_the_pane() {
        let mut p = new_parser(6, 20);
        p.process(b"\x1b]0;my task\x07one\r\ntwo \x1b[1;31mred\x1b[0m\r\nthree\x07");
        assert_eq!(p.callbacks().title, "my task");
        assert_eq!(p.callbacks().bells, 1);
        let lines = screen_lines(p.screen(), 2, 6);
        // The bottom of the content (two, three) fits; the cursor ends "three".
        assert_eq!(plain(&lines), ["two re", "three"]);
        let red = lines[0].spans.iter().find(|s| s.content.contains("re"));
        assert_eq!(red.unwrap().style.fg, Some(Color::Indexed(1)));
        let all = plain(&screen_lines(p.screen(), 10, 40));
        assert_eq!(all[..3], ["one", "two red", "three"]);
        assert_eq!(all.len(), 6);
    }

    #[test]
    fn scrolled_back_view_shows_history() {
        let mut p = new_parser(2, 10);
        p.process(b"a\r\nb\r\nc\r\nd");
        p.screen_mut().set_scrollback(2);
        assert_eq!(plain(&screen_lines(p.screen(), 2, 10)), ["a", "b"]);
    }
}
