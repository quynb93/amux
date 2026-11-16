//! `amux ui`: a terminal UI over parallel runs. A list of runs with live state, a live
//! preview of the selected run's screen, attaching in place (Ctrl-] comes back), keys to
//! spawn, stop and remove runs, a command palette (`/`) and themes.
//!
//! The UI is only a client. Runs are owned by their `amux __run-host` processes, so
//! quitting the UI (or a crash) leaves every run going. Actions run the same `amux spawn`,
//! `stop` and `rm` commands you would type, in the background, so they behave exactly as
//! they do in a shell and never block the screen.

pub mod attach;
mod draw;
pub mod palette;
pub mod preview;
pub mod spawn;
pub mod status;
pub mod theme;

use crate::runs::{self, Run};
use crate::store;
use crate::time;
use anyhow::{bail, Result};
use palette::{Action, Ctx};
use preview::Previews;
use ratatui::backend::{Backend, TestBackend};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::crossterm::{cursor, execute, terminal};
use ratatui::layout::Rect;
use ratatui::widgets::ListState;
use ratatui::Terminal;
use spawn::{Field, SpawnForm};
use status::{classify, last_output, next_attention, Kind, Show};
use std::collections::{HashMap, HashSet};
use std::io::{self, IsTerminal, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};
use theme::Theme;

/// How often run.json files are re-read.
const RELOAD_EVERY: Duration = Duration::from_millis(500);
/// Previews kept connected at once. Only these hold a socket, however many runs exist.
const PREVIEW_CACHE: usize = 4;

const USAGE: &str =
    "amux ui [--dump [--ansi] [--size <rows>x<cols>] [--select <run>] [--keys <keys>]]";

pub fn ui_cmd(argv: &[String]) -> Result<i32> {
    let mut dump = false;
    let mut ansi = false;
    let mut size = (30u16, 100u16);
    let mut select = None;
    let mut keys = String::new();
    let mut it = argv.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--dump" => dump = true,
            "--ansi" => ansi = true,
            "--size" => {
                size = it
                    .next()
                    .and_then(|s| parse_dims(s))
                    .ok_or_else(|| anyhow::anyhow!("--size takes <rows>x<cols>. Usage: {USAGE}"))?
            }
            "--select" => select = it.next().cloned(),
            "--keys" => keys = it.next().cloned().unwrap_or_default(),
            "--help" | "-h" => {
                println!("{USAGE}\n\nWatch, attach to, start and stop runs in one screen. Press / for commands, ? for keys.\n--dump prints one frame instead (--ansi in color, --keys presses keys first).");
                return Ok(0);
            }
            _ => bail!("Unknown option {a}. Usage: {USAGE}"),
        }
    }
    let (theme, notes) = Theme::from_config();
    let mut app = App::new(theme);
    if !notes.is_empty() {
        app.say(format!("Theme: {}", notes.join("; ")), true);
    }
    app.reload();
    if let Some(id) = select {
        app.select_id(&runs::find(&id)?.id);
    }
    if dump {
        // Keys to press first, to show a dialog: \n is Enter, \t Tab, \x1b Esc.
        for c in keys.chars() {
            let code = match c {
                '\n' | '\r' => KeyCode::Enter,
                '\t' => KeyCode::Tab,
                '\x1b' => KeyCode::Esc,
                c => KeyCode::Char(c),
            };
            app.key(KeyEvent::new(code, KeyModifiers::NONE));
        }
        // Let commands those keys started (/accounts, e…) finish, to show their result.
        let deadline = Instant::now() + Duration::from_secs(5);
        while app.busy > 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
            app.poll_done();
        }
        app.show_deferred();
        // One frame as text (with --ansi, in color), for scripts, tests and screenshots.
        for line in app.dump(size.0, size.1, ansi) {
            println!("{line}");
        }
        return Ok(0);
    }
    if !(io::stdin().is_terminal() && io::stdout().is_terminal()) {
        bail!("amux ui needs a terminal. Use `amux ps` in scripts.");
    }
    app.guard = true;
    let mut term = ratatui::try_init()?;
    let r = app.run(&mut term);
    ratatui::restore();
    r.map(|_| 0)
}

/// Keys that do something in the list.
fn is_shortcut(c: char) -> bool {
    c.is_ascii_digit()
        || matches!(
            c,
            'q' | 'j'
                | 'k'
                | 'g'
                | 'G'
                | 'a'
                | 'n'
                | 's'
                | 'S'
                | 'd'
                | 'D'
                | 'x'
                | 'f'
                | 'e'
                | '/'
                | ':'
                | '?'
        )
}

fn parse_dims(s: &str) -> Option<(u16, u16)> {
    let (r, c) = s.split_once('x')?;
    let (r, c) = (r.parse().ok()?, c.parse().ok()?);
    (r > 0 && c > 0).then_some((r, c))
}

/// A run as the list shows it.
struct Row {
    run: Run,
    kind: Kind,
    label: String,
}

enum Mode {
    Normal,
    Spawn(Box<SpawnForm>),
    Confirm(Confirm),
    Help,
    Palette(PaletteState),
    Output(OutputView),
}

/// The command palette while it is open.
struct PaletteState {
    input: String,
    /// The highlighted line.
    sel: usize,
    /// What arguments complete from, read when the palette opened.
    ctx: Ctx,
}

/// A command's output, shown over the list (`/accounts`, `/doctor`, `/theme`).
struct OutputView {
    title: String,
    lines: Vec<String>,
    scroll: usize,
}

/// Work that needs the whole terminal, done between frames.
enum Pending {
    Attach(String),
    /// `amux <argv>` in the foreground, such as `account add` (it signs you in). With
    /// `pause`, waits for Enter afterwards so you can read what it printed.
    Amux {
        argv: Vec<String>,
        pause: bool,
    },
    /// A run's output in your pager.
    Logs(String),
}

#[derive(Clone)]
enum Confirm {
    Stop {
        id: String,
        force: bool,
    },
    Remove {
        id: String,
        force: bool,
    },
    /// Every finished run.
    Clean,
}

impl Confirm {
    fn question(&self) -> String {
        match self {
            Confirm::Stop { id, force: false } => format!("Stop {id}? (SIGTERM)"),
            Confirm::Stop { id, force: true } => format!("Kill {id}? (SIGKILL)"),
            Confirm::Remove { id, force: false } => {
                format!("Remove {id}? Its worktree goes only if it has no uncommitted changes")
            }
            Confirm::Remove { id, force: true } => format!("Kill and remove {id}?"),
            Confirm::Clean => {
                "Remove every finished run? Worktrees go only if they have no uncommitted changes"
                    .into()
            }
        }
    }

    fn argv(&self) -> Vec<String> {
        let (cmd, id, force) = match self {
            Confirm::Stop { id, force } => ("stop", id, *force),
            Confirm::Remove { id, force } => ("rm", id, *force),
            Confirm::Clean => return vec!["rm".into(), "--done".into()],
        };
        let mut v = vec![cmd.to_string(), id.clone()];
        if force {
            v.push("--force".into());
        }
        v
    }
}

/// A background `amux` command finished.
struct Done {
    ok: bool,
    message: String,
    /// For a spawn: the new run, to select it.
    started: Option<String>,
    /// For a command whose whole output is shown: a title and the text.
    output: Option<(String, String)>,
}

struct App {
    theme: Theme,
    /// The runs shown (those matching `filter`).
    rows: Vec<Row>,
    /// All runs, matching or not.
    total: usize,
    /// Shows only runs whose name, account, branch or folder contains this.
    filter: String,
    /// Shows only runs in this state (`f`, /show).
    show: Show,
    /// The account new runs start with, from /switch-account.
    account: Option<String>,
    selected: Option<String>,
    list: ListState,
    previews: Previews,
    mode: Mode,
    status: Option<(String, bool, Instant)>,
    /// Runs that changed to need you (or finished a turn) while you looked elsewhere.
    unseen: HashSet<String>,
    last_kind: HashMap<String, Kind>,
    tx: mpsc::Sender<Done>,
    rx: mpsc::Receiver<Done>,
    busy: usize,
    last_reload: Option<Instant>,
    /// Lines scrolled back in the preview.
    scroll: usize,
    pending: Option<Pending>,
    /// Command output that finished while a dialog was open.
    deferred: Option<OutputView>,
    quit: bool,
    started: Instant,
    /// Guards against text typed into the list by mistake (see `key_normal`). Off for
    /// `--dump`, where keys come all at once.
    guard: bool,
    /// Until then, letters are taken as typing, not shortcuts.
    typing_until: Option<Instant>,
    /// When the open confirmation appeared.
    confirm_at: Instant,
}

/// How long letters count as typing after one that isn't a shortcut.
const TYPING: Duration = Duration::from_millis(1200);
/// Keys this soon after a confirmation opens were typed before you could read it.
const CONFIRM_GUARD: Duration = Duration::from_millis(500);

impl App {
    fn new(theme: Theme) -> Self {
        let (tx, rx) = mpsc::channel();
        App {
            theme,
            rows: vec![],
            total: 0,
            filter: String::new(),
            show: Show::All,
            account: None,
            selected: None,
            list: ListState::default(),
            previews: Previews::new(PREVIEW_CACHE),
            mode: Mode::Normal,
            status: None,
            unseen: HashSet::new(),
            last_kind: HashMap::new(),
            tx,
            rx,
            busy: 0,
            last_reload: None,
            scroll: 0,
            pending: None,
            deferred: None,
            quit: false,
            started: Instant::now(),
            guard: false,
            typing_until: None,
            confirm_at: Instant::now(),
        }
    }

    fn reload(&mut self) {
        let now = time::now_ms();
        let all = runs::list();
        self.total = all.len();
        let filter = self.filter.to_lowercase();
        let show = self.show;
        self.rows = all
            .into_iter()
            .filter(|run| filter.is_empty() || matches_filter(run, &filter))
            .map(|run| {
                let (kind, label) = classify(
                    &run.state(),
                    run.waiting.as_ref(),
                    run.status.as_ref(),
                    last_output(&run),
                    now,
                );
                Row { run, kind, label }
            })
            .filter(|r| show.matches(r.kind))
            .collect();
        let first = self.last_reload.is_none();
        for r in &self.rows {
            let prev = self.last_kind.insert(r.run.id.clone(), r.kind);
            let changed = prev.is_some_and(|p| p != r.kind);
            let notable = matches!(r.kind, Kind::Attention | Kind::Done | Kind::Failed)
                || (r.kind == Kind::Idle && prev == Some(Kind::Working));
            if !first && changed && notable && self.selected.as_deref() != Some(&r.run.id) {
                self.unseen.insert(r.run.id.clone());
            }
        }
        let ids: HashSet<&str> = self.rows.iter().map(|r| r.run.id.as_str()).collect();
        self.unseen.retain(|id| ids.contains(id.as_str()));
        self.last_kind.retain(|id, _| ids.contains(id.as_str()));
        // Keep the selection on the same run; if it went away, on the same position.
        let keep = self.selected.as_ref().and_then(|id| self.index_of(id));
        let idx = keep.or_else(|| {
            (!self.rows.is_empty())
                .then(|| self.list.selected().unwrap_or(0).min(self.rows.len() - 1))
        });
        self.set_index(idx);
        self.last_reload = Some(Instant::now());
        if let Some(r) = self.current() {
            let run = r.run.clone();
            if let Some(p) = self.previews.get(&run, r.kind.is_live()) {
                p.follow_size(&run);
            }
        }
    }

    fn index_of(&self, id: &str) -> Option<usize> {
        self.rows.iter().position(|r| r.run.id == id)
    }

    fn current(&self) -> Option<&Row> {
        self.list.selected().and_then(|i| self.rows.get(i))
    }

    fn set_index(&mut self, idx: Option<usize>) {
        let new = idx.and_then(|i| self.rows.get(i)).map(|r| r.run.id.clone());
        if new != self.selected {
            self.scroll = 0;
            if let Some(old) = &self.selected {
                if let Some(p) = self.previews.find(old) {
                    p.lock().parser.screen_mut().set_scrollback(0);
                }
            }
        }
        if let Some(id) = &new {
            self.unseen.remove(id);
        }
        self.selected = new;
        self.list.select(idx);
    }

    fn select_id(&mut self, id: &str) {
        let i = self.index_of(id);
        self.set_index(i);
    }

    fn move_by(&mut self, d: isize) {
        if self.rows.is_empty() {
            return;
        }
        let n = self.rows.len() as isize;
        let i = self.list.selected().unwrap_or(0) as isize;
        self.set_index(Some((i + d).clamp(0, n - 1) as usize));
    }

    fn say(&mut self, msg: impl Into<String>, error: bool) {
        self.status = Some((msg.into(), error, Instant::now()));
    }

    /// Runs `amux <argv>` in the background; the result arrives as a `Done`.
    fn background(&mut self, argv: Vec<String>, what: String) {
        self.busy += 1;
        self.say(format!("{what}…"), false);
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(run_amux(&argv));
        });
    }

    /// Runs `amux <argv>` in the background and shows all it prints under `title`.
    fn background_output(&mut self, argv: Vec<String>, title: String) {
        self.busy += 1;
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let (ok, text) = capture_amux(&argv);
            let _ = tx.send(Done {
                ok,
                message: String::new(),
                started: None,
                output: Some((title, text)),
            });
        });
    }

    fn poll_done(&mut self) -> bool {
        let mut any = false;
        while let Ok(d) = self.rx.try_recv() {
            any = true;
            self.busy = self.busy.saturating_sub(1);
            if let Some((title, text)) = d.output {
                let view = OutputView {
                    title,
                    lines: text.lines().map(str::to_string).collect(),
                    scroll: 0,
                };
                // Shown now, or once the dialog that is open closes.
                if matches!(self.mode, Mode::Normal | Mode::Palette(_)) {
                    self.mode = Mode::Output(view);
                } else {
                    self.deferred = Some(view);
                }
            } else {
                self.say(d.message, !d.ok);
            }
            self.reload();
            if let Some(id) = d.started {
                self.select_id(&id);
            }
        }
        any
    }

    /// Shows command output that arrived while a dialog was open, once it has closed.
    fn show_deferred(&mut self) -> bool {
        if !matches!(self.mode, Mode::Normal) {
            return false;
        }
        match self.deferred.take() {
            Some(view) => {
                self.mode = Mode::Output(view);
                true
            }
            None => false,
        }
    }

    fn run<B: Backend>(&mut self, term: &mut Terminal<B>) -> Result<()>
    where
        B::Error: Send + Sync + 'static,
    {
        let mut dirty = true;
        let mut last_draw = Instant::now();
        while !self.quit {
            dirty |= self.poll_done();
            dirty |= self.show_deferred();
            if self.last_reload.is_none_or(|t| t.elapsed() >= RELOAD_EVERY) {
                self.reload();
                dirty = true;
            }
            dirty |= self.previews.take_dirty();
            // The spinner and ages move on their own.
            let animate = self.rows.iter().any(|r| r.kind == Kind::Working);
            if dirty || (animate && last_draw.elapsed() >= Duration::from_millis(100)) {
                term.draw(|f| self.draw(f))?;
                last_draw = Instant::now();
                dirty = false;
            }
            if event::poll(Duration::from_millis(40))? {
                match event::read()? {
                    Event::Key(k) if k.kind != KeyEventKind::Release => self.key(k),
                    _ => {}
                }
                dirty = true;
            }
            match self.pending.take() {
                Some(Pending::Attach(id)) => {
                    self.attach_in_place(&id, term)?;
                    dirty = true;
                }
                Some(job) => {
                    self.foreground(job, term)?;
                    dirty = true;
                }
                None => {}
            }
        }
        Ok(())
    }

    fn attach_in_place<B: Backend>(&mut self, id: &str, term: &mut Terminal<B>) -> Result<()>
    where
        B::Error: Send + Sync + 'static,
    {
        let Some(row) = self.rows.iter().find(|r| r.run.id == id) else {
            return Ok(());
        };
        let run = row.run.clone();
        let live = self.previews.get(&run, true).is_some_and(|p| p.is_live());
        if !live {
            self.say(format!("{id} is not running."), true);
            return Ok(());
        }
        let mut out = io::stdout();
        execute!(out, terminal::LeaveAlternateScreen, cursor::Show)?;
        let outcome = match self.previews.find(id) {
            Some(p) => attach::attach_in_place(p),
            None => Ok(attach::Outcome::Ended),
        };
        execute!(out, terminal::EnterAlternateScreen, cursor::Hide)?;
        // Repaint everything. Not `clear()`: it asks the terminal where the cursor is and
        // waits for the answer.
        let size = term.size()?;
        term.resize(Rect::new(0, 0, size.width, size.height))?;
        match outcome? {
            attach::Outcome::Detached => self.say(format!("Detached from {id}."), false),
            attach::Outcome::Ended => {
                self.previews.forget(id);
                self.say(format!("{id} ended."), false)
            }
        }
        self.reload();
        Ok(())
    }

    /// Gives the whole terminal to a command (sign-in, pager), then takes it back.
    fn foreground<B: Backend>(&mut self, job: Pending, term: &mut Terminal<B>) -> Result<()>
    where
        B::Error: Send + Sync + 'static,
    {
        let exe = std::env::current_exe()?;
        let (mut cmd, pause, what) = match job {
            Pending::Amux { argv, pause } => {
                let what = format!("amux {}", argv.join(" "));
                let mut c = Command::new(&exe);
                c.args(&argv);
                (c, pause, what)
            }
            Pending::Logs(id) => {
                let mut c = Command::new("sh");
                c.args(["-c", "\"$0\" logs \"$1\" | ${PAGER:-less -R}"])
                    .arg(&exe)
                    .arg(&id);
                (c, false, format!("logs of {id}"))
            }
            Pending::Attach(_) => return Ok(()),
        };
        let mut out = io::stdout();
        execute!(out, terminal::LeaveAlternateScreen, cursor::Show)?;
        terminal::disable_raw_mode()?;
        let status = cmd.env("AMUX_NO_UPDATE_CHECK", "1").status();
        if pause {
            print!("\n  Press Enter to return to amux ui ");
            let _ = out.flush();
            let _ = io::stdin().read_line(&mut String::new());
        }
        terminal::enable_raw_mode()?;
        execute!(out, terminal::EnterAlternateScreen, cursor::Hide)?;
        let size = term.size()?;
        term.resize(Rect::new(0, 0, size.width, size.height))?;
        match status {
            Ok(s) if s.success() => self.say(format!("Back from {what}."), false),
            Ok(s) => self.say(
                format!("{what} ended with code {}.", s.code().unwrap_or(1)),
                true,
            ),
            Err(e) => self.say(format!("Could not run {what}: {e}"), true),
        }
        self.reload();
        Ok(())
    }

    fn key(&mut self, k: KeyEvent) {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let typing = matches!(self.mode, Mode::Spawn(_) | Mode::Palette(_));
        if ctrl && k.code == KeyCode::Char('c') && !typing {
            self.quit = true;
            return;
        }
        let was_confirm = matches!(self.mode, Mode::Confirm(_));
        self.key_mode(k);
        if !was_confirm && matches!(self.mode, Mode::Confirm(_)) {
            self.confirm_at = Instant::now();
        }
    }

    fn key_mode(&mut self, k: KeyEvent) {
        match std::mem::replace(&mut self.mode, Mode::Normal) {
            Mode::Normal => self.key_normal(k),
            Mode::Help => {}
            Mode::Palette(p) => {
                if let Some(p) = self.key_palette(p, k) {
                    self.mode = Mode::Palette(p);
                }
            }
            Mode::Output(mut o) => {
                let max = o.lines.len().saturating_sub(1);
                match k.code {
                    KeyCode::Down | KeyCode::Char('j') => o.scroll = (o.scroll + 1).min(max),
                    KeyCode::Up | KeyCode::Char('k') => o.scroll = o.scroll.saturating_sub(1),
                    KeyCode::PageDown => o.scroll = (o.scroll + 10).min(max),
                    KeyCode::PageUp => o.scroll = o.scroll.saturating_sub(10),
                    _ => return,
                }
                self.mode = Mode::Output(o);
            }
            // Typed ahead, before the question could be read: neither yes nor no.
            Mode::Confirm(c) if self.guard && self.confirm_at.elapsed() < CONFIRM_GUARD => {
                self.mode = Mode::Confirm(c);
            }
            Mode::Confirm(c) => match k.code {
                KeyCode::Char('y' | 'Y') | KeyCode::Enter => {
                    let what = c.question();
                    self.background(c.argv(), what.trim_end_matches('?').to_string());
                }
                _ => self.say("Cancelled.", false),
            },
            Mode::Spawn(mut f) => {
                if self.key_spawn(&mut f, k) {
                    self.mode = Mode::Spawn(f);
                }
            }
        }
    }

    fn key_normal(&mut self, k: KeyEvent) {
        let cur = self.current().map(|r| (r.run.id.clone(), r.kind));
        // Letters in the list are shortcuts, so text meant for a run would open dialogs
        // and panels (and "sky" would stop it). A letter that isn't a shortcut starts a
        // short typing window: until it ends, letters only repeat how to type into the run.
        if let KeyCode::Char(c) = k.code {
            let plain = !k
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
            let typing = self.typing_until.is_some_and(|t| Instant::now() < t);
            if plain && self.guard && (typing || !is_shortcut(c)) {
                self.typing_until = Some(Instant::now() + TYPING);
                let hint = match &cur {
                    Some((id, kind)) if kind.is_live() => {
                        format!("Letters here are shortcuts. Press ⏎ to type into {id}; Ctrl-] comes back.")
                    }
                    _ => "Letters here are shortcuts; ? lists them.".to_string(),
                };
                self.say(hint, false);
                return;
            }
        }
        match k.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('j') | KeyCode::Down => self.move_by(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_by(-1),
            KeyCode::Char('g') | KeyCode::Home => {
                self.set_index((!self.rows.is_empty()).then_some(0))
            }
            KeyCode::Char('G') | KeyCode::End => self.set_index(self.rows.len().checked_sub(1)),
            KeyCode::Char(c @ '1'..='9') => {
                let i = c as usize - '1' as usize;
                if i < self.rows.len() {
                    self.set_index(Some(i));
                }
            }
            KeyCode::Tab => {
                let kinds: Vec<Kind> = self.rows.iter().map(|r| r.kind).collect();
                // Unseen runs first: they changed while you were away.
                let unseen = self
                    .rows
                    .iter()
                    .position(|r| self.unseen.contains(&r.run.id));
                let from = self.list.selected().unwrap_or(0);
                match unseen.or_else(|| next_attention(&kinds, from)) {
                    Some(i) => self.set_index(Some(i)),
                    None => self.say("Nothing needs you.", false),
                }
            }
            KeyCode::Enter | KeyCode::Char('a') => {
                if let Some((id, kind)) = cur {
                    self.attach_run(id, kind)
                }
            }
            KeyCode::Char('n') => self.open_spawn(None, ""),
            KeyCode::Char(c @ ('s' | 'S')) => {
                if let Some((id, kind)) = cur {
                    self.stop_run(id, kind, c == 'S')
                }
            }
            KeyCode::Char(c @ ('d' | 'D' | 'x')) => {
                if let Some((id, kind)) = cur {
                    self.remove_run(id, kind, c == 'D')
                }
            }
            KeyCode::Char('/' | ':') => self.open_palette(),
            KeyCode::Char('f') => {
                self.show = self.show.next();
                self.reload();
            }
            KeyCode::Char('e') => {
                if let Some((id, _)) = cur {
                    self.explain(id)
                }
            }
            KeyCode::Esc if self.scroll == 0 && self.filtered() => {
                self.filter.clear();
                self.show = Show::All;
                self.reload();
            }
            KeyCode::PageUp | KeyCode::PageDown | KeyCode::Esc => {
                self.scroll = match k.code {
                    KeyCode::PageUp => self.scroll + 10,
                    KeyCode::PageDown => self.scroll.saturating_sub(10),
                    _ => 0,
                };
                if let Some(p) = self.selected.as_ref().and_then(|id| self.previews.find(id)) {
                    let mut s = p.lock();
                    s.parser.screen_mut().set_scrollback(self.scroll);
                    // The emulator caps it at the history it has.
                    self.scroll = s.parser.screen().scrollback();
                }
            }
            KeyCode::Char('?') => self.mode = Mode::Help,
            _ => {}
        }
    }

    /// Whether the list hides runs (a text or state filter).
    fn filtered(&self) -> bool {
        !self.filter.is_empty() || self.show != Show::All
    }

    fn explain(&mut self, id: String) {
        let label = self
            .rows
            .iter()
            .find(|r| r.run.id == id)
            .map(|r| r.label.clone())
            .unwrap_or_default();
        self.background_output(
            vec!["explain".into(), id.clone()],
            format!("Why {id} is {label}"),
        );
    }

    fn attach_run(&mut self, id: String, kind: Kind) {
        if kind.is_live() {
            self.pending = Some(Pending::Attach(id));
        } else {
            self.say(format!("{id} has ended; d removes it."), true);
        }
    }

    fn stop_run(&mut self, id: String, kind: Kind, force: bool) {
        if kind.is_live() {
            self.mode = Mode::Confirm(Confirm::Stop { id, force });
        } else {
            self.say(format!("{id} is not running."), true);
        }
    }

    fn remove_run(&mut self, id: String, kind: Kind, force: bool) {
        if kind.is_live() && !force {
            self.say(
                format!("{id} is still running. s stops it; D kills and removes it."),
                true,
            );
        } else {
            self.mode = Mode::Confirm(Confirm::Remove { id, force });
        }
    }

    fn open_spawn(&mut self, account: Option<&str>, prompt: &str) {
        let accounts = account_ids();
        if accounts.is_empty() {
            self.say(
                "No accounts yet. Add one with /add-account <provider> <name>.",
                true,
            );
            return;
        }
        // The account asked for, else the one you switched to, else the selected run's.
        let preferred = account
            .map(str::to_string)
            .or_else(|| self.account.clone())
            .or_else(|| self.current().map(|r| r.run.account.clone()));
        if let Some(a) = account.filter(|a| !accounts.iter().any(|x| x == a)) {
            self.say(format!("No account \"{a}\". /accounts lists them."), true);
            return;
        }
        let cwd = std::env::current_dir().unwrap_or_default();
        let mut form = SpawnForm::new(accounts, preferred.as_deref(), in_git_repo(&cwd));
        form.prompt = prompt.to_string();
        self.mode = Mode::Spawn(Box::new(form));
    }

    fn open_palette(&mut self) {
        self.mode = Mode::Palette(PaletteState {
            input: "/".into(),
            sel: 0,
            ctx: Ctx {
                runs: self.rows.iter().map(|r| r.run.id.clone()).collect(),
                accounts: account_ids(),
                themes: theme::available(),
            },
        });
    }

    /// Returns the palette if it stays open.
    fn key_palette(&mut self, mut p: PaletteState, k: KeyEvent) -> Option<PaletteState> {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let items = palette::items(&p.input, &p.ctx);
        let last = items.len().saturating_sub(1);
        match k.code {
            KeyCode::Esc => return None,
            KeyCode::Char('c') if ctrl => return None,
            KeyCode::Up => p.sel = p.sel.saturating_sub(1),
            KeyCode::Char('p') if ctrl => p.sel = p.sel.saturating_sub(1),
            KeyCode::Down => p.sel = (p.sel + 1).min(last),
            KeyCode::Char('n') if ctrl => p.sel = (p.sel + 1).min(last),
            KeyCode::Tab => {
                if let Some(it) = items.get(p.sel) {
                    p.input = it.fill.clone();
                    p.sel = 0;
                }
            }
            KeyCode::Enter => {
                let line = match items.get(p.sel) {
                    // A command that needs an argument: fill it in and wait for it.
                    Some(it) if !it.complete => {
                        p.input = it.fill.clone();
                        p.sel = 0;
                        return Some(p);
                    }
                    Some(it) => it.fill.clone(),
                    None => p.input.clone(),
                };
                return match palette::parse(&line) {
                    Ok(action) => {
                        self.run_action(action);
                        None
                    }
                    Err(e) => {
                        self.say(e, true);
                        Some(p)
                    }
                };
            }
            KeyCode::Backspace => {
                p.input.pop();
                if p.input.is_empty() {
                    return None;
                }
                p.sel = 0;
            }
            KeyCode::Char('w') if ctrl => {
                spawn::delete_word(&mut p.input);
                if p.input.is_empty() {
                    p.input.push('/');
                }
                p.sel = 0;
            }
            KeyCode::Char('u') if ctrl => {
                p.input = "/".into();
                p.sel = 0;
            }
            KeyCode::Char(c) if !ctrl => {
                p.input.push(c);
                p.sel = 0;
            }
            _ => {}
        }
        Some(p)
    }

    /// A run by name or unique prefix among those shown; the selected one when None.
    fn resolve_run(&mut self, name: Option<String>) -> Option<(String, Kind)> {
        let Some(name) = name else {
            let cur = self.current().map(|r| (r.run.id.clone(), r.kind));
            if cur.is_none() {
                self.say("No run selected.", true);
            }
            return cur;
        };
        let exact = self.rows.iter().find(|r| r.run.id == name);
        let mut m = self.rows.iter().filter(|r| r.run.id.starts_with(&name));
        let found = exact.or_else(|| match (m.next(), m.next()) {
            (Some(r), None) => Some(r),
            _ => None,
        });
        match found {
            Some(r) => Some((r.run.id.clone(), r.kind)),
            None => {
                self.say(format!("No run matches \"{name}\"."), true);
                None
            }
        }
    }

    fn run_action(&mut self, action: Action) {
        match action {
            Action::New { account, prompt } => self.open_spawn(account.as_deref(), &prompt),
            Action::Attach(r) => {
                if let Some((id, kind)) = self.resolve_run(r) {
                    self.attach_run(id, kind)
                }
            }
            Action::Stop(r) => {
                if let Some((id, kind)) = self.resolve_run(r) {
                    self.stop_run(id, kind, false)
                }
            }
            Action::Kill(r) => {
                if let Some((id, kind)) = self.resolve_run(r) {
                    self.stop_run(id, kind, true)
                }
            }
            Action::Remove(r) => {
                if let Some((id, kind)) = self.resolve_run(r) {
                    self.remove_run(id, kind, false)
                }
            }
            Action::Clean => self.mode = Mode::Confirm(Confirm::Clean),
            Action::Logs(r) => {
                if let Some((id, _)) = self.resolve_run(r) {
                    self.pending = Some(Pending::Logs(id));
                }
            }
            Action::Filter(text) => {
                self.filter = text;
                self.reload();
                if !self.filter.is_empty() {
                    self.say(
                        format!("{} of {} runs match.", self.rows.len(), self.total),
                        false,
                    );
                }
            }
            Action::SwitchAccount(a) => {
                if !account_ids().contains(&a) {
                    self.say(format!("No account \"{a}\". /accounts lists them."), true);
                    return;
                }
                self.account = Some(a.clone());
                self.background(
                    vec!["account".into(), "default".into(), a.clone()],
                    format!("Switching to {a}"),
                );
            }
            Action::AddAccount(args) => {
                let mut argv = vec!["account".to_string(), "add".to_string()];
                argv.extend(args);
                self.pending = Some(Pending::Amux { argv, pause: true });
            }
            Action::Accounts => {
                self.background_output(vec!["account".into(), "list".into()], "Accounts".into())
            }
            Action::Doctor => self.background_output(vec!["doctor".into()], "Doctor".into()),
            Action::Theme(None) => {
                let mut lines: Vec<String> = theme::available()
                    .into_iter()
                    .map(|n| {
                        let mark = if n == self.theme.name { "●" } else { "○" };
                        format!("{mark} {n}")
                    })
                    .collect();
                lines.extend([
                    String::new(),
                    "/theme <name> switches and saves it in config.json.".into(),
                    format!(
                        "Your own: {}/<name>.json",
                        crate::paths::tildify(&theme::themes_dir())
                    ),
                    "  { \"extends\": \"dark\", \"colors\": { \"accent\": \"#ff9e64 bold\" } }"
                        .into(),
                    String::new(),
                    "Roles:".into(),
                ]);
                lines.extend(
                    theme::ROLES
                        .chunks(6)
                        .map(|c| format!("  {}", c.join(", "))),
                );
                self.mode = Mode::Output(OutputView {
                    title: "Themes".into(),
                    lines,
                    scroll: 0,
                });
            }
            Action::Theme(Some(name)) => match Theme::named(&name) {
                Ok((t, notes)) => match Theme::save_choice(&name) {
                    Err(e) => {
                        self.theme = t;
                        self.say(format!("Theme {name}, but not saved: {e:#}"), true)
                    }
                    Ok(()) => {
                        // Read back as a restart would, so ui.colors overrides apply too.
                        let (t, more) = Theme::from_config();
                        self.theme = t;
                        let notes: Vec<String> = notes.into_iter().chain(more).collect();
                        if notes.is_empty() {
                            self.say(format!("Theme {name}, saved in config.json."), false)
                        } else {
                            self.say(format!("Theme {name}: {}", notes.join("; ")), true)
                        }
                    }
                },
                Err(e) => self.say(e, true),
            },
            Action::Explain(r) => {
                if let Some((id, _)) = self.resolve_run(r) {
                    self.explain(id)
                }
            }
            Action::Show(state) => {
                let next = match state {
                    None => Some(self.show.next()),
                    Some(s) => Show::parse(&s),
                };
                match next {
                    Some(s) => {
                        self.show = s;
                        self.reload();
                    }
                    None => self.say(
                        "Show which? waiting, working, idle, ended or all.".to_string(),
                        true,
                    ),
                }
            }
            Action::Help => self.mode = Mode::Help,
            Action::Quit => self.quit = true,
        }
    }

    /// Returns whether the form stays open.
    fn key_spawn(&mut self, f: &mut SpawnForm, k: KeyEvent) -> bool {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        match k.code {
            KeyCode::Esc => return false,
            KeyCode::Char('c') if ctrl => return false,
            KeyCode::Tab | KeyCode::Down => f.next_field(false),
            KeyCode::BackTab | KeyCode::Up => f.next_field(true),
            KeyCode::Enter => {
                let provider = f.account().and_then(|a| a.split('@').next()).unwrap_or("");
                let template = store::get_provider(provider)
                    .ok()
                    .and_then(|p| p.prompt_args);
                match f.argv(template.as_deref()) {
                    Ok(argv) => {
                        let what = format!("Starting {}", f.account().unwrap_or("run"));
                        self.background(argv, what);
                        return false;
                    }
                    Err(e) => f.error = Some(e),
                }
            }
            KeyCode::Left | KeyCode::Right if f.focus == Field::Account => {
                f.cycle_account(k.code == KeyCode::Left)
            }
            KeyCode::Left | KeyCode::Right | KeyCode::Char(' ') if f.focus == Field::Worktree => {
                f.worktree = !f.worktree
            }
            KeyCode::Char('w') if ctrl => {
                if let Some(t) = f.text_mut() {
                    spawn::delete_word(t);
                }
            }
            KeyCode::Char('u') if ctrl => {
                if let Some(t) = f.text_mut() {
                    t.clear();
                }
            }
            KeyCode::Backspace => {
                if let Some(t) = f.text_mut() {
                    t.pop();
                }
            }
            KeyCode::Char(c) if !ctrl => {
                if let Some(t) = f.text_mut() {
                    t.push(c);
                }
            }
            _ => {}
        }
        true
    }

    /// One frame rendered off screen, as text. Waits briefly for the selected run's
    /// preview to receive the host's replay.
    fn dump(&mut self, rows: u16, cols: u16, ansi: bool) -> Vec<String> {
        // Previews connect while drawing, so draw once to start them, then let the hosts'
        // screens arrive before the frame that is printed.
        let Ok(mut warm) = Terminal::new(TestBackend::new(cols, rows));
        let Ok(_) = warm.draw(|f| self.draw(f));
        let deadline = Instant::now() + Duration::from_millis(1500);
        let mut quiet_since = Instant::now();
        while self.previews.any_live() && Instant::now() < deadline {
            if self.previews.take_dirty() {
                quiet_since = Instant::now();
            } else if quiet_since.elapsed() > Duration::from_millis(200) {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        // TestBackend can't fail.
        let Ok(mut term) = Terminal::new(TestBackend::new(cols, rows));
        let Ok(_) = term.draw(|f| self.draw(f));
        let buf = term.backend().buffer();
        if ansi {
            return (0..rows).map(|y| ansi_line(buf, y, cols)).collect();
        }
        (0..rows)
            .map(|y| {
                (0..cols)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }
}

/// One row of a rendered buffer with SGR escape sequences, for `--dump --ansi`.
fn ansi_line(buf: &ratatui::buffer::Buffer, y: u16, cols: u16) -> String {
    use ratatui::style::{Color, Modifier};
    fn color(c: Color, bg: bool) -> String {
        let base = if bg { 40 } else { 30 };
        match c {
            Color::Reset => format!("{}", base + 9),
            Color::Black => format!("{base}"),
            Color::Red => format!("{}", base + 1),
            Color::Green => format!("{}", base + 2),
            Color::Yellow => format!("{}", base + 3),
            Color::Blue => format!("{}", base + 4),
            Color::Magenta => format!("{}", base + 5),
            Color::Cyan => format!("{}", base + 6),
            Color::Gray => format!("{}", base + 7),
            Color::DarkGray => format!("{}", base + 60),
            Color::LightRed => format!("{}", base + 61),
            Color::LightGreen => format!("{}", base + 62),
            Color::LightYellow => format!("{}", base + 63),
            Color::LightBlue => format!("{}", base + 64),
            Color::LightMagenta => format!("{}", base + 65),
            Color::LightCyan => format!("{}", base + 66),
            Color::White => format!("{}", base + 67),
            Color::Indexed(i) => format!("{};5;{i}", base + 8),
            Color::Rgb(r, g, b) => format!("{};2;{r};{g};{b}", base + 8),
        }
    }
    let mut out = String::new();
    let mut last = None;
    for x in 0..cols {
        let cell = &buf[(x, y)];
        let key = (cell.fg, cell.bg, cell.modifier);
        if last != Some(key) {
            let mut codes = vec!["0".to_string(), color(cell.fg, false), color(cell.bg, true)];
            for (m, code) in [
                (Modifier::BOLD, "1"),
                (Modifier::DIM, "2"),
                (Modifier::ITALIC, "3"),
                (Modifier::UNDERLINED, "4"),
                (Modifier::REVERSED, "7"),
            ] {
                if cell.modifier.contains(m) {
                    codes.push(code.into());
                }
            }
            out.push_str(&format!("\x1b[{}m", codes.join(";")));
            last = Some(key);
        }
        out.push_str(cell.symbol());
    }
    out.push_str("\x1b[0m");
    out
}

fn account_ids() -> Vec<String> {
    store::load_accounts()
        .unwrap_or_default()
        .iter()
        .map(|a| a.id())
        .collect()
}

/// Whether a run's name, account, branch or folder contains `needle` (lowercase).
fn matches_filter(run: &Run, needle: &str) -> bool {
    let branch = run.worktree.as_ref().map_or("", |w| w.branch.as_str());
    [run.id.as_str(), &run.account, branch, &run.cwd]
        .iter()
        .any(|h| h.to_lowercase().contains(needle))
}

fn in_git_repo(dir: &Path) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "--is-inside-work-tree"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Runs this amux binary with `argv`, without a terminal, and sums up the result.
fn run_amux(argv: &[String]) -> Done {
    let out = std::env::current_exe().and_then(|exe| {
        Command::new(exe)
            .args(argv)
            .env("AMUX_NO_UPDATE_CHECK", "1")
            .stdin(Stdio::null())
            .output()
    });
    match out {
        Ok(o) => summarize(
            o.status.success(),
            &String::from_utf8_lossy(&o.stdout),
            &String::from_utf8_lossy(&o.stderr),
        ),
        Err(e) => Done {
            ok: false,
            message: format!("Could not run amux: {e}"),
            started: None,
            output: None,
        },
    }
}

/// Runs this amux binary with `argv` and returns everything it printed (stdout, then
/// stderr), for showing whole.
fn capture_amux(argv: &[String]) -> (bool, String) {
    let out = std::env::current_exe().and_then(|exe| {
        Command::new(exe)
            .args(argv)
            .env("AMUX_NO_UPDATE_CHECK", "1")
            .stdin(Stdio::null())
            .output()
    });
    match out {
        Ok(o) => {
            let mut text = String::from_utf8_lossy(&o.stdout).into_owned();
            text.push_str(&String::from_utf8_lossy(&o.stderr));
            (o.status.success(), text.trim_end().to_string())
        }
        Err(e) => (false, format!("Could not run amux: {e}")),
    }
}

/// The line worth showing from a command's output, and the run a spawn started.
fn summarize(ok: bool, stdout: &str, stderr: &str) -> Done {
    let lines = || stdout.lines().chain(stderr.lines()).map(str::trim);
    let started = lines().find_map(|l| {
        let rest = l.strip_prefix("amux: started ")?;
        Some(rest.split_whitespace().next()?.to_string())
    });
    let pick = if ok {
        lines().find(|l| !l.is_empty())
    } else {
        lines().rfind(|l| !l.is_empty())
    };
    let message = pick
        .unwrap_or(if ok { "Done." } else { "Failed." })
        .trim_start_matches("amux: ")
        .to_string();
    Done {
        ok,
        message,
        started,
        output: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn draws_on_tiny_terminals_without_panicking() {
        let mut app = App::new(Theme::built_in("default").unwrap());
        for i in 0..5 {
            app.rows.push(Row {
                run: Run {
                    id: format!("run-{i}"),
                    account: "fake@one".into(),
                    exit_code: Some(0),
                    ..Default::default()
                },
                kind: Kind::Done,
                label: "done".into(),
            });
        }
        app.set_index(Some(0));
        for name in theme::BUILT_IN {
            app.theme = Theme::built_in(name).unwrap();
            let modes = [
                Mode::Normal,
                Mode::Help,
                Mode::Spawn(Box::new(SpawnForm::new(
                    vec!["fake@one".into()],
                    None,
                    true,
                ))),
                Mode::Palette(PaletteState {
                    input: "/st".into(),
                    sel: 0,
                    ctx: Ctx::default(),
                }),
                Mode::Output(OutputView {
                    title: "Accounts".into(),
                    lines: vec!["a very long line ".repeat(20), "b".into()],
                    scroll: 1,
                }),
                Mode::Confirm(Confirm::Clean),
            ];
            for mode in modes {
                app.mode = mode;
                for (rows, cols) in [(1, 1), (3, 20), (9, 80), (5, 120), (40, 6), (30, 100)] {
                    let lines = app.dump(rows, cols, false);
                    assert_eq!(lines.len(), rows as usize);
                    assert_eq!(app.dump(rows, cols, true).len(), rows as usize);
                }
            }
        }
    }

    #[test]
    fn filters_runs_by_name_account_branch_or_folder() {
        let run = Run {
            id: "fix-login".into(),
            account: "claude@work".into(),
            cwd: "/src/app".into(),
            worktree: Some(runs::Worktree {
                repo: "/src/app".into(),
                path: "/x".into(),
                branch: "amux/fix-login".into(),
            }),
            ..Default::default()
        };
        for needle in ["login", "claude@", "amux/", "/src/app"] {
            assert!(matches_filter(&run, needle), "{needle}");
        }
        assert!(!matches_filter(&run, "codex"));
    }

    #[test]
    fn ansi_dump_carries_styles() {
        use ratatui::style::{Color, Style};
        let mut buf = ratatui::buffer::Buffer::empty(Rect::new(0, 0, 3, 1));
        buf.set_string(0, 0, "ab", Style::new().fg(Color::Rgb(1, 2, 3)));
        let line = ansi_line(&buf, 0, 3);
        assert!(
            line.starts_with("\x1b[0;38;2;1;2;3;49mab\x1b[0;39;49m "),
            "{line:?}"
        );
        assert!(line.ends_with("\x1b[0m"));
    }

    #[test]
    fn keeps_command_output_until_the_open_dialog_closes() {
        let mut app = App::new(Theme::built_in("default").unwrap());
        app.mode = Mode::Help;
        app.busy = 1;
        let _ = app.tx.send(Done {
            ok: true,
            message: String::new(),
            started: None,
            output: Some(("Accounts".into(), "a\nb".into())),
        });
        app.poll_done();
        assert!(matches!(app.mode, Mode::Help));
        assert!(!app.show_deferred());
        app.mode = Mode::Normal;
        assert!(app.show_deferred());
        assert!(
            matches!(&app.mode, Mode::Output(o) if o.title == "Accounts" && o.lines == ["a", "b"])
        );
    }

    #[test]
    fn parses_dump_sizes() {
        assert_eq!(parse_dims("30x100"), Some((30, 100)));
        assert_eq!(parse_dims("0x100"), None);
        assert_eq!(parse_dims("30"), None);
    }

    #[test]
    fn sums_up_command_output() {
        let d = summarize(
            true,
            "",
            "amux: started claude-2 as claude@work in ~/x\n  amux attach claude-2\n",
        );
        assert!(d.ok);
        assert_eq!(d.started.as_deref(), Some("claude-2"));
        assert_eq!(d.message, "started claude-2 as claude@work in ~/x");
        let d = summarize(false, "", "amux: claude-2 is not running.\n");
        assert!(!d.ok && d.started.is_none());
        assert_eq!(d.message, "claude-2 is not running.");
        assert_eq!(summarize(true, "", "").message, "Done.");
    }

    #[test]
    fn confirm_builds_commands() {
        let c = Confirm::Remove {
            id: "a".into(),
            force: true,
        };
        assert_eq!(c.argv(), ["rm", "a", "--force"]);
        let c = Confirm::Stop {
            id: "a".into(),
            force: false,
        };
        assert_eq!(c.argv(), ["stop", "a"]);
    }
}
