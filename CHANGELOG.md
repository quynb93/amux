# Changelog

All notable changes to amux are listed here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and amux uses [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added

- Homebrew install for macOS and Linux: `brew install quynb93/tap/amux`. Each release updates the formula.
- npm install: `npm install -g @quynb/amux`. The npm package is now named `@quynb/amux`, because `amux` is taken on npm. The command is still `amux`.
- `amux update` prints `brew upgrade amux` when amux was installed with Homebrew.

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
