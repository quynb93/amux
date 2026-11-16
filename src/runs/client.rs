//! Talking to a run's host from outside it: finding its socket, its terminal size, and
//! typing into it. `amux send`, the review and verify features and `amux ui` all go
//! through here.

use super::{run_dir, write_frame, Run, State, INPUT, WATCH};
use anyhow::{anyhow, bail, Result};
use std::io;
use std::os::unix::net::UnixStream;
use std::time::Duration;

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
/// is reached through a short symlink in a private temporary folder. Unlike `amux attach`,
/// the UI never changes its working directory: background `amux spawn`s inherit it.
pub fn connect(id: &str) -> io::Result<UnixStream> {
    // `<machine>:<run>`: through SSH. Local run names never contain a colon.
    if let Some((m, run)) = crate::remote::split(id) {
        return m.connect(&run);
    }
    let path = run_dir(id).join("sock");
    if path.as_os_str().len() < 100 {
        return UnixStream::connect(path);
    }
    let link_dir = short_dir()?;
    let link = link_dir.join("s");
    let r = std::os::unix::fs::symlink(&path, &link).and_then(|_| UnixStream::connect(&link));
    let _ = std::fs::remove_file(&link);
    let _ = std::fs::remove_dir(&link_dir);
    r
}

/// A new mode-0700 folder under /tmp, whose path is short whatever $TMPDIR is.
fn short_dir() -> io::Result<std::path::PathBuf> {
    let mut template = *b"/tmp/amux-XXXXXX\0";
    if unsafe { libc::mkdtemp(template.as_mut_ptr().cast()) }.is_null() {
        return Err(io::Error::last_os_error());
    }
    let s = std::str::from_utf8(&template[..template.len() - 1]).map_err(io::Error::other)?;
    Ok(s.into())
}

/// Removes what could act as a terminal command rather than text: escape sequences, other
/// control characters, and so a prompt can never end a bracketed paste early and have
/// the rest read as keystrokes. Newlines and tabs stay.
pub fn sanitize(text: &str) -> String {
    text.replace("\r\n", "\n")
        .chars()
        .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
        .collect()
}

/// The bytes for a named key, for answering a CLI's prompt without a keyboard: enter,
/// esc, tab, up, down, left, right, space, backspace, ctrl-c, ctrl-d, or one letter or digit.
pub fn key_bytes(name: &str) -> Option<&'static [u8]> {
    Some(match name.to_ascii_lowercase().as_str() {
        "enter" | "return" => b"\r",
        "esc" | "escape" => b"\x1b",
        "tab" => b"\t",
        "up" => b"\x1b[A",
        "down" => b"\x1b[B",
        "right" => b"\x1b[C",
        "left" => b"\x1b[D",
        "space" => b" ",
        "backspace" => b"\x7f",
        "ctrl-c" => b"\x03",
        "ctrl-d" => b"\x04",
        s if s.len() == 1 => {
            const ONE: &[u8; 36] = b"abcdefghijklmnopqrstuvwxyz0123456789";
            let i = ONE.iter().position(|c| *c == s.as_bytes()[0])?;
            &ONE[i..i + 1]
        }
        _ => return None,
    })
}

/// Presses one named key (see `key_bytes`) in a run.
pub fn send_key(run: &Run, name: &str) -> Result<()> {
    let bytes = key_bytes(name).ok_or_else(|| {
        anyhow!("Unknown key \"{name}\": enter, esc, tab, up, down, left, right, space, backspace, ctrl-c, ctrl-d, or a letter or digit.")
    })?;
    if run.state() != State::Running {
        bail!("{} is not running.", run.id);
    }
    let stream = connect(&run.id).map_err(|_| anyhow!("Could not connect to {}.", run.id))?;
    let mut w = &stream;
    write_frame(&mut w, WATCH, &[1])?;
    write_frame(&mut w, INPUT, bytes)?;
    std::thread::sleep(Duration::from_millis(50));
    Ok(())
}

/// Types `text` into a run as if pasted, then presses Enter unless `submit` is false.
/// Multi-line text goes in as one bracketed paste when the CLI accepts that (Claude Code
/// and Codex do), so its newlines don't submit early; otherwise newlines become spaces.
pub fn send_text(run: &Run, text: &str, submit: bool) -> Result<()> {
    match run.state() {
        State::Running => {}
        State::Queued => bail!("{} is queued and hasn't started yet.", run.id),
        _ => bail!("{} is not running.", run.id),
    }
    let text = sanitize(text);
    let paste = super::screen::capture(run).screen().bracketed_paste();
    let mut payload = Vec::new();
    if text.contains('\n') && paste {
        payload.extend_from_slice(b"\x1b[200~");
        payload.extend_from_slice(text.trim_end_matches('\n').as_bytes());
        payload.extend_from_slice(b"\x1b[201~");
    } else {
        payload.extend_from_slice(text.replace('\n', " ").trim_end().as_bytes());
    }
    let stream = connect(&run.id).map_err(|_| anyhow!("Could not connect to {}.", run.id))?;
    let mut w = &stream;
    // Only sending, not watching: the host keeps notifying as if no one were attached.
    write_frame(&mut w, WATCH, &[1])?;
    if !payload.is_empty() {
        write_frame(&mut w, INPUT, &payload)?;
    }
    if submit {
        // A CLI that sees Enter in the same read as the text takes it as part of a paste.
        std::thread::sleep(Duration::from_millis(150));
        write_frame(&mut w, INPUT, b"\r")?;
    }
    // Let the host read the frames before the connection closes.
    std::thread::sleep(Duration::from_millis(50));
    Ok(())
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
    fn sanitizing_keeps_text_and_drops_terminal_commands() {
        assert_eq!(
            sanitize("fix\r\nthe \x1b[201~tests\x07\tnow"),
            "fix\nthe [201~tests\tnow"
        );
    }
}
