# Contributing to amux

Thanks for helping. This guide covers setting up a dev environment, testing changes without touching your real accounts, and getting a pull request merged.

## Ways to contribute

- **Report a bug.** Open an [issue](https://github.com/quynb93/amux/issues/new/choose) with the output of `amux --version` and `amux doctor`, and the steps to reproduce it.
- **Add or fix a provider.** If a CLI changes where it keeps its login or sessions, or you use a CLI amux doesn't support yet, a provider definition is often the most useful contribution. See [Adding a provider](#adding-a-provider).
- **Improve the docs.** Typos, unclear steps and missing examples are all worth a PR.
- **Build a feature.** For anything larger than a small fix, open an issue first so we can agree on the approach before you spend time on it. The [roadmap](README.md#roadmap) lists what's planned.

## Development setup

You need Node.js 20 or newer, and macOS or Linux.

```sh
git clone https://github.com/quynb93/amux.git
cd amux
npm install        # installs TypeScript and builds dist/
npm link           # optional: makes your working copy the `amux` on PATH
```

| Script | What it does |
|---|---|
| `npm run build` | Compile `src/` to `dist/` and make `dist/cli.js` executable. |
| `npm run dev` | Recompile on every change. If you use `npm link`, run `chmod +x dist/cli.js` once. |
| `npm run typecheck` | Type-check the source and the tests. |
| `npm test` | Compile and run the test suite. |
| `npm run clean` | Delete `dist/` and `.test-build/`. |

## Test against a sandbox, not your real accounts

amux works on your real `~/.claude`, `~/.codex` and `~/.gemini`. When you try a change by hand, point it at a throwaway directory so you can't damage your logins or sessions:

```sh
export SANDBOX="$(mktemp -d)"
export HOME="$SANDBOX" AMUX_HOME="$SANDBOX/.amux" AMUX_NO_UPDATE_CHECK=1

node dist/cli.js account add claude test --no-login
node dist/cli.js account list
ls -la "$SANDBOX/.amux/accounts/claude/test/.claude"
```

Setting `HOME` moves the shared homes into the sandbox too, because built-in providers resolve `~` from `HOME`.

You can test launch behavior without a real CLI by using a fake provider whose "binary" prints what it receives. `test/cli.test.ts` shows the setup.

Never commit anything from a real home directory. Session transcripts, `.claude.json` and tokens contain private data.

## Running the tests

```sh
npm test
```

The suite uses Node's built-in test runner and needs no extra packages. Every test runs in its own temporary `HOME` and `AMUX_HOME`, and no test reads or writes the Keychain. The suite runs in CI on macOS and Ubuntu, with Node 20 and 22.

| File | Covers |
|---|---|
| `test/overlay.test.ts` | Linking, pruning, stray detection and adoption in overlays |
| `test/store.test.ts` | Config merging, custom providers, account resolution |
| `test/launch.test.ts` | The environment passed to the CLI: home, privacy, base URL, conflicting variables |
| `test/sessions.test.ts` | Parsing Claude and Codex session files, and building handoffs |
| `test/update.test.ts` | Version comparison and repo parsing for the update check |
| `test/util.test.ts` | Argument parsing and output helpers |
| `test/cli.test.ts` | End to end: runs the built CLI against a sandbox with a fake provider |

Add or update tests along with your change. A bug fix should come with a test that fails without the fix.

## Project layout

```
src/
  cli.ts               entry point: help text and command routing
  commands/
    account.ts         amux account …
    sessions.ts        amux sessions, amux resume
    maintenance.ts     amux providers, env, sync, doctor
  providers/
    types.ts           ProviderSpec, the description of one CLI
    builtin.ts         Claude Code, Codex and Gemini CLI definitions
  overlay.ts           builds and refreshes overlay directories
  launch.ts            builds the CLI's environment and runs it
  store.ts             config.json, accounts.json, account and provider lookup
  sessions.ts          reads each CLI's session files, builds handoffs
  secrets.ts           token storage (Keychain on macOS, 0600 file elsewhere)
  update.ts            background update check
  paths.ts, util.ts    shared helpers
test/                  *.test.ts, one file per module, plus end-to-end tests
docs/                  architecture and provider reference
```

[docs/architecture.md](docs/architecture.md) explains how these pieces fit together.

## Adding a provider

Most CLIs need only a definition, with no new code:

1. Find out how the CLI moves its home directory (an environment variable such as `FOO_HOME`), which files in that home hold the login, and where it writes sessions. The CLI's docs or source are the authority. Don't guess file names.
2. Try the definition in your sandbox's `~/.amux/config.json` first, sign in to two accounts, and check that each one keeps its own login. Field reference: [docs/providers.md](docs/providers.md).
3. To ship it as a built-in, add it to `src/providers/builtin.ts`, add a row to the table in the README, and add a test.

Reading a new session format (for `amux sessions` and handoff) needs a parser in `src/sessions.ts` and a fixture-based test in `test/sessions.test.ts`.

## Code guidelines

- **No runtime dependencies.** amux runs on Node's standard library. Discuss any new dependency in an issue first. Dev dependencies need a good reason too.
- **Never delete or overwrite user data you didn't create.** Overlay code may remove only symlinks that amux made. Moving or deleting real files needs an explicit flag (like `--adopt` or `--purge`).
- **Keep secrets out of output and logs.** Don't print tokens, don't write them anywhere except `secrets.ts`, and create private files with mode `0600`.
- **Respect organization policy.** amux won't accept changes that turn off, hide from or work around settings an organization manages. Opt-outs must be ones the provider documents.
- **Error messages say what to do next.** For example: ``No account "claude@x". Add it with `amux account add claude x`.``
- TypeScript runs in strict mode with unused-code checks. Comments explain *why*, not *what*. Follow the existing style: 2-space indent, double quotes, semicolons. `.editorconfig` covers the basics.

## Commits and pull requests

1. Create a branch from `main`.
2. Keep each PR to one change. A refactor and a feature are easier to review as two PRs.
3. Make sure `npm run typecheck` and `npm test` pass.
4. Update the README or `docs/` if behavior or commands change.
5. Add a line under `## [Unreleased]` in [CHANGELOG.md](CHANGELOG.md) for anything users will notice.
6. Write commit messages in the imperative mood, with a short subject line. For example: `Keep remote-settings sidecars private per account`.

The PR template has a short checklist. CI must pass before a merge.

## Releasing (maintainers)

1. Move the `## [Unreleased]` entries in `CHANGELOG.md` into a new `## [x.y.z] - YYYY-MM-DD` section.
2. Set `"version"` in `package.json` to `x.y.z`, then run `npm install` to update the lockfile.
3. Commit with the message `Release vx.y.z`, then tag and push:
   ```sh
   git tag vx.y.z
   git push origin main vx.y.z
   ```
4. The [release workflow](.github/workflows/release.yml) checks that the tag matches `package.json`, runs the tests, and publishes a GitHub release with that version's changelog section as the notes. Installed copies of amux see the new version within a day, through the update check.

## Code of conduct

Be respectful and constructive. Harassment and personal attacks are not tolerated in issues, PRs or discussions. Maintainers may remove content or block people who don't follow this.
