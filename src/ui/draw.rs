//! Drawing `amux ui`: the header with counts, the run list, the preview, the footer, and
//! the spawn and help dialogs.

use super::preview::screen_lines;
use super::spawn::{Field, SpawnForm};
use super::status::Kind;
use super::{App, Mode};
use crate::paths::tildify;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, List, ListItem, Paragraph, Wrap};
use ratatui::Frame;
use std::path::Path;
use std::time::Duration;

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

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
            let w = (body.width * 3 / 10).clamp(30, 48);
            let [l, p] =
                Layout::horizontal([Constraint::Length(w), Constraint::Min(10)]).areas(body);
            (l, p)
        } else {
            // Not clamp(): on a short terminal the upper bound is below the lower one.
            let max = (body.height / 2).max(1);
            let h = (self.rows.len() as u16 * 2 + 2).min(max).max(4.min(max));
            let [l, p] = Layout::vertical([Constraint::Length(h), Constraint::Min(3)]).areas(body);
            (l, p)
        };
        self.draw_list(f, list_area);
        self.draw_preview(f, preview_area);
        self.draw_footer(f, footer);
        match &self.mode {
            Mode::Spawn(form) => draw_spawn(f, area, form),
            Mode::Help => draw_help(f, area),
            _ => {}
        }
    }

    fn draw_header(&self, f: &mut Frame, area: Rect) {
        let mut spans = vec![
            Span::styled(" amux ", Style::new().bold().reversed()),
            " ".into(),
        ];
        if self.rows.is_empty() {
            spans.push("no runs".dim());
        } else {
            let count = |k: Kind| self.rows.iter().filter(|r| r.kind == k).count();
            let parts = [
                (Kind::Attention, "waiting"),
                (Kind::Working, "working"),
                (Kind::Idle, "idle"),
                (Kind::Starting, "starting"),
                (Kind::Done, "done"),
                (Kind::Failed, "failed"),
                (Kind::Lost, "lost"),
            ];
            let mut first = true;
            for (k, name) in parts {
                let n = count(k);
                if n == 0 {
                    continue;
                }
                if !first {
                    spans.push(" · ".dim());
                }
                first = false;
                spans.push(Span::styled(format!("{n} {name}"), kind_style(k)));
            }
        }
        if self.busy > 0 {
            spans.push(format!("  {} ", self.spinner()).cyan());
        }
        f.render_widget(Line::from(spans), area);
        let help = Line::from("? help ".dim()).right_aligned();
        f.render_widget(help, area);
    }

    fn spinner(&self) -> &'static str {
        SPINNER[(self.started.elapsed().as_millis() / 100) as usize % SPINNER.len()]
    }

    fn draw_list(&mut self, f: &mut Frame, area: Rect) {
        let block = Block::bordered()
            .title(format!(" Runs ({}) ", self.rows.len()))
            .border_style(Style::new().dark_gray());
        if self.rows.is_empty() {
            let msg = Paragraph::new(vec![
                Line::from(""),
                Line::from(" No runs yet."),
                Line::from(vec![" Press ".into(), "n".bold(), " to start one.".into()]),
            ])
            .block(block);
            f.render_widget(msg, area);
            return;
        }
        let width = area.width.saturating_sub(4) as usize;
        let spin = self.spinner();
        let items: Vec<ListItem> = self
            .rows
            .iter()
            .enumerate()
            .map(|(i, r)| {
                let glyph = match r.kind {
                    Kind::Working => spin,
                    k => kind_glyph(k),
                };
                let unseen = self.unseen.contains(&r.run.id);
                let mut id_style = Style::new().bold();
                if unseen {
                    id_style = id_style.underlined();
                }
                let num = if i < 9 {
                    format!("{} ", i + 1)
                } else {
                    "  ".into()
                };
                let head = format!("{num}{glyph} {}", r.run.id);
                let pad = width.saturating_sub(head.chars().count() + r.label.chars().count());
                let line1 = Line::from(vec![
                    Span::styled(num, Style::new().dark_gray()),
                    Span::styled(format!("{glyph} "), kind_style(r.kind)),
                    Span::styled(r.run.id.clone(), id_style),
                    " ".repeat(pad.max(1)).into(),
                    Span::styled(r.label.clone(), kind_style(r.kind)),
                ]);
                let place = match &r.run.worktree {
                    Some(w) => w.branch.clone(),
                    None => tildify(Path::new(&r.run.cwd)),
                };
                let detail = clip(
                    &format!("{} · {place}", r.run.account),
                    width.saturating_sub(4),
                );
                let line2 = Line::from(Span::styled(
                    format!("    {detail}"),
                    Style::new().dark_gray(),
                ));
                ListItem::new(vec![line1, line2])
            })
            .collect();
        let list = List::new(items)
            .block(block)
            .highlight_style(Style::new().bg(Color::Indexed(236)))
            .highlight_symbol("▌");
        f.render_stateful_widget(list, area, &mut self.list);
    }

    fn draw_preview(&mut self, f: &mut Frame, area: Rect) {
        let Some(row) = self.current() else {
            f.render_widget(
                Block::bordered().border_style(Style::new().dark_gray()),
                area,
            );
            return;
        };
        let run = row.run.clone();
        let (kind, label) = (row.kind, row.label.clone());
        let mut title = vec![
            Span::raw(" "),
            Span::styled(run.id.clone(), Style::new().bold()),
            Span::styled(format!(" · {}", run.account), Style::new().dark_gray()),
        ];
        if let Some(w) = &run.worktree {
            title.push(Span::styled(
                format!(" · {}", w.branch),
                Style::new().dark_gray(),
            ));
        }
        title.push(" ".into());
        let bottom = match (&run.waiting, kind) {
            (Some(w), Kind::Attention) => Line::from(vec![
                " waiting: ".yellow().bold(),
                clip(&w.reason, area.width.saturating_sub(16) as usize).yellow(),
                " ".into(),
            ]),
            _ if !kind.is_live() => {
                let why = run
                    .error
                    .clone()
                    .map(|e| format!(" {label}: {e} "))
                    .unwrap_or_else(|| format!(" {label} · d removes it "));
                Line::from(clip(&why, area.width.saturating_sub(4) as usize).dark_gray())
            }
            _ => match run
                .status
                .as_ref()
                .and_then(|s| Some((s, s.message.as_ref()?)))
            {
                // What the CLI says it is doing (OSC 7501), such as Claude's task summary.
                Some((s, msg)) => Line::from(vec![
                    format!(" {}: ", s.state).cyan(),
                    clip(msg, area.width.saturating_sub(16) as usize).into(),
                    " ".into(),
                ]),
                None => Line::from(" Enter attaches · Ctrl-] comes back ".dark_gray()),
            },
        };
        let border = if kind == Kind::Attention {
            Style::new().yellow()
        } else {
            Style::new().dark_gray()
        };
        let mut block = Block::bordered()
            .title(Line::from(title))
            .title_bottom(bottom)
            .border_style(border);
        let p = self.previews.get(&run, kind.is_live());
        let Some(p) = p else {
            f.render_widget(block, area);
            return;
        };
        let s = p.lock();
        let screen = s.parser.screen();
        let cli_title = s.parser.callbacks().title.clone();
        if !cli_title.is_empty() {
            let t = clip(&cli_title, (area.width / 3) as usize);
            block = block.title(Line::from(format!(" {t} ").dark_gray()).right_aligned());
        }
        let inner = block.inner(area);
        let mut lines = screen_lines(screen, inner.height, inner.width);
        if screen.scrollback() > 0 {
            if let Some(first) = lines.first_mut() {
                *first = Line::from(format!(
                    " ↑ {} lines back · Esc returns ",
                    screen.scrollback()
                ))
                .reversed();
            }
        }
        drop(s);
        f.render_widget(Paragraph::new(lines).block(block), area);
    }

    fn draw_footer(&self, f: &mut Frame, area: Rect) {
        let line = match (&self.mode, &self.status) {
            (Mode::Confirm(c), _) => Line::from(vec![
                format!(" {} ", c.question()).bold(),
                "y".bold().yellow(),
                " yes  any other key cancels".dark_gray(),
            ]),
            (_, Some((msg, err, at))) if at.elapsed() < Duration::from_secs(6) => {
                let s = if *err {
                    Style::new().red()
                } else {
                    Style::new().green()
                };
                Line::from(Span::styled(format!(" {msg}"), s))
            }
            _ => {
                let keys = [
                    ("⏎", "attach"),
                    ("n", "new"),
                    ("s", "stop"),
                    ("d", "remove"),
                    ("⇥", "next waiting"),
                    ("PgUp", "scroll"),
                    ("q", "quit"),
                ];
                let mut spans = vec![];
                for (k, what) in keys {
                    spans.push(format!(" {k}").bold());
                    spans.push(format!(" {what} ").dark_gray());
                }
                Line::from(spans)
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
        Kind::Failed => "✗",
        Kind::Lost => "?",
    }
}

fn kind_style(k: Kind) -> Style {
    match k {
        Kind::Starting => Style::new().dark_gray(),
        Kind::Working => Style::new().cyan(),
        Kind::Attention => Style::new().yellow().add_modifier(Modifier::BOLD),
        Kind::Idle => Style::new().gray(),
        Kind::Done => Style::new().green(),
        Kind::Failed => Style::new().red(),
        Kind::Lost => Style::new().magenta(),
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

fn draw_spawn(f: &mut Frame, area: Rect, form: &SpawnForm) {
    let r = centered(area, 72, 13);
    f.render_widget(Clear, r);
    let block = Block::bordered()
        .title(" New run ".bold())
        .title_bottom(Line::from(
            " ⇥ next field  ⏎ start  Esc cancel ".dark_gray(),
        ))
        .border_style(Style::new().cyan());
    let inner = block.inner(r);
    f.render_widget(block, r);
    let field = |label: &str, value: String, focus: bool, hint: &str| -> Line<'static> {
        let marker = if focus { "▸ " } else { "  " };
        let mut v = vec![
            Span::styled(format!("{marker}{label:<9}"), Style::new().bold()),
            Span::styled(
                value,
                if focus {
                    Style::new().reversed()
                } else {
                    Style::new()
                },
            ),
        ];
        if focus && !hint.is_empty() {
            v.push(format!("  {hint}").dark_gray());
        }
        Line::from(v)
    };
    let w = inner.width.saturating_sub(14) as usize;
    let text = |s: &str, focus: bool| {
        let shown = if s.chars().count() > w {
            let skip = s.chars().count() - w;
            format!("…{}", s.chars().skip(skip + 1).collect::<String>())
        } else {
            s.to_string()
        };
        if focus {
            format!("{shown}▏")
        } else if shown.is_empty() {
            "—".into()
        } else {
            shown
        }
    };
    let mut lines = vec![
        field(
            "Account",
            format!("◂ {} ▸", form.account().unwrap_or("-")),
            form.focus == Field::Account,
            "←/→",
        ),
        field(
            "Prompt",
            text(&form.prompt, form.focus == Field::Prompt),
            form.focus == Field::Prompt,
            "",
        ),
        field(
            "Name",
            text(&form.name, form.focus == Field::Name),
            form.focus == Field::Name,
            "optional",
        ),
        field(
            "Worktree",
            if form.worktree {
                "[x] own git worktree"
            } else {
                "[ ] this checkout"
            }
            .into(),
            form.focus == Field::Worktree,
            "space",
        ),
    ];
    if form.worktree {
        lines.push(field(
            "Branch",
            text(&form.branch, form.focus == Field::Branch),
            form.focus == Field::Branch,
            "default amux/<run>",
        ));
    }
    lines.push(Line::from(""));
    if let Some(e) = &form.error {
        lines.push(Line::from(format!("  {e}").red()));
    } else {
        let cwd = std::env::current_dir().unwrap_or_default();
        lines.push(Line::from(format!("  in {}", tildify(&cwd)).dark_gray()));
    }
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

const HELP_TEXT: &[(&str, &str)] = &[
    ("↑ ↓  j k", "select a run"),
    ("1–9", "jump to a run"),
    (
        "⇥ Tab",
        "next run that needs you (waiting, then failed, done)",
    ),
    ("⏎  a", "attach here; Ctrl-] comes back to this list"),
    ("n", "start a new run"),
    ("s  S", "stop the run (SIGTERM), or kill it (SIGKILL)"),
    ("d  D", "remove a finished run, or kill and remove"),
    ("PgUp PgDn", "scroll the preview back; Esc returns"),
    ("q", "quit; runs keep going"),
];

fn draw_help(f: &mut Frame, area: Rect) {
    let r = centered(area, 70, HELP_TEXT.len() as u16 + 6);
    f.render_widget(Clear, r);
    let mut lines = vec![Line::from("")];
    for (k, what) in HELP_TEXT {
        lines.push(Line::from(vec![
            Span::styled(format!("  {k:<11}"), Style::new().bold()),
            Span::raw(*what),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(
        "  ◆ waiting  ● working  ◌ idle  ✓ done  ✗ failed".dark_gray(),
    ));
    let block = Block::bordered()
        .title(" amux ui ".bold())
        .title_bottom(Line::from(" any key closes ".dark_gray()))
        .border_style(Style::new().cyan());
    f.render_widget(Paragraph::new(lines).block(block), r);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clips_with_an_ellipsis() {
        assert_eq!(clip("hello", 10), "hello");
        assert_eq!(clip("hello world", 6), "hello…");
    }
}
