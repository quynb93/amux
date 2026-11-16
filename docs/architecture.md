# Architecture

amux is a launcher. It never talks to a model API and never edits a CLI's files. Its job is to prepare a directory and an environment so that a normal, unmodified CLI runs as a particular account.

## The overlay model

```
                    ┌──────────────────────── shared home (~/.claude) ───────────────────────┐
                    │ skills/  plugins/  projects/  settings.json  …   .claude.json (unused)  │
                    └───────▲───────────────────────▲──────────────────────────────────────┘
                            │ symlinks              │ symlinks
     ┌──────── overlay: claude@work ─────┐   ┌──────── overlay: claude@personal ─┐
     │ skills -> …   projects -> …        │   │ skills -> …   projects -> …        │
     │ .claude.json  (work's login)       │   │ .claude.json  (personal's login)   │
     └────────────────▲───────────────────┘   └────────────────▲───────────────────┘
                      │ CLAUDE_CONFIG_DIR                       │ CLAUDE_CONFIG_DIR
               amux claude@work                          amux claude@personal
```

Each account has an overlay at `~/.amux/accounts/<provider>/<name>/<basename of shared home>`. For every entry in the shared home, the overlay has a symlink to it, unless the provider lists that name in `privateEntries`. Private entries are created by the CLI itself inside the overlay, the first time the account signs in.

Because the overlay is a real directory, the CLI needs no changes. Writes to shared entries go through the symlinks into the shared home, and writes to private entries stay in the overlay. Two accounts can run at the same time, since each process has its own `CLAUDE_CONFIG_DIR` (or `CODEX_HOME`, …).

On macOS, Claude Code stores the OAuth login in the Keychain under a service name derived from the config directory path, so each overlay gets its own Keychain entry automatically.

## Launch sequence

`amux claude@work --model opus` runs these steps:

1. **Resolve the account** (`store.rs`): `claude@work` is looked up in `accounts.json`. Plain `claude` falls back to the configured default, or the only account.
2. **Sync the overlay** (`overlay.rs`): link new shared entries, repoint wrong links, prune dangling ones, and unlink entries that have since become private. Real files are never deleted. Real files that should be shared are reported as *stray*, and `amux sync --adopt` can move them.
3. **Build the environment** (`launch.rs`): copy the current environment and remove variables that would point at another account (`CLAUDE_CONFIG_DIR`, `CLAUDE_CODE_OAUTH_TOKEN`, …). Then set the home variable, `AMUX_ACCOUNT`, and the account's options: privacy variables, a token from `secrets.rs`, and the base URL.
4. **Log and run**: append a line to `launches.jsonl` (mode 0600), then `exec` the CLI. amux replaces itself with the CLI process, so the CLI owns the terminal, receives Ctrl-C directly, and its exit code is the exit code.

## Overlay sync rules

| Overlay entry | Shared entry | Private name? | Action |
|---|---|---|---|
| missing | exists | no | create a symlink |
| symlink to the wrong place | exists | no | repoint it |
| symlink to the shared entry | exists | **yes** | unlink it (the entry became private) |
| dangling symlink | gone | — | unlink it |
| real file or directory | exists or not | no | report as stray |
| real file or directory | — | yes | leave it: this is the account's own data |

`sync_overlay` is idempotent and runs on every launch, so it must stay fast and must never delete a real file.

## Sessions and handoff

`sessions.rs` reads each CLI's session files from the shared home:

| Format | Location | Parsed from |
|---|---|---|
| `claude` | `projects/<cwd-slug>/<id>.jsonl` | `user` and `assistant` records, skipping meta, sidechain and subagent records |
| `codex` | `sessions/YYYY/MM/DD/rollout-*.jsonl` | `session_meta` and `response_item` messages, with duplicate prompts collapsed |
| `gemini` | `tmp/<project>/chats/session-*.json` | `messages`, with the cwd recovered from `projects.json` |

To list sessions, amux reads only the first 256 KB of each file, so listing stays fast on long histories. Injected context, such as `AGENTS.md`, environment blocks and slash-command wrappers, is skipped when picking a title.

`build_handoff` writes the first user request, then as many of the most recent turns as fit in a character budget (60,000 by default). If the newest turn is too large, its tail is kept rather than dropping the turn.

## Update check

`update.rs` follows the pattern Claude Code and Codex use. On each run it reads `~/.amux/update-check.json` and prints a notice if a newer version is cached. If the cache is more than a day old, it starts a detached `amux __update-check` process that asks the GitHub releases API for the latest tag and rewrites the cache. The command the user typed never waits on the network.

## State on disk

| Path | Written by | Contents |
|---|---|---|
| `config.json` | `amux account default`, the user | Provider overrides and default accounts |
| `accounts.json` | `amux account …` | Account list: provider, name, auth type, options. No secrets. |
| `accounts/<p>/<n>/` | `sync_overlay`, the CLI | The overlay, plus a `token` file on Linux |
| `handoffs/*.md` | `amux resume` | Transcripts for cross-CLI handoff (mode 0600) |
| `launches.jsonl` | every launch | Time, account, directory and arguments (mode 0600) |
| `update-check.json` | update check | Last check time and latest version |

JSON files are written atomically: amux writes a temp file, then renames it over the old one.
