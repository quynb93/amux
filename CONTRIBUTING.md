# Contributing to amux

Thanks for helping. This guide covers setting up a dev environment, testing changes without touching your real accounts, and getting a pull request merged.

## Ways to contribute

- **Report a bug.** Open an [issue](https://github.com/quynb93/amux/issues/new/choose) with the output of `amux --version` and `amux doctor`, and the steps to reproduce it.
- **Add or fix a provider.** If a CLI changes where it keeps its login or sessions, or you use a CLI amux doesn't support yet, a provider definition is often the most useful contribution. See [Adding a provider](#adding-a-provider).
- **Improve the docs.** Typos, unclear steps and missing examples are all worth a PR.
- **Build a feature.** For anything larger than a small fix, open an issue first so we can agree on the approach before you spend time on it. The [roadmap](README.md#roadmap) lists what's planned.

## Development setup

You need [Rust](https://rustup.rs) 1.88 or newer, and macOS or Linux.

```sh
git clone https://github.com/quynb93/amux.git
cd amux
cargo build                  # debug build at target/debug/amux
cargo install --path .       # optional: puts your working copy's `amux` on PATH
```

| Command | What it does |
|---|---|
| `cargo run -- <args>` | Build and run amux, e.g. `cargo run -- account list`. |
| `cargo test` | Run the unit tests and the end-to-end tests. |
| `cargo clippy --all-targets -- -D warnings` | Lint. CI fails on any warning. |
| `cargo fmt` | Format the code. CI checks it with `cargo fmt --check`. |
| `cargo build --release` | Optimized build at `target/release/amux`. |

## Test against a sandbox, not your real accounts

amux works on your real `~/.claude`, `~/.codex` and `~/.gemini`. When you try a change by hand, point it at a throwaway directory so you can't damage your logins or sessions:

```sh
export SANDBOX="$(mktemp -d)"
export HOME="$SANDBOX" AMUX_HOME="$SANDBOX/.amux" AMUX_NO_UPDATE_CHECK=1

cargo run -- account add claude test --no-login
cargo run -- account list
ls -la "$SANDBOX/.amux/accounts/claude/test/.claude"
```

Setting `HOME` moves the shared homes into the sandbox too, because built-in providers resolve `~` from `HOME`.

You can test launch behavior without a real CLI by using a fake provider whose "binary" is a shell script that prints what it receives. `tests/cli.rs` shows the setup.

Never commit anything from a real home directory. Session transcripts, `.claude.json` and tokens contain private data.

## Running the tests

```sh
cargo test
```

Unit tests sit next to the code they test, in a `#[cfg(test)] mod tests` block. They use throwaway directories from `src/testutil.rs`, and no test reads or writes the Keychain. `tests/cli.rs` runs the built binary end to end against a sandboxed `HOME` and `AMUX_HOME`. CI runs everything on macOS and Ubuntu, and checks that the code still builds with the minimum supported Rust version.

| Tests in | Cover |
|---|---|
| `src/overlay.rs` | Linking, pruning, stray detection and adoption in overlays |
| `src/store.rs` | Provider merging, defaults, and keeping unknown fields |
| `src/launch.rs` | The environment passed to the CLI: home, privacy, base URL, dropped variables |
| `src/sessions.rs` | Parsing Claude, Codex and Gemini session files, and building handoffs |
| `src/update.rs` | Version comparison, tag validation and upgrade commands |
| `src/util.rs`, `src/time.rs` | Argument parsing, output helpers and timestamps |
| `tests/cli.rs` | End to end: the account lifecycle, launches, `env`, and error messages |

Add or update tests along with your change. A bug fix should come with a test that fails without the fix.

## Project layout

```
src/
  main.rs              entry point: help text and command routing
  commands/
    account.rs         amux account …
    sessions.rs        amux sessions, amux resume
    maintenance.rs     amux providers, env, sync, doctor
  providers.rs         ProviderSpec and the Claude Code, Codex and Gemini CLI definitions
  overlay.rs           builds and refreshes overlay directories
  launch.rs            builds the CLI's environment and runs it
  store.rs             config.json, accounts.json, account and provider lookup
  sessions.rs          reads each CLI's session files, builds handoffs
  secrets.rs           token storage (Keychain on macOS, 0600 file elsewhere)
  update.rs            background update check
  paths.rs, fsx.rs     amux's paths, and file helpers that keep files private
  util.rs, time.rs     argument parsing, tables, timestamps
tests/cli.rs           end-to-end tests
npm/                   the npm package: a small launcher plus the prebuilt binaries
scripts/               install.sh and the Homebrew formula generator
docs/                  architecture and provider reference
```

[docs/architecture.md](docs/architecture.md) explains how these pieces fit together.

## Adding a provider

Most CLIs need only a definition, with no new code:

1. Find out how the CLI moves its home directory (an environment variable such as `FOO_HOME`), which files in that home hold the login, and where it writes sessions. The CLI's docs or source are the authority. Don't guess file names.
2. Try the definition in your sandbox's `~/.amux/config.json` first, sign in to two accounts, and check that each one keeps its own login. Field reference: [docs/providers.md](docs/providers.md).
3. To ship it as a built-in, add it to `src/providers.rs`, add a row to the table in the README, and add a test.

Reading a new session format (for `amux sessions` and handoff) needs a parser in `src/sessions.rs` and a fixture-based test next to it.

## Code guidelines

- **Few dependencies.** amux uses the standard library plus a handful of well-known crates (`serde_json`, `anyhow`, `ureq`, `url`, `sha2`). Discuss any new crate in an issue first: each one adds build time, binary size and supply-chain risk.
- **Never delete or overwrite user data you didn't create.** Overlay code may remove only symlinks that amux made. Moving or deleting real files needs an explicit flag (like `--adopt` or `--purge`).
- **Keep secrets out of output and logs.** Don't print tokens, don't write them anywhere except `secrets.rs`, and create private files with mode `0600`.
- **Respect organization policy.** amux won't accept changes that turn off, hide from or work around settings an organization manages. Opt-outs must be ones the provider documents.
- **Error messages say what to do next.** For example: ``No account "claude@x". Add it with `amux account add claude x`.``
- Run `cargo fmt` and keep `cargo clippy` clean. Return errors with `anyhow` rather than panicking. Comments explain *why*, not *what*.

## Commits and pull requests

1. Create a branch from `main`.
2. Keep each PR to one change. A refactor and a feature are easier to review as two PRs.
3. Make sure `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings` and `cargo test` pass.
4. Update the README or `docs/` if behavior or commands change.
5. Add a line under `## [Unreleased]` in [CHANGELOG.md](CHANGELOG.md) for anything users will notice.
6. Write commit messages in the imperative mood, with a short subject line. For example: `Keep remote-settings sidecars private per account`.

The PR template has a short checklist. CI must pass before a merge.

## Releasing (maintainers)

1. Move the `## [Unreleased]` entries in `CHANGELOG.md` into a new `## [x.y.z] - YYYY-MM-DD` section.
2. Set the version to `x.y.z` in both `Cargo.toml` and `npm/package.json`, then run `cargo check` to update `Cargo.lock`.
3. Commit with the message `Release vx.y.z`, then tag and push:
   ```sh
   git tag vx.y.z
   git push origin main vx.y.z
   ```
4. The [release workflow](.github/workflows/release.yml) then:
   - checks that the tag, `Cargo.toml` and `npm/package.json` agree and that the changelog has the section, and runs the tests,
   - builds static binaries for macOS (arm64, x86_64) and Linux (arm64, x86_64, musl), each as `amux-<target>.tar.gz` with a `.sha256` file,
   - builds the npm package with all four binaries as `amux.tgz`, and checks that it installs and runs,
   - publishes a GitHub release with the archives, `amux.tgz` and `install.sh`, using that version's changelog section as the notes,
   - regenerates the formula in [quynb93/homebrew-tap](https://github.com/quynb93/homebrew-tap) with `scripts/homebrew-formula.sh` (needs the `TAP_DEPLOY_KEY` secret),
   - publishes `amux.tgz` to npm as `@quynb/amux`, with provenance (needs the `NPM_TOKEN` secret).

   If a secret is missing, its step is skipped with a notice, and the GitHub release still goes out. Installed copies of amux see the new version within a day, through the update check.

To try the build and packaging without publishing anything, run the workflow by hand from the Actions tab (or `gh workflow run release.yml --ref <branch>`). The packaged files are attached to the run as the `release` artifact.

### Release secrets

| Secret | What it is | How to create it |
|---|---|---|
| `TAP_DEPLOY_KEY` | Private half of an SSH key that can push to `quynb93/homebrew-tap` and nothing else | `ssh-keygen -t ed25519 -N "" -f tap_key`, add `tap_key.pub` as a deploy key with write access on the tap repo, then `gh secret set TAP_DEPLOY_KEY -R quynb93/amux < tap_key`, and delete both files. |
| `NPM_TOKEN` | npm granular access token with read and write access to `@quynb/amux` | On npmjs.com: Access Tokens → Generate New Token → Granular. Then `gh secret set NPM_TOKEN -R quynb93/amux`. |

## Code of conduct

Be respectful and constructive. Harassment and personal attacks are not tolerated in issues, PRs or discussions. Maintainers may remove content or block people who don't follow this.
