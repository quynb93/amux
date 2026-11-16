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

To list sessions, amux reads the first 32 KB of each JSONL transcript (256 KB if the first prompt isn't there yet) and stops parsing once it has the id, directory and first prompt, so listing stays fast on long histories. Gemini sessions are single JSON documents, so they are read whole. Injected context, such as `AGENTS.md`, environment blocks and slash-command wrappers, is skipped when picking a title.

`build_handoff` writes the first user request, then as many of the most recent turns as fit in a character budget (60,000 by default). If the newest turn is too large, its tail is kept rather than dropping the turn.

## Parallel runs

`amux spawn` resolves the account and prepares its overlay and environment exactly as a launch does. It claims `runs/<id>/` (creating the folder is the claim, so concurrent spawns never share an id), optionally runs `git worktree add`, and writes `run.json`. Then it starts `amux __run-host <id>` in a new session (`setsid`) with the account's environment and waits until the host has recorded the CLI's pid, or an error.

The host (`runs/host.rs`) is a small terminal multiplexer, in the spirit of dtach:

- It opens a pseudo-terminal and starts the CLI on the slave side in a new session, with the terminal as its controlling terminal. The CLI behaves as it would in a real terminal.
- A pump thread reads the master side. Each chunk is appended to `output.log` (rotated to `output.log.1` at 4 MB), kept in a 256 KB in-memory buffer, and written to every attached client. A client that can't take output within 2 seconds is dropped.
- It listens on `runs/<id>/sock`. The folder is mode 0700, so only you can connect. The socket is bound by relative name, because socket paths are limited to about 100 bytes.
- A new client first gets the buffered output (from a line boundary once the buffer has wrapped), then live output.
- Clients send framed messages: a kind byte, a big-endian length, and the payload. `INPUT` bytes are written to the CLI. `RESIZE` sets the terminal size and sends `SIGWINCH` to the CLI's process group, so the CLI redraws for the client that just attached.
- When the CLI exits, the host records the exit code (128 + signal if it was killed), closes the clients and removes the socket.

`amux attach` (`runs/attach.rs`) puts the terminal in raw mode and copies bytes both ways. It polls the window size and sends `RESIZE` when it changes. On Ctrl-], or when the host closes, it restores the terminal and switches off modes a full-screen CLI may have left on: the alternate screen, a hidden cursor, mouse reporting and bracketed paste.

The pump also feeds each chunk to a detector (`runs/detect.rs`). It strips escape sequences as they arrive, even when one is split across chunks, and keeps the last 4 KB of text. It reports "waiting" when the text contains one of the provider's `waitingPatterns`, or when the CLI rings the bell or sends a notification OSC (9, 777 or 99). It reports "cleared" when a client sends input, or after 4 KB more output with no new signal. The host records the reason and time in `run.json`. When no client is attached, it sends a notification through `runs/notify.rs`. The CLI's own bell already reaches an attached terminal.

`amux ps` derives each run's state from `run.json` and whether the host pid is alive. It reads "working or idle" from the modification time of `output.log`.

## Update check

`update.rs` follows the pattern Claude Code and Codex use. On each run it reads `~/.amux/update-check.json` and prints a notice if a newer version is cached. If the cache is more than a day old, it starts a detached `amux __update-check` process that asks the GitHub releases API for the latest tag and rewrites the cache. The command the user typed never waits on the network.

## State on disk

| Path | Written by | Contents |
|---|---|---|
| `config.json` | `amux account default`, the user | Provider overrides and default accounts |
| `accounts.json` | `amux account …` | Account list: provider, name, auth type, options. No secrets. |
| `accounts/<p>/<n>/` | `sync_overlay`, the CLI | The overlay, plus a `token` file on Linux |
| `handoffs/*.md` | `amux resume` | Transcripts for cross-CLI handoff (mode 0600) |
| `runs/<id>/` | `amux spawn`, the run host | `run.json`, `output.log` (mode 0600) and the attach socket |
| `worktrees/` | `amux spawn --worktree` | Git worktrees for runs |
| `launches.jsonl` | every launch | Time, account, directory and arguments (mode 0600) |
| `update-check.json` | update check | Last check time and latest version |

JSON files are written atomically: amux writes a temp file, then renames it over the old one.
