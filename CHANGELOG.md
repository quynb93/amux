# Changelog

All notable changes to amux are listed here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and amux uses [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Changed

- `amux peek`, `amux explain`, `amux send` and the phone app's screen ask the run's host for its screen alone (a new `SNAPSHOT` frame after `WATCH`); the host sends it and closes, so they no longer wait for the output to go quiet. Measured on macOS (release build, median of 30): `amux peek` 161 ms → 3 ms on an idle run and 3,053 ms → 3 ms on a busy one. Runs started before upgrading keep their old host, which ignores the frame; for those, the client still reads until 150 ms of quiet as before.
- `amux send` looks at the screen only for multi-line text (to know whether the CLI takes a bracketed paste): 378 ms → 225 ms idle, 3,255 ms → 224 ms on a busy run (what is left is its deliberate pauses before Enter and before closing).
- The run host queues output for each client and writes it from a thread per client, instead of writing to every client in turn under its lock. A client that falls 4 MB behind, or whose socket takes nothing for 2 seconds, is disconnected; an `amux attach` in a terminal that froze that long ends, and you can attach again.
- Auto-approve reads the screen when new output has a prompt's words, at most every 100 ms otherwise, and once more when output has been quiet 100 ms, instead of rendering the whole screen for every chunk. With any allow rule, throughput of a 50 MB burst went from 28 MB/s to 62 MB/s and the host's CPU for it from 1.76 s to 0.79 s; a prompt on screen is still answered within about 100 ms.
- Prompt and usage-limit detection keeps its 4 KB window lowercased as it arrives and searches only the new text (memchr), falling back to the old full search while the window holds cased non-ASCII text or invalid UTF-8, so it finds exactly what it did. With `limitPatterns`, 43 → 60 MB/s.
- `output.log` is written through a 64 KB buffer, flushed whenever the host is about to wait for more output and at least every 100 ms, instead of one write per chunk. With the changes above, a plain 50 MB burst: 51 → 61 MB/s, host CPU 0.92 s → 0.81 s. `amux logs` shows new output within a few milliseconds.
- `amux events` and `amux ui` parse a run's `run.json`, `verify.json` and `pr.json` again only when the file changed (modification time, size, inode), and look at `output.log` only for running runs. With 104 runs, `amux events` used 0.68 s of CPU per 30 s and now 0.41 s; `amux ui` 0.66 s → 0.32 s. `amux serve` runs one `amux events` for all phones instead of one per open app.
- The run host's hook reader sleeps until there is something to do instead of waking every 250 ms.

### Fixed

- One client that stopped reading (a frozen terminal, a suspended `amux attach`, a stuck preview) held up the CLI and every other client for up to 2 seconds. Measured: the longest gap seen by a reading client next to a non-reading one went from 2,014 ms to 17 ms.
### Security

- A run's socket now checks the process at the other end (`LOCAL_PEERPID` on macOS, `SO_PEERCRED` on Linux). A process an agent started (a descendant of any run's CLI) may type into a run only when that run is its own worker (`lead`), so a lead still answers its workers with `amux send`, but no agent types into its own prompt or another team's runs; state reports (`amux __hook`) are taken only from the run's own CLI. `amux send` from inside a run says why it was refused. The hook's own check now fails closed when it can't read its parent. `amux mcp` started inside a run limits `send` and `stop` to that run's workers. The scheduler now starts detached (a grandchild), so starting it from inside a run doesn't tie it to that run.
- `amux serve --tls`: the self-signed certificate is now a server certificate (`basicConstraints=critical,CA:FALSE`, `keyUsage=digitalSignature`, `extendedKeyUsage=serverAuth`, with the same addresses). Before, openssl made a CA certificate, so trusting it on a phone as the instructions said let whoever had the key vouch for any site. An existing `~/.amux/tls/self-*` CA certificate is made again on the next start; remove the old one from the phone. The instructions now recommend `--tailscale` first.

## [0.11.14] - 2026-11-19

An MCP server for agent teams, and reviewing runs on other machines in the UI.

### Added

- `amux mcp`: a Model Context Protocol server over stdio, so an agent that prefers tools to shell commands can run an amux team. Tools: `spawn`, `wait` (blocks up to 10 minutes until a worker finishes its turn, needs an answer or ends, and returns its report or its question and the end of its screen; `runs` waits for given runs instead of a team), `list`, `peek`, `send`, `report`, `diff`, `verify`, `overlap`, `land` and `stop`, each with a JSON Schema for its arguments. Each runs the CLI command's own code, so the same rules and limits apply (a lead's `--workers` cap, no workers of workers, slots); failures are tool errors with the command's message. Started inside a run, the server inherits `AMUX_RUN`, so its spawns are that run's workers. It speaks MCP 2026-07-28 (stateless, `server/discover`) and, through `initialize`, 2025-11-25, 2025-06-18, 2025-03-26 and 2024-11-05.
- `amux mcp --install [claude|codex|gemini]`, `--remove` and `--status`: adds the server to Claude Code (`mcpServers` in each account's `.claude.json` and the one new accounts are copied from), Codex (a marked `[mcp_servers.amux]` block in the shared `config.toml`, forwarding `AMUX_RUN` and `AMUX_HOME`, with an 11-minute tool timeout) and Gemini CLI (`mcpServers` in the shared `settings.json`). Only amux's own entry is added or removed, another server called `amux` is left alone, and files are rewritten atomically. `amux doctor` says where it is installed.
- Review runs on other machines in `amux ui`: `c` on `gpu:claude-1` opens its diff with comments and hunk dropping, as for local runs. amux on that machine reads the diff (`amux __review-data`, internal), keeps the comments (`amux comment`, `amux comments`), and drops hunks (`amux review --drop … --expect <digest>`) with the same all-or-nothing check, the refusal while the agent is working, the undo patch in `runs/<id>/dropped/` there and the note to the agent. `--expect` refuses the drop if the run's diff changed since the review was read, so `file:n` names the same hunk. `amux review <machine>:<run>` now runs on that machine instead of refusing. When amux there is too old to serve a review, `c` says so and opens the diff in your pager as before.

### Fixed

- A Codex run is linked to its own session even when another Codex run started in the same folder within the same second or two. Codex keeps a session's rollout file open for as long as the session lives (the codex-rs rollout recorder, in every release checked from rust-v0.10.0 to rust-v0.170.0), so amux takes the rollout that the run's Codex process (or a process it started, such as the native binary under the npm wrapper) holds open: on Linux from `/proc/<pid>/fd`, on macOS from `proc_pidinfo`. Nothing is passed to Codex, so the agent sees nothing different. Before Codex has written its file (the first turn), amux falls back to the session that started in the folder just after the run, skipping any rollout another live run's Codex holds.

## [0.11.13] - 2026-11-19

Web push to the phone app, and fixes from the 0.11.12 reviews.

### Added

- Web Push: `amux phone webpush` sends notifications straight to the phone app (`amux serve`), with no ntfy or Pushover app. In the app, Monitor → Notifications → Turn on subscribes the device; several devices can, and one whose push service answers 404 or 410 is forgotten. Messages are encrypted for the device (RFC 8291, `aes128gcm`) and signed with amux's own VAPID key (RFC 8292), made on first use and kept in `~/.amux/webpush-key.json` (0600); the devices are in `~/.amux/webpush-subs.json` (0600). Same events, priorities (`Urgency: high` for waiting, stalls, limits and failures), throttling, `brief` mode and links as the other channels. It needs HTTPS (`--tailscale`, or `--tls` with a trusted certificate), and on iPhone and iPad the home-screen app on iOS 16.4 or later; the app says so when push isn't available. The only new network requests go to the push service of each browser that subscribed.

### Fixed

- Two Codex runs in the same folder under different accounts no longer record one account's usage for both. Each run is matched to the session its Codex started just after the run did (sub-agent sessions aside), a session another live run already reads is skipped, and the match is kept in `run.json` (`session`). A run that resumes a session (`amux failover`, `amux revive`) reads that session. `amux usage` only rereads runs whose session is known.
- A Linear issue and a Jira issue with the same key (ENG-1) no longer share the run name `eng-1`. The second gets `jira-eng-1` (or `linear-eng-1`) and its own branch, so `amux spawn --issue jira:ENG-1` no longer fails with "run already exists", and `amux intake` starts it instead of marking it as taken. Intake now tells an issue started by hand by its tracker and key (and repo), not by whether a run with its name exists.
- Linear and Jira intake read up to 500 open issues, so issues already started (still open until their PR merges) no longer hide newer ones.
- A Jira issue's prompt now has its 50 newest comments (from Get comments, newest first, put back in order) instead of the first page of the issue's `comment` field. Linear's queries now say how they are ordered: an issue's 50 newest comments, and for `amux intake` the 100 oldest open issues (`last: 100, orderBy: createdAt`), so with more than 100 open issues, the ones that have waited longest are started first, as with Jira.
- Dropping hunks in `amux ui`'s review no longer freezes the screen while `git apply` and `git commit` (with your pre-commit hooks) run: the drop runs in the background, the footer says it is dropping, and the review is read again when it is done.
- The end-to-end tests no longer leave run hosts and fake CLIs running when a test fails or doesn't stop its runs: each test sandbox stops the hosts and CLI process groups recorded in its own `runs/*/run.json` when it is dropped, after checking with `ps` that each process still belongs to that sandbox.
- The end-to-end tests wait up to 15 seconds (was 5) for a run to reach a state, so they pass on slower CI runners. They still go on as soon as the state is reached.

## [0.11.12] - 2026-11-19

Accounts that switch before the limit, state hooks, sandboxed runs, Linear and Jira issues, and hunk-level review.

### Added

- `amux usage` shows how much of each account's 5-hour and 7-day windows is used, with a bar, when each resets and when the numbers were last updated, next to its limit state and live runs. `--json` includes the numbers (`quota.fiveHour`, `quota.sevenDay`). `amux ui` shows each account's usage in its header, re-read every 15 seconds, and the phone app shows it under Accounts. amux reads the numbers from local files only and makes no network requests for them.
- `amux usage --track on` records Claude Code's numbers through its status line: amux puts `amux __statusline` in the shared `~/.claude/settings.json`, which saves the `rate_limits` Claude Code passes to it for the account, then runs your own status line command with the same input. `--track off` puts your status line back as it was. Codex's numbers are read from the `token_count` events in each Codex run's session file. Gemini CLI keeps none on disk.
- `"limits": { "switchAt": 90 }` moves work off an account before it runs out. `amux spawn <cli>` (no account named) takes the account with the most headroom and skips accounts at or over `switchAt`. With `"onLimit": "failover"`, a run whose account passes `switchAt` moves to an account under it, once, when its turn has ended and no terminal is attached, never mid-turn. It never moves back to an account that is also over `switchAt`. You get a `limited` notification and the `on-near-limit` hook runs.
- Folder accounts: `amux map <dir> <cli>@<name>` makes plain `amux <cli>` (and `amux spawn <cli>`, `amux env <cli>`…) use that account in the folder and every folder below it. The deepest mapped folder wins, a named account still wins over the map, and the map wins over the default account. `amux map` lists the folders, `amux map --rm <dir> [<cli>]` removes one, and `amux spawn` says when the map chose the account. If that account is limited, `amux spawn` stops and says until when instead of moving the run to another account. Kept in `config.json` under `"folders"`.
- `amux state-hooks install | remove | status`: exact run state for CLIs that don't report it with OSC 7501. It adds hook entries to Claude Code's and Gemini CLI's `settings.json` that tell the run's host when the CLI works, waits for you (with the prompt's message, such as "approve Bash: cargo test") and finishes its turn. They do nothing outside amux runs, always exit 0 and print nothing. OSC 7501 reports still win, and hook reports win over `waitingPatterns` and output timing. `remove` takes out only amux's entries. `amux doctor` says whether they are installed, and `amux explain` says when a hook decided a run's state.
- Sandboxed runs: `amux spawn --sandbox[=docker|podman] --image <image>` runs the CLI in a container, so "allow everything" is safe for runs nobody watches. The run host still owns the terminal, so attach, auto-approve, `ui`, `diff`, `verify` and autopilot work as before. Only the run's checkout (plus the main `.git` folder for a worktree), the account's overlay and the shared home are mounted, each at the same path as on your machine. The CLI runs as your uid and gid, the account's variables and token are passed by name, and `--network off` gives it no network. amux never pulls an image: it checks that the engine and the image are there, and that the account can sign in from inside (a stored token, or a login file in its overlay; a macOS Keychain login can't be read from a container). Defaults go in `"sandbox"` in `~/.amux/config.json` (`engine`, `image`, `network`, `mounts`, and `always` to sandbox every spawn). `amux try` takes the same flags. `amux ps` shows `[sandbox]` and `run.json` has a `sandbox` field. `amux stop` and `rm --force` remove the container (`amux-<home>-<run>`, where `<home>` is a hash of the amux home and uid, so homes and users sharing a daemon never touch each other's containers) if it doesn't end. Providers can name their login files with `loginFiles`. See [docs/sandbox.md](docs/sandbox.md), which has an example Dockerfile.
- Linear and Jira issues as runs. `amux spawn --issue linear:ENG-123` (or `jira:PROJ-45`, or the issue's URL) reads the issue's title, description and comments and starts a run on the branch `amux/eng-123`. Jira's rich text is turned into plain text. `amux intake --from linear --label <l> [--team <key>]` and `amux intake --from jira --jql <query>` (or `--label` with `--project`) start a run for each new issue, once, like GitHub intake, and work with `amux schedule` and `--autopilot`.
- `amux tracker login linear` and `amux tracker login jira <site-url> <email>` check a Linear API key or Jira API token and store it in the macOS Keychain (a `0600` file in `~/.amux/trackers/` on Linux). `LINEAR_API_KEY`, and `JIRA_API_TOKEN` with `JIRA_URL` and `JIRA_EMAIL`, work instead. `amux tracker` shows what's set up, and `amux tracker logout` forgets a key.
- The pull request of a run on a Linear issue starts with `Fixes ENG-123`, so Linear's GitHub integration links it; a Jira one starts with the key. Both link to the issue. When amux opens the PR (`amux pr` or autopilot), it comments the PR's link on the issue; `"trackers": { "comment": false }` in `config.json` turns that off. amux doesn't change an issue's status.
- Keep or drop single hunks of a run's changes. In `amux ui`'s review (`c`), space marks the hunk under the cursor (or every hunk of a file, on its name) and `D` drops the marked ones after a y/n confirm. `amux review <run> --hunks` lists hunks numbered per file, and `--drop <file>[:<n>]` drops them from scripts. The hunks are reverse-applied to the run's worktree, and a new untracked file is deleted. When nothing else was uncommitted there, the drop is committed as "Drop N hunks (amux review)". The agent is told what was dropped so it doesn't add it back. Nothing is dropped if any hunk no longer matches the reviewed diff or the agent is mid-turn, and each drop is saved as a patch in `runs/<id>/dropped/` to undo it.
- Dev-server ports in `amux ui`: once something listens on a worktree run's `PORT`, the run's row, board card and preview title show it, and `o` opens `http://localhost:<port>` in the browser. `amux open <run>` does the same from the shell. The UI checks the ports in the background every few seconds, never per frame.
- Prompt patterns without a release: `amux update --patterns` fetches `patterns.json` (the `waitingPatterns` and `limitPatterns` of the built-in CLIs) from the repo's main branch and saves it as `~/.amux/patterns.json`, and the daily update check does the same unless `AMUX_NO_UPDATE_CHECK` is set. The file is checked first (64 KB at most, valid JSON, only those two keys for built-in CLIs, one line of plain text per pattern) and isn't saved if it fails. It sits between the built-in patterns and your own `config.json`, and auto-approve never reads it.
- Qwen Code is a built-in provider (`amux qwen`): `QWEN_HOME` moves its home, and `oauth_creds.json`, `google_accounts.json` and `mcp-oauth-tokens.json` stay private per account. GitHub Copilot CLI, OpenCode, Cursor Agent and Amp were checked and left out, because no single variable moves their login; docs/providers.md says why for each.

### Changed

- In the review view, space now marks a hunk instead of paging down. PgDn, Ctrl-F and Ctrl-D page down; PgUp, Ctrl-B and Ctrl-U page up.
- The review diff shows a renamed file as removed and added, so each part can be dropped on its own.
- `amux doctor` no longer fails for a built-in CLI that isn't installed when you have no accounts for it, and notes any provider's home variable set in your shell.

### Fixed

- Typing an answer into a run could clear the *next* question a fast CLI printed right after it, so the run showed working instead of waiting and no notification went out. amux now clears the wait before the answer reaches the CLI.

## [0.11.11] - 2026-11-19

Tagged, but not published: a test failed in the release build. Everything in it is in 0.11.12.

## [0.11.10] - 2026-11-18

Fixes from running autopilot on real issues with Claude Code.

### Fixed

- Auto-approve read no Bash command in Claude Code 2.1.3xx, which draws a `│` bar before each command line, so every command prompt waited for you. Commands are read without the bar again.
- Autopilot pushes the agent's new commits itself when its turn ends, and asks the agent only to commit. Before, CI fixes and review changes waited on the agent's own `git push`, which the starter rules deny, so the run sat at a permission prompt.
- Autopilot no longer calls a PR with merge conflicts ready, and with `=merge` no longer gets stuck trying to merge it. It asks the agent to merge the base branch in and resolve the conflicts, then pushes the result. It also waits while GitHub is still working out whether the PR merges.

## [0.11.9] - 2026-11-18

### Added

- Autopilot: `amux spawn --autopilot` (or `--autopilot=merge`, also through `amux intake`) takes a run from its first commit to a pull request that's ready to merge. When the agent's turn ends with commits, amux opens the PR. It then hands failing CI back with the failed logs (once per commit, 3 times at most) and hands back new review comments from unresolved threads and reviews. It tells you when the PR is ready, needs a review, is stuck or was merged, and with `=merge` merges it itself. Text only goes to the agent when its turn has ended. `amux autopilot` lists runs on autopilot with their phase and PR, `--log` shows a run's history, and `amux autopilot <run> [--merge] [--retries <n>] | --off` turns it on or off for a running run. The scheduler drives it. Each step sends an `autopilot` phone notification and runs the `on-autopilot` hook.

## [0.11.8] - 2026-11-18

### Fixed

- `brew install quynb93/tap/amux` and `brew upgrade amux` no longer fail with "Your Command Line Tools are too outdated". Releases now publish Homebrew bottles, so brew pours the prebuilt binary instead of treating the formula as a source build.

## [0.11.7] - 2026-11-17

### Added

- Auto-approve: `amux allow` keeps rules (`bash: cargo test*`, `edit`, deny rules that win) in `~/.amux/approve.json`, and each run's host answers Claude Code permission prompts that ask for what they allow. Shell commands are judged part by part; substitution, redirection, background jobs and multi-line commands always wait for you, and edits are only answered inside the run's folder. `--starter` adds a safe set, `--check` says what would happen to a prompt, and `--log` shows every decision (`runs/<run>/approvals.jsonl`). `amux spawn --no-auto-approve` makes a run always ask. Answers run the `on-approved` hook.

## [0.11.6] - 2026-11-17

### Added

- `amux spawn --issue <n>`: a GitHub issue (number, `#n` or URL, read with `gh`) becomes the run's first prompt, in its own worktree on `amux/issue-<n>`. `--prompt` adds your own instructions. `amux pr` opens its pull request with `Fixes #<n>`.
- `amux intake [--label <l>] [--limit <n>] [--dry-run]`: a run for each new open issue (with the label), oldest first, each started once.
- `amux schedule`: run any amux command by itself, `every 30m`, `daily 9:00`, `weekdays 9:00`, `mondays 9:00` or on a cron line, in local time, from the folder it was added in. `schedule run` runs one now, `log` shows what it printed, and `pause`, `resume` and `rm` manage them. A background scheduler starts them, comes back with any amux command, and exits when no schedules are left. A failed scheduled command sends a `schedule` phone notification.

## [0.11.5] - 2026-11-17

### Changed

- `amux update` now installs the latest release instead of printing a command. Homebrew and npm installs are upgraded by `brew upgrade amux` or `npm install -g`. Other installs download the release archive for this machine, check it against the published SHA-256, make sure the new binary runs and reports the expected version, and rename it over the old one, so a failed or tampered download changes nothing. `amux update --check` keeps the old behavior.

## [0.11.4] - 2026-11-17

Fixes from running a real lead with Claude Code: it split a two-module task between two workers, then reviewed, tested and landed their work in about two minutes.

### Added

- `amux lead … -- <cli flags>` passes flags (permission mode, allowed tools, model) to the lead's CLI, and the lead gives the same flags to its workers.
- `amux land --command <cmd>` tests with that command instead of the configured or detected one.
- Python projects with only `test_*.py` files (and no pytest config) are tested with `python3 -m unittest`.

### Fixed

- `amux land` no longer refuses a run whose worktree has untracked files, such as the `__pycache__` a test run leaves. Only uncommitted changes to tracked files stop it. amux's own test runs also set `PYTHONDONTWRITEBYTECODE`.
- A prompt now goes before the CLI's own flags, so a flag that takes several values (`claude --allowedTools a b`) can't swallow it.
- The lead was told to answer a worker's menu with Enter. Claude Code's folder-trust menu has "No, exit" highlighted, so it's now told to read the prompt first and press the keys for the answer it wants.
- Detected pytest runs use `python3`, which macOS has, instead of `python`.
- `amux serve` no longer exits when its output is closed (`amux serve | head`, a closed terminal); it keeps serving.

## [0.11.3] - 2026-11-17

A lead agent that runs a team.

### Added

- **`amux lead <acct> -p <goal>`** starts a lead agent. Its prompt tells it how to run a team with amux: split the goal into parts, start a worker for each in its own worktree, wait for them, review, test and land their work. `--workers <n>` caps how many it has going at once (3 by default), and `--worker <acct>` picks the workers' accounts.
- **Workers.** A run started from inside a run is that run's worker (`lead` in `ps --json`), and its prompt ends with how to report back. Workers can't start workers of their own.
- **`amux report <text>`**, from inside a run, records what it did for its lead.
- **`amux team [<lead>]`** lists a lead's workers with their state, changes, test results and reports. **`amux team wait`** blocks until the next worker finishes its turn, needs an answer or ends, then prints its report (or what it's asking and the end of its screen), each change once. It returns 124 after `--timeout` (9 minutes by default), so an agent can loop on it instead of polling.
- `amux spawn` warns when amux's own options (`--prompt`, `--worktree`, `--name`…) come after the account, where they'd go to the CLI.

### Fixed

- Examples in the agent skill and the README passed the prompt after the account (`claude -p …`), which starts Claude in one-shot print mode instead of as a helper that can be steered.

## [0.11.2] - 2026-11-17

Themes.

### Added

- **Fifteen popular palettes built in:** Catppuccin Mocha and Latte, Tokyo Night, Dracula, Gruvbox dark and light, Nord, Solarized dark and light, Rosé Pine and Rosé Pine Dawn, Kanagawa, Everforest, One Dark and GitHub Light. They paint their own background, also under popups and behind a run's screen, where the agent uses its default colors.
- **`amux theme [<name>]`** lists the themes with a color swatch of each, or chooses one. `--json` for scripts.
- **The phone app follows the theme**, and its Monitor tab has a theme picker that switches `amux ui` too. A theme file of your own (`~/.amux/themes/<name>.json`) that extends a palette carries over to the app.
- A `background` role for theme files.

## [0.11.1] - 2026-11-17

The phone app over HTTPS.

### Added

- **`amux serve --tailscale`** serves the app at `https://<machine>.<tailnet>.ts.net` with a real certificate, through `tailscale serve`, to your own devices only. amux itself listens on 127.0.0.1, and Tailscale stops serving when amux stops.
- **`amux serve --tls`** serves HTTPS itself with a self-signed certificate (made once with `openssl`, kept in `~/.amux/tls/`) and prints its fingerprint. The phone can install it from `/amux-cert.pem` so the browser stops warning.
- **`amux serve --cert <pem> --key <pem>`** serves HTTPS with your own certificate (`tailscale cert`, mkcert, Let's Encrypt).
- Over HTTPS the login cookie is marked `Secure`. Phone notifications link to the HTTPS address.

## [0.11.0] - 2026-11-17

Your phone hears from amux even when the app is closed.

### Added

- **`amux phone`** sends notifications to your phone through ntfy (ntfy.sh with a topic nobody will guess, or your own server with a token), Pushover, or any webhook (Slack and Discord ones work as is).
  - You're told when a run is waiting for you, stalled, hit a usage limit or ended, and when its tests or CI checks pass or fail. `amux phone events` picks which.
  - A tap opens the run in the phone app (`amux serve`, at the address it last listened on, or `amux phone open <url>`). The link never carries the token.
  - `amux phone brief on` sends only run names and events, never what the agents said. `amux phone test` checks it; `amux phone off` stops it.
  - Sent by a short-lived `amux __push` process, so a slow network never holds up a run, and at most once a minute per run and event. Failures go to `~/.amux/phone.log`.
- The phone app opens a run from a link (`/#run=<name>`).

## [0.10.0] - 2026-11-17

Runs survive a restart: amux brings them back where they were, with the session they were having.

### Added

- **`amux revive [<run>...] | --all`** brings back lost runs, whose host ended without recording an exit (a restart, a crash, or `kill -9`).
  - Each one keeps its name, folder, worktree, ports, group and guards.
  - Its CLI resumes the session it was having (`claude --resume <id>`, `codex resume <id>`), and once it's up the agent is told the computer restarted and to carry on. `--fresh` starts a run whose session can't be found again with its original command, and `--dry-run` shows what would run.
  - `amux revive` on its own lists lost runs and whether a restart lost them (by the machine's boot time).
- **`amux revive --at-login`** installs a login item (a LaunchAgent on macOS, a systemd user unit on Linux) that runs `amux revive --all` 30 seconds after each login, with your `PATH`. `--no-at-login` removes it.
- `amux explain` says whether a lost run was lost in a restart. `amux ui` says how many runs were lost when it opens and has `/revive`, and the phone app has a Revive button.

## [0.9.0] - 2026-11-17

Agents working in parallel can now be run against each other and the best kept, and they share decisions instead of colliding on ports and env files.

### Added

- **Shared notes for a repo.** `amux note "use zod, not yup"` adds a decision that every new run on the repo starts its prompt with. The notes live in the repo's git folder (`.git/amux/notes.jsonl`), so every worktree sees them and they're never committed. `amux notes [--rm <n>]` lists or removes them.
- **`amux broadcast <text> [--note]`** sends a message to every live run on the repo (for example "main changed: rebase before you push"). `--note` also keeps it as a note for new runs.
- **No more collisions between parallel dev servers.**
  - Each worktree run gets its own block of ten ports (`PORT` and `AMUX_PORT`, starting at 41000) and its own `COMPOSE_PROJECT_NAME`.
  - `.env`, `.env.local` and `.env.development.local` are copied from the main checkout into new worktrees, which otherwise start without them. Plain files only, never through symlinks, and never over an existing file.
  - Change the copied files with `"worktree": { "copy": [...] }`, or turn ports off with `"ports": false`.

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
