# Sandboxed runs

`amux spawn --sandbox` runs the agent CLI inside a Docker or Podman container instead of directly on your machine. The CLI can then only write to the run's checkout (not its git hooks or config), the account's overlay, and the shared home but its settings, instruction files and code; it reads nothing else of yours but the shared home's settings (see [What the sandbox protects](#what-the-sandbox-protects)). That makes "allow everything" (`--dangerously-skip-permissions`, Codex's `--yolo`, broad `amux allow` rules) much safer for runs nobody watches: autopilot, `amux intake`, `amux try`.

```sh
amux spawn --sandbox --image amux-agents --worktree --prompt "fix the flaky login test" claude@ci --dangerously-skip-permissions
amux spawn --sandbox=podman --image amux-agents --worktree codex@me
amux try 3 claude@ci -p "speed up the parser" --sandbox --image amux-agents
```

Everything else stays the same. The run host still runs on your machine and owns the run's terminal, so `amux attach`, `ps`, `ui`, `send`, `wait`, auto-approve, the phone app, `amux diff`, `verify`, `pr` and autopilot work as they do for any run. In `amux ps` the account shows `[sandbox]`, and in `amux ui` the run says `sandboxed`. `run.json` has a `sandbox` field with the engine, image, network, mounts, variables and the container's name.

## The container

For a worktree run of `claude@ci` started with `amux spawn --sandbox --image amux-agents --worktree claude@ci --model opus`, the run host starts:

```sh
docker run --rm -it --init --pull never --name amux-3f9a1c2e-claude-1 \
  --label amux.run=claude-1 --label amux.home=3f9a1c2e \
  --user 501:20 -w /Users/me/.amux/worktrees/app-claude-1 \
  -v /Users/me/.amux/worktrees/app-claude-1:/Users/me/.amux/worktrees/app-claude-1 \
  -v /Users/me/src/app/.git:/Users/me/src/app/.git:ro \
  -v /Users/me/.amux/accounts/claude/ci/.claude:/Users/me/.amux/accounts/claude/ci/.claude \
  -v /Users/me/.claude:/Users/me/.claude \
  -v /Users/me/.amux/worktrees/app-claude-1/.git:/Users/me/.amux/worktrees/app-claude-1/.git:ro \
  -v /Users/me/src/app/.git/objects:/Users/me/src/app/.git/objects \
  -v /Users/me/src/app/.git/refs:/Users/me/src/app/.git/refs \
  -v /Users/me/src/app/.git/logs:/Users/me/src/app/.git/logs \
  -v /Users/me/src/app/.git/packed-refs:/Users/me/src/app/.git/packed-refs \
  -v /Users/me/src/app/.git/worktrees/app-claude-1:/Users/me/src/app/.git/worktrees/app-claude-1 \
  -v /Users/me/src/app/.git/worktrees/app-claude-1/commondir:/Users/me/src/app/.git/worktrees/app-claude-1/commondir:ro \
  -v /Users/me/src/app/.git/worktrees/app-claude-1/gitdir:/Users/me/src/app/.git/worktrees/app-claude-1/gitdir:ro \
  -v /Users/me/src/app/.git/worktrees/app-claude-1/config.worktree:/Users/me/src/app/.git/worktrees/app-claude-1/config.worktree:ro \
  -v /Users/me/.claude/settings.json:/Users/me/.claude/settings.json:ro \
  -v /Users/me/.claude/CLAUDE.md:/Users/me/.claude/CLAUDE.md:ro \
  -v /Users/me/.claude/hooks:/Users/me/.claude/hooks:ro \
  -v /Users/me/.claude/skills:/Users/me/.claude/skills:ro \
  … (the rest of the provider's sandboxReadOnly, each :ro)
  -v /dev/null:/Users/me/.claude/.credentials.json:ro \
  -e CLAUDE_CONFIG_DIR -e AMUX_ACCOUNT -e CLAUDE_CODE_OAUTH_TOKEN -e PORT -e AMUX_PORT -e COMPOSE_PROJECT_NAME \
  -e AMUX_RUN -e TERM -e COLORTERM -e LANG -e HOME=/tmp \
  -e GIT_AUTHOR_NAME=Me -e GIT_COMMITTER_NAME=Me -e GIT_AUTHOR_EMAIL=me@example.com -e GIT_COMMITTER_EMAIL=me@example.com \
  amux-agents claude --model opus
```

- **Mounts, all at the same path as on your machine**, so file paths in the CLI's output, git, `amux diff` and `amux verify` all line up:
  - the run's checkout: in a git repository always the run's own worktree (`--sandbox` implies `--worktree` there), or the folder itself outside git. A plain checkout can't be sandboxed: its `.git` would be the agent's to write, and a `.git/commondir` or `config.worktree` it creates there points git at a config of its own (`core.fsmonitor`, `core.sshCommand`, `diff.external`) that runs at your next git command;
  - the main repository's `.git` folder, which the worktree's `.git` file points to, **read-only**: its `config`, `hooks/`, `info/` and everything else git reads can't be changed;
  - **writable over it**, only what git writes for commits, branches, tags, merges, rebases, stashes and fetches: `objects/`, `refs/`, `logs/`, `packed-refs` and the run's own `worktrees/<name>/` (`logs/`, `packed-refs` and `config.worktree` are created empty first if missing, so they can be mounted);
  - **read-only again over that:** the worktree's `.git` file and its `commondir`, `gitdir` and `config.worktree`, which say where its repository and config are. So the agent can commit, branch and tag, but can't add a hook or an `fsmonitor`, `diff.external`, `core.hooksPath`, `core.sshCommand` or alias that git on your machine would run. Rewriting `packed-refs` (deleting a branch or tag that git has packed, `git gc`, `git pack-refs`) and changing the repository's config (`git branch --set-upstream-to`) fail in the container;
  - the account's overlay (`~/.amux/accounts/<cli>/<name>/<home>`), which holds its settings and login;
  - the shared home (`~/.claude`, `~/.codex`, `~/.gemini`), because the overlay's entries are links into it. It is writable: the CLI keeps far more than sessions there (Claude Code writes `projects/`, `todos/`, `file-history/`, `shell-snapshots/`, `plans/`, `debug/`, `session-env/`, `paste-cache/`, `ide/`, …). Sessions written in the container show up in `amux sessions` as usual;
  - **read-only over it:** the entries that hold settings, instructions or code the CLI also loads on your machine, the provider's `sandboxReadOnly`. For Claude Code: `settings.json`, `settings.local.json`, `CLAUDE.md`, `keybindings.json`, `hooks/`, `skills/`, `plugins/`, `agents/`, `commands/`, `output-styles/` and `rules/`. For Codex: `config.toml`, `AGENTS.md`, `AGENTS.override.md`, `hooks/`, `skills/`, `prompts/`, `plugins/`, `rules/` and `agents/`. For Gemini CLI: `settings.json`, `GEMINI.md`, `extensions/`, `commands/`, `skills/`, `agents/` and `hooks/`. A provider that doesn't say gets all of these names. Each one missing is created on your machine first, so the agent can't create it in the container: a folder, `{}` for a JSON settings file, `{"bindings": []}` for `keybindings.json` and an empty file otherwise, all of which the CLI reads as "nothing set", the same as no file. A mount point can't be removed or renamed, so the agent can't swap one for its own. One that is a symbolic link can't be kept read-only this way (the agent could replace the link), so the spawn is refused: put the file itself there;
  - **hidden:** the shared home's own login, the provider's `privateEntries` and `loginFiles` found there (`.credentials.json`, `auth.json`, `oauth_creds.json`, …), with `/dev/null` (or an empty folder) mounted over each. The account's own login is in its overlay;
  - anything you list in `"mounts"`.

  Nothing else from your home folder is mounted: not `~/.ssh`, not `~/.gitconfig`, not your other projects.
- **Variables** are passed by name (`-e NAME`), so their values, including a token, never appear in `ps` or in `run.json`. These are the ones amux sets for any launch (the home variable, `AMUX_ACCOUNT`, privacy, base URL, token), the run's `PORT`, `AMUX_PORT` and `COMPOSE_PROJECT_NAME`, and `AMUX_RUN`. `HOME` is `/tmp` inside the container, since your real home isn't there. Your git name and email (from `git config` in the run's folder) are passed as `GIT_AUTHOR_*` and `GIT_COMMITTER_*`, so commits made in the container are still yours.
- **Your uid and gid** (`--user`), so files the agent writes belong to you. With Podman, `--userns=keep-id` is added too, so rootless Podman keeps your uid.
- **`--pull never`**: amux checks that the image is on the machine before it starts the run, and never pulls one.
- **`--init`** runs a small init process as PID 1, so the CLI gets signals and its child processes are reaped.
- **The CLI** is the provider's `bin` without its folder (`claude`, `codex`, `gemini`), so the image must have it on its `PATH`.

## Tests of a sandboxed run

`amux verify <run> --test`, `amux land` and `amux compare` run the project's tests, and the tests are the agent's code. For a sandboxed run they run in the run's image, with its mounts, user and network: `<engine> run --rm -i --init --pull never --name amux-<home>-<run>-test-<pid> --label amux.test=<run> … <image> sh -c '<test command>'`, with `CI=1`, `AMUX_RUN`, `HOME` and your git identity set and none of the account's variables. Pass `--host-test` to `verify` or `land` to run them on your machine instead, as before. The image needs the project's toolchain for this.

## What the sandbox protects

- **It protects** your files outside the mounts (your home, `~/.ssh`, `~/.gitconfig`, other projects), the repository's git hooks and config, the shared home's settings, instruction files, skills, plugins and hooks (`sandboxReadOnly`), and the shared home's own login from a run that does anything it likes inside the container.
- **amux's own git commands in a run's checkout** (autopilot's push, `land`, `verify --push`, `diff`, review, overlap) run with `-c core.hooksPath=/dev/null -c core.fsmonitor=false -c protocol.ext.allow=never -c core.pager=cat -c core.editor=true`, diffs with `--no-ext-diff`, and pushes with `--no-verify`, sandboxed or not. Your own hooks don't run for them either.
- **It doesn't protect** what the run may write: the checkout (including its `.gitattributes`, build scripts, CI config and tests, which run wherever you run them: `--host-test`, your editor, CI), the rest of the shared home (sessions, history, caches), and the account's overlay. The overlay holds that account's own settings (for Claude Code, `.claude.json` with its MCP servers): an agent can change them for later runs of the same account outside the sandbox. Use an account for sandboxed runs that you don't also use unsandboxed.
- **The network** is open unless `--network off`: the sandbox limits files, not what the agent can reach or send.
- **The image, the engine and the kernel** are trusted: a container escape is out of scope.

## Sign-in inside the container

A container can't read the macOS Keychain, which is where Claude Code keeps its login on macOS. So an account needs one of these:

- **A stored token.** amux passes it in the CLI's token variable. For Claude Code, create one with `claude setup-token`, then:

  ```sh
  amux account token claude@ci < token.txt     # or: amux account add claude ci --token
  ```

  Codex takes an `OPENAI_API_KEY` and Gemini CLI a `GEMINI_API_KEY` the same way.
- **A login kept in a file in the overlay.** The mount covers it: Codex's `auth.json`, Gemini CLI's `oauth_creds.json`, and Claude Code's `.credentials.json` on Linux.

If an account has neither, `amux spawn --sandbox` refuses to start and says how to add a token. Other CLIs can name their login files with `"loginFiles"` in their provider definition (see [providers.md](providers.md)). A provider without `loginFiles` isn't checked.

## An image

amux doesn't come with an image and never pulls one. Build your own with the CLIs you use. A minimal one:

```dockerfile
# docs/sandbox.md: an image for `amux spawn --sandbox`
FROM node:22-bookworm-slim

# git for the agent's commits; ripgrep and the rest are what agents reach for most.
RUN apt-get update \
 && apt-get install -y --no-install-recommends git ca-certificates ripgrep curl less procps \
 && rm -rf /var/lib/apt/lists/*

# The CLIs. Keep only the ones you use.
RUN npm install -g @anthropic-ai/claude-code @openai/codex @google/gemini-cli \
 && npm cache clean --force

# The run's checkout is mounted at its own path and amux sets -w, so no WORKDIR is needed.
# HOME is set to /tmp by amux; any user id works.
```

Add your project's toolchain (Rust, Python, Go…) so the agent can build and test. Then:

```sh
docker build -t amux-agents -f Dockerfile.agents .
amux spawn --sandbox --image amux-agents claude@ci
```

## Settings

Set defaults in `~/.amux/config.json`:

```json
{
  "sandbox": {
    "engine": "docker",
    "image": "amux-agents",
    "network": "on",
    "mounts": ["~/.cache/cargo-registry:/tmp/.cargo/registry", "~/datasets:ro"],
    "always": false
  }
}
```

| Key | Meaning |
|---|---|
| `engine` | `docker` (the default), `podman`, or a path to an engine that takes `docker run` arguments. `--sandbox=<engine>` overrides it. |
| `image` | The image to run. `--image` overrides it. Required: there is no default. |
| `network` | `"on"` (the default) or `"off"`, which runs the container with `--network none`. `--network on\|off` overrides it. |
| `mounts` | More folders to mount: `"path"` (at the same path), `"path:ro"`, `"src:/dst"` or `"src:/dst:ro"`. `~` is your home. Each must exist. |
| `always` | `true` sandboxes every `amux spawn`, including the runs autopilot, `intake`, `try` and `schedule` start. `--no-sandbox` opts one out. |

`--image` and `--network` on their own also turn on `--sandbox`. `amux failover` and `amux revive` keep a sandboxed run sandboxed.

## Stopping

The container is named `amux-<home>-<run>`, where `<home>` is a short hash of your amux home (`AMUX_HOME`, resolved) and your uid, so two amux homes or two users on one Docker or Podman daemon never pick the same name for runs that share a name. The container also carries the labels `amux.run=<run>` and `amux.home=<home>`, and its name and home are saved in `run.json`, so `amux revive`, `stop` and `rm` always find the same container.

`amux stop` sends the signal as usual. If the run hasn't ended after 5 seconds, amux removes the container. `amux rm --force` does the same, and the run host removes the container whenever the engine's client exits (and before it starts one, in case a revived run's old container is still there), so no container is left running after its run has ended. amux only ever removes a container whose labels match the run and the home: it lists them with `<engine> ps -a -q --filter label=amux.run=<run> --filter label=amux.home=<home>` and removes those with `<engine> rm -f <id>`. A run started before containers had a home in their name keeps its old name, `amux-<run>`, and is matched by its `amux.run` label and that name.

## Limits

- **amux commands inside the container.** amux itself isn't in the image, and `~/.amux` isn't mounted. So inside a sandboxed run, `amux report`, `amux spawn` (a lead's workers), `amux slot` and `amux note` don't work. A sandboxed run can't be a team lead. Report for it from your machine instead: `amux report --run <run> <text>`. amux doesn't mount the run's own folder (`~/.amux/runs/<run>`) for this, because it holds the run's socket, which can type into the run.
- **Ports.** Worktree runs still get `PORT` and `AMUX_PORT`, but amux doesn't publish them from the container. To reach a dev server in the container, publish the port yourself (with a wrapper engine script) or run the server outside.
- **`--network off` also cuts off the model's API.** The container gets `--network none`, so a CLI that talks to a hosted model can't reach it. It suits CLIs that need no network, or that reach their model through a Unix socket you mount. With the network on, the agent can reach anything your machine can: the sandbox limits files, not network access.
- **Docker Desktop on macOS** shares `/Users`, `/Volumes`, `/private` and `/tmp` by default. If your shared home or repository is somewhere else, add it under Settings → Resources → File sharing.
- **Gemini CLI**: `GEMINI_CLI_HOME` points at the account's folder, and only its `.gemini` overlay inside it is mounted.
- Only on macOS and Linux, like runs in general.
