# Architecture

amux is a launcher. It never talks to a model API and never edits a CLI's files, except one setting when you ask it to: `amux usage --track on` points `statusLine` in Claude Code's `settings.json` at `amux __statusline`, and `--track off` puts back what was there. `amux state-hooks install` and `amux mcp --install` likewise add only their own entries, and their `remove` takes out only those. Its job is to prepare a directory and an environment so that a normal, unmodified CLI runs as a particular account.

## Code layout

The code is in layers. Each layer uses only the ones below it, so the CLI and the UI are two faces of the same core:

```
main.rs          argument dispatch; `<machine>:<run>` is forwarded over SSH
commands/        the CLI: parse arguments, call the core, print       ─┐
ui/              amux ui: draw, keys, mouse, views; calls the core     ─┤ faces
serve.rs         the phone web app; every action runs an amux command ─┤
mcp/             `amux mcp`: MCP over stdio; tools run commands/ code ─┘
tls.rs           HTTPS for serve: plain-or-TLS connections, certificates, Tailscale
palettes.rs      named color palettes, made into TUI roles (ui/theme.rs) and the app's CSS
remote.rs        other machines: SSH, forwarding, the socket pipe
plugins.rs       `amux <plugin>` and ~/.amux/hooks
work/            what runs do to code and to the machine:
                   slots (machine limits), verify (tests or push), github (PR, CI),
                   issues (GitHub, Linear and Jira issues as prompts), trackers (Linear and Jira APIs),
                   approve (auto-approve rules), review (diff and line comments),
                   autopilot (commits → PR → CI and review feedback → ready or merged),
                   limits and quota (usage limits, each account's usage, picking accounts).
                   Nothing here prints.
runs/            a run's life: host (pty + socket), client (connect, send input),
                   state (classify: working, waiting, idle…), detect, program (OSC 7501),
                   procs (a CLI's process tree and the files it holds open),
                   screen, attach, notify (desktop), push (phone: ntfy, Pushover, webhook,
                   Web Push), webpush (RFC 8291 encryption and VAPID, with `ring`)
git.rs, launch.rs, overlay.rs, store.rs, sessions.rs, providers.rs, …   foundations
```

`commands/schedule.rs` also holds the scheduler, `amux __scheduler`: one process (it holds `scheduler.lock`) that starts each schedule's amux command when it's due. `main.rs` starts it on any command when schedules exist and it isn't running. It also drives autopilot (`work/autopilot.rs`): each tick it steps every run with an active `autopilot.json`, at most once per poll (60 s), and it keeps going while `autopilot.on` exists. A step asks `gh` for the PR, its checks and its reviews (one GraphQL call), types into the agent only when its turn has ended, and records each change of phase in `autopilot.log`.

Issues come from GitHub through `gh`, and from Linear and Jira through `work/trackers.rs`, which makes its HTTPS requests with `ureq` (already used for the update check and phone notifications): GraphQL to Linear, REST v3 to Jira Cloud with basic auth, `/rest/api/3/search/jql` for intake. Jira's rich text (ADF) is turned into plain text there. A run on a Linear or Jira issue keeps it in `run.json` as `tracker` (kind, key, URL, Linear's internal id), which `work/github.rs` uses for the PR body and the comment back on the issue. Tests point `AMUX_LINEAR_URL` and `JIRA_URL` at a local server, and set `AMUX_KEYCHAIN=0` so keys go to files instead of the Keychain.

Remote runs reuse everything: `runs::client::connect("gpu:x")` returns a socket that `remote.rs` bridges to `amux __pipe x` over SSH. Previews, typing and attaching work on them unchanged. The review view (`ui/review.rs`) reads a remote run's diff and comments from `amux __review-data x` there, and comments and drops through `amux comment`, `amux comments` and `amux review --drop … --expect <digest>` there, so the files are only ever read and changed by amux on their own machine. `serve.rs` keeps to the CLI's commands on purpose, so the phone can't do anything the CLI can't.

`runs/` and `work/` never import from `commands/` or `ui/`. Something that both faces need, such as classifying a run's state or typing into it, belongs in `runs/` or `work/`, not in one of the faces.

In `ui/`, `mod.rs` holds the app state, the event loop and reloading. `keys.rs`, `typing.rs` (working in a run's pane, the mouse) and `actions.rs` (what commands do) add to the same `App`. `draw.rs` and `views.rs` (the board and the tiles), `review.rs`, `palette.rs` and `spawn.rs` draw. `input.rs` turns keys, pastes and mouse events back into terminal bytes.

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

1. **Resolve the account** (`store.rs`): `claude@work` is looked up in `accounts.json`. Plain `claude` uses the account `amux map` set for the current folder or the deepest folder above it, else the configured default, else the only account.
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
- A pump thread reads the master side. Each chunk is appended to `output.log` (rotated to `output.log.1` at 4 MB), fed to a terminal emulator (`vt100`) that holds the CLI's current screen, and written to every attached client. A client that can't take output within 2 seconds is dropped.
- It listens on `runs/<id>/sock`. The folder is mode 0700, so only you can connect. The socket is bound by relative name, because socket paths are limited to about 100 bytes.
- A new client first gets a snapshot of the current screen (`runs/screen.rs`: clear, every cell with its attributes, the cursor, and the input modes the CLI turned on), then live output from the same point. Replaying a slice of raw output instead goes wrong: full-screen CLIs such as Claude Code redraw with relative cursor moves, so a slice that starts mid-way paints text in the wrong places. When the CLI exits, the host saves its final screen as `screen.ans`. `amux peek`, `amux explain` and the preview of an ended run use it. Runs from hosts before 0.5.1 have no `screen.ans`, so their whole log is replayed from the start.
- Clients send framed messages: a kind byte, a big-endian length, and the payload. `INPUT` bytes are written to the CLI. `RESIZE` sets the terminal size and sends `SIGWINCH` to the CLI's process group, so the CLI redraws for the client that just attached. `WATCH` (`[1]` or `[0]`) marks a client as only watching, like the preview in `amux ui`. A watching client doesn't count as attached when the host decides whether to notify you. A `WATCH` sent as the first frame is read before the replay. The host also writes the current terminal size to `runs/<id>/size`, so a watching client can emulate the screen at the right size.
- When the CLI exits, the host records the exit code (128 + signal if it was killed), closes the clients and removes the socket.

`amux attach` (`runs/attach.rs`) puts the terminal in raw mode and copies bytes both ways. It polls the window size and sends `RESIZE` when it changes. On Ctrl-], or when the host closes, it restores the terminal and switches off modes a full-screen CLI may have left on: the alternate screen, a hidden cursor, mouse reporting and bracketed paste.

The pump also feeds each chunk to a detector (`runs/detect.rs`). It strips escape sequences as they arrive, even when one is split across chunks, and keeps the last 4 KB of text. It reports "waiting" when the text contains one of the provider's `waitingPatterns`, or when the CLI rings the bell or sends a notification OSC (9, 777 or 99). It reports "cleared" when a client sends input, or after 4 KB more output with no new signal. The host records the reason, the time and what showed it (`source`: report, pattern, bell or notification) in `run.json`. `amux explain` reads these to say why a run shows its state. When no client is attached, it sends a notification through `runs/notify.rs`: to the desktop, and to the phone through `runs/push.rs` once `amux phone` is set up. The CLI's own bell already reaches an attached terminal.

With `amux phone webpush`, `runs/webpush.rs` sends to the phone app itself. The app's service worker (`assets/sw.js`, served at `/sw.js`) subscribes with the server's VAPID public key and POSTs the subscription to `/api/push`. Each message is the JSON `{title, body, url, tag}`, encrypted for each device (`aes128gcm`: an ephemeral P-256 key, ECDH with the device's key, HKDF with its auth secret, AES-128-GCM, one record) and POSTed to its endpoint with `TTL`, `Urgency` and `Authorization: vapid t=<ES256 JWT for the endpoint's origin>, k=<public key>`. The crypto is `ring`, already there for rustls. The unit tests check the encryption against RFC 8291's example byte for byte; `tests/cli.rs` decrypts what a stand-in push service receives.

A CLI can also report its own state with OSC 7501, the Program Status Protocol (`runs/program.rs`): `ESC ] 7501 ; state=blocked:kind=permission:msg=<base64> ST`. Codex sends it on any terminal. Claude Code (2.1.295 and later) sends it once the terminal answers the probe `ESC ] 7501 ; ? ST`, so the pump answers that probe by writing the reply to the CLI's input. It only answers live output, never a replay, so each probe gets one answer.

From the first report on, the reports decide instead of the text, bell and OSC 9 signals:
- `blocked` sets "waiting", with the CLI's message as the reason.
- Any other state clears it. Typing doesn't clear it, because the CLI says when it moves on.
- While the latest report is `idle` or `done`, `waitingPatterns` still apply, because a CLI doesn't report its own dialogs. Claude Code's confirmations after a turn are an example. Prompt text from before the latest state change is forgotten. Such a match is cleared by your answer, the next `working` or `blocked` report, or 4 KB more output. Bells and OSC 9 no longer count once reports have started.
- The root record (state, kind, message, since) is saved in `run.json` as `status`. Child records, such as subagents, are ignored.
- When the CLI exits, `working` and `blocked` records are dropped and `done` and `error` are kept, as the spec says. A new shell prompt (OSC 133 A) also drops `working` and `blocked`.

CLIs that don't send OSC 7501 can report through state hooks instead (`runs/state_hooks.rs`). `amux state-hooks install` adds entries to the hooks in Claude Code's and Gemini CLI's `settings.json` in the shared home, so every account gets them. Each runs `amux __hook <cli> <event>`. Without `AMUX_RUN` it exits at once. Inside a run it reads the hook's JSON from stdin, maps the event to a state (`PermissionRequest` → blocked with "approve Bash: …", `Stop` → idle, and so on), checks that the run's CLI is its parent, grandparent or great-grandparent (so a CLI the agent starts inside the run doesn't report for it), and sends one `STATE` frame (kind 3, JSON `{state, kind, message}`, plus `tool` for Claude Code's tool events and `echo` for its permission notification) to the run's socket. A subagent's tool events (input with `agent_id`) send nothing unless they block. It always exits 0 with no output, and gives up after 2 seconds. The host sees a leading `STATE` frame before it would send the screen, never counts that connection as a client, and hands it to one hook-reader thread that reads the reports in the order they came, so a slow hook never holds up the accept loop.

The detector treats hook reports like OSC 7501 reports, with three differences. The first OSC 7501 report takes over, and hook reports are ignored from then on. An answer ends a hook's block, since no hook fires when you answer a prompt, and the record becomes `working`: Enter, a digit or letter, Esc or Ctrl-C, but not focus reports, arrow keys or pasted text before Enter (`detect::Keys`). A block on one tool call isn't ended by a report about another (`tool` differs: a parallel call), and a repeat of a block (`echo`) that was answered in the last 10 seconds is ignored. While auto-approve has rules, a hook's permission block is held back until the prompt shows and auto-approve passes on it, or 1.5 seconds go by, so a prompt auto-approve answers never notifies you or fires `on-waiting`. `status.via` is `"hook"`, and `state.rs` counts a hook's `working` only while output is fresh, since no hook fires when you interrupt a turn. Entries are recognized by their command (`amux … __hook …`), so `remove` takes out exactly those. The settings are rewritten with `fsx::write_atomic`, through a symlink to its target, keeping the file's mode.

`amux mcp` (`src/mcp/`) is a Model Context Protocol server over stdio: JSON-RPC 2.0, one message per line. `mod.rs` holds the protocol. It is dual-era: a client that opens with `initialize` gets the legacy revision it asks for (2025-11-25 back to 2024-11-05, else 2025-11-25), and a request that carries `_meta["io.modelcontextprotocol/protocolVersion"]` is served statelessly as 2026-07-28 (`resultType`, `serverInfo` in `_meta`, `server/discover`; another version gets `-32022` with the supported list). Parse errors, batches, bad ids, unknown methods and unknown tools get the JSON-RPC errors (`-32700`, `-32600`, `-32601`, `-32602`); a tool that fails returns `isError: true` with the command's message. At start the server keeps close-on-exec copies of fds 0 and 1 for the protocol, points fd 0 at /dev/null and fd 1 at stderr, so nothing that prints or reads stdin can touch the protocol. The reader answers `initialize`, `ping` and `tools/list` itself and runs each `tools/call` on its own thread; `notifications/cancelled` stops a `wait` and drops the answer, and on EOF it lets running calls finish (a `wait` ends at once). `tools.rs` checks arguments against each tool's JSON Schema, then runs the CLI command as its own `amux` process (`run_amux`: `amux spawn …`, `peek`, `send`, `diff`, `verify`, `land`, …) with stdout and stderr on one pipe, so its output and its children's (git, ssh for `machine:run`) become the tool's text, calls run side by side without mixing their output, and every rule and limit is the CLI's. `spawn` takes the new run's name from amux's own "started <run> as" line. `wait` (`team::wait_team`, `wait_runs`), `list` (`ps_values`) and `report` (`team::save_report`) run in the server's process. The server inherits `AMUX_RUN`, so `lead_for_spawn` makes its spawns that run's workers. `install.rs` edits the CLIs' MCP config: `mcpServers.amux` in each Claude account's `.claude.json` (per account, so in every overlay, plus the file `seed_overlay` copies), a marker-delimited `[mcp_servers.amux]` block in Codex's shared `config.toml` (with `env_vars` and `tool_timeout_sec`), and `mcpServers.amux` in Gemini CLI's shared `settings.json`. An entry is amux's when its args are exactly `["mcp"]` and its command is named `amux`.

`amux ui` (`src/ui/`) is one more client of the same hosts. It keeps a vt100 terminal emulator per previewed run, fed from the host socket, and holds at most four previews open whatever the number of runs. Its actions run `amux spawn`, `stop` and `rm` in the background. See [tui-design.md](tui-design.md).

A sandboxed run (`amux spawn --sandbox`, `runs/sandbox.rs`) works the same way, except that the host starts `<engine> run --rm -it … <image> <cli> <args>` on the terminal instead of the CLI. Spawn decides everything about the container and saves it in `run.json` as `sandbox`: the engine, image and network, the mounts (the run's checkout, a worktree's main `.git` folder, the account's overlay and the shared home, each at its own path) and the variables, passed by name so no value is on the command line. The host builds the command from that alone, so `amux revive` starts the same container. The container is named `amux-<home>-<run>` (`<home>` a short hash of AMUX_HOME and your uid, also its `amux.home` label, both saved in `run.json`), and the host removes it when the engine's client exits, only if its labels match the run and the home. See [sandbox.md](sandbox.md).

`amux ps` derives each run's state from `run.json` and whether the host pid is alive. It reads "working or idle" from the modification time of `output.log`.

## Update check

`update.rs` follows the pattern Claude Code and Codex use. On each run it reads `~/.amux/update-check.json` and prints a notice if a newer version is cached. If the cache is more than a day old, it starts a detached `amux __update-check` process that asks the GitHub releases API for the latest tag and rewrites the cache. The command the user typed never waits on the network.

The same background process fetches `patterns.json` from the repo's main branch (`patterns.rs`), as `amux update --patterns` does on demand. It holds `waitingPatterns` and `limitPatterns` for the built-in providers. It is validated (size, JSON, only those two keys, plain one-line text) and saved to `~/.amux/patterns.json` only if it passes, and is checked again when loaded. `store::load_providers` layers it between the built-in definitions and `config.json`. Nothing in `work/approve.rs` reads it.

## State on disk

| Path | Written by | Contents |
|---|---|---|
| `config.json` | `amux account default`, `amux map`, the user | Provider overrides, default accounts, and accounts per folder (`folders`) |
| `accounts.json` | `amux account …` | Account list: provider, name, auth type, options. No secrets. |
| `accounts/<p>/<n>/` | `sync_overlay`, the CLI | The overlay, plus a `token` file on Linux |
| `handoffs/*.md` | `amux resume` | Transcripts for cross-CLI handoff (mode 0600) |
| `runs/<id>/` | `amux spawn`, the run host | `run.json`, `output.log` (mode 0600) and the attach socket |
| `runs/<id>/review.json`, `pr.json` | `amux comment`, `amux ui`, `amux pr`, `amux checks` | Review comments on the run's lines until they are sent, and its pull request and CI state as last seen |
| `runs/<id>/verify.json`, `verify.log` | `amux verify` | How the last test run or push for that run went, and the test output. Kept apart from `run.json`, which the host alone writes while the run is alive. |
| `worktrees/` | `amux spawn --worktree` | Git worktrees for runs |
| `slots/<class>/` | `amux slot`, the run host | `<n>.lock` per slot, held with `flock(2)` by whoever runs in it (the content says who), and `queue/<pid>` per waiter, served oldest first. See `slots.rs`. |
| `machines.json` | `amux machine` | Other machines: name, SSH destination, amux path |
| `plugins/`, `hooks/` | you | Plugins (`amux <name>`) and event hooks (`on-<event>`) |
| `serve-token` | `amux serve` | The phone app's token (mode 0600) |
| `tls/self-<id>.pem`, `.key` | `amux serve --tls` | Self-signed certificates, one per set of addresses (key mode 0600) |
| `serve.json` | `amux serve` | The address it last listened on, for links in phone notifications |
| `phone.json`, `phone-sent.json`, `phone.log` | `amux phone`, `amux __push` | Where phone notifications go (mode 0600), when each run and event was last sent, and failed sends |
| `webpush-key.json`, `webpush-subs.json` | `amux phone webpush`, `amux serve`, `amux __push` | The VAPID key pair (PKCS#8, made on first use) and the devices subscribed from the phone app (endpoint, p256dh, auth), both mode 0600. A device is dropped when its push service answers 404 or 410. |
| `schedules.json`, `schedules.lock`, `schedules/<name>.log` | `amux schedule`, `amux __scheduler` | Schedules and how each last went (edited under the lock), and each one's output |
| `scheduler.lock`, `scheduler.log` | `amux __scheduler` | Held with `flock(2)` by the one scheduler while it runs; when it started and stopped |
| `approve.json` | `amux allow` | Auto-approve rules: allow and deny lists, read by each run host every few seconds |
| `runs/<id>/autopilot.json`, `autopilot.log`, `autopilot.on` | `amux spawn --autopilot`, `amux autopilot`, the scheduler | A run's autopilot settings and progress (phase, CI fixes sent, review comments sent), its history, and the marker that keeps the scheduler going |
| `runs/<id>/approvals.jsonl` | the run host | Each permission prompt it judged: what it asked, approved, denied or left for you, and why |
| `intake.json` | `amux intake` | Issues already started, per repo, so none starts twice: GitHub numbers, and `linear:ENG-1` / `jira:PROJ-2` keys |
| `trackers.json`, `trackers/<linear\|jira>.token` | `amux tracker login` | Jira's site and email; on Linux, the Linear key and Jira token (mode 0600). On macOS the keys are in the Keychain (`tracker:linear`, `tracker:jira`). |
| `limits.json` | the run host, `amux usage --clear` | Accounts that hit their usage limit, until the reset time |
| `usage/<p>@<n>.json` | `amux __statusline` (Claude Code's status line), the run host and `amux usage` (Codex session files) | The account's latest 5-hour and 7-day usage and reset times (work/quota.rs). Windows past their reset are ignored when read. The run host checks it every 20 s, to move a run before the limit (`limits.switchAt`) once its turn has ended |
| `statusline.json` | `amux usage --track on` | The `statusLine` it replaced in Claude Code's shared `settings.json`, which `amux __statusline` runs and `--track off` puts back |
| `<shared home>/settings.json` (Claude Code, Gemini CLI) | `amux state-hooks install`, `remove` | Only amux's own hook entries (`amux __hook …`); nothing else in the file changes |
| `<overlay>/.claude.json`, `~/.claude.json` (Claude Code), `<shared home>/config.toml` (Codex), `<shared home>/settings.json` (Gemini CLI) | `amux mcp --install`, `--remove` | Only the `amux` MCP server entry (`amux mcp`); nothing else in the file changes |
| `launches.jsonl` | every launch | Time, account, directory and arguments (mode 0600) |
| `update-check.json` | update check | Last check time and latest version |
| `patterns.json` | update check, `amux update --patterns` | Prompt and limit text for the built-in providers, from the repo |

JSON files are written atomically: amux writes a temp file, then renames it over the old one.
