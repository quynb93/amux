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
- **Network:** amux makes one request of its own, the daily update check to `api.github.com`, which sends nothing but a `User-Agent: amux/<version>` header. Everything else is the CLI you launch.

## Scope

In scope: anything in amux that exposes credentials or private files to other users or processes, writes outside `~/.amux` and the configured shared homes, or deletes user data unexpectedly.

Out of scope: vulnerabilities in the CLIs amux launches. Please report those to Anthropic, OpenAI or Google.
