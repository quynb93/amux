# Provider reference

A *provider* tells amux how one CLI keeps its home directory: which variable moves it, which files belong to a single account, and how to sign in, resume and read sessions.

The built-in providers (`claude`, `codex`, `gemini`, `qwen`) are in [`src/providers.rs`](../src/providers.rs). You can add more, or override parts of a built-in, in `~/.amux/config.json` under `"providers"`.

## Qwen Code

The `qwen` provider was checked against the [Qwen Code source](https://github.com/QwenLM/qwen-code):

- `packages/core/src/config/storage.ts`: `getGlobalQwenDir()` uses `QWEN_HOME` as the whole `~/.qwen` directory (so `homeEnvPoints` is `home`), and the login is in `oauth_creds.json` and `google_accounts.json`, MCP logins in `mcp-oauth-tokens.json`. Sessions go to `projects/` in the same directory.
- `packages/cli/src/config/top-level-options.ts`: `--resume <id>` and `-i <prompt>`.
- `packages/cli/src/i18n/locales/en.js`: the prompt text used for `waitingPatterns`.

It has no login command (it signs in with `/auth` on the first run), and amux can't read its session files yet, so it has no `sessions` format.

### Checked and not built in

A CLI can be a provider only if one variable moves the directory that holds its login. These don't, as of their docs and source in November 2026:

- **GitHub Copilot CLI.** `COPILOT_HOME` moves all of `~/.copilot` ([config dir reference](https://docs.github.com/en/copilot/reference/copilot-cli-reference/cli-config-dir-reference)), but the OAuth token goes in the system keychain under the service `copilot-cli` for every home, and only falls back to `config.json` when there is no keychain ([authentication](https://docs.github.com/en/copilot/how-tos/copilot-cli/set-up-copilot-cli/authenticate-copilot-cli)). Accounts would not be kept apart.
- **OpenCode.** Logins live in its SQLite database under `XDG_DATA_HOME`. `OPENCODE_CONFIG_DIR` moves only the config, and `OPENCODE_DB` moves the database, sessions included. Moving `XDG_DATA_HOME` would move every app's data.
- **Cursor Agent.** `CURSOR_CONFIG_DIR` moves `cli-config.json` ([configuration](https://cursor.com/docs/cli/reference/configuration)), but the docs only say the login is "securely stored locally".
- **Amp.** Settings move only with the `--settings-file` flag ([settings](https://ampcode.com/docs/cli/settings)); no variable is documented for its settings or secrets.

If one of these gains a home variable, a definition in `config.json` is enough to use it.

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
      "privateEntries": ["credentials.json", "state*"],
      "sharedDirs": ["sessions"],
      "loginArgs": ["login"],
      "statusArgs": ["whoami"],
      "resumeArgs": ["--resume", "{id}"],
      "promptArgs": ["{prompt}"],
      "tokenEnv": "MYCLI_API_KEY",
      "baseUrlEnv": "MYCLI_BASE_URL",
      "privacyEnv": { "MYCLI_TELEMETRY": "0" }
    }
  ]
}
```

An entry whose `id` matches a built-in is merged into it, so you only list the fields you change. A new `id` must have at least `bin`, `sharedHome` and `homeEnv`.

## Fields

| Field | Required | Description |
|---|---|---|
| `id` | yes | Short name used in commands: `amux <id>@<name>`. |
| `label` | no | Display name. Defaults to `id`. |
| `bin` | yes | The executable: a name found on `PATH`, or an absolute path. |
| `sharedHome` | yes | The real home directory that accounts share, e.g. `~/.mycli`. `~` is expanded. |
| `homeEnv` | yes | The environment variable that moves the CLI's home, e.g. `MYCLI_HOME`. |
| `homeEnvPoints` | no | `"home"` (default) if `homeEnv` points at the home directory itself. `"parent"` if it points at a directory that *contains* it; Gemini CLI's `GEMINI_CLI_HOME` works this way. |
| `privateEntries` | no | Names in the home directory that belong to one account and are never linked: credentials, the signed-in account, per-account caches. Use exact names, or a trailing `*` for a prefix (`state*` matches `state.db` and `state.db-wal`). |
| `sharedDirs` | no | Directories created in the shared home up front, so the CLI never creates a private copy inside an overlay. List the session directory here. |
| `loginArgs` | no | Arguments that sign in, run by `amux account add`. Leave this out if the CLI signs in on first run. |
| `statusArgs` | no | Arguments that print who is signed in, run by `amux account status`. |
| `resumeArgs` | no | Arguments that resume a session. `{id}` is replaced with the session ID. |
| `promptArgs` | no | Arguments that start an interactive session with a first prompt. `{prompt}` is replaced. This is needed to hand sessions *to* this CLI. |
| `tokenEnv` | no | Variable that holds a token, used by accounts added with `--token`. |
| `baseUrlEnv` | no | Variable that sets the API endpoint, used by `amux account baseurl`. |
| `tokenHint` | no | Shown when amux asks for a token, e.g. how to create one. |
| `waitingPatterns` | no | Text the CLI shows when it waits for you, such as a permission prompt. Matched case-insensitively against a background run's output to show `waiting` in `amux ps` and send a notification. Once the CLI reports its own state with OSC 7501, as Codex and Claude Code 2.1.295+ do, these only apply while it reports idle or done, to catch dialogs it doesn't report. |
| `conflictingEnv` | no | Variables removed from the environment at launch, so one inherited from your shell can't override the account. The home variable and `tokenEnv` are always removed. |
| `privacyEnv` | no | Variables to set when an account has privacy mode on. Use only opt-outs that the CLI documents. |
| `loginFiles` | no | Files in the overlay that hold a login, such as `["auth.json"]`. `amux spawn --sandbox` checks that one exists (or that the account has a token) before it starts the CLI in a container. Leave this out and amux doesn't check. |
| `sessions` | no | Session file format: `"claude"`, `"codex"` or `"gemini"`. Leave this out if amux can't read this CLI's sessions; `amux sessions` then skips it. |

## Choosing private entries

Getting `privateEntries` right is what keeps accounts apart. A file is private if it contains any of these:

- credentials or tokens,
- the identity of the signed-in account,
- state fetched for one account, such as policy or remote settings, rate limits or usage,
- a value that must match one of the above, such as a signature or "last synced" file.

Anything else (skills, plugins, settings, sessions, history, caches that aren't tied to an account) should stay shared. Shared is the default.

When an entry becomes private in a newer amux version, the next launch or `amux sync` removes the old symlink from each overlay, and the CLI writes a fresh copy for that account. The shared copy is left in place.

## Testing a definition

1. In a sandbox (see [CONTRIBUTING.md](../CONTRIBUTING.md#test-against-a-sandbox-not-your-real-accounts)), add the provider to `$AMUX_HOME/config.json` and run `amux providers`.
2. Add two accounts and sign in to a different login in each.
3. Check that each overlay holds its own credentials, and that `amux account status` shows two different identities.
4. Start a session in one account and check that `amux sessions` lists it, and that the other account can resume it.
5. Run `amux sync` and check that it reports no `unshared` entries. If it does, the CLI writes something you haven't classified yet.
