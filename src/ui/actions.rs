//! What the UI does for you: open dialogs and views, and run `amux` commands for stop,
//! remove, verify, review and the rest of the palette.

use super::*;

impl App {
    /// Checks, in the background and every so often, whether runs working on the same repo
    /// would conflict, so the list can warn before anyone merges. Only while two or more
    /// runs with worktrees are alive; at once for --dump.
    pub(super) fn refresh_conflicts(&mut self) {
        let parallel = self
            .rows
            .iter()
            .filter(|r| r.kind.is_live() && r.run.worktree.is_some() && !r.run.id.contains(':'))
            .count();
        if parallel < 2 {
            return;
        }
        let compute = || -> HashMap<String, Vec<String>> {
            let mut m: HashMap<String, Vec<String>> = HashMap::new();
            for rep in crate::work::overlap::reports(None, &[]).unwrap_or_default() {
                for c in &rep.runs {
                    if !c.conflicts_with_target.is_empty() {
                        m.entry(c.run.clone()).or_default().push(rep.target.clone());
                    }
                }
                for p in rep.pairs.iter().filter(|p| !p.conflicts.is_empty()) {
                    m.entry(p.a.clone()).or_default().push(p.b.clone());
                    m.entry(p.b.clone()).or_default().push(p.a.clone());
                }
            }
            m
        };
        if !self.guard {
            *self.conflicts.lock().unwrap_or_else(|e| e.into_inner()) = (compute(), true);
            return;
        }
        let due = self
            .conflicts_at
            .is_none_or(|t| t.elapsed() >= CONFLICTS_EVERY);
        let idle = self.conflicts.lock().map(|c| c.1).unwrap_or(false);
        if due && idle {
            self.conflicts_at = Some(Instant::now());
            if let Ok(mut c) = self.conflicts.lock() {
                c.1 = false;
            }
            let shared = self.conflicts.clone();
            std::thread::spawn(move || {
                let m = compute();
                *shared.lock().unwrap_or_else(|e| e.into_inner()) = (m, true);
            });
        }
    }

    /// Checks, in the background and every few seconds, which live worktree runs have
    /// something listening on their port: a quick connect to localhost, never per frame.
    pub(super) fn refresh_ports(&mut self) {
        let ports: Vec<u16> = self
            .rows
            .iter()
            .filter(|r| r.kind.is_live() && !r.run.id.contains(':'))
            .filter_map(|r| r.run.port)
            .collect();
        let check = move || -> HashSet<u16> {
            ports
                .into_iter()
                .filter(|p| crate::work::notes::listening(*p))
                .collect()
        };
        if !self.guard {
            *self.listening.lock().unwrap_or_else(|e| e.into_inner()) = (check(), true);
            return;
        }
        let due = self.ports_at.is_none_or(|t| t.elapsed() >= PORTS_EVERY);
        let idle = self.listening.lock().map(|c| c.1).unwrap_or(false);
        if due && idle {
            self.ports_at = Some(Instant::now());
            if let Ok(mut c) = self.listening.lock() {
                c.1 = false;
            }
            let shared = self.listening.clone();
            std::thread::spawn(move || {
                let s = check();
                *shared.lock().unwrap_or_else(|e| e.into_inner()) = (s, true);
            });
        }
    }

    /// The run's port, when its dev server is up: for the list, the board and the preview.
    pub(super) fn live_port(&self, run: &Run) -> Option<u16> {
        let port = run.port?;
        let l = self.listening.lock().ok()?;
        l.0.contains(&port).then_some(port)
    }

    /// Opens the run's dev server in the browser (`o`).
    pub(super) fn open_port(&mut self, id: String) {
        let Some(run) = self
            .rows
            .iter()
            .find(|r| r.run.id == id)
            .map(|r| r.run.clone())
        else {
            return;
        };
        match (run.port, self.live_port(&run)) {
            (None, _) => self.say(
                format!("{id} has no port of its own; worktree runs get one."),
                true,
            ),
            (Some(p), None) => self.say(
                format!("Nothing is listening on :{p} yet. Its dev server gets PORT={p}."),
                true,
            ),
            (Some(_), Some(_)) => {
                self.background(vec!["open".into(), id.clone()], format!("Opening {id}"))
            }
        }
    }

    /// What a run's changes conflict with, for the list and the board.
    pub(super) fn conflict_note(&self, id: &str) -> Option<String> {
        let c = self.conflicts.lock().ok()?;
        let with = c.0.get(id)?;
        Some(format!("✗ conflicts with {}", with.join(", ")))
    }

    /// Switches to `v`, or back to the list if it is showing already.
    pub(super) fn toggle_view(&mut self, v: View) {
        self.view = if self.view == v { View::List } else { v };
        self.fitted = None;
        let what = match self.view {
            View::List => "List and preview.",
            View::Board => "Board: a column per state. ←→ moves across, ↑↓ within; b goes back.",
            View::Tiles => {
                "Tiles: up to four live runs at once. Click one to type into it; t goes back."
            }
        };
        self.say(what, false);
    }

    /// Asks GitHub again, in the background, about open PRs whose checks are still
    /// running, at most once a minute per run. Off in `--dump`.
    pub(super) fn refresh_checks(&mut self) {
        if !self.guard {
            return;
        }
        for r in &self.rows {
            let Some(pr) = &r.pr else { continue };
            let settled = matches!(pr.checks.as_str(), "passing" | "failing" | "none");
            if pr.state != "OPEN" || settled {
                continue;
            }
            let due = self
                .checked
                .get(&r.run.id)
                .is_none_or(|t| t.elapsed() >= Duration::from_secs(60));
            if due {
                self.checked.insert(r.run.id.clone(), Instant::now());
                detach_amux(&["checks".into(), r.run.id.clone(), "--quiet".into()]);
            }
        }
    }

    /// Whether the list hides runs (a text or state filter).
    pub(super) fn filtered(&self) -> bool {
        !self.filter.is_empty() || self.show != Show::All
    }

    pub(super) fn explain(&mut self, id: String) {
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

    /// Opens a run's diff to read and comment on.
    pub(super) fn open_review(&mut self, id: String) {
        let Some(run) = self
            .rows
            .iter()
            .find(|r| r.run.id == id)
            .map(|r| r.run.clone())
        else {
            return;
        };
        // On another machine: its diff in the pager (its files aren't here to review).
        if crate::remote::split(&id).is_some() {
            self.pending = Some(Pending::Diff(id));
            return;
        }
        match review::ReviewView::open(&run) {
            Ok(r) => self.mode = Mode::Review(Box::new(r)),
            Err(e) => self.say(e, true),
        }
    }

    /// Asks how to check a run's work: its tests here, or a push for CI.
    pub(super) fn verify(&mut self, id: String) {
        let Some(run) = self
            .rows
            .iter()
            .find(|r| r.run.id == id)
            .map(|r| r.run.clone())
        else {
            return;
        };
        let plan = if crate::remote::split(&id).is_some() {
            // On another machine: amux there works out the details when asked.
            verify::Plan {
                id: id.clone(),
                test: Some("its tests, on that machine".into()),
                branch: run.worktree.as_ref().map(|w| w.branch.clone()),
                remote: run.worktree.as_ref().map(|_| "its remote".into()),
                ..Default::default()
            }
        } else {
            verify::Plan::for_run(&run)
        };
        let (test, push, _) = Confirm::verify_choices(&plan);
        if test.is_none() && push.is_none() {
            self.say(
                format!("{id}: no test command found and no branch to push. `amux verify {id}` says more."),
                true,
            );
            return;
        }
        self.mode = Mode::Confirm(Confirm::Verify(Box::new(plan)));
    }

    pub(super) fn attach_run(&mut self, id: String, kind: Kind) {
        if kind.is_live() {
            self.pending = Some(Pending::Attach(id));
        } else {
            self.say(format!("{id} has ended; d removes it."), true);
        }
    }

    pub(super) fn stop_run(&mut self, id: String, kind: Kind, force: bool) {
        if kind.is_live() {
            self.mode = Mode::Confirm(Confirm::Stop { id, force });
        } else {
            self.say(format!("{id} is not running."), true);
        }
    }

    pub(super) fn remove_run(&mut self, id: String, kind: Kind, force: bool) {
        if kind.is_live() && !force {
            self.say(
                format!("{id} is still running. s stops it; D kills and removes it."),
                true,
            );
        } else {
            self.mode = Mode::Confirm(Confirm::Remove { id, force });
        }
    }

    pub(super) fn open_spawn(&mut self, account: Option<&str>, prompt: &str) {
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

    pub(super) fn open_palette(&mut self) {
        self.mode = Mode::Palette(PaletteState {
            input: "/".into(),
            sel: 0,
            ctx: Ctx {
                runs: self.rows.iter().map(|r| r.run.id.clone()).collect(),
                accounts: account_ids(),
                themes: theme::available(),
                plugins: crate::plugins::list().into_iter().map(|p| p.name).collect(),
            },
        });
    }

    /// A run by name or unique prefix among those shown; the selected one when None.
    pub(super) fn resolve_run(&mut self, name: Option<String>) -> Option<(String, Kind)> {
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

    pub(super) fn run_action(&mut self, action: Action) {
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
            Action::Review(r) => {
                if let Some((id, _)) = self.resolve_run(r) {
                    self.open_review(id);
                }
            }
            Action::View(v) => match v.as_deref().map(View::parse) {
                None => self.toggle_view(match self.view {
                    View::List => View::Board,
                    View::Board => View::Tiles,
                    View::Tiles => View::List,
                }),
                Some(Some(v)) => {
                    // From the list, toggling to `v` shows it; for the list, that's a no-op.
                    self.view = View::List;
                    if v != View::List {
                        self.toggle_view(v);
                    }
                }
                Some(None) => self.say("Which view? list, board or tiles.", true),
            },
            Action::Layout(args) if args.first().map(String::as_str) == Some("save") => {
                self.layout.view = self.view;
                match self.layout.save() {
                    Ok(()) => self.say("Layout saved in config.json.", false),
                    Err(e) => self.say(format!("Not saved: {e:#}"), true),
                }
            }
            Action::Layout(args) => match self.layout.apply(&args) {
                Ok(msg) => {
                    self.fitted = None;
                    self.say(msg, false)
                }
                Err(e) => self.say(e, true),
            },
            Action::Plugin(mut args) => {
                let name = args.remove(0);
                let mut argv = vec!["plugin".to_string(), "run".into(), name.clone()];
                if let Some(id) = self.selected.clone() {
                    argv.extend(["--run".to_string(), id]);
                }
                argv.extend(args);
                self.background_output(argv, format!("Plugin {name}"));
            }
            Action::Compare(task) => self.background_output(
                vec!["compare".into(), task.clone()],
                format!("Attempts at {task}"),
            ),
            Action::Revive(r) => {
                let mut argv = vec!["revive".to_string()];
                match r {
                    Some(name) => argv.push(name),
                    None => argv.push("--all".into()),
                }
                self.background_output(argv, "Reviving lost runs".into());
            }
            Action::Gc => self.background_output(vec!["gc".into()], "Leftover processes".into()),
            Action::Usage => self.background_output(vec!["usage".into()], "Usage limits".into()),
            Action::Failover(r) => {
                if let Some((id, _)) = self.resolve_run(r) {
                    self.background(
                        vec!["failover".into(), id.clone()],
                        format!("Moving {id} to another account"),
                    );
                }
            }
            Action::Overlap => {
                self.background_output(vec!["overlap".into()], "Overlapping runs".into())
            }
            Action::Land(r) => {
                if let Some((id, _)) = self.resolve_run(r) {
                    self.background_output(
                        vec!["land".into(), id.clone()],
                        format!("Landing {id}"),
                    );
                }
            }
            Action::Diff(r) => {
                if let Some((id, _)) = self.resolve_run(r) {
                    self.pending = Some(Pending::Diff(id));
                }
            }
            Action::Send(text) => {
                if let Some((id, kind)) = self.resolve_run(None) {
                    if kind.is_live() {
                        self.background(
                            vec!["send".into(), id.clone(), "--".into(), text],
                            format!("Sending to {id}"),
                        );
                    } else {
                        self.say(format!("{id} has ended."), true);
                    }
                }
            }
            Action::Verify(r) => {
                if let Some((id, _)) = self.resolve_run(r) {
                    self.verify(id)
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
}
