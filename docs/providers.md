# Provider reference

A *provider* tells amux how one CLI keeps its home directory: which variable moves it, which files belong to a single account, and how to sign in, resume and read sessions.

The built-in providers (`claude`, `codex`, `gemini`) are in [`src/providers.rs`](../src/providers.rs). You can add more, or override parts of a built-in, in `~/.amux/config.json` under `"providers"`.

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
| `conflictingEnv` | no | Variables removed from the environment at launch, so one inherited from your shell can't override the account. The home variable and `tokenEnv` are always removed. |
| `privacyEnv` | no | Variables to set when an account has privacy mode on. Use only opt-outs that the CLI documents. |
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
