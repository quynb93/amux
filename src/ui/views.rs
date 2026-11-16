//! Other ways to see the runs: a board with a column per state (`b`), like a kanban
//! board of agents, and tiles with up to four live screens side by side (`t`), like a
//! terminal split into panes. The list and one preview (the default) is in draw.rs.

use super::draw::{panel, span};
use super::*;
use ratatui::layout::{Constraint, Layout};
use ratatui::text::Line;
use ratatui::widgets::Paragraph;
use ratatui::Frame;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum View {
    #[default]
    List,
    Board,
    Tiles,
}

impl View {
    pub fn parse(s: &str) -> Option<View> {
        Some(match s {
            "list" => View::List,
            "board" | "kanban" => View::Board,
            "tiles" | "grid" | "panes" => View::Tiles,
            _ => return None,
        })
    }
}

/// The board's columns, by what a run needs from you.
pub const COLUMNS: [&str; 4] = ["Needs you", "Working", "Idle", "Done"];

pub fn column_of(k: Kind) -> usize {
    match k {
        Kind::Attention => 0,
        Kind::Working | Kind::Starting => 1,
        Kind::Idle => 2,
        _ => 3,
    }
}

impl App {
    /// The runs of a board column, as indexes into `rows`, in list order.
    fn column(&self, c: usize) -> Vec<usize> {
        (0..self.rows.len())
            .filter(|&i| column_of(self.rows[i].kind) == c)
            .collect()
    }

    /// Moves the board selection: `dx` columns across (to the nearest row), `dy` within.
    pub(super) fn board_move(&mut self, dx: isize, dy: isize) {
        let Some(cur) = self.list.selected() else {
            if !self.rows.is_empty() {
                self.set_index(Some(0));
            }
            return;
        };
        let col = column_of(self.rows[cur].kind);
        if dy != 0 {
            let items = self.column(col);
            let pos = items.iter().position(|&i| i == cur).unwrap_or(0);
            let next = pos
                .saturating_add_signed(dy)
                .min(items.len().saturating_sub(1));
            self.set_index(items.get(next).copied());
            return;
        }
        let pos = self.column(col).iter().position(|&i| i == cur).unwrap_or(0);
        // The next column across that has runs, at the same height if it can.
        let mut c = col as isize + dx;
        while (0..COLUMNS.len() as isize).contains(&c) {
            let items = self.column(c as usize);
            if !items.is_empty() {
                self.set_index(Some(items[pos.min(items.len() - 1)]));
                return;
            }
            c += dx;
        }
    }

    pub(super) fn draw_board(&mut self, f: &mut Frame, area: Rect) {
        let t = self.theme.clone();
        self.cards.clear();
        let cols = Layout::horizontal([Constraint::Ratio(1, 4); 4]).split(area);
        let selected = self.list.selected();
        for (c, name) in COLUMNS.iter().enumerate() {
            let items = self.column(c);
            let style = match c {
                0 => t.waiting,
                1 => t.working,
                2 => t.idle,
                _ => t.done,
            };
            let border = if items.iter().any(|&i| Some(i) == selected) {
                t.accent
            } else {
                t.border
            };
            let block = panel(&t, border).title(Line::from(vec![
                span(format!(" {name} "), style),
                span(format!("{} ", items.len()), t.muted),
            ]));
            let inner = block.inner(cols[c]);
            f.render_widget(block, cols[c]);
            // Four lines a card: three of text and a gap. Scrolled to keep the selection.
            let fit = (inner.height / 4).max(1) as usize;
            let pos = items.iter().position(|&i| Some(i) == selected).unwrap_or(0);
            let skip = (pos + 1).saturating_sub(fit);
            for (n, &i) in items.iter().skip(skip).take(fit).enumerate() {
                let r = Rect::new(inner.x, inner.y + n as u16 * 4, inner.width, 3);
                self.draw_card(f, r, i, Some(i) == selected);
                self.cards.push((r, i));
            }
            if items.is_empty() {
                let none = Paragraph::new(Line::from(span(" –", t.muted)));
                f.render_widget(none, inner);
            }
        }
    }

    fn draw_card(&self, f: &mut Frame, r: Rect, i: usize, selected: bool) {
        let t = &self.theme;
        let row = &self.rows[i];
        let w = r.width as usize;
        let mark = if selected { "▌" } else { " " };
        let dot = if self.unseen.contains(&row.run.id) {
            " •"
        } else {
            ""
        };
        let head = format!("{}{dot}", row.run.id);
        let pad = w.saturating_sub(head.chars().count() + row.label.chars().count() + 2);
        let mut where_ = row.run.account.clone();
        if let Some(wt) = &row.run.worktree {
            where_.push_str(&format!(" · {}", wt.branch));
        }
        let mut what = match (&row.run.waiting, &row.run.status) {
            (Some(wt), _) => wt.reason.clone(),
            (None, Some(s)) => s.message.clone().unwrap_or_default(),
            _ => String::new(),
        };
        if let Some(c) = self.conflict_note(&row.run.id) {
            what = format!("{c} {what}");
        } else if let Some(pr) = &row.pr {
            what = format!("{} {what}", pr.label());
        } else if let Some(v) = &row.verify {
            what = format!("{v} {what}");
        }
        let clip = |s: &str| -> String { s.chars().take(w.saturating_sub(2)).collect() };
        let mut lines = vec![
            Line::from(vec![
                span(mark, t.accent),
                span(head, if selected { t.title } else { t.text }),
                span(" ".repeat(pad.max(1)), t.muted),
                span(row.label.clone(), status_style(t, row.kind)),
            ]),
            Line::from(vec![span(mark, t.accent), span(clip(&where_), t.muted)]),
            Line::from(vec![span(mark, t.accent), span(clip(what.trim()), t.muted)]),
        ];
        if selected {
            lines = lines.into_iter().map(|l| l.style(t.selection)).collect();
        }
        f.render_widget(Paragraph::new(lines), r);
    }

    /// The runs shown as tiles: live ones in list order, always with the selected one.
    fn tile_runs(&self) -> Vec<usize> {
        let mut out: Vec<usize> = (0..self.rows.len())
            .filter(|&i| self.rows[i].kind.is_live())
            .take(self.layout.max_tiles)
            .collect();
        if let Some(s) = self.list.selected() {
            if !out.contains(&s) {
                if out.len() == self.layout.max_tiles {
                    out.pop();
                }
                out.push(s);
                out.sort_unstable();
            }
        }
        out
    }

    pub(super) fn draw_tiles(&mut self, f: &mut Frame, area: Rect) {
        let runs = self.tile_runs();
        if runs.is_empty() {
            self.draw_pane(f, area, None, true);
            return;
        }
        // One row for one or two; two rows beyond that, two or three across.
        let n = runs.len() as u32;
        let (rows, across) = match n {
            1 | 2 => (1, n),
            3 | 4 => (2, 2),
            _ => (2, 3),
        };
        let cells: Vec<Rect> = Layout::vertical(vec![Constraint::Ratio(1, rows); rows as usize])
            .split(area)
            .iter()
            .flat_map(|r| {
                Layout::horizontal(vec![Constraint::Ratio(1, across); across as usize])
                    .split(*r)
                    .to_vec()
            })
            .collect();
        let selected = self.list.selected();
        for (cell, i) in cells.into_iter().zip(runs) {
            self.draw_pane(f, cell, Some(i), Some(i) == selected);
        }
    }

    /// The run under a click on a board card or a tile, if any.
    pub(super) fn hit(&self, at: Position) -> Option<usize> {
        self.cards
            .iter()
            .chain(self.panes.iter())
            .find(|(r, _)| r.contains(at))
            .map(|(_, i)| *i)
    }
}

fn status_style(t: &Theme, k: Kind) -> ratatui::style::Style {
    match k {
        Kind::Attention => t.waiting,
        Kind::Working | Kind::Starting => t.working,
        Kind::Idle => t.idle,
        Kind::Done => t.done,
        Kind::Failed => t.failed,
        Kind::Lost => t.lost,
        Kind::Stopped => t.stopped,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app_with(kinds: &[Kind]) -> App {
        let mut app = App::new(Theme::built_in("default").unwrap());
        for (n, k) in kinds.iter().enumerate() {
            app.rows.push(Row {
                run: Run {
                    id: format!("r{n}"),
                    ..Default::default()
                },
                kind: *k,
                label: String::new(),
                verify: None,
                pr: None,
            });
        }
        app.set_index(Some(0));
        app
    }

    #[test]
    fn board_moves_within_and_across_columns() {
        use Kind::*;
        // Columns: needs you [r1], working [r0, r3], idle [], done [r2].
        let mut app = app_with(&[Working, Attention, Done, Working]);
        app.board_move(0, 1);
        assert_eq!(app.selected.as_deref(), Some("r3"));
        app.board_move(1, 0);
        assert_eq!(
            app.selected.as_deref(),
            Some("r2"),
            "the empty idle column is skipped"
        );
        app.board_move(-1, 0);
        assert_eq!(app.selected.as_deref(), Some("r0"));
        app.board_move(-1, 0);
        assert_eq!(app.selected.as_deref(), Some("r1"));
        app.board_move(-1, 0);
        assert_eq!(
            app.selected.as_deref(),
            Some("r1"),
            "no column further left"
        );
    }

    #[test]
    fn tiles_show_live_runs_and_always_the_selected_one() {
        use Kind::*;
        let mut app = app_with(&[Working, Done, Idle, Attention, Working, Working]);
        assert_eq!(app.tile_runs(), [0, 2, 3, 4]);
        app.set_index(Some(1));
        assert_eq!(app.tile_runs(), [0, 1, 2, 3]);
    }
}
