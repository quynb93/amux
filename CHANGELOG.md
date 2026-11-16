# Changelog

All notable changes to amux are listed here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and amux uses [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added

- `amux ui`: one screen for all your runs. A list with live state (working, waiting, idle, done), a live preview of the selected run's screen, Enter to attach in place (Ctrl-] comes back to the list), Tab to jump to the next run that needs you, and keys to start (`n`), stop (`s`) and remove (`d`) runs. Quitting the UI leaves every run going.

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
