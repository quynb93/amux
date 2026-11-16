//! `amux ui`: a terminal UI over parallel runs. A list of runs with live state, a live
//! preview of the selected run's screen, attaching in place (Ctrl-] comes back), and keys
//! to spawn, stop and remove runs.
//!
//! The UI is only a client. Runs are owned by their `amux __run-host` processes, so
//! quitting the UI (or a crash) leaves every run going. Actions run the same `amux spawn`,
//! `stop` and `rm` commands you would type, in the background, so they behave exactly as
//! they do in a shell and never block the screen.

pub mod attach;
mod draw;
pub mod preview;
pub mod spawn;
pub mod status;

use crate::runs::{self, Run};
use crate::store;
use crate::time;
use anyhow::{bail, Result};
use preview::Previews;
use ratatui::backend::{Backend, TestBackend};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::crossterm::{cursor, execute, terminal};
use ratatui::layout::Rect;
use ratatui::widgets::ListState;
use ratatui::Terminal;
use spawn::{Field, SpawnForm};
use status::{classify, last_output, next_attention, Kind};
use std::collections::{HashMap, HashSet};
use std::io::{self, IsTerminal};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// How often run.json files are re-read.
const RELOAD_EVERY: Duration = Duration::from_millis(500);
/// Previews kept connected at once. Only these hold a socket, however many runs exist.
const PREVIEW_CACHE: usize = 4;

const USAGE: &str = "amux ui [--dump [--size <rows>x<cols>] [--select <run>]]";

pub fn ui_cmd(argv: &[String]) -> Result<i32> {
    let mut dump = false;
    let mut size = (30u16, 100u16);
    let mut select = None;
    let mut it = argv.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--dump" => dump = true,
            "--size" => {
                size = it
                    .next()
                    .and_then(|s| parse_dims(s))
                    .ok_or_else(|| anyhow::anyhow!("--size takes <rows>x<cols>. Usage: {USAGE}"))?
            }
            "--select" => select = it.next().cloned(),
            _ => bail!("Unknown option {a}. Usage: {USAGE}"),
        }
    }
    let mut app = App::new();
    app.reload();
    if let Some(id) = select {
        app.select_id(&runs::find(&id)?.id);
    }
    if dump {
        // One frame as plain text, for scripts and tests.
        for line in app.dump(size.0, size.1) {
            println!("{line}");
        }
        return Ok(0);
    }
    if !(io::stdin().is_terminal() && io::stdout().is_terminal()) {
        bail!("amux ui needs a terminal. Use `amux ps` in scripts.");
    }
    let mut term = ratatui::try_init()?;
    let r = app.run(&mut term);
    ratatui::restore();
    r.map(|_| 0)
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
}

#[derive(Clone)]
enum Confirm {
    Stop { id: String, force: bool },
    Remove { id: String, force: bool },
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
        }
    }

    fn argv(&self) -> Vec<String> {
        let (cmd, id, force) = match self {
            Confirm::Stop { id, force } => ("stop", id, *force),
            Confirm::Remove { id, force } => ("rm", id, *force),
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
}

struct App {
    rows: Vec<Row>,
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
    attach: Option<String>,
    quit: bool,
    started: Instant,
}

impl App {
    fn new() -> Self {
        let (tx, rx) = mpsc::channel();
        App {
            rows: vec![],
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
            attach: None,
            quit: false,
            started: Instant::now(),
        }
    }

    fn reload(&mut self) {
        let now = time::now_ms();
        self.rows = runs::list()
            .into_iter()
            .map(|run| {
                let (kind, label) =
                    classify(&run.state(), run.waiting.as_ref(), last_output(&run), now);
                Row { run, kind, label }
            })
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

    fn poll_done(&mut self) -> bool {
        let mut any = false;
        while let Ok(d) = self.rx.try_recv() {
            any = true;
            self.busy = self.busy.saturating_sub(1);
            self.say(d.message, !d.ok);
            self.reload();
            if let Some(id) = d.started {
                self.select_id(&id);
            }
        }
        any
    }

    fn run<B: Backend>(&mut self, term: &mut Terminal<B>) -> Result<()>
    where
        B::Error: Send + Sync + 'static,
    {
        let mut dirty = true;
        let mut last_draw = Instant::now();
        while !self.quit {
            dirty |= self.poll_done();
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
            if let Some(id) = self.attach.take() {
                self.attach_in_place(&id, term)?;
                dirty = true;
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

    fn key(&mut self, k: KeyEvent) {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && k.code == KeyCode::Char('c') && !matches!(self.mode, Mode::Spawn(_)) {
            self.quit = true;
            return;
        }
        match std::mem::replace(&mut self.mode, Mode::Normal) {
            Mode::Normal => self.key_normal(k),
            Mode::Help => {}
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
            KeyCode::Enter | KeyCode::Char('a') => match cur {
                Some((id, kind)) if kind.is_live() => self.attach = Some(id),
                Some((id, _)) => self.say(format!("{id} has ended; d removes it."), true),
                None => {}
            },
            KeyCode::Char('n') => self.open_spawn(),
            KeyCode::Char(c @ ('s' | 'S')) => match cur {
                Some((id, kind)) if kind.is_live() => {
                    self.mode = Mode::Confirm(Confirm::Stop {
                        id,
                        force: c == 'S',
                    })
                }
                Some((id, _)) => self.say(format!("{id} is not running."), true),
                None => {}
            },
            KeyCode::Char(c @ ('d' | 'D' | 'x')) => match cur {
                Some((id, kind)) if kind.is_live() && c != 'D' => self.say(
                    format!("{id} is still running. s stops it; D kills and removes it."),
                    true,
                ),
                Some((id, _)) => {
                    self.mode = Mode::Confirm(Confirm::Remove {
                        id,
                        force: c == 'D',
                    })
                }
                None => {}
            },
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

    fn open_spawn(&mut self) {
        let accounts: Vec<String> = store::load_accounts()
            .unwrap_or_default()
            .iter()
            .map(|a| a.id())
            .collect();
        if accounts.is_empty() {
            self.say("No accounts yet. Add one with `amux account add`.", true);
            return;
        }
        let preferred = self.current().map(|r| r.run.account.clone());
        let cwd = std::env::current_dir().unwrap_or_default();
        self.mode = Mode::Spawn(Box::new(SpawnForm::new(
            accounts,
            preferred.as_deref(),
            in_git_repo(&cwd),
        )));
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
    fn dump(&mut self, rows: u16, cols: u16) -> Vec<String> {
        let deadline = Instant::now() + Duration::from_millis(1500);
        let mut quiet_since = Instant::now();
        while Instant::now() < deadline {
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
        },
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
