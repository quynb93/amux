# Changelog

All notable changes to amux are listed here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and amux uses [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added

- **Best of N.** `amux try 3 claude -p "<task>" --name fix` starts several attempts at one task, each in its own worktree. `amux try claude@work,codex@me …` gives each attempt its own account or CLI.
  - `amux compare fix` runs each attempt's tests in a test slot and ranks them: passing first, then the smaller change (fewer files, fewer lines). It also shows commits and time, and recommends one (`--json` for scripts, `/compare` in `amux ui`).
  - `amux pick fix-2 --land` keeps the winner and lands it through the merge queue. The other attempts are stopped and removed. Their branches are kept, and so are any worktrees holding uncommitted work.

## [0.8.0] - 2026-11-17

Usage limits no longer stop the work: amux spots them, routes new runs to accounts with room, and moves a limited run's session to another account. Runs that work without getting anywhere are flagged, and processes finished runs leave behind are cleaned up.

### Added

- **Guards against runaway and stuck agents.**
  - Each run's host takes a fingerprint of the run's files (HEAD and uncommitted changes) every minute. A run that keeps working with no change for the `stall` time (30 minutes by default) is flagged under "needs you" as `stalled: working 34m with no change to its files`, with a notification and the `stalled` hook. It clears when the files change.
  - `maxTime` flags runs that have been alive too long.
  - `onStall` can also `interrupt` (press Esc, which ends the agent's turn) or `stop` the run.
  - Set these in `"limits"` in config.json, or per run with `amux spawn --stall 20m --max-time 2h` ("0" turns one off).
- **`amux gc [--dry-run]`** ends processes that finished runs left behind: dev servers, watchers, test runners. They're found by the run's process group, by `AMUX_RUN` in their environment (Linux), or by working inside the run's own worktree. It never matches by folder for runs without a worktree. Also `/gc` in `amux ui`.
- **Usage limits across accounts.**
  - When a run's CLI says its account hit its usage limit ("You've hit your limit · resets 7pm"), the run shows as waiting with that message, the account is marked limited until the reset time the message names, and a notification and the `limited` hook go out. The messages come from each provider's new `limitPatterns`, with defaults for Claude Code, Codex and Gemini CLI.
  - `amux usage` lists each account, whether it is limited and until when, and its live runs. `--clear` forgets a limit that lifted early.
  - `amux spawn <provider>` (no account named) skips a limited account for one that isn't.
  - `amux failover <run> [--to <account>]` stops the run and resumes the same session, in the same folder and worktree, under another account, then tells the agent to carry on. With `"limits": { "onLimit": "failover" }` this happens as soon as the limit appears.
  - Also in `amux ui` (`/usage`, `/failover`) and the phone app (accounts under Monitor, and "Move to another account").

## [0.7.0] - 2026-11-17

Catch conflicts between parallel agents while they work, and land their branches one at a time.

### Added

- **Catch conflicts between parallel runs before they merge.** `amux overlap` shows which runs touch the same files, and whether their changes would actually conflict with each other or with the branch they land in. Uncommitted work counts too. The merges happen in memory (`git merge-tree`), so nothing on disk changes. `--check` exits 1 on a conflict, and `--json` prints it all for scripts. `amux ui` marks runs that conflict (`✗ conflicts with …`) in the list and on the board, and the phone app lists overlaps under Monitor.
- **`amux land <run>...`, a local merge queue.** One run at a time, it rebases onto the target branch (the main checkout's, or `--into`), runs the tests in a test slot, and fast-forwards the branch, so each run lands on top of the ones before it. A run with uncommitted work, a conflict or failing tests is skipped, and its agent is told exactly what to fix (`--no-tell` to stay quiet). It never force-moves the branch or overwrites uncommitted work in the main checkout. `--dry-run` says what would land. In `amux ui` use `/land`; in the phone app, "Land into its branch".

## [0.6.1] - 2026-11-17

The first published release with these changes. The v0.6.0 tag was never released: its release check found the screen bug fixed below.

Many agents on one machine without overloading it, steered from a phone, another laptop or another agent, with their work checked, reviewed and shipped from amux.

### Added

- **Sharing the machine between agents.** `amux slot -- <command>` runs a test suite, build or dev server only when a slot of its class is free (by default a quarter of the CPUs for `test` and `build`, and 2 for `dev`), and holds new work back while the load average or free memory is past a limit. Waiters are served first come, first served, and a slot frees itself when its process ends, however it ends. With `--timeout` or `--no-wait` it exits 75 when no slot came free, so an agent can push the branch for CI instead of piling onto the machine.
- `amux slots` shows each class, who holds its slots and who waits, and the machine's load and free memory. `amux slots --hook` prints a Claude Code `PreToolUse` hook (`amux __slot-hook`) that turns away unwrapped `npm test`, `cargo build`, `npm run dev` and the like, with the wrapped command to run instead, plus instructions for `AGENTS.md`.
- `amux verify <run>` checks a run's work: `--test` runs the project's tests in its folder, waiting for a test slot, and `--push` pushes its branch so CI runs them. With neither, it says which is better right now. In `amux ui`, `v` (or `/verify`) offers both and picks the push with ⏎ when the machine is busy. The run list shows how the last check went.
- `amux send <run> <text>` types a prompt into a run and presses Enter, without attaching. `--wait` also waits for the turn to end. Multi-line text goes in as a bracketed paste, and control characters are removed.
- `amux wait <run>... --until waiting|idle|exited [--timeout <secs>]` blocks until runs need you, finish their turn, or exit, and exits 124 on timeout. With `spawn`, `send` and `peek`, one agent can run and steer helpers.
- `amux diff <run> [--stat]` shows everything a run changed since its worktree started, committed or not, and lists untracked files. Worktree runs now record the commit they started from. In `amux ui`, `/diff` opens it in your pager.
- `amux ps --json` prints every run's state, branch, worktree and last verify, for scripts.
- `amux verify <run> --send-failures` hands a failed test run's output to the agent to fix. In `amux ui`'s verify choice, ⏎ (or `f`) does it after a failure. `/send <text>` types into the selected run.
- **Typing and mouse in `amux ui`.** Enter, a click on the screen, or just typing a word types into the selected run in its pane, as in a terminal, with paste. Ctrl-] goes back to the list, Ctrl-B then a key runs a command, and Alt-↑/↓ switches runs. Clicks select runs, and the wheel moves the list or scrolls the screen. CLIs that ask for mouse reports get them. `a` still gives a run the whole terminal.
- `amux pr <run> [--draft] [--base <branch>]` pushes the run's branch and opens its pull request through the GitHub CLI, or finds the open one. `amux checks <run>` shows its CI checks, and `--send-failures` hands the failed steps' logs to the agent. `amux ui` shows the PR and checks on each run and refreshes them while checks run, and `ps --json` includes them.
- **Review comments.** In `amux ui`, `c` opens a run's diff (`/review`). ⏎ comments on the line under the cursor, `x` deletes a comment, and `S` sends all pending comments to the agent as one message, with file:line and the code for each. From the CLI: `amux comment <run> <file>:<line> <note>`, and `amux comments <run> [--send | --rm <n> | --clear]`. `/diff` still opens the plain diff in your pager.
- **Other machines over SSH.** `amux machine add <name> <user@host>`, `amux spawn --on <machine>[:<dir>]`, and `<machine>:<run>` wherever a run goes. `ps` lists every machine's runs. `amux ui` shows them with live screens and typing, over one reused SSH connection per machine.
- **`amux serve`**: a full remote control in a web app for a phone or another laptop (add it to the home screen).
  - Runs by space (repo) or on a board, with live updates (Server-Sent Events).
  - Each run's screen with keys and replies, its diff with line comments, and tests, push, PR, CI and stop or remove.
  - Starting runs with account, machine, folder, worktree and prompt.
  - A monitor of load, memory, slots and machines.
  - Security: it needs a token, sets an HttpOnly SameSite cookie, requires a custom header on actions, sends a strict CSP, and listens on localhost by default.
- `amux spawn --dir <folder> --prompt <text>`; `--json` for `account list`, `slots` and `comments`.
- **Plugins and hooks.** `amux <name>` runs `~/.amux/plugins/<name>` or `amux-<name>` on PATH. `/plugin` runs one in `amux ui`, and `amux plugin new` starts one. `~/.amux/hooks/on-waiting|on-exited|on-verify|on-checks` run on those events. Only files you own and no one else can write to are run.
- **Layouts.** `/layout` sets the list width and side, and up to six tiles. `/layout save` keeps them in `ui.layout`.
- **`amux skill [--install]`**: an agent skill that teaches Claude Code, Codex and Gemini how to run, steer and check other agents with amux.
- `amux send <run> --key <key>` presses enter, esc, the arrows, ctrl-c, or a letter or digit.
- **Board and tiles in `amux ui`.** `b` shows runs as cards in a column per state (needs you, working, idle, done). `t` tiles up to four live runs side by side; click one to type into it. `/view list|board|tiles` switches.
- `amux events [<run>...] [--text]` streams changes as they happen: runs added or removed, state changes with the reason a run waits, verify results and PR checks. JSON lines by default.
- Internals: the code is now in layers. `runs/` covers a run's life (including its state and the client that types into it), and `work/` covers slots, verify, GitHub and review. Both sit below `commands/` (the CLI) and `ui/`. There is one git helper instead of four, and `ui/mod.rs` is split into keys, typing, actions and views. See docs/architecture.md.
- Runs can be limited too: with `"limits": { "runs": 4 }`, an extra `amux spawn` returns right away as `queued` and starts by itself when a run ends. `amux ps`, `amux ui` and `amux explain` show it as queued; `amux stop` cancels it. All limits live under `"limits"` in `~/.amux/config.json`.

### Fixed

- A client that connected while the CLI's output was cut mid escape sequence, for example between `ESC[1` and `A`, could draw the screen a line off. This affected `amux attach`, `peek`, `explain` and the UI's preview. The host now sends the cut-off start along with the screen snapshot.
- A run's own report (OSC 7501) is saved before the "waiting" state it causes, so `ps` never shows waiting without its reason.

- The run host saves a run's final screen before recording that it ended, so a preview or `amux peek` never catches it in between and replays the log instead.
- A new client's snapshot of a run now includes the window title the CLI set, so the task title on `amux ui`'s preview border no longer goes missing.
- `amux peek` and `amux explain` wait up to 2 seconds for a busy host's first output, instead of 150 ms, before giving up with an empty screen.
- `amux peek`, `amux explain` and the preview now agree on the size of a run's screen, and an ended run's still picture is no longer cropped to the host's last size.
- In `amux ui`, `0` is no longer treated as a shortcut, and a very narrow new-run dialog no longer draws past its border.

## [0.5.1] - 2026-11-16

Fixes `amux ui` showing garbled screens, and letters typed into its list triggering shortcuts.

### Added

- `amux peek <run>` prints what is on a run's screen now, or its last screen once it has ended.

### Fixed

- `amux ui` showed overlapping, unrelated text for runs of Claude Code and other full-screen CLIs, because it replayed only the last 256 KB of output, which starts mid-redraw. The run host now keeps the CLI's screen in a terminal emulator and gives every new client (the UI's preview, `amux attach`, `amux explain`) an exact snapshot. It saves the final screen when the CLI exits. Older runs are redrawn from their whole log.
- A preview in `amux ui` no longer makes the CLI redraw when you select its run.
- In `amux ui`, letters typed into the list by mistake no longer open panels or dialogs one after another. A letter that isn't a shortcut shows how to type into the run (⏎), and letters right after it are ignored. Stop and remove confirmations ignore keys pressed in the first half second, so typed-ahead text can't answer them.
- The new-run dialog no longer lets a long folder path spill past its border.

## [0.5.0] - 2026-11-16

`amux ui`: one screen to watch, attach to, start and stop all your runs.

### Added

- `amux ui`: one screen for all your runs. A list with live state (working, waiting, idle, done), a live preview of the selected run's screen, Enter to attach in place (Ctrl-] comes back to the list), Tab to jump to the next run that needs you, and keys to start (`n`), stop (`s`) and remove (`d`) runs. Quitting the UI leaves every run going.
- A command palette in `amux ui` (`/`): `/new`, `/attach`, `/stop`, `/kill`, `/rm`, `/clean`, `/logs`, `/filter`, `/switch-account`, `/add-account`, `/accounts`, `/theme`, `/doctor`, `/help` and `/quit`. Arguments complete from your runs, accounts and themes, and each command shows its shortcut key. `/add-account` signs you in without leaving the UI.
- Themes for `amux ui`: `default` (your terminal's colors), `dark`, `light` and `mono`. Pick one with `/theme` or in `config.json` (`ui.theme`), change single colors with `ui.colors`, or add your own in `~/.amux/themes/<name>.json`. `NO_COLOR` picks `mono`, and a light terminal background picks `light`.
- Runs read the state a CLI reports about itself with OSC 7501 (the Program Status Protocol). Codex sends it, and Claude Code 2.1.295 and later sends it once asked, which the run host now does. `amux ps` and `amux ui` then show the CLI's own state: `waiting` with its message (such as "approve Write: a.rs"), `running`, `idle` when the turn is done, or `error`. This replaces guessing from prompt text and output timing for those CLIs. The state is saved in `run.json` as `status`.
- `amux explain <run> [--json]`: why a run shows its state. It names the signal that decided: the CLI's own OSC 7501 report, a prompt from `waitingPatterns`, a bell, a notification, or output timing. It also says what amux watches for in that run and shows the bottom of its screen. In `amux ui`, `e` shows the same. Hosts now record what showed a wait, as `waiting.source` in `run.json`.
- State filters in `amux ui`: `f` steps through waiting, working, idle and ended runs, `/show <state>` picks one, and Esc shows all again.

### Changed

- The run host takes a new `WATCH` message from clients that only show a run's screen, such as the preview in `amux ui`. They don't count as attached, so the desktop notification still goes out. Older hosts ignore the message.

### Fixed

- `amux attach`: Ctrl-] detaches even after the CLI has turned on the kitty keyboard protocol, and detaching turns that protocol and modifyOtherKeys off again.

## [0.4.0] - 2026-11-16

Background runs now tell you when they need you.

### Added

- `amux ps` shows `waiting` when a run's CLI is asking you something, such as a permission prompt. Each provider's prompt text is set with `waitingPatterns`. Bells and terminal notification sequences count as well.
- Desktop notifications when a run starts waiting or exits while no terminal is attached. Turn them off with `"notify": false` in `config.json`, or run your own command with `"notifyCommand"`.

### Fixed

- A Claude Code launched by amux from inside another Claude Code session no longer inherits that session's markers (`CLAUDECODE`, `CLAUDE_CODE_SESSION_ID`, the messaging socket and token, and others). It runs as its own session, with its transcript saved.

## [0.3.0] - 2026-11-16

Run several agents at once, across accounts and CLIs, each in its own terminal and optionally its own git worktree. macOS and Linux.

### Added

- Parallel runs. `amux spawn` starts a CLI in the background in its own terminal, under any account, and optionally in its own git worktree and branch (`--worktree`, `--branch`, `--base`).
- `amux ps` lists runs and shows whether each is working or idle. `amux attach` connects your terminal to a run, and Ctrl-] detaches.
- `amux logs`, `amux stop` and `amux rm` show, end and remove runs. `amux rm` deletes a run's worktree only when it has no uncommitted or untracked files.
- The CLI sees `AMUX_RUN` in its environment when it was started by `amux spawn`.

### Fixed

- The install script works with macOS `/bin/sh` again.

## [0.2.1] - 2026-11-16

0.2.0 was never published to npm, and its tag missed the fixes below. Use 0.2.1.

### Security

- Account names and provider ids in `accounts.json` and `config.json` are checked before amux uses them as folder names, so a hand-edited name like `../..` can't point `amux account rm --purge` outside `~/.amux`.
- `amux resume` refuses session ids that could be read as a CLI option, such as `--some-flag`.
- `amux sync --adopt` never replaces a file that already exists in the shared home, including a broken symlink.
- The cached update version is validated before it is printed.

### Fixed

- The background update check no longer lingers as a zombie process under the CLI amux launches, and Ctrl-C doesn't reach it.
- A rejected or cancelled token no longer leaves a token account behind.
- `amux account status` no longer hangs when the CLI leaves a helper process running.
- Entries that become private after a provider update are unlinked from the shared home, so the account gets its own copy.
- The npm launcher passes SIGTERM and SIGHUP on to amux.
- Release: the npm package is published again.

### Changed

- Provider specs can set `tokenHint` and `conflictingEnv` in `config.json`. See docs/providers.md.
- `amux resume <id>` opens only the Claude transcripts whose file name matches the id.
- amux reads `config.json` once per run.

## [0.2.0] - 2026-11-16

amux is now a single native binary written in Rust. Commands, flags, output and the files in `~/.amux` are unchanged, so existing accounts keep working.

### Changed

- Rewritten in Rust. amux no longer needs Node.js, and starts in about 3 ms instead of about 30 ms. `amux sessions` is about 2.5 times faster.
- Releases ship static binaries for macOS (Apple silicon and Intel) and Linux (arm64 and x86_64), each with a SHA-256 checksum.
- The Homebrew formula installs the binary and no longer depends on Node.js.
- The npm package `@quynb/amux` contains the prebuilt binaries and a small launcher. Installing it runs no scripts.
- On macOS and Linux, amux replaces itself with the CLI it launches, so the CLI receives Ctrl-C and other signals directly.
- `amux update` prints the upgrade command for how amux was installed: Homebrew, npm, or the install script.
- Fields amux doesn't recognize in `accounts.json` and `config.json` are kept when it rewrites them.

### Added

- An install script for macOS and Linux: `curl -fsSL https://github.com/quynb93/amux/releases/latest/download/install.sh | sh`. It verifies the checksum before installing.

### Security

- On macOS, `amux account token` passes the token to the Keychain on stdin, so it no longer appears in the process list.
- Login accounts no longer inherit the provider's token variable (such as `OPENAI_API_KEY`) from your shell, and `amux env` prints `unset` for it.
- Handoff file names are sanitized, so a crafted session id can't write outside `~/.amux/handoffs`.
- The Linux token file is replaced atomically with mode 0600, and folders amux creates in the shared home are 0700.
- Pressing Ctrl-C at the token prompt turns terminal echo back on.

### Fixed

- `amux sessions` and `amux resume` scan less of `~/.claude/projects` and allocate less memory.
- One session file with a non-text id no longer breaks `amux resume`.
- Several amux runs starting at once no longer each start an update check, and a failed check no longer hides a known update.

## [0.1.2] - 2026-11-16

### Added

- Homebrew install for macOS and Linux: `brew install quynb93/tap/amux`. Each release updates the formula.
- npm install: `npm install -g @quynb/amux`.
- `amux update` prints `brew upgrade amux` when amux was installed with Homebrew.

### Changed

- The npm package is now named `@quynb/amux`, because `amux` is taken on npm. The command is still `amux`. To upgrade from 0.1.0 or 0.1.1, run `npm uninstall -g amux` first: otherwise npm refuses to install, because the old `amux` package still owns the `amux` command.

## [0.1.1] - 2026-11-16

### Fixed

- Installing with `npm install -g github:quynb93/amux` failed, because npm could not find `tsc` while building. Each release now includes a prebuilt `amux.tgz`, and the install command is `npm install -g https://github.com/quynb93/amux/releases/latest/download/amux.tgz`. `amux update` prints the matching command.

## [0.1.0] - 2026-11-16

First public release.

### Added

- Multiple accounts per CLI over one shared home, for Claude Code, Codex and Gemini CLI. Other CLIs can be added in `~/.amux/config.json`.
- Sign-in through each CLI's own login, or a stored token (macOS Keychain, or a 0600 file on Linux).
- `amux sessions` lists sessions from every CLI. `amux resume` resumes a session under any account, or hands it to another CLI with a transcript.
- Privacy mode (`amux account privacy`), which applies Claude Code's documented telemetry opt-outs to one account.
- A per-account API endpoint (`amux account baseurl`, `--base-url`) for gateways and relays.
- A background update check against GitHub releases, plus `amux update` and `amux --version`.
- `amux env`, `amux sync [--adopt]`, `amux providers` and `amux doctor`.
