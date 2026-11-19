//! The review view in `amux ui` (`c`): a run's diff, line by line, where ⏎ writes a
//! comment on the line under the cursor and `S` sends every pending comment to the agent.
//! Space marks the hunk under the cursor to drop, and `D` drops the marked hunks from the
//! run's worktree after a y/n confirm.
//!
//! A run on another machine is reviewed through amux there, over SSH: `amux
//! __review-data` reads its diff and comments, `amux comment`/`amux comments` keep its
//! comments, and `amux review --drop … --expect <digest>` drops hunks there, with the same
//! checks, undo patch and note to the agent as here.

use super::draw::{border_hints, hints, panel, span};
use super::theme::Theme;
use crate::remote::Machine;
use crate::runs::Run;
use crate::work::hunks::{self, Hunk};
use crate::work::review::{self, Anchor, Comment, DiffLine, LineKind, ReviewData};
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
    /// The marked hunks are being dropped in the background (git apply, then git commit
    /// with the repo's hooks, which can take a while).
    dropping: bool,
    pub comments: Vec<Comment>,
    /// The diff line under the cursor.
    pub cursor: usize,
    /// The first screen row shown.
    top: usize,
    /// The comment being written, when one is.
    pub input: Option<String>,
    /// Where the diff was last drawn, for paging and the mouse.
    area: Rect,
    /// The run is on another machine: comments and drops go through amux there.
    remote: Option<Remote>,
}

/// A run on another machine, reviewed through amux there.
#[derive(Clone)]
struct Remote {
    machine: Machine,
    /// The run's name there.
    run: String,
    /// The digest of the diff shown, which a drop passes back so amux there refuses if
    /// the diff changed since.
    digest: String,
}

impl Remote {
    fn output(&self, args: &[&str]) -> anyhow::Result<String> {
        let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
        self.machine.output(&args)
    }

    fn fetch(&self) -> anyhow::Result<ReviewData> {
        let out = self.output(&["__review-data", &self.run])?;
        Ok(serde_json::from_str(&out)?)
    }

    fn comments(&self) -> anyhow::Result<Vec<Comment>> {
        Ok(serde_json::from_str(
            &self.output(&["comments", &self.run, "--json"])?,
        )?)
    }
}

/// Why a review of a run on another machine didn't open.
pub enum RemoteOpen {
    Failed(String),
    /// amux there is older and can't serve one: show the diff in the pager instead.
    TooOld(String),
}

/// How `amux review --drop` names a hunk, for amux on the run's machine.
fn drop_spec(h: &Hunk) -> String {
    if h.untracked {
        h.file.clone()
    } else {
        format!("{}:{}", h.file, h.number)
    }
}

/// What `amux review --drop … --json` prints.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct RemoteDropped {
    hunks: usize,
    files: usize,
    commit: Option<String>,
    #[serde(default)]
    commit_error: Option<String>,
    #[serde(default)]
    undo: String,
    #[serde(default)]
    told: bool,
}

/// What a key in the review view asks the app to do.
pub enum Outcome {
    Stay,
    Close,
    /// Send the pending comments to the run.
    Send,
    /// Say something in the footer (and whether it is an error).
    Say(String, bool),
    /// Drop these hunks in the background; `finish_drop` reports how it went.
    Drop(Box<DropJob>),
}

/// Dropping hunks, done on a background thread so the UI goes on drawing.
pub struct DropJob {
    run: Run,
    changes: review::Changes,
    picked: Vec<Hunk>,
    remote: Option<Remote>,
}

/// How a drop went, for the app: what to say, the note to tell the agent (when it is
/// running), and the review read again (None when nothing is left to review, or when
/// the drop failed and the open review stays).
pub struct DropDone {
    pub id: String,
    pub ok: bool,
    pub say: String,
    pub tell: Option<String>,
    pub view: Option<ReviewView>,
}

impl DropJob {
    /// Drops the hunks and reads the review again. Slow: git apply, git commit (the
    /// repo's hooks) and git diff.
    pub fn run(self) -> DropDone {
        if let Some(r) = self.remote.clone() {
            return self.run_remote(r);
        }
        let id = self.run.id.clone();
        let picked: Vec<&Hunk> = self.picked.iter().collect();
        let done = match hunks::drop_from_run(&self.run, &self.changes, &picked, false) {
            Ok(d) => d,
            // Nothing was dropped: the review open stays as it is, hunks still marked.
            Err(e) => {
                return DropDone {
                    id,
                    ok: false,
                    say: format!("{e:#}"),
                    tell: None,
                    view: None,
                }
            }
        };
        let fresh = crate::runs::load(&id).unwrap_or(self.run);
        let live = fresh.state() == crate::runs::State::Running;
        let say = dropped_say(&id, &done.summary(), &done.commit, &done.commit_error);
        DropDone {
            id,
            ok: true,
            say,
            tell: live.then(|| done.note()),
            view: ReviewView::open(&fresh).ok(),
        }
    }

    /// Drops the hunks on the run's machine (`amux review --drop … --expect … --json`),
    /// which checks, saves the undo patch and tells the agent there; then reads the
    /// review again.
    fn run_remote(self, r: Remote) -> DropDone {
        let id = self.run.id.clone();
        let mut args = vec!["review".to_string(), r.run.clone()];
        for h in &self.picked {
            args.push("--drop".into());
            args.push(drop_spec(h));
        }
        args.extend(["--expect".into(), r.digest.clone(), "--json".into()]);
        let done = r
            .machine
            .output(&args)
            .and_then(|out| Ok(serde_json::from_str::<RemoteDropped>(&out)?));
        let done = match done {
            Ok(d) => d,
            Err(e) => {
                return DropDone {
                    id,
                    ok: false,
                    say: format!("{e:#}"),
                    tell: None,
                    view: None,
                }
            }
        };
        let summary = hunks::counted(done.hunks, done.files);
        let mut say = dropped_say(&id, &summary, &done.commit, &done.commit_error);
        if done.told {
            say.push_str(" Told the agent.");
        }
        if !done.undo.is_empty() {
            say.push_str(&format!(" Undo patch on {}: {}", r.machine.name, done.undo));
        }
        let view = r
            .fetch()
            .ok()
            .and_then(|d| ReviewView::from_data(&self.run, r, d).ok());
        DropDone {
            id,
            ok: true,
            say,
            // amux there told the agent already.
            tell: None,
            view,
        }
    }
}

/// "Dropped 2 hunks in 1 file from w, committed as abc123."
fn dropped_say(
    id: &str,
    summary: &str,
    commit: &Option<String>,
    commit_error: &Option<String>,
) -> String {
    let how = match (commit, commit_error) {
        (Some(c), _) => format!(", committed as {c}."),
        (None, Some(_)) => ". Not committed (git commit failed); it's in the worktree.".into(),
        (None, None) => ". Uncommitted; the agent's next commit takes it.".into(),
    };
    format!("Dropped {summary} from {id}{how}")
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
        Self::build(run, changes, review::load(&run.id), None)
    }

    /// A run on another machine (`machine`, where it is named `name`), read through amux
    /// there.
    pub fn open_remote(run: &Run, machine: Machine, name: String) -> Result<Self, RemoteOpen> {
        let r = Remote {
            machine,
            run: name,
            digest: String::new(),
        };
        match r.fetch() {
            Ok(d) => Self::from_data(run, r, d).map_err(RemoteOpen::Failed),
            Err(e) if format!("{e:#}").contains("Unknown command") => Err(RemoteOpen::TooOld(format!(
                "amux on {} is too old to review {} here; showing its diff. Update amux there to comment and drop hunks.",
                r.machine.name, run.id
            ))),
            Err(e) => Err(RemoteOpen::Failed(format!("{e:#}"))),
        }
    }

    fn from_data(run: &Run, mut r: Remote, d: ReviewData) -> Result<Self, String> {
        r.digest = d.digest.clone();
        Self::build(run, d.changes(), d.comments, Some(r))
    }

    fn build(
        run: &Run,
        changes: review::Changes,
        comments: Vec<Comment>,
        remote: Option<Remote>,
    ) -> Result<ReviewView, String> {
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
            dropping: false,
            comments,
            cursor,
            top: 0,
            input: None,
            area: Rect::default(),
            remote,
        })
    }

    /// The run's comments, read again (on its machine for a remote run). Keeps the ones
    /// shown when they can't be read.
    fn load_comments(&self) -> Vec<Comment> {
        match &self.remote {
            None => review::load(&self.id),
            Some(r) => r.comments().unwrap_or_else(|_| self.comments.clone()),
        }
    }

    fn add_comment(&self, at: &Anchor, code: &str, note: &str) -> anyhow::Result<()> {
        let Some(r) = &self.remote else {
            return review::add(&self.id, at.clone(), code, note).map(|_| ());
        };
        let place = format!("{}:{}", at.file, at.line);
        let mut args = vec!["comment", &r.run, &place];
        if at.removed {
            args.push("--removed");
        }
        args.push(note.trim());
        r.output(&args).map(|_| ())
    }

    /// Asking whether to drop the marked hunks.
    pub fn confirming(&self) -> bool {
        self.confirm
    }

    /// The drop failed: the marked hunks stay marked, to try again.
    pub fn drop_failed(&mut self) {
        self.dropping = false;
    }

    /// Takes the review read again after a drop, keeping the place you were at.
    pub fn replace_after_drop(&mut self, mut v: ReviewView) {
        v.cursor = self.cursor.min(v.lines.len().saturating_sub(1));
        v.top = self.top;
        *self = v;
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

    /// Starts dropping the marked hunks from the run's worktree; the app runs the job in
    /// the background and hands back the diff read again.
    fn drop_marked(&mut self) -> Outcome {
        let picked: Vec<Hunk> = self.marked.iter().map(|h| self.hunks[*h].clone()).collect();
        // On another machine hunks are named `file:n`: a file named like that would be
        // taken for it.
        if self.remote.is_some() {
            if let Some(h) = picked.iter().find(|p| {
                self.hunks
                    .iter()
                    .any(|h| h.file == drop_spec(p) && !p.untracked)
            }) {
                return Outcome::Say(
                    format!(
                        "Can't name {} on its machine; drop it with `amux review` there.",
                        h.place()
                    ),
                    true,
                );
            }
        }
        self.dropping = true;
        Outcome::Drop(Box::new(DropJob {
            run: self.run.clone(),
            changes: self.changes.clone(),
            picked,
            remote: self.remote.clone(),
        }))
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
            // A comment typed meanwhile would be on a line the drop may take away.
            KeyCode::Char(' ' | 'D' | 'm' | 'i') | KeyCode::Enter if self.dropping => {
                return Outcome::Say("Still dropping the marked hunks…".into(), false)
            }
            KeyCode::Char(' ') => return self.toggle(),
            KeyCode::Char('D') => {
                if self.marked.is_empty() {
                    return Outcome::Say(
                        "No hunks marked: space marks the one under the cursor.".into(),
                        true,
                    );
                }
                // amux on a remote run's machine checks that itself, with its state.
                if let Some(why) = self
                    .remote
                    .is_none()
                    .then(|| hunks::busy(&self.run))
                    .flatten()
                {
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
        match self.add_comment(&at, &line.text, note) {
            Ok(()) => {
                self.comments = self.load_comments();
                let n = self.pending();
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
        let done = match &self.remote {
            None => {
                let mut i = 0;
                self.comments.retain(|_| {
                    let keep = !mine.contains(&i);
                    i += 1;
                    keep
                });
                review::save(&self.id, &self.comments).map(|()| mine.len())
            }
            // `amux comments --rm` there, numbered as the comments are there now (one
            // added, removed or sent since they were read here would shift or change
            // them), last first so the numbers stay right.
            Some(r) => {
                let gone: Vec<Comment> = mine.iter().map(|i| self.comments[*i].clone()).collect();
                let done = r.comments().and_then(|now| {
                    let at: Vec<usize> = now
                        .iter()
                        .enumerate()
                        .filter(|(_, c)| gone.contains(c))
                        .map(|(i, _)| i)
                        .collect();
                    at.iter().rev().try_for_each(|i| {
                        r.output(&["comments", &r.run, "--rm", &(i + 1).to_string()])
                            .map(|_| ())
                    })?;
                    Ok(at.len())
                });
                self.comments = self.load_comments();
                done
            }
        };
        match done {
            Ok(n) => Outcome::Say(
                format!("Deleted {n} comment{}.", if n == 1 { "" } else { "s" }),
                false,
            ),
            Err(e) => Outcome::Say(format!("{e:#}"), true),
        }
    }

    /// After sending: reload, so the comments show as sent.
    pub fn reload_comments(&mut self) {
        self.comments = self.load_comments();
    }

    pub fn footer(&self, t: &Theme) -> Line<'static> {
        if self.dropping {
            let (h, f) = self.marked_counts();
            let mut v = vec![span(
                format!(
                    " ⋯ Dropping {} from {}'s worktree (git apply, then commit)… ",
                    hunks::counted(h, f),
                    self.id
                ),
                t.title,
            )];
            v.extend(hints(t, &[("j k", "move"), ("Esc", "close")]));
            return Line::from(v);
        }
        if self.confirm {
            let (h, f) = self.marked_counts();
            // A remote run's pids are on its machine, so `state()` can't be asked here:
            // it is live there unless it was listed as ended.
            let live = match self.remote {
                None => self.run.state() == crate::runs::State::Running,
                Some(_) => self.run.exit_code.is_none() && self.run.error.is_none(),
            };
            let tell = if live { " and tell its agent" } else { "" };
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

#[cfg(test)]
mod tests {
    use super::*;

    fn text(l: &Line) -> String {
        l.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn drops_in_the_background_and_says_so_meanwhile() {
        let changes = review::Changes {
            dir: std::env::temp_dir(),
            text: "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1,2 +1,2 @@\n a\n-b\n+c\n"
                .into(),
            untracked: vec![],
        };
        let run = Run {
            id: "dr".into(),
            ..Default::default()
        };
        let lines = review::parse_diff(&changes.text);
        let hunks = hunks::parse(&changes);
        let cursor = lines.iter().position(|l| l.anchor().is_some()).unwrap();
        let mut v = ReviewView {
            id: run.id.clone(),
            run,
            changes,
            lines,
            hunks,
            marked: BTreeSet::from([0]),
            confirm: true,
            dropping: false,
            comments: vec![],
            cursor,
            top: 0,
            input: None,
            area: Rect::default(),
            remote: None,
        };
        let t = Theme::built_in("default").unwrap();
        // y hands the work to the app instead of doing it in the key handler.
        let job = match v.key(KeyEvent::from(KeyCode::Char('y'))) {
            Outcome::Drop(job) => job,
            _ => panic!("no drop job"),
        };
        assert_eq!(job.picked.len(), 1);
        assert!(v.dropping);
        let f = text(&v.footer(&t));
        assert!(
            f.contains("Dropping 1 hunk in 1 file from dr's worktree"),
            "{f}"
        );
        // Marking or dropping again waits for it.
        assert!(matches!(
            v.key(KeyEvent::from(KeyCode::Char('D'))),
            Outcome::Say(..)
        ));
        assert_eq!(v.marked.len(), 1);
    }
}
