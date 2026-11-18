# `amux ui`: design

## Status

**Built (MVP, branch `amux/tui-design`).** `amux ui` opens one screen over every run:

- a run list with live state: `waiting` (from the host's `Run.waiting`, PR #3), `working`, `idle`, `starting`, `done`, `exit N`, `lost`. Header counts per state. A marker on runs that changed while you looked elsewhere.
- a live preview of the selected run's screen: a vt100 emulator fed from the run host's socket, in color, with the CLI's window title and the waiting reason in the frame. PgUp/PgDn scrolls back.
- **Enter** attaches in place, on the same connection with no replay. **Ctrl-]** returns to the list.
- **Tab** jumps to the next run that needs you. **1–9** jump to a run.
- **n** opens the spawn dialog (account, prompt, name, worktree, branch). **s** / **S** stop or kill a run, **d** / **D** remove one or kill and remove it, each with a y/n confirm. **?** shows help, **q** quits and every run keeps going.
- `amux ui --dump [--size RxC] [--select run]` prints one frame as text, for scripts and tests.
- One backward-compatible host protocol addition, `WATCH`, so a preview doesn't silence notifications. The host also publishes the terminal size in `runs/<id>/size`.

**Try it** in a sandbox. The fake CLI in `tests/cli.rs` works, as does any real account:

```sh
cargo build
amux spawn --worktree claude@work          # or: amux spawn fake@one with a sandbox AMUX_HOME
amux spawn --name review codex@personal
target/debug/amux ui
```

**Left to do**, in order of value. Each is covered in [Next steps](#next-steps):

1. ~~OSC 7501 program status in the host~~ **Done.** See Next steps.
2. ~~`/` search and state filters~~ **Done**: the command palette, `/filter`, and `f` / `/show` for waiting, working, idle or ended runs. herdr's `b`/`w`/`i`/`d` keys became one key that steps through the states, because `d` already removes a run here.
3. A switch key while attached, so you can move to the next waiting run without going back to the list.
4. Host-pushed state events, instead of polling `run.json` every 500 ms.
5. Resume for `lost` runs after a reboot, and `amux gc` for stale worktrees and merged branches.
6. Mouse support.

`cargo fmt`, `cargo clippy --all-targets --locked -- -D warnings` and `cargo test` pass. The tests include two new end-to-end tests: `ui --dump` against live and finished runs, and a watching client that must not silence notifications.

---

## 1. What the others do

amux already has the hard part: a run host per agent that owns the PTY, logs output, serves a Unix socket, and (since 0.4) detects waiting prompts and notifies you. The question is what the screen on top should look like. Three tools are the reference points:

- **herdr**, a Rust terminal multiplexer for agents: <https://github.com/herdrdev/herdr>
- **Orca**, an Electron "agent IDE": <https://github.com/stablyai/orca>
- **Rex**, a native terminal server from Superlogical: <https://www.superlogical.com/rex/docs>. Rex is closed source, so its row in the table comes from its docs and third-party write-ups, not from code.

### Comparison

| | herdr | Orca | Rex | **amux ui** |
|---|---|---|---|---|
| Form | TUI, single Rust binary | Electron desktop app, plus mobile | Native macOS client plus a server | TUI inside the amux binary |
| License | Apache-2.0 | MIT | Closed (open source promised) | MIT |
| Process model | Server owns the PTYs; the TUI is one of several clients | Separate terminal daemon (node-pty) survives app quit and update | Server owns the sessions; clients connect | One `__run-host` per run owns its PTY; the UI is just a client |
| Terminal parsing | libghostty-vt on the server; sends surface deltas to clients | `@xterm/headless` in the daemon for snapshots; xterm.js + WebGL in the renderer | Raw bytes teed to each client; each client parses with libghostty | Host forwards raw bytes; the UI parses with `vt100` only for runs it previews |
| Layout | workspace → tab → pane splits, sidebar, goto picker | project → worktree sidebar, tabs and splits, kanban "Agent Dashboard" | session → window → block, native splits, Lua keymap | Run list plus one preview; attach takes the whole terminal |
| Keys | tmux-style prefix (`ctrl+b`), `ctrl+alt` chords, mouse | Cmd shortcuts, Cmd-J jump palette | Lua `rex.bind`, prefix chords, command palette | Single keys, no prefix; Ctrl-] (same as `amux attach`) |
| Attach/detach | `herdr agent attach`, `--takeover`, `ctrl+b q` | Implicit: quit and relaunch reattaches warm | Client close keeps sessions | `amux attach` / `amux ui`; quitting the UI never stops a run |
| State detection | Process identification plus versioned TOML screen-scrape manifests (regions, regexes), agent self-reports and hooks, debounced | Managed hooks for about 30 agents, then OSC 7501, then OSC title | Authored **OSC 7501** (program status): agents report idle, working, blocked or done | Host: provider `waitingPatterns`, bell, OSC 9/777/99 (`detect.rs`); output age for working/idle |
| Notifications | Toast, OSC 9/99 to the outer terminal, system notifier, sounds; suppressed for the active tab | System notification, sound, bell inbox, Dock badge, mobile push | Lua events only; nothing built in | Host: desktop notification or `notifyCommand`, only when nobody is attached (a UI preview doesn't count) |
| Worktrees | Created per workspace; remove asks before forcing; branches never deleted | One task, one worktree, one branch; `.worktreeinclude`; deletes branch with confirm | None (community add-ons) | `--worktree` at spawn; `rm` removes only clean worktrees and keeps branches |
| Persistence | Detach keeps processes; layout and resume argv restored after a server restart | Daemon survives app restarts; after a reboot, layout and scrollback but no processes | Sessions survive client close; after a server restart, layout with fresh shells | Hosts survive the UI and terminal; `run.json` and logs survive a reboot (runs show `lost`) |
| Scale and performance | Benchmarks at 15+ panes; issues about lag with many agents and a 60 Hz cap | Reports of 2 GB of RAM, input lag under dense output, renderer CPU | Claims of very many sessions (unverified) | One small process per run; the UI parses only up to four previews |
| Praise | tmux feel, state sidebar, socket API, persistence | Worktree UX, diff review, mobile, integrations | Speed, Ghostty lineage, scriptable CLI | n/a |
| Complaints | Detection gaps with wrapped binaries, lag, the TUI and the app can't see each other's agents | Heavy, about 8k open issues, runs agents with `--dangerously-skip-permissions` / `--yolo` by default | macOS-only client, invite-only, closed | n/a |

### What amux should copy

- **Attention first** (herdr, Orca). The point of a supervisor is to show which agent needs you. `amux ui` puts the waiting count in the header, colors waiting runs yellow, shows the prompt text in the preview frame, and makes Tab jump to the next one. herdr's `done` means "finished and not yet looked at", tracked per client. We copy that as the unseen marker.
- **A server-side PTY owner with thin clients** (all three). amux has this already, and the UI keeps it. Quitting or crashing the UI never touches a run.
- **Only render what is visible** (herdr's hot-path rule). No per-frame filesystem or process scans. The UI parses output only for previews it holds, at most four.
- **Agent-reported state over scraping** (Orca's hooks, Rex's OSC 7501). This is the top item in Next steps. The host is in the right place to answer the OSC 7501 probe.
- **Safe worktree cleanup** (herdr). Never delete uncommitted work, and keep branches. `amux rm` already does this, and the UI calls `amux rm`.

### What amux does better, and why

- **No guessing which agent runs in a pane.** herdr finds the agent from the PTY's foreground process and misses wrapped binaries such as nix's `.claude-unwrapped`. amux started the CLI itself, so `run.json` already records the provider and account.
- **Multi-account, and per run.** None of the three know about accounts. In amux, the spawn dialog's first field is the account, and every row shows it.
- **No second server.** herdr's TUI and app keep separate servers, and Orca adds a daemon. `amux ui` is one more client of the same run hosts that `amux ps` and `amux attach` use. Anything started in a shell shows up in the UI, and anything started in the UI shows up in `amux ps`.
- **Light.** One binary. Measured cost of the UI is in section 4. No Electron, no web renderer, and no daemon kept running when no runs exist.
- **Attach is the real terminal.** In place, the CLI gets the whole terminal and your keystrokes byte for byte, exactly as in `amux attach`. It is not drawn through a second emulator. Copy and paste, scrollback and image protocols behave as they do in your terminal. herdr and Orca render through an emulator, which is where their lag and emoji-width bugs come from.
- **Safe defaults.** Orca starts agents with permission prompts off. amux keeps the CLI's own prompts, since surfacing "waiting for permission" is the whole point.

---

## 2. Architecture

```
  amux ui ──────────────┐   (one process; quitting it leaves every run alone)
   │ list:  runs::list() every 500 ms  →  classify(state, Run.waiting, output age)
   │ preview: ≤4 sockets, each a client of a run host, read by a thread into vt100
   │ attach:  hand the terminal to the preview's socket, Ctrl-] takes it back
   │ actions: `amux spawn | stop | rm` as background subprocesses
   ▼
 runs/<id>/sock  ◀──── amux __run-host <id>  ────▶  PTY  ────▶  claude / codex / gemini
                         output.log, run.json (waiting, exit), size, notifications
```

The UI is a **client of the existing run hosts**. It doesn't replace them. Run state stays where it is: `run.json` is written by the host, `output.log` by the pump. The UI reads the same files `amux ps` reads.

### Run list with live state

`ui/status.rs` classifies each run from three signals:

1. `Run::state()`: starting, running, exited, failed, lost.
2. `Run.waiting`, which the host sets from `detect.rs`: the prompt text, bell or notification that showed it.
3. The modification time of `output.log`: working if it changed in the last 5 s, otherwise idle.

These are the same signals as `amux ps`, so the two never disagree. Rows keep their start order, so they don't jump under the cursor. Attention is shown instead by color, glyph (◆ waiting, spinner working, ◌ idle, ✓ done, ✗ failed), header counts, and Tab. Tab visits, in order: runs that changed while unseen, then waiting, failed, done, idle.

### Live preview

`ui/preview.rs`. A preview is an ordinary host client:

1. It connects to `runs/<id>/sock` and sends `WATCH [1]` as its first bytes. When the path is longer than the socket limit, it connects through a short symlink in a fresh mode-0700 folder under `/tmp`. Unlike `amux attach`, the UI never changes its working directory, because the background `amux spawn` commands inherit it.
2. The host sends a snapshot of its current screen, then streams live output. A reader thread feeds both into a `vt100::Parser` sized to the run's terminal, which it reads from the host's `size` file, falling back to `run.json`.
3. Replay starts at a line boundary, so it can begin in the middle of a full-screen redraw. To repair that, the preview sends a `RESIZE` with the run's *current* size: nothing moves, and the CLI redraws (SIGWINCH). This is skipped for hosts from before 0.5, which don't publish `size`. That way the preview can never resize a run to a guessed size.
4. Drawing crops the emulator's screen to the pane, keeping the bottom of the content (or the cursor) in view, since that is where CLIs ask questions. The run is **not** resized to the pane. Shrinking an agent to a 60-column box would make it reflow, and you would attach to a squashed transcript.
5. A finished run's preview is the tail of its log through the same emulator.

`Previews` keeps the four most recently viewed connections open, so flipping between a few runs is instant and holds at most four sockets. Each reader thread marks its preview dirty; the UI redraws only when something is dirty, a key was pressed, or a spinner is visible (10 fps).

### Attach in place and switching

`ui/attach.rs`. On Enter:

1. Leave the alternate screen. Under the preview's lock, paint the emulator's current screen (`state_formatted`, which also restores bracketed paste and keypad modes), then turn on passthrough, so the reader thread writes output straight to the terminal. No byte is lost or doubled between the picture and the stream.
2. Send `WATCH [0]` (a person is attached now, so the host doesn't notify) and a `RESIZE` to the real terminal size.
3. Read stdin directly (`poll` plus `read` on fd 0, not the buffered `Stdin`) and forward it as `INPUT` frames, until Ctrl-]. Ctrl-] also counts in its kitty keyboard encoding (`ESC[93;5u`), which a CLI may have turned on. Follow terminal resizes.
4. On Ctrl-], when the run ends, or on an error (a Drop guard does this): passthrough off, `WATCH [1]`, reset the modes a CLI may have left on (the alternate screen, mouse, bracketed paste, focus events, kitty keyboard, modifyOtherKeys), re-enter the UI and repaint.

Switching runs is Ctrl-] then j/k (or 1–9, or Tab) then Enter. The preview already holds a live connection, so attaching again costs one redraw. A one-chord switch while attached is in Next steps.

### Spawn, stop and remove

The spawn dialog (`ui/spawn.rs`) builds the same argument list you would type: `spawn [--name n] [--worktree [--branch b]] <account> <prompt args>`. The prompt goes through the provider's `promptArgs` template, for example `-i {prompt}` for Gemini. The worktree box is on by default inside a git repository. The UI then runs `amux spawn …`, `amux stop <id> [--force]` or `amux rm <id> [--force]` as a background subprocess with stdin closed. It shows the command's own message in the footer and selects the new run. Sharing the commands means:

- A run started from the UI is indistinguishable from one started in a shell.
- Worktree cleanup, uncommitted-change checks and error text stay in one place.
- A slow `git worktree add` or a 5 s `stop` never freezes the screen.

### Keybindings

| Key | Action |
|---|---|
| ↑ ↓ / j k, g G, 1–9 | Select a run |
| Tab | Next run that needs you |
| Enter / a | Attach in place; **Ctrl-]** returns |
| n | New run (Tab between fields, ←/→ account, Space worktree, Enter start, Esc cancel) |
| s / S | Stop (SIGTERM) / kill (SIGKILL), with confirm |
| d / D | Remove a finished run / kill and remove, with confirm |
| PgUp PgDn, Esc | Scroll the preview back, and back to live |
| / or : | Command palette (see below) |
| Esc | Clear the filter, or return the preview to live |
| f | Show only waiting, working, idle or ended runs (steps through them) |
| e | Why the run shows its state (`amux explain`) |
| c | Review the run's diff: Enter comments on a line, S sends the comments, Space marks a hunk to drop, D drops the marked hunks (y/n) |
| ? | Help |
| q, Ctrl-C | Quit (runs keep going) |

### Command palette and themes

The palette is how herdr's goto picker and Rex's command palette fit a single-list UI. `/` opens it at the top, where the eye already is.
- Typing ranks commands fuzzily. Prefix beats substring, which beats letters in order, and a command's own name beats its aliases.
- Each line shows what the command does and the single key that does the same, so the palette teaches the shortcuts.
- After a space, the argument completes from runs (`/stop`, `/logs`), accounts (`/switch-account`, `/new`) or themes (`/theme`). Tab completes, and Enter runs. A command that needs an argument waits for one.
- Commands that need the whole terminal, such as `/add-account` (it signs you in) and `/logs` (your pager), get it the same way attach does, and come back after.
- `/accounts` and `/doctor` show their output over the list.

Parsing and completion live in `ui/palette.rs` as plain functions and are unit tested.

Themes (`ui/theme.rs`) work by role, not by color: text, muted, border, title, accent, key, selection, header, input, one role per run state, and success and error. A role is a git-style spec (`"#e8b65a bold"`, `"bold on blue"`). No drawing code names a color.

The default theme uses only the terminal's 16 colors, plus bold and dim. That way it follows the user's own palette on light and dark terminals, and color only marks meaning: the accent and the states. `dark` and `light` are truecolor themes checked for contrast. `mono` is for `NO_COLOR`. `/theme name` switches live and saves the choice in `config.json` under `ui`, a key older amux versions keep untouched. `amux ui --dump --ansi --keys "/"` renders any screen in color without a terminal, which is how the themes were reviewed.

There is no prefix key: the list has the keyboard, and the CLI has it only while attached. Ctrl-] is the same detach key as `amux attach`, so there is one thing to learn. herdr found that `ctrl+alt` chords are free in most terminals. We would use that family if we add keys that work while attached.

### Scaling to 20+ runs

| Cost | Per run | Notes |
|---|---|---|
| Host process | 1 small process, 3 threads, one emulated screen | Unchanged by the UI |
| List refresh | 1 small JSON read and 1 `stat` every 500 ms | 20 runs: 40 syscalls per tick |
| Sockets and parsers | 0, except the ≤4 previewed runs | Hidden runs cost the UI nothing |
| Drawing | Only on change; the list is a scrolling `ListState` | Two rows per run |

Measured with 28 runs (release build, fake CLI, Apple Silicon): `amux ui --dump` takes 0.23 s, most of it waiting for the preview's replay. The interactive UI uses 0.12 s of CPU over 10 s idle (about 1%) and 4.6 MB resident. The test runs were quiet; streaming runs add only the cost of the previewed ones.

The list scrolls, and 1–9 plus Tab cover the common jumps. Past about 50 runs, polling `run.json` would be the first thing to replace: see host-pushed events in Next steps.

---

## 3. Host protocol changes (backward compatible)

| Change | Old host, new UI | New host, old client |
|---|---|---|
| `WATCH` frame (kind 2, payload `[1]` watching, `[0]` attached). A watching client doesn't count as attached, so waiting and exit notifications still fire while a preview is open. If it is the client's first frame, the host reads it before the replay (it peeks for up to 20 ms), so a preview never counts as attached, not even during the replay. | Ignored (unknown kinds were always skipped). The preview then suppresses notifications, as any client did in 0.4. | Never sent; every client counts as attached, as before. |
| `runs/<id>/size` ("rows cols"), written atomically at start and on every `RESIZE` | No file, so the preview uses `run.json`'s start size and does not send the redraw `RESIZE` | Unused |

Nothing else changed in the protocol. The edits to `src/runs/host.rs` are about 70 lines: a `Client` struct with an `AtomicBool`, a `Hub::attached()` helper, the leading-`WATCH` peek, and the size file. `src/runs/mod.rs` gains one constant. `Run`, `run.json` and `detect.rs` are untouched.

`src/runs/attach.rs` (`amux attach`) changed in two small ways that the UI shares. `find_detach` also recognizes the kitty encoding of Ctrl-], and `RESET_MODES` (now `pub`) also pops the kitty keyboard mode and modifyOtherKeys, so a shell isn't left in those modes after detaching. Terminals that don't support them ignore both sequences.

Why not put the size in `run.json`? The host rewrites `run.json` whenever `waiting` changes, and a separate file keeps resizes from racing those writes. The file's presence also tells the UI the host is new enough to trust for a redraw.

---

## 4. Crate choices

| Need | Choice | Why | Alternatives |
|---|---|---|---|
| TUI framework | `ratatui` 0.30, `default-features = false, features = ["crossterm"]` | Standard, and it is what herdr uses. Diffed rendering, `TestBackend` for `--dump` and tests | `cursive` (heavier, retained-mode) |
| Terminal backend | `crossterm` 0.29, through ratatui | Pure Rust, raw mode and events | `termion` (Unix only, less maintained) |
| VT emulator | `vt100` 0.18 | Small, pure Rust, scrollback, title and bell callbacks, and `state_formatted()` to repaint on attach | `alacritty_terminal`: more complete, but pulls in alacritty's event loop, config and threads, for a larger binary. libghostty-vt (herdr): best fidelity, but a Zig/C build dependency |

**MSRV.** No crate in the tree declares a `rust-version` above 1.88. The highest are `darling` and `instability`, which are proc-macro dependencies of ratatui, both at 1.88. Built and tested with 1.99, and clippy is clean. The `let Ok(x) = infallible;` patterns in `ui/mod.rs` need 1.82 or later. I couldn't build with a 1.88 toolchain here because only stable is installed. CI's MSRV job is the place to confirm.

**Binary size** (release profile: LTO, `codegen-units = 1`, stripped, aarch64-apple-darwin):

| Build | Size |
|---|---|
| `main` (0.4.0) | 2,476,240 bytes (2.48 MB) |
| with `amux ui` (ratatui, crossterm, vt100 and `src/ui/`) | 2,873,920 bytes (2.87 MB) |
| **Difference** | **+397 KB (+16%)** |

ratatui's `default-features = false` already drops the termion and termwiz backends and the extra widgets. If size matters more later, putting `amux ui` behind a default-on cargo feature would let minimal builds leave out all three crates.

---

## 5. Next steps

1. **OSC 7501 in the host. Done.** Before building it, a second research pass compared how herdr, Orca and Rex learn agent state:
   - **herdr** dropped hook-based state for Claude and Codex in 0.6.7, because of stuck states (#198, #349) and prompts that fire no hook (#249). Its hooks now only record the session ID.
   - **Orca** installs about 15 hook events into the user's config and fights a long tail of bugs: `CLAUDE_CONFIG_DIR`, duplicate hooks, trust prompts, missed cancel events.
   - **Codex** sends OSC 7501 on any terminal without being asked (verified in `codex-rs/tui/src/terminal_program_status.rs`). Claude Code 2.1.295+ sends it once the probe is answered (its changelog, and Orca's live testing).

   So the host now:
   - answers the probe;
   - parses root-record reports into `Run.waiting` (blocked) and a new optional `Run.status` (state, kind, message, since);
   - lets those reports decide once the first one arrives;
   - keeps `waitingPatterns`, bells and OSC 9 as the fallback for CLIs that don't report;
   - applies the spec's lifetimes: drop `working` and `blocked` on exit or OSC 133 A, keep `done` and `error`.

   `amux ps` and `amux ui` show the reported state, and the preview frame shows the CLI's message while it works. Two things are unverified, because they need a real Claude Code:
   - Once the probe is answered, Claude stops putting its spinner in the window title (Orca's finding). amux doesn't use the title for state.
   - Claude follows the probe with `CSI c`, a request for the terminal's identity (DA1). When no terminal is attached, nobody answers that request. This was already true before this change.

   Also, if an attached terminal speaks OSC 7501 itself, as Rex and Orca do, the CLI gets two replies.
   
   `amux explain <run>` (and `e` in the UI) says which signal decided, from `waiting.source`, which the host now records. It also lists what amux watches for in that run, and shows the bottom of its screen as evidence, like herdr's `agent explain`. Still to do: notifying when a reported turn ends (`working` → `done`) while nobody is attached.
2. **Search and filters.** `/` fuzzy match over run id, account, branch and title. `w`, `i`, `d` to show only waiting, idle or done runs.
3. **Switch while attached.** A `ctrl+alt` chord, or Ctrl-] followed by n or p within 500 ms, to move to the next waiting run without seeing the list.
4. **Host-pushed state.** A `SUBSCRIBE` frame that makes the host send framed state changes (waiting, exit) on a separate connection kind. The list could then stop polling. This needs a framed host-to-client stream, which is why it is its own protocol version.
5. **Lost runs and cleanup.** Record each provider's resume argv (session id) so a `lost` run can be resumed with one key. Add `amux gc` to list stale worktrees and merged `amux/*` branches.
6. **Debounce idle.** herdr waits for 3 confirmations 100 ms apart before showing idle. Our 5 s output-age rule is coarse but never flickers. Revisit once OSC 7501 gives real state.

## Sources

- herdr: <https://github.com/herdrdev/herdr>, including `AGENTS.md`, `src/detect/`, `src/pane/agent_detection.rs` and the docs under `docs/`
- Orca: <https://github.com/stablyai/orca>, including `src/main/daemon/`, `src/main/agent-hooks/README.md` and `docs/site/content/docs/`
- Rex: <https://www.superlogical.com/rex/docs>, including the program status spec at `/rex/docs/build/program-status`. Architecture details come from third-party write-ups, such as <https://syntackle.com/blog/herdr-rex-and-orca-free-tools-for-running-ai-coding-agents-in-parallel>, and are unverified against source.
