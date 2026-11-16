//! Drawing `amux ui`: the header, the run list, the preview, the footer, and the overlays
//! (command palette, new-run form, help, command output). Every style comes from the
//! theme, by role; nothing here names a color.

use super::palette::{self, Item};
use super::preview::screen_lines;
use super::spawn::{Field, SpawnForm};
use super::status::{Kind, Show};
use super::theme::Theme;
use super::{App, Confirm, Mode, OutputView, PaletteState, View};
use crate::paths::tildify;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, List, ListItem, Paragraph, Wrap};
use ratatui::Frame;
use std::path::Path;
use std::time::Duration;

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
/// Marks the selected row, and the selected line in lists.
const MARKER: &str = "▌";

pub(super) fn panel<'a>(t: &Theme, border: Style) -> Block<'a> {
    // Titles get the text color, so they don't take on a colored border's.
    let text = t.text.fg(t.text.fg.unwrap_or_default());
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(border)
        .title_style(text)
        .style(t.text)
}

pub(super) fn span(s: impl Into<String>, style: Style) -> Span<'static> {
    Span::styled(s.into(), style)
}

/// `key label` pairs as spans: keys stand out, labels recede.
pub(super) fn hints(t: &Theme, pairs: &[(&str, &str)]) -> Vec<Span<'static>> {
    let mut v = vec![];
    for (i, (k, label)) in pairs.iter().enumerate() {
        if i > 0 {
            v.push(span("   ", t.muted));
        }
        v.push(span(*k, t.key));
        v.push(span(format!(" {label}"), t.muted));
    }
    v
}

/// Hints framed for a border title: ` a  b `.
pub(super) fn border_hints(t: &Theme, pairs: &[(&str, &str)]) -> Line<'static> {
    let mut h = vec![span(" ", t.muted)];
    h.extend(hints(t, pairs));
    h.push(span(" ", t.muted));
    Line::from(h)
}

fn width_of(spans: &[Span]) -> usize {
    spans.iter().map(|s| s.content.chars().count()).sum()
}

impl App {
    pub(super) fn draw(&mut self, f: &mut Frame) {
        let area = f.area();
        let [header, body, footer] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(1),
        ])
        .areas(area);
        self.draw_header(f, header);
        let wide = body.width >= 100;
        let (list_area, preview_area) = if wide {
            let w = match self.layout.list_width {
                Some(w) => w.min(body.width.saturating_sub(30)).max(20),
                None => (body.width * 3 / 10).clamp(32, 48),
            };
            if self.layout.list_right {
                let [p, l] =
                    Layout::horizontal([Constraint::Min(10), Constraint::Length(w)]).areas(body);
                (l, p)
            } else {
                let [l, p] =
                    Layout::horizontal([Constraint::Length(w), Constraint::Min(10)]).areas(body);
                (l, p)
            }
        } else {
            // Not clamp(): on a short terminal the upper bound is below the lower one.
            let max = (body.height / 2).max(1);
            let h = (self.rows.len() as u16 * 2 + 2).min(max).max(4.min(max));
            let [l, p] = Layout::vertical([Constraint::Length(h), Constraint::Min(3)]).areas(body);
            (l, p)
        };
        self.panes.clear();
        self.list_area = Rect::default();
        self.preview_area = Rect::default();
        match self.view {
            View::List => {
                self.list_area = list_area;
                self.draw_list(f, list_area);
                self.draw_preview(f, preview_area);
            }
            View::Board => self.draw_board(f, body),
            View::Tiles => self.draw_tiles(f, body),
        }
        self.draw_footer(f, footer);
        let t = &self.theme;
        match &self.mode {
            Mode::Spawn(form) => draw_spawn(f, area, t, form),
            Mode::Help => draw_help(f, area, t),
            Mode::Palette(p) => draw_palette(f, area, t, p),
            Mode::Output(o) => draw_output(f, area, t, o),
            Mode::Normal | Mode::Confirm(_) | Mode::Review(_) => {}
        }
        if let Mode::Review(r) = &mut self.mode {
            let t = self.theme.clone();
            r.draw(f, body, &t);
        }
    }

    fn draw_header(&self, f: &mut Frame, area: Rect) {
        let t = &self.theme;
        let mut spans = vec![span(" amux ", t.header), span("  ", t.muted)];
        if self.rows.is_empty() {
            spans.push(span(
                if self.filtered() {
                    "no matches"
                } else {
                    "no runs"
                },
                t.muted,
            ));
        } else {
            let parts = [
                (Kind::Attention, "waiting"),
                (Kind::Working, "working"),
                (Kind::Idle, "idle"),
                (Kind::Starting, "starting"),
                (Kind::Done, "done"),
                (Kind::Stopped, "stopped"),
                (Kind::Failed, "failed"),
                (Kind::Lost, "lost"),
            ];
            let mut first = true;
            for (k, name) in parts {
                let n = self.rows.iter().filter(|r| r.kind == k).count();
                if n == 0 {
                    continue;
                }
                if !first {
                    spans.push(span(" · ", t.muted));
                }
                first = false;
                spans.push(span(format!("{n} {name}"), kind_style(t, k)));
            }
        }
        if self.show != Show::All {
            spans.push(span(format!("   showing {}", self.show.name()), t.accent));
        }
        if !self.filter.is_empty() {
            spans.push(span(format!("   filter “{}”", self.filter), t.accent));
        }
        if self.filtered() {
            spans.push(span("  Esc shows all", t.muted));
        }
        if self.busy > 0 {
            spans.push(span(format!("   {}", self.spinner()), t.accent));
        }
        let right = hints(t, &[("/", "commands"), ("?", "help")]);
        let room = (area.width as usize).saturating_sub(width_of(&spans) + 2);
        f.render_widget(Line::from(spans), area);
        if width_of(&right) < room {
            let mut r = right;
            r.push(span(" ", t.muted));
            f.render_widget(Line::from(r).right_aligned(), area);
        }
    }

    pub(super) fn spinner(&self) -> &'static str {
        SPINNER[(self.started.elapsed().as_millis() / 100) as usize % SPINNER.len()]
    }

    fn draw_list(&mut self, f: &mut Frame, area: Rect) {
        let t = self.theme.clone();
        let count = if !self.filtered() {
            format!(" Runs {} ", self.rows.len())
        } else {
            format!(" Runs {} of {} ", self.rows.len(), self.total)
        };
        // The panel the keys go to has the accent border.
        let border = if self.focus_run { t.border } else { t.accent };
        let block = panel(&t, border).title(span(count, t.title));
        if self.rows.is_empty() {
            let nothing = match (self.show, self.filter.is_empty()) {
                (Show::All, _) => format!(" Nothing matches “{}”.", self.filter),
                (s, true) => format!(" No {} runs.", s.name()),
                (s, false) => format!(" No {} runs match “{}”.", s.name(), self.filter),
            };
            let lines = if !self.filtered() {
                vec![
                    Line::from(""),
                    Line::from(span(" No runs yet.", t.title)),
                    Line::from(""),
                    Line::from(hints(&t, &[(" n", "start a run")])),
                    Line::from(hints(&t, &[(" /", "all commands")])),
                    Line::from(hints(&t, &[(" ?", "keys")])),
                ]
            } else {
                vec![
                    Line::from(""),
                    Line::from(span(nothing, t.text)),
                    Line::from(hints(&t, &[(" Esc", "show all runs")])),
                ]
            };
            f.render_widget(Paragraph::new(lines).block(block), area);
            return;
        }
        // Inside the border, after the marker column.
        let width = area.width.saturating_sub(3) as usize;
        let spin = self.spinner();
        let selected = self.list.selected();
        let items: Vec<ListItem> = self
            .rows
            .iter()
            .enumerate()
            .map(|(i, r)| {
                let (glyph, glyph_style) = match r.kind {
                    Kind::Working => (spin, t.accent),
                    k => (kind_glyph(k), kind_style(&t, k)),
                };
                let num = if i < 9 {
                    format!("{} ", i + 1)
                } else {
                    "  ".into()
                };
                let id_style = if selected == Some(i) { t.title } else { t.text };
                let dot = if self.unseen.contains(&r.run.id) {
                    " •"
                } else {
                    ""
                };
                let used = num.chars().count() + 2 + r.run.id.chars().count() + dot.chars().count();
                let pad = width.saturating_sub(used + r.label.chars().count() + 1);
                let line1 = Line::from(vec![
                    span(num, t.muted),
                    span(format!("{glyph} "), glyph_style),
                    span(r.run.id.clone(), id_style),
                    span(dot, t.accent),
                    span(" ".repeat(pad.max(1)), t.muted),
                    span(r.label.clone(), kind_style(&t, r.kind)),
                ]);
                let place = match &r.run.worktree {
                    Some(w) => w.branch.clone(),
                    None => tildify(Path::new(&r.run.cwd)),
                };
                let mut detail = format!("{} · {place}", r.run.account);
                if let Some(c) = self.conflict_note(&r.run.id) {
                    detail.push_str(&format!(" · {c}"));
                }
                if let Some(pr) = &r.pr {
                    detail.push_str(&format!(" · {}", pr.label()));
                }
                if let Some(v) = &r.verify {
                    detail.push_str(&format!(" · {v}"));
                }
                let detail = clip(&detail, width.saturating_sub(5));
                let line2 = Line::from(span(format!("    {detail}"), t.muted));
                ListItem::new(vec![line1, line2])
            })
            .collect();
        let list = List::new(items)
            .block(block)
            .highlight_style(t.selection)
            .highlight_symbol(Line::from(span(MARKER, t.accent)))
            .repeat_highlight_symbol(true);
        f.render_stateful_widget(list, area, &mut self.list);
    }

    fn draw_preview(&mut self, f: &mut Frame, area: Rect) {
        let idx = self.list.selected();
        self.draw_pane(f, area, idx, true);
    }

    /// One run's live screen in a pane. `selected`: the run the keys go to, which is
    /// fitted to the pane while you type into it. Other panes (tiles) only show.
    pub(super) fn draw_pane(
        &mut self,
        f: &mut Frame,
        area: Rect,
        idx: Option<usize>,
        selected: bool,
    ) {
        let t = self.theme.clone();
        let focused = selected && self.focus_run;
        let Some(row) = idx.and_then(|i| self.rows.get(i)) else {
            let intro = vec![
                Line::from(""),
                Line::from(span(
                    "  Each run is an agent working in the background.",
                    t.text,
                )),
                Line::from(span(
                    "  Pick one on the left to watch its screen live here.",
                    t.muted,
                )),
            ];
            f.render_widget(Paragraph::new(intro).block(panel(&t, t.border)), area);
            return;
        };
        let run = row.run.clone();
        let (kind, label) = (row.kind, row.label.clone());
        let mut sub = format!("  {}", run.account);
        if let Some(w) = &run.worktree {
            sub.push_str(&format!(" · {}", w.branch));
        }
        let title = vec![
            span(" ", t.muted),
            span(run.id.clone(), t.title),
            span(format!("{sub} "), t.muted),
        ];
        let room = area.width.saturating_sub(10) as usize;
        let bottom = match (&run.waiting, kind) {
            (Some(w), Kind::Attention) => Line::from(vec![
                span(" ◆ ", t.waiting),
                span(clip(&w.reason, room), t.waiting),
                span(" ", t.muted),
            ]),
            _ if !kind.is_live() => {
                let why = match &run.error {
                    Some(e) => format!(" {label}: {e} "),
                    None => format!(" {label} · d removes it "),
                };
                Line::from(span(clip(&why, room), t.muted))
            }
            _ => match run
                .status
                .as_ref()
                .and_then(|s| Some((s, s.message.as_ref()?)))
            {
                // What the CLI says it is doing (OSC 7501), such as Claude's task summary.
                Some((s, msg)) => {
                    let glyph = if s.state == "working" {
                        self.spinner()
                    } else {
                        "·"
                    };
                    Line::from(vec![
                        span(format!(" {glyph} "), t.accent),
                        span(clip(msg, room), t.text),
                        span(" ", t.muted),
                    ])
                }
                None if focused => border_hints(&t, &[("Ctrl-]", "list"), ("Ctrl-B", "commands")]),
                None if selected => border_hints(&t, &[("⏎", "type here"), ("a", "full screen")]),
                None => Line::from(""),
            },
        };
        let border = if focused {
            t.accent
        } else if kind == Kind::Attention {
            Style::new().fg(t.waiting.fg.unwrap_or_default())
        } else {
            t.border
        };
        let mut block = panel(&t, border)
            .title(Line::from(title))
            .title_bottom(bottom);
        let inner = block.inner(area);
        if let Some(i) = idx {
            self.panes.push((inner, i));
        }
        if selected {
            self.preview_area = inner;
            self.fit_focused(inner.height, inner.width);
        }
        let p = self.previews.get(&run, kind.is_live());
        let Some(p) = p else {
            f.render_widget(block, area);
            return;
        };
        let s = p.lock();
        let screen = s.parser.screen();
        let cli_title = s.parser.callbacks().title.clone();
        if !cli_title.is_empty() {
            let c = clip(&cli_title, (area.width / 3) as usize);
            block = block.title(Line::from(span(format!(" {c} "), t.muted)).right_aligned());
        }
        let inner = block.inner(area);
        let mut lines = screen_lines(screen, inner.height, inner.width);
        let empty = lines
            .iter()
            .all(|l| l.spans.iter().all(|s| s.content.trim().is_empty()));
        if empty {
            let msg = if kind == Kind::Starting {
                "Starting…"
            } else {
                "No output yet."
            };
            lines = vec![
                Line::from(""),
                Line::from(span(format!("  {msg}"), t.muted)),
            ];
        }
        if screen.scrollback() > 0 {
            if let Some(first) = lines.first_mut() {
                *first = Line::from(span(
                    format!(
                        " ↑ {} lines back · Esc returns to live ",
                        screen.scrollback()
                    ),
                    t.selection.add_modifier(Modifier::BOLD),
                ));
            }
        }
        drop(s);
        f.render_widget(Paragraph::new(lines).block(block), area);
    }

    fn draw_footer(&self, f: &mut Frame, area: Rect) {
        let t = &self.theme;
        let line = match (&self.mode, &self.status) {
            (Mode::Review(r), Some((msg, err, at)))
                if r.input.is_none() && at.elapsed() < Duration::from_secs(4) =>
            {
                let (mark, style) = if *err {
                    ("✗", t.error)
                } else {
                    ("✓", t.success)
                };
                Line::from(vec![
                    span(format!(" {mark} "), style),
                    span(msg.clone(), style),
                ])
            }
            (Mode::Review(r), _) => r.footer(t),
            (Mode::Confirm(c @ Confirm::Verify(p)), _) => {
                let (test, push, prefer_push) = Confirm::verify_choices(p);
                let mut v = vec![span(format!(" {} ", c.question()), t.title)];
                if p.failed {
                    v.push(span("  f", t.key));
                    v.push(span(" send failures to agent (⏎)", t.muted));
                }
                if p.ci_failed {
                    v.push(span("  k", t.key));
                    v.push(span(" send failing CI to agent", t.muted));
                }
                for (key, what, preferred) in [("t", test, !prefer_push), ("p", push, prefer_push)]
                {
                    if let Some(what) = what {
                        v.push(span(format!("  {key}"), t.key));
                        let mark = if preferred && !p.failed { " (⏎)" } else { "" };
                        v.push(span(format!(" {what}{mark}"), t.muted));
                    }
                }
                if p.branch.is_some() && p.remote.is_some() {
                    v.push(span("  r", t.key));
                    v.push(span(" open PR", t.muted));
                }
                v.push(span("   Esc", t.key));
                v.push(span(" cancels", t.muted));
                Line::from(v)
            }
            (Mode::Confirm(c), _) => Line::from(vec![
                span(format!(" {} ", c.question()), t.title),
                span("  y", t.key),
                span(" yes   ", t.muted),
                span("any other key", t.key),
                span(" cancels", t.muted),
            ]),
            (_, Some((msg, err, at))) if at.elapsed() < Duration::from_secs(6) => {
                let (mark, style) = if *err {
                    ("✗", t.error)
                } else {
                    ("✓", t.success)
                };
                Line::from(vec![
                    span(format!(" {mark} "), style),
                    span(msg.clone(), style),
                ])
            }
            _ if self.prefix => Line::from(hints(
                t,
                &[
                    (" Ctrl-B +", ""),
                    ("n", "new"),
                    ("s", "stop"),
                    ("v", "verify"),
                    ("c", "review"),
                    ("↑↓", "run"),
                    ("⏎", "full screen"),
                    ("/", "commands"),
                    ("?", "keys"),
                    ("Esc", "cancel"),
                ],
            )),
            _ if self.focus_run => {
                let id = self.selected.clone().unwrap_or_default();
                let mut v = vec![span(format!(" Typing into {id}  "), t.accent)];
                v.extend(hints(
                    t,
                    &[
                        ("Ctrl-]", "list"),
                        ("Ctrl-B", "commands"),
                        ("Alt-↑↓", "other run"),
                        ("wheel", "scroll"),
                    ],
                ));
                Line::from(v)
            }
            _ => {
                // As many hints as fit, most useful first.
                let all = [
                    ("⏎", "type in run"),
                    ("a", "full screen"),
                    ("n", "new"),
                    ("/", "commands"),
                    ("⇥", "next waiting"),
                    ("f", "filter"),
                    ("e", "why"),
                    ("v", "verify"),
                    ("c", "review"),
                    ("b", "board"),
                    ("t", "tiles"),
                    ("s", "stop"),
                    ("d", "remove"),
                    ("PgUp", "scroll"),
                    ("q", "quit"),
                ];
                let mut shown = vec![span(" ", t.muted)];
                for pair in all {
                    let mut next = shown.clone();
                    if next.len() > 1 {
                        next.push(span("   ", t.muted));
                    }
                    next.extend(hints(t, &[pair]));
                    if width_of(&next) > area.width as usize {
                        break;
                    }
                    shown = next;
                }
                Line::from(shown)
            }
        };
        f.render_widget(line, area);
    }
}

fn kind_glyph(k: Kind) -> &'static str {
    match k {
        Kind::Starting => "○",
        Kind::Working => "●",
        Kind::Attention => "◆",
        Kind::Idle => "◌",
        Kind::Done => "✓",
        Kind::Stopped => "■",
        Kind::Failed => "✗",
        Kind::Lost => "?",
    }
}

fn kind_style(t: &Theme, k: Kind) -> Style {
    match k {
        Kind::Starting => t.starting,
        Kind::Working => t.working,
        Kind::Attention => t.waiting,
        Kind::Idle => t.idle,
        Kind::Done => t.done,
        Kind::Stopped => t.stopped,
        Kind::Failed => t.failed,
        Kind::Lost => t.lost,
    }
}

/// Cuts `s` to `max` characters, with an ellipsis when cut.
fn clip(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    Rect::new(
        area.x + (area.width - w) / 2,
        area.y + (area.height - h) / 2,
        w,
        h,
    )
}

/// A dialog near the top, like a command palette: the eye is already there.
fn top(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width.saturating_sub(4)).max(area.width.min(20));
    let h = h.min(area.height.saturating_sub(2)).max(area.height.min(3));
    let y = area.y + area.height.saturating_sub(h).min(2);
    Rect::new(area.x + area.width.saturating_sub(w) / 2, y, w, h)
}

/// A text field: the text with a cursor, the end shown when it is too long.
fn field_text(s: &str, width: usize, focus: bool) -> String {
    let n = s.chars().count();
    let shown = if n > width.saturating_sub(1) {
        let skip = (n + 2).saturating_sub(width.max(2));
        format!("…{}", s.chars().skip(skip).collect::<String>())
    } else {
        s.to_string()
    };
    if focus {
        format!("{shown}▏")
    } else {
        shown
    }
}

fn draw_palette(f: &mut Frame, area: Rect, t: &Theme, p: &PaletteState) {
    let items = palette::items(&p.input, &p.ctx);
    let visible = items.len().clamp(1, 10) as u16;
    let r = top(area, 78, visible + 4);
    f.render_widget(Clear, r);
    let block = panel(t, t.accent)
        .title(span(" Commands ", t.title))
        .title_bottom(border_hints(
            t,
            &[
                ("↑↓", "choose"),
                ("⇥", "complete"),
                ("⏎", "run"),
                ("Esc", "close"),
            ],
        ));
    let inner = block.inner(r);
    f.render_widget(block, r);
    let w = inner.width as usize;
    let mut lines = vec![
        Line::from(vec![
            span(" › ", t.accent),
            span(field_text(&p.input, w.saturating_sub(4), true), t.text),
        ]),
        Line::from(span("─".repeat(w), t.border)),
    ];
    if items.is_empty() {
        lines.push(Line::from(span(
            "   No such command. /help lists them all.",
            t.muted,
        )));
    }
    let sel = p.sel.min(items.len().saturating_sub(1));
    let start = sel.saturating_sub(visible.saturating_sub(1) as usize);
    for (i, it) in items.iter().enumerate().skip(start).take(visible as usize) {
        lines.push(palette_line(t, it, w, i == sel));
    }
    f.render_widget(Paragraph::new(lines), inner);
}

fn palette_line(t: &Theme, it: &Item, w: usize, selected: bool) -> Line<'static> {
    let base = if selected { t.selection } else { Style::new() };
    let marker = if selected { MARKER } else { " " };
    // The selected command also shows how to call it.
    let full = if selected && !it.usage.is_empty() {
        format!("{} {}", it.label, it.usage)
    } else {
        it.label.clone()
    };
    let label = clip(&full, (w / 2).max(8));
    let label_w = label.chars().count().max(18);
    let key_w = it.key.chars().count();
    let detail = clip(&it.detail, w.saturating_sub(label_w + key_w + 7));
    let pad = w.saturating_sub(2 + label_w + 2 + detail.chars().count() + key_w + 2);
    Line::from(vec![
        span(marker, t.accent.patch(base)),
        span(" ", base),
        span(
            format!("{label:<label_w$}"),
            (if selected { t.title } else { t.text }).patch(base),
        ),
        span("  ", base),
        span(detail, t.muted.patch(base)),
        span(" ".repeat(pad), base),
        span(it.key.clone(), t.key.patch(base)),
        span("  ", base),
    ])
}

fn draw_output(f: &mut Frame, area: Rect, t: &Theme, o: &OutputView) {
    let longest = o.lines.iter().map(|l| l.chars().count()).max().unwrap_or(0);
    let w = longest.saturating_add(6).clamp(44, 100) as u16;
    let h = u16::try_from(o.lines.len())
        .unwrap_or(u16::MAX)
        .saturating_add(4);
    let r = top(area, w, h);
    f.render_widget(Clear, r);
    let block = panel(t, t.accent)
        .title(span(format!(" {} ", o.title), t.title))
        .title_bottom(border_hints(t, &[("↑↓", "scroll"), ("Esc", "close")]));
    let inner = block.inner(r);
    f.render_widget(block, r);
    let lines: Vec<Line> = std::iter::once(Line::from(""))
        .chain(
            o.lines
                .iter()
                .skip(o.scroll)
                .map(|l| Line::from(span(format!(" {l}"), t.text))),
        )
        .collect();
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

fn draw_spawn(f: &mut Frame, area: Rect, t: &Theme, form: &SpawnForm) {
    let r = centered(area, 72, if form.worktree { 11 } else { 10 });
    f.render_widget(Clear, r);
    let block = panel(t, t.accent)
        .title(span(" New run ", t.title))
        .title_bottom(border_hints(
            t,
            &[("⇥", "next field"), ("⏎", "start"), ("Esc", "cancel")],
        ));
    let inner = block.inner(r);
    f.render_widget(block, r);
    let w = inner.width.saturating_sub(16) as usize;
    let field = |label: &str, value: String, focus: bool, hint: &str| -> Line<'static> {
        let mut v = vec![
            span(
                if focus {
                    format!(" {MARKER} ")
                } else {
                    "   ".into()
                },
                t.accent,
            ),
            span(
                format!("{label:<10}"),
                if focus { t.title } else { t.muted },
            ),
            span(value, if focus { t.input } else { t.text }),
        ];
        if focus && !hint.is_empty() {
            v.push(span(format!("  {hint}"), t.muted));
        }
        Line::from(v)
    };
    let text = |s: &str, focus: bool, empty: &str| {
        if s.is_empty() && !focus {
            empty.to_string()
        } else {
            field_text(s, w, focus)
        }
    };
    let mut lines = vec![
        Line::from(""),
        field(
            "Account",
            format!("‹ {} ›", form.account().unwrap_or("-")),
            form.focus == Field::Account,
            "←/→",
        ),
        field(
            "Prompt",
            text(&form.prompt, form.focus == Field::Prompt, "—"),
            form.focus == Field::Prompt,
            "",
        ),
        field(
            "Name",
            text(&form.name, form.focus == Field::Name, "automatic"),
            form.focus == Field::Name,
            "optional",
        ),
        field(
            "Worktree",
            if form.worktree {
                "● own git worktree"
            } else {
                "○ this checkout"
            }
            .into(),
            form.focus == Field::Worktree,
            "space toggles",
        ),
    ];
    if form.worktree {
        lines.push(field(
            "Branch",
            text(&form.branch, form.focus == Field::Branch, "amux/<run>"),
            form.focus == Field::Branch,
            "optional",
        ));
    }
    lines.push(Line::from(""));
    match &form.error {
        Some(e) => lines.push(Line::from(span(format!("   ✗ {e}"), t.error))),
        None => {
            let cwd = std::env::current_dir().unwrap_or_default();
            // One line, cut from the left, so a long path doesn't spill past the border.
            let room = (inner.width as usize).saturating_sub(14);
            lines.push(Line::from(span(
                format!("   Starts in {}", tail_fit(&tildify(&cwd), room)),
                t.muted,
            )));
        }
    }
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

const HELP_KEYS: &[(&str, &str)] = &[
    ("↑ ↓  j k  1–9", "select a run"),
    ("⇥", "next run that needs you"),
    (
        "⏎  click",
        "type into the run in its pane (or just start typing)",
    ),
    ("Ctrl-]", "back to the list from typing"),
    (
        "Ctrl-B key",
        "while typing: run a command key, such as Ctrl-B s",
    ),
    ("Alt-↑ Alt-↓", "while typing: the run above or below"),
    ("a", "the run full screen; Ctrl-] comes back"),
    ("wheel", "scroll the list or the screen; click selects"),
    ("n", "start a new run"),
    ("s  S", "stop, or kill"),
    ("d  D", "remove a finished run, or kill and remove"),
    ("f", "show waiting, working, idle or ended runs"),
    ("e", "why a run shows its state"),
    ("b  t", "a board by state, or live tiles side by side"),
    ("v", "verify: run its tests here, or push its branch for CI"),
    ("c", "review: the run's diff; ⏎ comments on a line, S sends"),
    ("PgUp PgDn", "scroll the preview; Esc back to live"),
    ("/", "command palette"),
    ("q", "quit; runs keep going"),
];

fn draw_help(f: &mut Frame, area: Rect, t: &Theme) {
    let r = centered(area, 72, HELP_KEYS.len() as u16 + 8);
    f.render_widget(Clear, r);
    let mut lines = vec![Line::from("")];
    for (k, what) in HELP_KEYS {
        lines.push(Line::from(vec![
            span(format!("  {k:<15}"), t.key),
            span(*what, t.text),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        span(format!("  {:<15}", "Commands"), t.key),
        span(
            "/new /send /logs /diff /filter /show /explain /verify",
            t.text,
        ),
    ]));
    lines.push(Line::from(span(
        format!("  {:<15}/switch-account /add-account /accounts /theme", ""),
        t.text,
    )));
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        span(format!("  {} ", kind_glyph(Kind::Attention)), t.waiting),
        span("waiting   ", t.muted),
        span(format!("{} ", SPINNER[2]), t.accent),
        span("working   ", t.muted),
        span(format!("{} ", kind_glyph(Kind::Idle)), t.idle),
        span("idle   ", t.muted),
        span(format!("{} ", kind_glyph(Kind::Done)), t.done),
        span("done   ", t.muted),
        span(format!("{} ", kind_glyph(Kind::Failed)), t.failed),
        span("failed   ", t.muted),
        span("• ", t.accent),
        span("changed", t.muted),
    ]));
    let block = panel(t, t.accent)
        .title(span(" amux ui ", t.title))
        .title_bottom(Line::from(span(" any key closes ", t.muted)));
    f.render_widget(Paragraph::new(lines).block(block), r);
}

/// `s` cut from the left to at most `max` characters, marked with "…".
fn tail_fit(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let keep = max.saturating_sub(1);
    let tail: String = s.chars().skip(n - keep).collect();
    format!("…{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clips_with_an_ellipsis() {
        assert_eq!(clip("hello", 10), "hello");
        assert_eq!(clip("hello world", 6), "hello…");
    }

    #[test]
    fn long_field_text_keeps_its_end_in_view() {
        assert_eq!(field_text("abc", 10, true), "abc▏");
        let s = field_text("abcdefghijkl", 6, true);
        assert!(s.starts_with('…') && s.ends_with("l▏"), "{s}");
        assert!(s.chars().count() <= 6, "{s}");
    }
}
