//! The review view in `amux ui` (`c`): a run's diff, line by line, where ⏎ writes a
//! comment on the line under the cursor and `S` sends every pending comment to the agent.
//! Space marks the hunk under the cursor to drop, and `D` drops the marked hunks from the
//! run's worktree after a y/n confirm.

use super::draw::{border_hints, hints, panel, span};
use super::theme::Theme;
use crate::runs::Run;
use crate::work::hunks::{self, Hunk};
use crate::work::review::{self, Comment, DiffLine, LineKind};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEventKind};
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::Line;
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use std::collections::BTreeSet;

pub struct ReviewView {
    pub id: String,
    run: Run,
    /// The diff the lines and hunks come from, for dropping hunks.
    changes: review::Changes,
    pub lines: Vec<DiffLine>,
    /// The hunks of the diff; `DiffLine::hunk` indexes into it.
    hunks: Vec<Hunk>,
    /// Hunks marked to drop (space).
    marked: BTreeSet<usize>,
    /// Asking whether to drop the marked hunks (`D`).
    confirm: bool,
    pub comments: Vec<Comment>,
    /// The diff line under the cursor.
    pub cursor: usize,
    /// The first screen row shown.
    top: usize,
    /// The comment being written, when one is.
    pub input: Option<String>,
    /// Where the diff was last drawn, for paging and the mouse.
    area: Rect,
}

/// What a key in the review view asks the app to do.
pub enum Outcome {
    Stay,
    Close,
    /// Send the pending comments to the run.
    Send,
    /// Say something in the footer (and whether it is an error).
    Say(String, bool),
    /// Hunks were dropped: say so, tell the agent this note if there is one, and close
    /// the view if nothing is left to review.
    Dropped {
        say: String,
        tell: Option<String>,
        close: bool,
    },
}

/// One screen row: a diff line, or a comment shown under it.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Row {
    Line(usize),
    Note(usize, usize),
}

impl ReviewView {
    pub fn open(run: &Run) -> Result<ReviewView, String> {
        let changes = review::changes(run).map_err(|e| format!("{e:#}"))?;
        let lines = review::parse_diff(&changes.text);
        if lines.is_empty() {
            return Err(format!("{} hasn't changed anything yet.", run.id));
        }
        let hunks = hunks::parse(&changes);
        let cursor = lines.iter().position(|l| l.anchor().is_some()).unwrap_or(0);
        Ok(ReviewView {
            id: run.id.clone(),
            run: run.clone(),
            changes,
            lines,
            hunks,
            marked: BTreeSet::new(),
            confirm: false,
            comments: review::load(&run.id),
            cursor,
            top: 0,
            input: None,
            area: Rect::default(),
        })
    }

    /// Asking whether to drop the marked hunks.
    pub fn confirming(&self) -> bool {
        self.confirm
    }

    pub fn pending(&self) -> usize {
        review::pending(&self.comments)
    }

    fn notes_on(&self, i: usize) -> impl Iterator<Item = usize> + '_ {
        let at = self.lines[i].anchor();
        self.comments
            .iter()
            .enumerate()
            .filter(move |(_, c)| at.as_ref() == Some(&c.at))
            .map(|(n, _)| n)
    }

    fn rows(&self) -> Vec<Row> {
        let mut out = Vec::with_capacity(self.lines.len());
        for i in 0..self.lines.len() {
            out.push(Row::Line(i));
            out.extend(self.notes_on(i).map(|n| Row::Note(i, n)));
        }
        out
    }

    fn files(&self) -> usize {
        self.lines
            .iter()
            .filter(|l| l.kind == LineKind::File)
            .count()
    }

    fn move_to(&mut self, i: usize) {
        self.cursor = i.min(self.lines.len().saturating_sub(1));
    }

    fn move_by(&mut self, d: isize) {
        self.move_to(self.cursor.saturating_add_signed(d));
    }

    /// The next (or previous) file's first line.
    fn jump_file(&mut self, back: bool) {
        let is_file = |l: &DiffLine| l.kind == LineKind::File;
        let found = if back {
            self.lines[..self.cursor].iter().rposition(&is_file)
        } else {
            self.lines
                .iter()
                .skip(self.cursor + 1)
                .position(is_file)
                .map(|p| p + self.cursor + 1)
        };
        if let Some(i) = found {
            self.move_to(i);
        }
    }

    /// The hunks a mark on line `i` covers: its own, or every hunk of the file on a
    /// file's first line.
    fn hunks_at(&self, i: usize) -> Vec<usize> {
        let l = &self.lines[i];
        match (l.hunk, l.kind) {
            (Some(h), _) => vec![h],
            (None, LineKind::File) => (0..self.hunks.len())
                .filter(|h| self.hunks[*h].file == l.file)
                .collect(),
            _ => vec![],
        }
    }

    /// Space: marks the hunk under the cursor to drop, or unmarks it.
    fn toggle(&mut self) -> Outcome {
        let hs = self.hunks_at(self.cursor);
        if hs.is_empty() {
            return Outcome::Say(
                "Nothing to drop on this line: move to a change (binary changes can't be dropped here).".into(),
                true,
            );
        }
        if hs.iter().all(|h| self.marked.contains(h)) {
            for h in &hs {
                self.marked.remove(h);
            }
        } else {
            self.marked.extend(&hs);
        }
        Outcome::Stay
    }

    /// The marked hunks: how many, and in how many files.
    fn marked_counts(&self) -> (usize, usize) {
        let files: BTreeSet<&str> = self
            .marked
            .iter()
            .map(|h| self.hunks[*h].file.as_str())
            .collect();
        (self.marked.len(), files.len())
    }

    /// Drops the marked hunks from the run's worktree, then reads the diff again.
    fn drop_marked(&mut self) -> Outcome {
        let picked: Vec<&Hunk> = self.marked.iter().map(|h| &self.hunks[*h]).collect();
        let done = match hunks::drop_from_run(&self.run, &self.changes, &picked, false) {
            Ok(d) => d,
            Err(e) => return Outcome::Say(format!("{e:#}"), true),
        };
        let fresh = crate::runs::load(&self.id).unwrap_or_else(|_| self.run.clone());
        let live = fresh.state() == crate::runs::State::Running;
        let mut say = format!("Dropped {} from {}", done.summary(), self.id);
        say.push_str(&match (&done.commit, &done.commit_error) {
            (Some(c), _) => format!(", committed as {c}."),
            (None, Some(_)) => ". Not committed (git commit failed); it's in the worktree.".into(),
            (None, None) => ". Uncommitted; the agent's next commit takes it.".into(),
        });
        let close = match ReviewView::open(&fresh) {
            Ok(mut v) => {
                v.cursor = self.cursor.min(v.lines.len().saturating_sub(1));
                v.top = self.top;
                *self = v;
                false
            }
            Err(_) => true,
        };
        Outcome::Dropped {
            say,
            tell: live.then(|| done.note()),
            close,
        }
    }

    pub fn key(&mut self, k: KeyEvent) -> Outcome {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        if self.confirm {
            self.confirm = false;
            return match k.code {
                KeyCode::Char('y' | 'Y') | KeyCode::Enter => self.drop_marked(),
                _ => Outcome::Say("Nothing dropped.".into(), false),
            };
        }
        if let Some(text) = &mut self.input {
            match k.code {
                KeyCode::Esc => self.input = None,
                KeyCode::Enter => {
                    let note = std::mem::take(text);
                    self.input = None;
                    return self.save(&note);
                }
                KeyCode::Backspace => {
                    text.pop();
                }
                KeyCode::Char('u') if ctrl => text.clear(),
                KeyCode::Char('w') if ctrl => super::spawn::delete_word(text),
                KeyCode::Char(c) if !ctrl => text.push(c),
                _ => {}
            }
            return Outcome::Stay;
        }
        let page = (self.area.height as isize - 2).max(1);
        match k.code {
            KeyCode::Esc | KeyCode::Char('q') => return Outcome::Close,
            KeyCode::Down | KeyCode::Char('j') => self.move_by(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_by(-1),
            KeyCode::PageDown => self.move_by(page),
            KeyCode::Char('f' | 'd') if ctrl => self.move_by(page),
            KeyCode::PageUp => self.move_by(-page),
            KeyCode::Char('b' | 'u') if ctrl => self.move_by(-page),
            KeyCode::Char(' ') => return self.toggle(),
            KeyCode::Char('D') => {
                if self.marked.is_empty() {
                    return Outcome::Say(
                        "No hunks marked: space marks the one under the cursor.".into(),
                        true,
                    );
                }
                if let Some(why) = hunks::busy(&self.run) {
                    return Outcome::Say(why, true);
                }
                self.confirm = true;
            }
            KeyCode::Home | KeyCode::Char('g') => self.move_to(0),
            KeyCode::End | KeyCode::Char('G') => self.move_to(usize::MAX),
            KeyCode::Char('n') | KeyCode::Tab => self.jump_file(false),
            KeyCode::Char('N') | KeyCode::BackTab => self.jump_file(true),
            KeyCode::Enter | KeyCode::Char('m' | 'i') => {
                return match self.lines[self.cursor].anchor() {
                    Some(_) => {
                        self.input = Some(String::new());
                        Outcome::Stay
                    }
                    None => Outcome::Say(
                        "Pick a changed line or one around it to comment on.".into(),
                        true,
                    ),
                }
            }
            KeyCode::Char('x' | 'd') => return self.delete(),
            KeyCode::Char('S') => {
                return if self.pending() == 0 {
                    Outcome::Say("No comments to send yet: ⏎ writes one.".into(), true)
                } else {
                    Outcome::Send
                }
            }
            _ => {}
        }
        Outcome::Stay
    }

    pub fn paste(&mut self, text: &str) {
        if let Some(t) = &mut self.input {
            t.push_str(&text.replace(['\n', '\r'], " "));
        }
    }

    pub fn mouse(&mut self, kind: MouseEventKind, row: u16) {
        match kind {
            MouseEventKind::ScrollDown => self.move_by(3),
            MouseEventKind::ScrollUp => self.move_by(-3),
            MouseEventKind::Down(MouseButton::Left)
                if self.input.is_none()
                    && row >= self.area.y
                    && row < self.area.y + self.area.height =>
            {
                let at = self.top + (row - self.area.y) as usize;
                if let Some(Row::Line(i) | Row::Note(i, _)) = self.rows().get(at) {
                    self.cursor = *i;
                }
            }
            _ => {}
        }
    }

    fn save(&mut self, note: &str) -> Outcome {
        let line = &self.lines[self.cursor];
        let Some(at) = line.anchor() else {
            return Outcome::Stay;
        };
        if note.trim().is_empty() {
            return Outcome::Say("Empty comment, not saved.".into(), true);
        }
        match review::add(&self.id, at.clone(), &line.text, note) {
            Ok(n) => {
                self.comments = review::load(&self.id);
                Outcome::Say(
                    format!(
                        "Comment on {at} saved; {n} to send. S sends them to {}.",
                        self.id
                    ),
                    false,
                )
            }
            Err(e) => Outcome::Say(format!("{e:#}"), true),
        }
    }

    /// Deletes the pending comments on the line under the cursor.
    fn delete(&mut self) -> Outcome {
        let mine: Vec<usize> = self
            .notes_on(self.cursor)
            .filter(|n| self.comments[*n].sent_at.is_none())
            .collect();
        if mine.is_empty() {
            return Outcome::Say("No unsent comment on this line.".into(), true);
        }
        let mut n = 0;
        let mut i = 0;
        self.comments.retain(|_| {
            let keep = !mine.contains(&i);
            i += 1;
            n += usize::from(!keep);
            keep
        });
        match review::save(&self.id, &self.comments) {
            Ok(()) => Outcome::Say(
                format!("Deleted {n} comment{}.", if n == 1 { "" } else { "s" }),
                false,
            ),
            Err(e) => Outcome::Say(format!("{e:#}"), true),
        }
    }

    /// After sending: reload, so the comments show as sent.
    pub fn reload_comments(&mut self) {
        self.comments = review::load(&self.id);
    }

    pub fn footer(&self, t: &Theme) -> Line<'static> {
        if self.confirm {
            let (h, f) = self.marked_counts();
            let tell = if self.run.state() == crate::runs::State::Running {
                " and tell its agent"
            } else {
                ""
            };
            let mut v = vec![span(
                format!(
                    " Drop {} from {}'s worktree{tell}? ",
                    hunks::counted(h, f),
                    self.id
                ),
                t.title,
            )];
            v.extend(hints(t, &[("y", "drop"), ("n", "keep")]));
            return Line::from(v);
        }
        if let Some(text) = &self.input {
            let at = self.lines[self.cursor]
                .anchor()
                .map(|a| a.to_string())
                .unwrap_or_default();
            let mut v = vec![
                span(format!(" Comment on {at}: "), t.title),
                span(format!("{text}▏"), t.input),
            ];
            v.push(span("   ", t.muted));
            v.extend(hints(t, &[("⏎", "save"), ("Esc", "cancel")]));
            return Line::from(v);
        }
        let send = format!("send {}", self.pending());
        let drop = format!("drop {}", self.marked.len());
        let mut v = vec![span(" ", t.muted)];
        v.extend(hints(
            t,
            &[
                ("⏎", "comment"),
                ("S", send.as_str()),
                ("␣", "mark hunk"),
                ("D", drop.as_str()),
                ("x", "delete"),
                ("n N", "file"),
                ("j k", "move"),
                ("Esc", "close"),
            ],
        ));
        Line::from(v)
    }

    pub fn draw(&mut self, f: &mut Frame, area: Rect, t: &Theme) {
        super::draw::clear(f, area, t);
        let pending = self.pending();
        let mut title = format!(
            " Review {} · {} file{} · {} comment{} to send ",
            self.id,
            self.files(),
            if self.files() == 1 { "" } else { "s" },
            pending,
            if pending == 1 { "" } else { "s" },
        );
        if !self.marked.is_empty() {
            let (h, f) = self.marked_counts();
            title.push_str(&format!("· {} to drop ", hunks::counted(h, f)));
        }
        let block = panel(t, t.accent)
            .title(span(title, t.title))
            .title_bottom(border_hints(
                t,
                &[
                    ("⏎", "comment"),
                    ("S", "send"),
                    ("␣", "mark"),
                    ("D", "drop"),
                    ("Esc", "close"),
                ],
            ));
        let inner = block.inner(area);
        f.render_widget(block, area);
        self.area = inner;
        let rows = self.rows();
        let height = inner.height as usize;
        // Keep the cursor's line, and the comments under it, in view.
        let at = rows
            .iter()
            .position(|r| *r == Row::Line(self.cursor))
            .unwrap_or(0);
        let end = rows[at..]
            .iter()
            .take_while(|r| matches!(r, Row::Line(i) | Row::Note(i, _) if *i == self.cursor))
            .count()
            + at;
        if at < self.top {
            self.top = at;
        } else if height > 0 && end > self.top + height {
            self.top = end.saturating_sub(height).min(at);
        }
        let width = inner.width as usize;
        let mut out = Vec::with_capacity(height);
        for r in rows.iter().skip(self.top).take(height) {
            match *r {
                Row::Line(i) => {
                    let l = &self.lines[i];
                    let num = l.new.or(l.old).map_or(String::new(), |n| n.to_string());
                    let has_note = self.notes_on(i).next().is_some();
                    let mark = if has_note { "●" } else { " " };
                    let style = match l.kind {
                        LineKind::Added => t.success,
                        LineKind::Removed => t.error,
                        LineKind::Hunk => t.accent,
                        LineKind::File => t.title,
                        LineKind::Meta => t.muted,
                        LineKind::Context => t.text,
                    };
                    let dropped = l.hunk.is_some_and(|h| self.marked.contains(&h));
                    let mut text = match l.kind {
                        LineKind::File => format!("── {} ", l.file),
                        _ => l.text.replace('\t', "    "),
                    };
                    if dropped && (l.kind == LineKind::Hunk || l.text == review::UNTRACKED) {
                        text.push_str("  ✗ drop");
                    }
                    let style = if dropped {
                        t.muted.add_modifier(Modifier::CROSSED_OUT)
                    } else {
                        style
                    };
                    let room = width.saturating_sub(7);
                    let text: String = text.chars().take(room).collect();
                    let mut line = Line::from(vec![
                        span(format!("{num:>5}"), t.muted),
                        span(mark, t.accent),
                        span(if dropped { "✗" } else { " " }, t.error),
                        span(text, style),
                    ]);
                    if i == self.cursor {
                        line = line.style(t.selection);
                    }
                    out.push(line);
                }
                Row::Note(_, n) => {
                    let c = &self.comments[n];
                    let (glyph, style) = if c.sent_at.is_some() {
                        ("✓ sent", t.muted)
                    } else {
                        ("💬", t.accent)
                    };
                    let note = crate::util::one_line(&c.note, width.saturating_sub(12).max(1));
                    out.push(Line::from(vec![
                        span("       ", t.muted),
                        span(format!("{glyph} "), style),
                        span(note, style),
                    ]));
                }
            }
        }
        f.render_widget(Paragraph::new(out), inner);
    }
}
