//! Working in a run from its pane, as in a terminal: keys, pastes and mouse events go
//! to the run while it has focus, and the run is fitted to the pane.

use super::*;

impl App {
    /// Scrolls the preview back (positive) or forward, within the history it has.
    pub(super) fn scroll_preview(&mut self, by: isize) {
        self.scroll = self.scroll.saturating_add_signed(by);
        if let Some(p) = self.selected.as_ref().and_then(|id| self.previews.find(id)) {
            let mut s = p.lock();
            s.parser.screen_mut().set_scrollback(self.scroll);
            // The emulator caps it at the history it has.
            self.scroll = s.parser.screen().scrollback();
        }
    }

    /// The selected run's live preview, which is what typing goes through.
    pub(super) fn live_preview(&self) -> Option<&preview::Preview> {
        let id = self.selected.as_ref()?;
        self.previews.find(id).filter(|p| p.is_live())
    }

    /// Sends bytes to the selected run as typed input. False if it can't take them.
    pub(super) fn send_bytes(&mut self, bytes: &[u8]) -> bool {
        let sent = self.live_preview().is_some_and(|p| p.send(bytes).is_ok());
        if sent && self.scroll > 0 {
            // Typing goes back to the live screen, as in a terminal.
            self.scroll_preview(-(self.scroll as isize));
        }
        sent
    }

    pub(super) fn send_key(&mut self, k: &KeyEvent) {
        let app_cursor = self
            .live_preview()
            .is_some_and(|p| p.lock().parser.screen().application_cursor());
        if let Some(bytes) = input::encode_key(k, app_cursor) {
            if !self.send_bytes(&bytes) {
                self.focus_run = false;
                self.say("The run isn't taking input; back to the list.", true);
            }
        }
    }

    /// Keys while typing into a run: everything goes to it, except Ctrl-] (back to the
    /// list), Ctrl-B (the next key is a command), and Alt-↑/↓ (another run).
    pub(super) fn key_run(&mut self, k: KeyEvent) {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let alt = k.modifiers.contains(KeyModifiers::ALT);
        if self.prefix {
            self.prefix = false;
            match k.code {
                // Ctrl-B twice: the CLI gets one.
                KeyCode::Char('b') if ctrl => {
                    self.send_bytes(&[2]);
                }
                KeyCode::Esc => {}
                KeyCode::Enter => {
                    if let Some((id, kind)) = self.current().map(|r| (r.run.id.clone(), r.kind)) {
                        self.attach_run(id, kind)
                    }
                }
                KeyCode::Char(c)
                    if !ctrl && (is_shortcut(c) || matches!(c, 'a' | 'z' | 'c' | 'i')) =>
                {
                    self.key_normal(k)
                }
                KeyCode::Up
                | KeyCode::Down
                | KeyCode::Tab
                | KeyCode::PageUp
                | KeyCode::PageDown => self.key_normal(k),
                _ => self.say("Not a command. Ctrl-B then ? lists them.", true),
            }
            return;
        }
        match k.code {
            // Ctrl-] is byte 0x1d, which terminals also send for Ctrl-5.
            KeyCode::Char(']' | '5') if ctrl => {
                self.focus_run = false;
                self.say(
                    "Back to the list. ⏎ or a click types into a run again.",
                    false,
                );
            }
            KeyCode::Char('b') if ctrl => self.prefix = true,
            KeyCode::Up if alt => self.move_by(-1),
            KeyCode::Down if alt => self.move_by(1),
            _ => self.send_key(&k),
        }
    }

    pub(super) fn paste(&mut self, text: &str) {
        match &mut self.mode {
            Mode::Palette(p) => p.input.push_str(&text.replace(['\n', '\r'], " ")),
            Mode::Spawn(f) => f.paste(text),
            Mode::Review(r) => r.paste(text),
            Mode::Normal if self.focus_run => {
                let bracketed = self
                    .live_preview()
                    .is_some_and(|p| p.lock().parser.screen().bracketed_paste());
                self.send_bytes(&input::encode_paste(text, bracketed));
            }
            Mode::Normal => match self.current().map(|r| r.kind) {
                Some(k) if k.is_live() => {
                    self.focus_run = true;
                    self.paste(text);
                }
                _ => self.say("Select a running run to paste into.", true),
            },
            _ => {}
        }
    }

    /// Clicks and the wheel: a click on a run selects it, a click on the screen types
    /// into it, and the wheel moves the list or scrolls the screen. A CLI that asked for
    /// mouse reports (and is being typed into) gets the events itself.
    pub(super) fn mouse(&mut self, m: MouseEvent) {
        let at = Position::new(m.column, m.row);
        match &mut self.mode {
            Mode::Output(o) => {
                let max = o.lines.len().saturating_sub(1);
                match m.kind {
                    MouseEventKind::ScrollDown => o.scroll = (o.scroll + 3).min(max),
                    MouseEventKind::ScrollUp => o.scroll = o.scroll.saturating_sub(3),
                    _ => {}
                }
                return;
            }
            Mode::Review(r) => {
                r.mouse(m.kind, m.row);
                return;
            }
            Mode::Normal => {}
            _ => return,
        }
        // A board card or a tile other than the selected run: select it, and a click on a
        // live tile types into it.
        if let Some(i) = self.hit(at) {
            if Some(i) != self.list.selected() {
                if let MouseEventKind::Down(MouseButton::Left) = m.kind {
                    self.set_index(Some(i));
                    self.focus_run = self.view == View::Tiles && self.rows[i].kind.is_live();
                }
                return;
            }
            if self.view == View::Board {
                if let MouseEventKind::Down(MouseButton::Left) = m.kind {
                    self.focus_run = false;
                }
                return;
            }
        }
        if self.list_area.contains(at) {
            match m.kind {
                MouseEventKind::Down(MouseButton::Left) => {
                    // Each run takes two lines, under the panel's top border.
                    let line = m.row.saturating_sub(self.list_area.y + 1) as usize;
                    let i = self.list.offset() + line / 2;
                    if m.row > self.list_area.y && i < self.rows.len() {
                        self.set_index(Some(i));
                        self.focus_run = false;
                    }
                }
                MouseEventKind::ScrollUp => self.move_by(-1),
                MouseEventKind::ScrollDown => self.move_by(1),
                _ => {}
            }
            return;
        }
        if !self.preview_area.contains(at) {
            return;
        }
        let (col, row) = (m.column - self.preview_area.x, m.row - self.preview_area.y);
        let modes = self.live_preview().map(|p| {
            let s = p.lock();
            let screen = s.parser.screen();
            (
                screen.mouse_protocol_mode(),
                screen.mouse_protocol_encoding(),
            )
        });
        if let (true, Some((mode, enc))) = (self.focus_run, modes) {
            if let Some(bytes) = input::encode_mouse(m.kind, col, row, mode, enc) {
                self.send_bytes(&bytes);
                return;
            }
        }
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) if modes.is_some() => {
                if !self.focus_run {
                    self.focus_run = true;
                    self.say("Typing into the run. Ctrl-] goes back to the list.", false);
                }
            }
            MouseEventKind::ScrollUp => self.scroll_preview(3),
            MouseEventKind::ScrollDown => self.scroll_preview(-3),
            _ => {}
        }
    }

    /// Keeps the host told which run a person is typing into, and stops typing into a run
    /// that ended or a selection that moved to an ended one.
    pub(super) fn sync_focus(&mut self) {
        if self.focus_run && self.live_preview().is_none() {
            let live = self.current().is_some_and(|r| r.kind.is_live());
            if !live {
                self.focus_run = false;
                self.prefix = false;
            }
        }
        let want = if self.focus_run {
            self.selected.clone()
        } else {
            None
        };
        if want == self.typing_into {
            return;
        }
        if let Some(old) = self.typing_into.take() {
            if let Some(p) = self.previews.find(&old) {
                p.set_watching(true);
            }
        }
        if let Some(id) = &want {
            if let Some(p) = self.previews.find(id) {
                p.set_watching(false);
            }
        }
        self.typing_into = want;
    }

    /// Fits the run being typed into to its pane, as a terminal of that size would.
    pub(super) fn fit_focused(&mut self, rows: u16, cols: u16) {
        if !self.focus_run || rows == 0 || cols == 0 {
            return;
        }
        let Some(id) = self.selected.clone() else {
            return;
        };
        if self.fitted == Some((id.clone(), rows, cols)) {
            return;
        }
        if let Some(p) = self.previews.find(&id).filter(|p| p.is_live()) {
            p.resize(rows, cols);
            self.fitted = Some((id, rows, cols));
        }
    }
}
