//! Keys: what each one does in the list, the dialogs, the palette and the new-run form.
//! Typing into a run is in `typing.rs`.

use super::*;

impl App {
    pub(super) fn key(&mut self, k: KeyEvent) {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let typing = matches!(self.mode, Mode::Spawn(_) | Mode::Palette(_));
        if ctrl && k.code == KeyCode::Char('c') && !typing {
            self.quit = true;
            return;
        }
        let was_confirm = matches!(self.mode, Mode::Confirm(_));
        if self.focus_run && matches!(self.mode, Mode::Normal) {
            self.key_run(k);
            return;
        }
        self.key_mode(k);
        if !was_confirm && matches!(self.mode, Mode::Confirm(_)) {
            self.confirm_at = Instant::now();
        }
    }

    pub(super) fn key_mode(&mut self, k: KeyEvent) {
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
            Mode::Confirm(Confirm::Verify(p)) => {
                let (test, push, prefer_push) = Confirm::verify_choices(&p);
                let pick = match k.code {
                    KeyCode::Char('t' | 'T') => "test",
                    KeyCode::Char('p' | 'P') => "push",
                    KeyCode::Char('f' | 'F') | KeyCode::Enter if p.failed => "send",
                    KeyCode::Char('r' | 'R') if p.branch.is_some() => "pr",
                    KeyCode::Char('k' | 'K') if p.ci_failed => "ci",
                    KeyCode::Enter if prefer_push => "push",
                    KeyCode::Enter => "test",
                    _ => return self.say("Cancelled.", false),
                };
                match pick {
                    "pr" => self.background(
                        vec!["pr".into(), p.id.clone()],
                        format!("Opening a PR for {}", p.id),
                    ),
                    "ci" => self.background(
                        vec!["checks".into(), p.id.clone(), "--send-failures".into()],
                        format!("Sending the failing checks to {}", p.id),
                    ),
                    "send" => self.background(
                        vec!["verify".into(), p.id.clone(), "--send-failures".into()],
                        format!("Sending the failures to {}", p.id),
                    ),
                    "test" if test.is_some() => {
                        // Detached: tests can take a while, and outlive the UI. The list
                        // shows how they go.
                        detach_amux(&["verify".into(), p.id.clone(), "--test".into()]);
                        self.say(
                            format!("Testing {} (waits for a test slot if needed).", p.id),
                            false,
                        );
                    }
                    "test" => self.say(
                        "No test command found; set \"verify\": { \"test\": … } in config.json.",
                        true,
                    ),
                    _ if push.is_some() => self.background(
                        vec!["verify".into(), p.id.clone(), "--push".into()],
                        format!("Pushing {}", p.branch.as_deref().unwrap_or_default()),
                    ),
                    _ => self.say(format!("{} has no branch and remote to push.", p.id), true),
                }
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
            Mode::Review(mut r) => match r.key(k) {
                review::Outcome::Close => {}
                review::Outcome::Stay => self.mode = Mode::Review(r),
                review::Outcome::Say(msg, err) => {
                    self.say(msg, err);
                    self.mode = Mode::Review(r);
                }
                review::Outcome::Send => {
                    self.background(
                        vec!["comments".into(), r.id.clone(), "--send".into()],
                        format!("Sending {} comments to {}", r.pending(), r.id),
                    );
                    self.mode = Mode::Review(r);
                }
            },
        }
    }

    pub(super) fn key_normal(&mut self, k: KeyEvent) {
        let cur = self.current().map(|r| (r.run.id.clone(), r.kind));
        // Letters in the list are shortcuts, so text meant for a run would open dialogs
        // and panels (and "sky" would stop it). A letter that isn't a shortcut starts a
        // short typing window: until it ends, letters only repeat how to type into the run.
        if let KeyCode::Char(c) = k.code {
            let plain = !k
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
            let typing = self.typing_until.is_some_and(|t| Instant::now() < t);
            // Typing a letter that isn't a shortcut means typing to the agent: move into its
            // pane and pass the letter on, so nothing typed is lost.
            if plain && !is_shortcut(c) && cur.as_ref().is_some_and(|(_, k)| k.is_live()) {
                self.focus_run = true;
                self.send_key(&k);
                return;
            }
            if plain && self.guard && (typing || !is_shortcut(c)) {
                self.typing_until = Some(Instant::now() + TYPING);
                let hint = match &cur {
                    Some((id, kind)) if kind.is_live() => {
                        format!("Letters here are shortcuts. Press ⏎ to type into {id}.")
                    }
                    _ => "Letters here are shortcuts; ? lists them.".to_string(),
                };
                self.say(hint, false);
                return;
            }
        }
        if self.view == View::Board {
            let moved = match k.code {
                KeyCode::Char('j') | KeyCode::Down => Some((0, 1)),
                KeyCode::Char('k') | KeyCode::Up => Some((0, -1)),
                KeyCode::Left => Some((-1, 0)),
                KeyCode::Right => Some((1, 0)),
                _ => None,
            };
            if let Some((dx, dy)) = moved {
                self.board_move(dx, dy);
                return;
            }
        }
        match k.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('b') => self.toggle_view(View::Board),
            KeyCode::Char('t') => self.toggle_view(View::Tiles),
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
            // Type into the run right in its pane.
            KeyCode::Enter | KeyCode::Char('i') => match cur {
                Some((_, kind)) if kind.is_live() => {
                    self.focus_run = true;
                    self.say("Typing into the run. Ctrl-] goes back to the list; Ctrl-B then a key runs a command.", false);
                }
                Some((id, kind)) => self.attach_run(id, kind),
                None => {}
            },
            // The run gets the whole terminal.
            KeyCode::Char('a' | 'z') => {
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
            KeyCode::Char('c') => {
                if let Some((id, _)) = cur {
                    self.open_review(id);
                }
            }
            KeyCode::Char('v') => {
                if let Some((id, _)) = cur {
                    self.verify(id)
                }
            }
            KeyCode::Esc if self.scroll == 0 && self.filtered() => {
                self.filter.clear();
                self.show = Show::All;
                self.reload();
            }
            KeyCode::PageUp => self.scroll_preview(10),
            KeyCode::PageDown => self.scroll_preview(-10),
            KeyCode::Esc => self.scroll_preview(-(self.scroll as isize)),
            KeyCode::Char('?') => self.mode = Mode::Help,
            _ => {}
        }
    }

    /// Returns the palette if it stays open.
    pub(super) fn key_palette(&mut self, mut p: PaletteState, k: KeyEvent) -> Option<PaletteState> {
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

    /// Returns whether the form stays open.
    pub(super) fn key_spawn(&mut self, f: &mut SpawnForm, k: KeyEvent) -> bool {
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
}
