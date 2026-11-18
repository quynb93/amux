//! `amux ui`: a terminal UI over parallel runs. A list of runs with live state, a live
//! preview of the selected run's screen, attaching in place (Ctrl-] comes back), keys to
//! spawn, stop and remove runs, a command palette (`/`) and themes.
//!
//! The UI is only a client. Runs are owned by their `amux __run-host` processes, so
//! quitting the UI (or a crash) leaves every run going. Actions run the same `amux spawn`,
//! `stop` and `rm` commands you would type, in the background, so they behave exactly as
//! they do in a shell and never block the screen.

mod actions;
pub mod attach;
mod draw;
mod input;
mod keys;
mod layout;
pub mod palette;
pub mod preview;
mod review;
pub mod spawn;
pub mod status;
pub mod theme;
mod typing;
mod views;

use crate::runs::{self, Run};
use crate::store;
use crate::time;
use crate::work::{github, verify};
use anyhow::{bail, Result};
use palette::{Action, Ctx};
use preview::Previews;
use ratatui::backend::{Backend, TestBackend};
use ratatui::crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::crossterm::{cursor, execute, terminal};
use ratatui::layout::{Position, Rect};
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
use views::View;

/// How often run.json files are re-read.
/// Results a background thread fills in, and whether no thread is working on them.
type Shared<T> = std::sync::Arc<std::sync::Mutex<(T, bool)>>;

/// How often parallel runs are checked for conflicts with each other.
const CONFLICTS_EVERY: Duration = Duration::from_secs(20);
/// How often worktree runs' ports are checked for a dev server.
const PORTS_EVERY: Duration = Duration::from_secs(3);
/// How often runs on other machines are fetched.
const REMOTE_EVERY: Duration = Duration::from_secs(2);
/// How often each account's usage (~/.amux/usage) is re-read for the header.
const QUOTA_EVERY: Duration = Duration::from_secs(15);
const RELOAD_EVERY: Duration = Duration::from_millis(500);
/// Previews kept connected at once. Only these hold a socket, however many runs exist.
/// Live previews kept connected: enough for the most tiles, plus one for the list view.
const PREVIEW_CACHE: usize = layout::MAX_TILES + 1;

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
    app.layout = layout::Layout::from_config();
    app.view = app.layout.view;
    if !notes.is_empty() {
        app.say(format!("Theme: {}", notes.join("; ")), true);
    }
    app.reload();
    let lost = app.rows.iter().filter(|r| r.kind == Kind::Lost).count();
    if lost > 0 {
        app.say(
            format!("{lost} run(s) were lost (a restart?). /revive brings them back with their sessions."),
            false,
        );
    }
    if let Some(id) = select {
        if crate::remote::split(&id).is_some() {
            app.select_id(&id);
        } else {
            app.select_id(&runs::find(&id)?.id);
        }
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
    let _ = execute!(io::stdout(), EnableMouseCapture, EnableBracketedPaste);
    let r = app.run(&mut term);
    let _ = execute!(io::stdout(), DisableMouseCapture, DisableBracketedPaste);
    ratatui::restore();
    r.map(|_| 0)
}

/// Keys that do something in the list.
fn is_shortcut(c: char) -> bool {
    matches!(c, '1'..='9')
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
                | 'v'
                | 'c'
                | 'o'
                | 'b'
                | 't'
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
    /// How its last `amux verify` went: "tests passed", "pushed", …
    verify: Option<String>,
    /// Its pull request and CI checks, as last seen.
    pr: Option<github::PullRequest>,
}

enum Mode {
    Normal,
    Spawn(Box<SpawnForm>),
    Confirm(Confirm),
    Help,
    Palette(PaletteState),
    Output(OutputView),
    /// A run's diff, with comments on its lines (`c`).
    Review(Box<review::ReviewView>),
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
    /// What a run changed, in your pager.
    Diff(String),
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
    /// Check a run's work: t runs its tests here, p pushes its branch for CI.
    Verify(Box<verify::Plan>),
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
            Confirm::Verify(p) => {
                let mut q = format!("Verify {}:", p.id);
                if p.dirty > 0 {
                    q.push_str(&format!(" {} uncommitted,", p.dirty));
                }
                if let Some(b) = &p.busy {
                    q.push_str(&format!(" {b},"));
                }
                q.trim_end_matches(',').to_string()
            }
        }
    }

    /// For a Verify choice: what `t` and `p` do, if they can, and which ⏎ picks.
    fn verify_choices(p: &verify::Plan) -> (Option<String>, Option<String>, bool) {
        let test = p.test.as_ref().map(|t| format!("test ({t})"));
        let push = match (&p.branch, &p.remote) {
            (Some(b), Some(_)) => Some(format!("push {b} for CI")),
            _ => None,
        };
        (test, push, p.recommend_push())
    }

    fn argv(&self) -> Vec<String> {
        let (cmd, id, force) = match self {
            Confirm::Stop { id, force } => ("stop", id, *force),
            Confirm::Remove { id, force } => ("rm", id, *force),
            Confirm::Clean => return vec!["rm".into(), "--done".into()],
            Confirm::Verify(p) => return vec!["verify".into(), p.id.clone()],
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
    /// Keys go to the selected run, typed in its pane, instead of being shortcuts.
    focus_run: bool,
    /// Ctrl-B was pressed while typing into a run: the next key is a command.
    prefix: bool,
    /// The run the host was told a person is typing into (so it doesn't notify).
    typing_into: Option<String>,
    /// The size the focused run was last fitted to, so it is resized only on change.
    fitted: Option<(String, u16, u16)>,
    /// When each run's PR checks were last asked about.
    checked: HashMap<String, Instant>,
    /// Where the list and the preview's inside were last drawn, for the mouse.
    list_area: Rect,
    preview_area: Rect,
    /// List and preview, a board by state (`b`), or live tiles (`t`).
    view: View,
    /// Widths, sides and tile counts, from config.json (`/layout`).
    layout: layout::Layout,
    /// Runs on other machines from their last fetch, and whether no fetch is running.
    remote: std::sync::Arc<std::sync::Mutex<(Vec<serde_json::Value>, bool)>>,
    remote_at: Option<Instant>,
    /// For each run, what its changes conflict with (other runs, its target branch), from
    /// a check in the background (`amux overlap`); and whether no check is running.
    conflicts: Shared<HashMap<String, Vec<String>>>,
    conflicts_at: Option<Instant>,
    /// Ports of worktree runs that something (a dev server) is listening on, from a check
    /// in the background; and whether no check is running.
    listening: Shared<HashSet<u16>>,
    ports_at: Option<Instant>,
    /// Where each tile and board card was drawn, and its run, for the mouse.
    panes: Vec<(Rect, usize)>,
    cards: Vec<(Rect, usize)>,
    /// Accounts' usage for the header, re-read every QUOTA_EVERY, and when.
    quota: Vec<QuotaItem>,
    quota_at: Option<Instant>,
}

/// One account in the header's usage: how much of its tighter window is used, whether it
/// is past `limits.switchAt` (or 90%), and whether it hit its limit.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct QuotaItem {
    pub account: String,
    pub used: Option<f64>,
    pub high: bool,
    pub limited: bool,
}

/// Accounts with usage numbers or a limit, for the header.
fn quota_items() -> Vec<QuotaItem> {
    use crate::work::{limits, quota};
    let accounts = store::load_accounts().unwrap_or_default();
    let quotas = quota::all(&accounts);
    let limited = limits::active();
    let high = quota::switch_at().unwrap_or(quota::HIGH);
    accounts
        .iter()
        .filter_map(|a| {
            let id = a.id();
            let used = quotas.get(&id).and_then(|q| q.used());
            let limited = limited.contains_key(&id);
            (used.is_some() || limited).then(|| QuotaItem {
                high: used.is_some_and(|u| u >= high),
                account: id,
                used,
                limited,
            })
        })
        .collect()
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
            focus_run: false,
            prefix: false,
            typing_into: None,
            fitted: None,
            checked: HashMap::new(),
            list_area: Rect::default(),
            preview_area: Rect::default(),
            view: View::default(),
            layout: layout::Layout::default(),
            panes: Vec::new(),
            remote: std::sync::Arc::new(std::sync::Mutex::new((vec![], true))),
            remote_at: None,
            conflicts: std::sync::Arc::new(std::sync::Mutex::new((HashMap::new(), true))),
            conflicts_at: None,
            listening: std::sync::Arc::new(std::sync::Mutex::new((HashSet::new(), true))),
            ports_at: None,
            cards: Vec::new(),
            quota: Vec::new(),
            quota_at: None,
        }
    }

    /// Runs on other machines, as last fetched (in the background; at once for --dump).
    fn remote_rows(&mut self) -> Vec<Row> {
        if crate::remote::list().is_empty() {
            return vec![];
        }
        if !self.guard {
            let v = crate::remote::runs(true);
            *self.remote.lock().unwrap_or_else(|e| e.into_inner()) = (v, true);
        } else {
            let fetch = {
                let mut r = self.remote.lock().unwrap_or_else(|e| e.into_inner());
                let due = self.remote_at.is_none_or(|t| t.elapsed() >= REMOTE_EVERY);
                let go = due && r.1;
                if go {
                    r.1 = false;
                }
                go
            };
            if fetch {
                self.remote_at = Some(Instant::now());
                let shared = self.remote.clone();
                std::thread::spawn(move || {
                    let v = crate::remote::runs(true);
                    *shared.lock().unwrap_or_else(|e| e.into_inner()) = (v, true);
                });
            }
        }
        let r = self.remote.lock().unwrap_or_else(|e| e.into_inner());
        r.0.iter().filter_map(remote_row).collect()
    }

    fn reload(&mut self) {
        let now = time::now_ms();
        let all = runs::list();
        let remote = self.remote_rows();
        self.total = all.len() + remote.len();
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
                let verify = verify::load(&run.id).map(|v| v.label());
                let pr = github::load(&run.id);
                Row {
                    run,
                    kind,
                    label,
                    verify,
                    pr,
                }
            })
            .chain(
                remote
                    .into_iter()
                    .filter(|r| filter.is_empty() || matches_filter(&r.run, &filter)),
            )
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
        self.refresh_checks();
        self.refresh_conflicts();
        if self.quota_at.is_none_or(|t| t.elapsed() >= QUOTA_EVERY) {
            self.quota = quota_items();
            self.quota_at = Some(Instant::now());
        }
        self.refresh_ports();
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
            if let Mode::Review(r) = &mut self.mode {
                r.reload_comments();
            }
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
                    Event::Mouse(m) => self.mouse(m),
                    Event::Paste(text) => self.paste(&text),
                    _ => {}
                }
                self.sync_focus();
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
        execute!(
            out,
            DisableMouseCapture,
            DisableBracketedPaste,
            terminal::LeaveAlternateScreen,
            cursor::Show
        )?;
        let outcome = match self.previews.find(id) {
            Some(p) => attach::attach_in_place(p),
            None => Ok(attach::Outcome::Ended),
        };
        execute!(
            out,
            terminal::EnterAlternateScreen,
            cursor::Hide,
            EnableMouseCapture,
            EnableBracketedPaste
        )?;
        // The run was sized to the whole terminal; fit it to the pane again when typing.
        self.fitted = None;
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
            Pending::Diff(id) => {
                let mut c = Command::new("sh");
                c.args(["-c", "\"$0\" diff \"$1\" --color | ${PAGER:-less -R}"])
                    .arg(&exe)
                    .arg(&id);
                (c, false, format!("changes of {id}"))
            }
            Pending::Attach(_) => return Ok(()),
        };
        let mut out = io::stdout();
        execute!(
            out,
            DisableMouseCapture,
            DisableBracketedPaste,
            terminal::LeaveAlternateScreen,
            cursor::Show
        )?;
        terminal::disable_raw_mode()?;
        let status = cmd.env("AMUX_NO_UPDATE_CHECK", "1").status();
        if pause {
            print!("\n  Press Enter to return to amux ui ");
            let _ = out.flush();
            let _ = io::stdin().read_line(&mut String::new());
        }
        terminal::enable_raw_mode()?;
        execute!(
            out,
            terminal::EnterAlternateScreen,
            cursor::Hide,
            EnableMouseCapture,
            EnableBracketedPaste
        )?;
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
/// A run on another machine, from its `ps --json` object.
fn remote_row(v: &serde_json::Value) -> Option<Row> {
    let run: Run = serde_json::from_value(v.get("run")?.clone()).ok()?;
    Some(Row {
        run,
        kind: Kind::from_name(v["kind"].as_str()?)?,
        label: v["label"].as_str().unwrap_or_default().to_string(),
        verify: v["verifyLabel"].as_str().map(str::to_string),
        pr: serde_json::from_value(v["pr"].clone()).ok(),
    })
}

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

/// Starts this amux binary with `argv` in its own session, with no terminal, and doesn't
/// wait: it keeps going after the UI quits. It reports through the files it writes.
fn detach_amux(argv: &[String]) {
    use std::os::unix::process::CommandExt;
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let mut cmd = Command::new(exe);
    cmd.args(argv)
        .env("AMUX_NO_UPDATE_CHECK", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    // Reap it when it ends, so it doesn't linger as a zombie while the UI runs.
    if let Ok(mut child) = cmd.spawn() {
        std::thread::spawn(move || child.wait());
    }
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
    fn mouse_selects_runs_and_wheel_moves_the_list() {
        let mut app = App::new(Theme::built_in("default").unwrap());
        for i in 0..3 {
            app.rows.push(Row {
                run: Run {
                    id: format!("r{i}"),
                    exit_code: Some(0),
                    ..Default::default()
                },
                kind: Kind::Done,
                label: "done".into(),
                verify: None,
                pr: None,
            });
        }
        app.set_index(Some(0));
        app.list_area = Rect::new(0, 1, 40, 20);
        let click = |row| MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 5,
            row,
            modifiers: KeyModifiers::NONE,
        };
        // Border at y=1, then two lines per run: r1 is on rows 4 and 5.
        app.mouse(click(5));
        assert_eq!(app.selected.as_deref(), Some("r1"));
        app.mouse(click(1));
        assert_eq!(
            app.selected.as_deref(),
            Some("r1"),
            "the border selects nothing"
        );
        app.mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            ..click(3)
        });
        assert_eq!(app.selected.as_deref(), Some("r2"));
        // An ended run can't be typed into: Enter doesn't start typing.
        app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(!app.focus_run);
    }

    #[test]
    fn shows_each_accounts_usage_in_the_header_when_it_fits() {
        let mut app = App::new(Theme::built_in("default").unwrap());
        app.quota = vec![
            QuotaItem {
                account: "claude@work".into(),
                used: Some(92.4),
                high: true,
                limited: false,
            },
            QuotaItem {
                account: "codex@me".into(),
                used: None,
                high: false,
                limited: true,
            },
        ];
        let wide = app.dump(6, 120, false);
        assert!(
            wide[0].contains("claude@work 92% · codex@me limited") && wide[0].contains("? help"),
            "{}",
            wide[0]
        );
        // Too narrow: only the hints, as before.
        let narrow = app.dump(6, 50, false);
        assert!(!narrow[0].contains("claude@work"), "{}", narrow[0]);
    }

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
                verify: Some("tests passed".into()),
                pr: None,
            });
        }
        app.set_index(Some(0));
        for name in theme::built_in_names() {
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
                base: None,
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
