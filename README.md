# amux

**Many accounts, one shared home, for Claude Code, Codex, Gemini CLI and other agent CLIs.**

[![CI](https://github.com/quynb93/amux/actions/workflows/ci.yml/badge.svg)](https://github.com/quynb93/amux/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
![Written in Rust](https://img.shields.io/badge/written%20in-Rust-orange)

Agent CLIs keep everything in one home directory: your login, but also your skills, plugins, MCP servers, settings and session history. Using a second account usually means a second home directory, and then nothing is shared between them.

amux gives each account its own login and shares everything else.

```console
$ amux account add claude work
$ amux account add claude personal
$ amux claude@work                  # Claude Code, signed in as work
$ amux claude@personal              # same skills, plugins and sessions, different login
$ amux resume 2f26fba2 --as claude@personal   # pick up a work session under the other account
```

## Contents

- [Features](#features)
- [Install](#install)
- [Quick start](#quick-start)
- [How it works](#how-it-works)
- [Usage](#usage)
  - [Accounts and sign-in](#accounts-and-sign-in)
  - [Sessions and handoff](#sessions-and-handoff)
  - [Privacy mode](#privacy-mode)
  - [Gateways and custom endpoints](#gateways-and-custom-endpoints)
  - [Shell integration](#shell-integration)
  - [Other CLIs](#other-clis)
  - [Updates](#updates)
- [Command reference](#command-reference)
- [Files and environment](#files-and-environment)
- [Moving from separate config dirs](#moving-from-separate-config-dirs)
- [Troubleshooting](#troubleshooting)
- [Responsible use](#responsible-use)
- [Roadmap](#roadmap) · [Contributing](#contributing) · [License](#license)

## Features

- **Multiple accounts per CLI.** Claude Code, Codex and Gemini CLI work out of the box. Any CLI that moves its home directory with an environment variable can be added with a few lines of JSON.
- **One shared home.** Skills, plugins, agents, commands, hooks, MCP config, settings and session history are shared. Only the login and per-account state stay separate.
- **Run accounts side by side.** Each launch gets its own overlay directory, so two accounts can run at the same time in different terminals.
- **Sign in your way.** Use the CLI's own login flow or a stored token (macOS Keychain, or a `0600` file on Linux).
- **Sessions across accounts and CLIs.** List every session from every CLI in one place. Resume one under another account, or hand it to a different CLI with a transcript handoff.
- **Per-account options.** Privacy mode applies the CLI's documented telemetry opt-outs. A base URL sends that account's requests to a gateway you control.
- **One small binary.** Written in Rust. It starts in a few milliseconds and needs no runtime such as Node.js.

## Install

amux runs on macOS and Linux. On Windows, use it inside [WSL](https://learn.microsoft.com/windows/wsl/); native Windows support is [planned](https://github.com/quynb93/amux/issues/1).

**Homebrew** (macOS and Linux):

```sh
brew install quynb93/tap/amux
```

**Install script** (macOS and Linux). It downloads the binary for your machine from the latest GitHub release, checks its SHA-256 checksum, and puts it in `~/.local/bin`:

```sh
curl -fsSL https://github.com/quynb93/amux/releases/latest/download/install.sh | sh
```

Set `AMUX_INSTALL_DIR` to install somewhere else, or `AMUX_VERSION=0.2.0` for a specific version.

**npm.** The package contains the prebuilt binaries, so installing it runs no scripts:

```sh
npm install -g @quynb/amux
```

**Manual download.** Each [release](https://github.com/quynb93/amux/releases/latest) has an `amux-<target>.tar.gz` for macOS (Apple silicon and Intel) and Linux (arm64 and x86_64, statically linked), with a `.sha256` file for each.

**From source** (Rust 1.88 or newer):

```sh
cargo install --git https://github.com/quynb93/amux --locked
```

To upgrade, run `amux update`. It prints the right command for how you installed amux: `brew upgrade amux`, the npm command, or the install script.

**Upgrading from 0.1.x.** Versions 0.1.x ran on Node.js. The binary releases replace them, and your accounts and settings in `~/.amux` carry over unchanged. If you installed 0.1.0 or 0.1.1 with npm, run `npm uninstall -g amux` first, because those were packaged as `amux` rather than `@quynb/amux`.

Check it worked:

```sh
amux --version
amux doctor          # finds the claude/codex/gemini binaries and their homes
```

## Quick start

```sh
# 1. Add accounts. `add` creates the account's overlay and starts the CLI's sign-in.
amux account add claude work
amux account add claude personal
amux account add codex work

# 2. Choose the account that plain `amux claude` uses.
amux account default claude@work

# 3. Run. Extra arguments go straight to the CLI.
amux claude                        # claude@work
amux claude@personal --model opus  # claude@personal with a flag for Claude Code

# 4. Check who is signed in where.
amux account status
```

## How it works

Each account gets a small overlay directory under `~/.amux/accounts/`. Inside it, every entry is a symlink back into the real shared home, except the files that hold that account's login and state:

```
~/.claude                                  shared home: skills, plugins, projects/, settings.json, …
~/.amux/accounts/claude/work/.claude       overlay for claude@work
    .claude.json                           private: the signed-in account and per-account state
    skills   -> ~/.claude/skills           everything else links to the shared home
    projects -> ~/.claude/projects
    settings.json -> ~/.claude/settings.json
```

`amux claude@work` sets the CLI's home variable to that overlay and starts the CLI. The CLI sees a normal home directory. Anything shared that it writes, such as sessions, goes through the links into the shared home. Its login stays in the overlay, or on macOS in a Keychain entry that Claude Code names after the config directory.

| CLI | Home variable amux sets | Kept private per account |
|---|---|---|
| `claude` | `CLAUDE_CONFIG_DIR` | `.claude.json`, `.credentials.json`, Keychain login, `policy-limits.json*`, `remote-settings.json*`, telemetry and stats state, `backups` |
| `codex` | `CODEX_HOME` | `auth.json` |
| `gemini` | `GEMINI_CLI_HOME` (the parent of `.gemini`) | `oauth_creds.json`, `google_accounts.json` |

Overlays are refreshed on every launch. New entries in the shared home get linked, and links whose target is gone get removed. amux never deletes a real file. See [docs/architecture.md](docs/architecture.md) for the details.

## Usage

### Accounts and sign-in

```sh
amux account add <cli> <name>                 # create, then run the CLI's login (e.g. `claude auth login`)
amux account add <cli> <name> --no-login      # create only; sign in later with `amux <cli>@<name>`
amux account add <cli> <name> --token         # store a token instead of a login (read from stdin)
amux account add <cli> <name> --note "client X"
amux account token claude@ci < token.txt      # store or replace a token later
amux account list
amux account default claude@work
amux account rm claude@old                    # keeps the overlay's private files
amux account rm claude@old --purge            # deletes the overlay too (the shared home is untouched)
```

Token sign-in sets the CLI's token variable at launch. For Claude Code that is `CLAUDE_CODE_OAUTH_TOKEN`, created with `claude setup-token`. For Codex it is `OPENAI_API_KEY`, and for Gemini CLI it is `GEMINI_API_KEY`. Tokens are stored in the macOS Keychain under the service `amux`. On Linux they go in a `0600` file inside the account's directory.

Account names may use letters, digits, `.`, `_` and `-`. If a CLI has a single account, or a default, then `amux claude` is enough. Otherwise use `amux claude@<name>`.

### Sessions and handoff

Sessions are written to the shared home, so every account sees all of them.

```sh
amux sessions                      # every CLI, newest first
amux sessions codex --here -n 50   # only Codex, only this directory
```

```
PROVIDER  SESSION                               UPDATED  DIR             FIRST PROMPT
claude    2f26fba2-6c1e-4f0b-9a57-3f0e1d1c8b11  5m ago   ~/code/app      Fix the login redirect loop
codex     01a01da9-77f2-4c0e-bb1f-2b8c0d6e4a90  2h ago   ~/code/api      Add pagination to /orders
```

Resume by ID or by any unique prefix of it:

```sh
amux resume 2f26fba2 --as claude@personal   # same CLI: native resume under another account
amux resume 01a01da9 --as claude@work       # Codex → Claude: transcript handoff
```

For the same CLI, `resume` uses the CLI's own resume command, which is useful when an account hits its rate limit. Across CLIs, amux writes the original request and the most recent turns to a Markdown file in `~/.amux/handoffs/`. It then starts the target CLI in the session's directory and asks it to read that file and continue.

### Privacy mode

```sh
amux account privacy claude@personal on
amux account add claude personal --privacy
```

Privacy mode starts Claude Code with its documented opt-out settings: `DISABLE_TELEMETRY=1`, `DISABLE_ERROR_REPORTING=1` and `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1`. The last one also turns off auto-update and `/bug`, so update Claude Code yourself on those accounts. The settings apply only to that account. **Settings managed by an organization still take precedence**, and amux does not try to change them. Codex and Gemini CLI have no privacy settings in amux yet.

### Gateways and custom endpoints

Send an account's API requests to an endpoint you or your organization run, such as an AI gateway, a LiteLLM proxy, or a Bedrock or Vertex relay:

```sh
amux account baseurl claude@work https://gateway.example.com
amux account add claude work --base-url https://gateway.example.com
amux account baseurl claude@work off
```

| CLI | Variable set |
|---|---|
| `claude` | `ANTHROPIC_BASE_URL` |
| `codex` | `OPENAI_BASE_URL` |
| others | the provider's `baseUrlEnv`, see [docs/providers.md](docs/providers.md) |

This changes only where requests are sent. Organization policy and telemetry still apply.

### Shell integration

```sh
alias wclaude='amux claude@work'
alias pclaude='amux claude@personal'

eval "$(amux env claude@work)"     # export the account's variables into this shell
claude                             # now runs as claude@work without amux
```

`amux env` prints the home variable, `AMUX_ACCOUNT`, and any privacy, token and base URL variables for that account.

### Other CLIs

Any CLI that moves its home directory with an environment variable can be added in `~/.amux/config.json`. The same file can also override built-in providers, for example to use a different shared home:

```json
{
  "providers": [
    { "id": "claude", "sharedHome": "~/.claude-main" },
    {
      "id": "mycli",
      "label": "My CLI",
      "bin": "mycli",
      "sharedHome": "~/.mycli",
      "homeEnv": "MYCLI_HOME",
      "privateEntries": ["credentials.json"],
      "sharedDirs": ["sessions"],
      "loginArgs": ["login"],
      "tokenEnv": "MYCLI_API_KEY"
    }
  ]
}
```

Run `amux providers` to see the result. Every field is described in [docs/providers.md](docs/providers.md).

### Updates

Like Claude Code and Codex, amux tells you when a new release is out:

```
Update available: amux 0.1.0 → 0.2.0  Run `amux update` or see https://github.com/quynb93/amux/releases/latest
```

Commands never wait on the network. The notice comes from a cached result, which a background process refreshes from GitHub releases at most once a day. `amux update` checks right away and prints the install command. Set `AMUX_NO_UPDATE_CHECK=1` to turn the check off. It is also skipped in CI and when output is not a terminal.

## Command reference

| Command | What it does |
|---|---|
| `amux <cli>[@name] [args…]` | Run the CLI as that account. Same as `amux run`. |
| `amux account add <cli> <name> [--token] [--privacy] [--base-url <url>] [--no-login] [--note <text>]` | Create an account and sign in. |
| `amux account list` | List accounts, auth type, options and overlay paths. |
| `amux account default <cli>@<name>` | Set the account that `amux <cli>` uses. |
| `amux account token <cli>@<name>` | Store or replace a token, read from stdin. |
| `amux account privacy <cli>@<name> on\|off` | Turn the CLI's telemetry opt-outs on or off. |
| `amux account baseurl <cli>@<name> <url\|off>` | Set or clear the account's API endpoint. |
| `amux account status [<cli>@<name>]` | Ask each CLI who is signed in. |
| `amux account rm <cli>@<name> [--purge]` | Remove an account. `--purge` also deletes its overlay. |
| `amux sessions [<cli>] [--here] [-n <count>]` | List sessions from every CLI, newest first. |
| `amux resume <id> [--as <cli>@<name>]` | Resume a session under any account, or hand it to another CLI. |
| `amux providers` | List providers, shared homes and private entries. |
| `amux env <cli>@<name>` | Print `export` lines for the account. |
| `amux sync [<cli>@<name>] [--adopt]` | Refresh overlay links. `--adopt` moves stray files into the shared home. |
| `amux doctor` | Check binaries, shared homes and overlays. |
| `amux update` | Check for a newer amux release. |
| `amux --version`, `amux help` | |

## Files and environment

```
~/.amux/
  config.json          providers and per-CLI default accounts
  accounts.json        the account list (no secrets)
  accounts/<cli>/<name>/   each account's overlay (and token file on Linux)
  handoffs/            transcripts written by cross-CLI `resume`
  launches.jsonl       local log of launches: time, account, directory, arguments (mode 0600)
  update-check.json    cached result of the update check
```

| Variable | Effect |
|---|---|
| `AMUX_HOME` | Use a different directory instead of `~/.amux`. Handy for testing. |
| `AMUX_NO_UPDATE_CHECK=1` | Turn off the update check. |
| `AMUX_ACCOUNT` | Set by amux in the launched CLI's environment, e.g. `claude@work`. Read it from hooks or scripts. |

amux makes no network requests of its own, except the daily update check to `api.github.com`.

## Moving from separate config dirs

If you already keep one directory per account, for example `~/.claude-work` and `~/.claude-personal`:

1. Choose one shared home. Use the default `~/.claude`, or set `sharedHome` in `~/.amux/config.json`.
2. Copy the session history from the other directories into it. Session files have unique names, so they merge cleanly:
   ```sh
   rsync -a ~/.claude-work/projects/ ~/.claude/projects/
   ```
3. Create an amux account for each login and sign in: `amux account add claude work`.
4. Once everything works, remove the old directories and any shell aliases that set `CLAUDE_CONFIG_DIR`.

## Troubleshooting

**`amux doctor` says a binary is not found.** Install that CLI, or set `"bin"` for it in `~/.amux/config.json` to an absolute path.

**"has unshared entries" on launch.** The CLI wrote a real file into the overlay before the shared home had that entry. Run `amux sync <cli>@<name> --adopt` to move it into the shared home. If the same name exists in both places, amux leaves it alone and you merge by hand.

**The CLI asks me to sign in again.** Each account has its own login. Sign in once with `amux <cli>@<name>` and the login is kept.

**I set `CLAUDE_CONFIG_DIR` / `CODEX_HOME` in my shell.** amux overrides these for each launch, so they don't leak from one account into another.

**Windows.** amux relies on symlinks and POSIX permissions and has not been tested on Windows. WSL works, and native support is tracked in [#1](https://github.com/quynb93/amux/issues/1).

## Responsible use

amux changes where a CLI keeps its files. It doesn't change what the CLI does.

- amux never changes, hides from or works around settings or policies managed by an organization. When an organization manages an account, its policy applies under amux exactly as it does without it.
- Privacy mode uses only the opt-outs that each CLI documents.
- Follow each provider's terms of service for the accounts you use.

## Roadmap

1. **Parallel runs and status.** `amux spawn` starts several agents across accounts and CLIs, each in its own git worktree. `amux ps` shows which are working, blocked or idle, and `amux attach` connects you to one.
2. **Messaging between sessions.** An `amux daemon` with a session registry and inboxes, plus an MCP server so sessions can `list_peers`, `send_message` and `delegate_task`.
3. **Rate-limit failover.** Detect a limit error and resume the session under the next account.
4. **Usage view.** Usage and limits for each account.
5. **Native Windows.** Tracked in [#1](https://github.com/quynb93/amux/issues/1).
6. **`amux import <dir>`.** Merge existing per-account directories into the shared home in one command.

Ideas and feedback are welcome in [issues](https://github.com/quynb93/amux/issues).

## Contributing

Bug reports, provider definitions and pull requests are welcome. See [CONTRIBUTING.md](CONTRIBUTING.md) to set up a dev environment, run the tests safely, and add a provider. To report a security issue, see [SECURITY.md](SECURITY.md).

## License

[MIT](LICENSE)
