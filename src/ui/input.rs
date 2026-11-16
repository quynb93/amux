//! Turning the UI's key, paste and mouse events back into the bytes a terminal would send,
//! so you can type into a run right in its pane, as in a real terminal.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEventKind};

/// The bytes a terminal sends for a key. `app_cursor`: the CLI asked for application
/// cursor keys (DECCKM), which changes what the arrows send.
pub fn encode_key(k: &KeyEvent, app_cursor: bool) -> Option<Vec<u8>> {
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    let alt = k.modifiers.contains(KeyModifiers::ALT);
    let shift = k.modifiers.contains(KeyModifiers::SHIFT);
    // xterm's modifier parameter: 1 + shift + 2·alt + 4·ctrl.
    let m = 1 + u8::from(shift) + 2 * u8::from(alt) + 4 * u8::from(ctrl);
    let csi = |final_: char| -> Vec<u8> {
        if m > 1 {
            format!("\x1b[1;{m}{final_}").into_bytes()
        } else if app_cursor {
            format!("\x1bO{final_}").into_bytes()
        } else {
            format!("\x1b[{final_}").into_bytes()
        }
    };
    let tilde = |n: u8| -> Vec<u8> {
        if m > 1 {
            format!("\x1b[{n};{m}~").into_bytes()
        } else {
            format!("\x1b[{n}~").into_bytes()
        }
    };
    let mut out = match k.code {
        KeyCode::Char(c) if ctrl => {
            let b = match c.to_ascii_lowercase() {
                c @ 'a'..='z' => c as u8 - b'a' + 1,
                '@' | ' ' | '2' => 0,
                '[' | '3' => 0x1b,
                '\\' | '4' => 0x1c,
                ']' | '5' => 0x1d,
                '^' | '6' => 0x1e,
                '_' | '-' | '7' => 0x1f,
                '?' | '8' => 0x7f,
                _ => return None,
            };
            vec![b]
        }
        KeyCode::Char(c) => c.to_string().into_bytes(),
        KeyCode::Enter => {
            // Shift-Enter: a newline in Claude Code and Codex, without sending.
            if shift || alt {
                return Some(b"\x1b\r".to_vec());
            }
            b"\r".to_vec()
        }
        KeyCode::Tab => b"\t".to_vec(),
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        KeyCode::Backspace if ctrl => vec![0x08],
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Esc => vec![0x1b],
        KeyCode::Up => return Some(csi('A')),
        KeyCode::Down => return Some(csi('B')),
        KeyCode::Right => return Some(csi('C')),
        KeyCode::Left => return Some(csi('D')),
        KeyCode::Home => return Some(csi('H')),
        KeyCode::End => return Some(csi('F')),
        KeyCode::Insert => return Some(tilde(2)),
        KeyCode::Delete => return Some(tilde(3)),
        KeyCode::PageUp => return Some(tilde(5)),
        KeyCode::PageDown => return Some(tilde(6)),
        KeyCode::F(n @ 1..=4) => {
            let c = (b'P' + n - 1) as char;
            return Some(if m > 1 {
                format!("\x1b[1;{m}{c}").into_bytes()
            } else {
                format!("\x1bO{c}").into_bytes()
            });
        }
        KeyCode::F(n @ 5..=12) => {
            let code = [15, 17, 18, 19, 20, 21, 23, 24][(n - 5) as usize];
            return Some(tilde(code));
        }
        _ => return None,
    };
    if alt {
        out.insert(0, 0x1b);
    }
    Some(out)
}

/// Pasted text, as the terminal would deliver it: wrapped in bracketed-paste markers when
/// the CLI turned that mode on. Markers inside the text are dropped, so a paste can't end
/// itself early and have the rest read as keystrokes.
pub fn encode_paste(text: &str, bracketed: bool) -> Vec<u8> {
    let clean = text
        .replace("\x1b[200~", "")
        .replace("\x1b[201~", "")
        .replace("\r\n", "\r")
        .replace('\n', "\r");
    if bracketed {
        format!("\x1b[200~{clean}\x1b[201~").into_bytes()
    } else {
        clean.into_bytes()
    }
}

/// A wheel or click for a CLI that asked for mouse reports, at a cell of its screen
/// (0-based). None for events the CLI didn't ask for.
pub fn encode_mouse(
    kind: MouseEventKind,
    col: u16,
    row: u16,
    mode: vt100::MouseProtocolMode,
    encoding: vt100::MouseProtocolEncoding,
) -> Option<Vec<u8>> {
    use ratatui::crossterm::event::MouseButton;
    use vt100::MouseProtocolMode as M;
    let (button, release) = match kind {
        MouseEventKind::ScrollUp => (64, false),
        MouseEventKind::ScrollDown => (65, false),
        MouseEventKind::Down(MouseButton::Left) => (0, false),
        MouseEventKind::Down(MouseButton::Middle) => (1, false),
        MouseEventKind::Down(MouseButton::Right) => (2, false),
        MouseEventKind::Up(_) => (3, true),
        MouseEventKind::Drag(MouseButton::Left) => (32, false),
        _ => return None,
    };
    let wanted = match mode {
        M::None => false,
        M::Press => !release && button < 32,
        M::PressRelease => !(32..64).contains(&button),
        M::ButtonMotion | M::AnyMotion => true,
    };
    if !wanted {
        return None;
    }
    let (x, y) = (col + 1, row + 1);
    Some(match encoding {
        vt100::MouseProtocolEncoding::Sgr => {
            let (b, end) = if release { (0, 'm') } else { (button, 'M') };
            format!("\x1b[<{b};{x};{y}{end}").into_bytes()
        }
        _ => {
            // The original encoding: each value + 32 in one byte, so it stops at 223.
            if x > 223 || y > 223 {
                return None;
            }
            vec![0x1b, b'[', b'M', 32 + button, 32 + x as u8, 32 + y as u8]
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, m: KeyModifiers) -> Vec<u8> {
        encode_key(&KeyEvent::new(code, m), false).unwrap()
    }

    #[test]
    fn encodes_keys_like_a_terminal() {
        let none = KeyModifiers::NONE;
        assert_eq!(key(KeyCode::Char('a'), none), b"a");
        assert_eq!(key(KeyCode::Char('é'), none), "é".as_bytes());
        assert_eq!(key(KeyCode::Char('c'), KeyModifiers::CONTROL), [3]);
        assert_eq!(key(KeyCode::Char('b'), KeyModifiers::ALT), b"\x1bb");
        assert_eq!(key(KeyCode::Enter, none), b"\r");
        assert_eq!(key(KeyCode::Enter, KeyModifiers::SHIFT), b"\x1b\r");
        assert_eq!(key(KeyCode::Backspace, none), [0x7f]);
        assert_eq!(key(KeyCode::Up, none), b"\x1b[A");
        assert_eq!(key(KeyCode::Left, KeyModifiers::CONTROL), b"\x1b[1;5D");
        assert_eq!(key(KeyCode::Delete, none), b"\x1b[3~");
        assert_eq!(key(KeyCode::F(1), none), b"\x1bOP");
        assert_eq!(key(KeyCode::F(5), none), b"\x1b[15~");
        assert_eq!(key(KeyCode::BackTab, KeyModifiers::SHIFT), b"\x1b[Z");
        let app = encode_key(&KeyEvent::new(KeyCode::Down, none), true).unwrap();
        assert_eq!(app, b"\x1bOB");
    }

    #[test]
    fn pastes_are_bracketed_and_cannot_end_early() {
        assert_eq!(encode_paste("a\nb", false), b"a\rb");
        assert_eq!(
            encode_paste("x\x1b[201~y", true),
            b"\x1b[200~xy\x1b[201~".to_vec()
        );
    }

    #[test]
    fn mouse_reports_only_what_the_cli_asked_for() {
        use vt100::{MouseProtocolEncoding as E, MouseProtocolMode as M};
        let up = MouseEventKind::ScrollUp;
        assert_eq!(encode_mouse(up, 0, 0, M::None, E::Sgr), None);
        assert_eq!(
            encode_mouse(up, 4, 9, M::PressRelease, E::Sgr).unwrap(),
            b"\x1b[<64;5;10M"
        );
        assert_eq!(
            encode_mouse(up, 4, 9, M::PressRelease, E::Default).unwrap(),
            vec![0x1b, b'[', b'M', 96, 37, 42]
        );
    }
}
