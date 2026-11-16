## What and why

<!-- What does this change, and why? Link the issue if there is one (Fixes #123). -->

## How I tested it

<!-- Commands you ran, ideally in a sandbox (HOME/AMUX_HOME set to a temp dir). -->

## Checklist

- [ ] `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings` and `cargo test` pass
- [ ] Added or updated tests
- [ ] Updated the README or `docs/` if commands or behavior changed
- [ ] Added a line under `## [Unreleased]` in CHANGELOG.md for anything users will notice
- [ ] Doesn't print, log or store secrets outside `secrets.rs`, and doesn't delete user data without an explicit flag
