# Security policy

## Reporting a vulnerability

Please don't report security issues in public issues.

Use GitHub's private reporting instead: **[Report a vulnerability](https://github.com/quynb93/amux/security/advisories/new)**. Include the amux version, your OS, steps to reproduce, and what an attacker could do. You should get a reply within a week. Fixes are released as soon as they're ready, and reporters are credited unless they prefer not to be.

Only the latest release gets security fixes.

## How amux handles credentials

amux sits next to your logins, so here is exactly what it touches:

- **CLI logins** made with each CLI's own sign-in stay where that CLI puts them: in the account's overlay directory, or in a Keychain entry the CLI creates. amux doesn't read, copy or send them.
- **Tokens** stored with `--token` or `amux account token` are kept in the macOS Keychain (service `amux`, account `<cli>@<name>`). On Linux they're kept in a `0600` file inside `~/.amux/accounts/<cli>/<name>/`. At launch the token is put in the CLI's environment. On macOS the token is sent to the `security` tool on its standard input, so it never appears in the process list.
- **`amux env`** prints the token for token-based accounts, because its output is meant for `eval`. Don't paste that output anywhere.
- **Local files** that can contain prompts or transcripts (`launches.jsonl`, `handoffs/`) are created with mode `0600`. `~/.amux` and the overlays use `0700`.
- **Tracker keys** stored with `amux tracker login` (a Linear API key, a Jira API token) are kept in the macOS Keychain (service `amux`, account `tracker:linear` or `tracker:jira`), or in a `0600` file in `~/.amux/trackers/` on Linux. They're sent only to Linear's API or to the Jira site you gave.
- **Network:** amux makes two requests of its own by default, both part of the daily update check and both sending nothing but a `User-Agent: amux/<version>` header: the latest release from `api.github.com`, and `patterns.json` from `raw.githubusercontent.com` (the prompt and usage-limit text it watches for). The fetched patterns are checked (only those two lists, size limits) and only decide what a run shows and when you're notified; auto-approve never reads them. Phone notifications (`amux phone`) and Linear or Jira issues (`amux tracker`) add requests to the services you set up. Everything else is the CLI you launch.

## What amux does and doesn't protect

- **An agent runs as you.** Without `--sandbox`, the CLI and everything it runs can do whatever your user can: read and change your files, run `amux` (including `amux env`), and read tokens amux stores, since the macOS Keychain gives its `amux` entries to any process of the same user that asks through `security`, and the Linux token files are `0600` files of yours. amux's permission prompts, auto-approve rules and guards decide what an agent is allowed to ask for; they are not a boundary against an agent that is already running commands.
- **`--sandbox`** runs the CLI in a container with only the checkout, the account's overlay and the mounts you configure, so it can't read the rest of your home or your Keychain. The token for that account is still in its environment.
- **Run sockets.** Each run's host checks which process connects to its socket. Processes an agent started may type only into that agent's own workers, and only a run's own CLI may report its state, so an agent can't answer its own prompts or drive other runs through amux. Your own processes are not limited.
- **`amux serve`** is reached with a secret token (a cookie after the first visit), answers only to its own host names, limits slow and concurrent connections, and runs only the commands the CLI has. Anyone with the link controls your runs; keep it to HTTPS (`--tailscale`, or `--tls`) or this machine. The `--tls` certificate is a server certificate, not a CA, so trusting it on a phone doesn't let anyone vouch for other sites.
- **Other machines** run amux there over your own SSH; arguments go as one encoded payload, so the remote shell doesn't interpret them.

## Scope

In scope: anything in amux that exposes credentials or private files to other users or processes, writes outside `~/.amux` and the configured shared homes, or deletes user data unexpectedly.

Out of scope: vulnerabilities in the CLIs amux launches. Please report those to Anthropic, OpenAI or Google.
