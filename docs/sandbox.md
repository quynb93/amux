# Sandboxed runs

`amux spawn --sandbox` runs the agent CLI inside a Docker or Podman container instead of directly on your machine. The CLI can then only touch the run's checkout and the account's own files. That makes "allow everything" (`--dangerously-skip-permissions`, Codex's `--yolo`, broad `amux allow` rules) much safer for runs nobody watches: autopilot, `amux intake`, `amux try`.

```sh
amux spawn --sandbox --image amux-agents --worktree --prompt "fix the flaky login test" claude@ci --dangerously-skip-permissions
amux spawn --sandbox=podman --image amux-agents --worktree codex@me
amux try 3 claude@ci -p "speed up the parser" --sandbox --image amux-agents
```

Everything else stays the same. The run host still runs on your machine and owns the run's terminal, so `amux attach`, `ps`, `ui`, `send`, `wait`, auto-approve, the phone app, `amux diff`, `verify`, `pr` and autopilot work as they do for any run. In `amux ps` the account shows `[sandbox]`, and in `amux ui` the run says `sandboxed`. `run.json` has a `sandbox` field with the engine, image, network, mounts and variables.

## The container

For a worktree run of `claude@ci` started with `amux spawn --sandbox --image amux-agents --worktree claude@ci --model opus`, the run host starts:

```sh
docker run --rm -it --init --pull never --name amux-claude-1 --label amux.run=claude-1 \
  --user 501:20 -w /Users/me/.amux/worktrees/app-claude-1 \
  -v /Users/me/.amux/worktrees/app-claude-1:/Users/me/.amux/worktrees/app-claude-1 \
  -v /Users/me/src/app/.git:/Users/me/src/app/.git \
  -v /Users/me/.amux/accounts/claude/ci/.claude:/Users/me/.amux/accounts/claude/ci/.claude \
  -v /Users/me/.claude:/Users/me/.claude \
  -e CLAUDE_CONFIG_DIR -e AMUX_ACCOUNT -e CLAUDE_CODE_OAUTH_TOKEN -e PORT -e AMUX_PORT -e COMPOSE_PROJECT_NAME \
  -e AMUX_RUN -e TERM -e COLORTERM -e LANG -e HOME=/tmp \
  -e GIT_AUTHOR_NAME=Me -e GIT_COMMITTER_NAME=Me -e GIT_AUTHOR_EMAIL=me@example.com -e GIT_COMMITTER_EMAIL=me@example.com \
  amux-agents claude --model opus
```

- **Mounts, all at the same path as on your machine**, so file paths in the CLI's output, git, `amux diff` and `amux verify` all line up:
  - the run's checkout: the top of the git checkout the run starts in (its worktree for `--worktree`), or the folder itself outside git;
  - for a worktree, the main repository's `.git` folder, which the worktree's `.git` file points to. Git needs it to read and write commits;
  - the account's overlay (`~/.amux/accounts/<cli>/<name>/<home>`), which holds its settings and login;
  - the shared home (`~/.claude`, `~/.codex`, `~/.gemini`), because the overlay's entries are links into it. Sessions written in the container show up in `amux sessions` as usual;
  - anything you list in `"mounts"`.

  Nothing else from your home folder is mounted: not `~/.ssh`, not `~/.gitconfig`, not your other projects.
- **Variables** are passed by name (`-e NAME`), so their values, including a token, never appear in `ps` or in `run.json`. These are the ones amux sets for any launch (the home variable, `AMUX_ACCOUNT`, privacy, base URL, token), the run's `PORT`, `AMUX_PORT` and `COMPOSE_PROJECT_NAME`, and `AMUX_RUN`. `HOME` is `/tmp` inside the container, since your real home isn't there. Your git name and email (from `git config` in the run's folder) are passed as `GIT_AUTHOR_*` and `GIT_COMMITTER_*`, so commits made in the container are still yours.
- **Your uid and gid** (`--user`), so files the agent writes belong to you. With Podman, `--userns=keep-id` is added too, so rootless Podman keeps your uid.
- **`--pull never`**: amux checks that the image is on the machine before it starts the run, and never pulls one.
- **`--init`** runs a small init process as PID 1, so the CLI gets signals and its child processes are reaped.
- **The CLI** is the provider's `bin` without its folder (`claude`, `codex`, `gemini`), so the image must have it on its `PATH`.

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

The container is named `amux-<run>`. `amux stop` sends the signal as usual. If the run hasn't ended after 5 seconds, amux removes the container with `<engine> rm -f amux-<run>`. `amux rm --force` does the same, and the run host removes the container whenever the engine's client exits, so no container is left running after its run has ended.

## Limits

- **amux commands inside the container.** amux itself isn't in the image, and `~/.amux` isn't mounted. So inside a sandboxed run, `amux report`, `amux spawn` (a lead's workers), `amux slot` and `amux note` don't work. A sandboxed run can't be a team lead. Report for it from your machine instead: `amux report --run <run> <text>`. amux doesn't mount the run's own folder (`~/.amux/runs/<run>`) for this, because it holds the run's socket, which can type into the run.
- **Ports.** Worktree runs still get `PORT` and `AMUX_PORT`, but amux doesn't publish them from the container. To reach a dev server in the container, publish the port yourself (with a wrapper engine script) or run the server outside.
- **`--network off` also cuts off the model's API.** The container gets `--network none`, so a CLI that talks to a hosted model can't reach it. It suits CLIs that need no network, or that reach their model through a Unix socket you mount. With the network on, the agent can reach anything your machine can: the sandbox limits files, not network access.
- **Docker Desktop on macOS** shares `/Users`, `/Volumes`, `/private` and `/tmp` by default. If your shared home or repository is somewhere else, add it under Settings → Resources → File sharing.
- **Gemini CLI**: `GEMINI_CLI_HOME` points at the account's folder, and only its `.gemini` overlay inside it is mounted.
- Only on macOS and Linux, like runs in general.
