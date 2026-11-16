---
name: amux
description: Run, steer and check other coding agents in parallel with amux — start helpers in their own git worktrees, send them prompts, wait for them, review their diffs, run tests without overloading the machine, and open PRs. Use when a task splits into independent parts, when you need a second agent (reviewer, tester), or before running a test suite, build or dev server on a shared machine.
---

# amux: parallel agents on one machine

You may be one of several agents on this computer. `amux` starts, steers and checks them.
Your own run's name, if amux started you, is in `$AMUX_RUN`.

## Share the machine (always)

Run test suites, builds and dev servers through a slot, so agents take turns:

```sh
amux slot --timeout 100 -- 'npm test'          # waits for a free "test" slot, then runs
amux slot --class dev -- npm run dev            # dev servers have their own slots
amux slots                                      # who runs what, load and free memory
```

Exit code **75** means no slot came free in time and nothing ran. Don't retry in a loop:
commit, push and let CI run it (`amux verify $AMUX_RUN --push`), or tell the user.

## Start helpers

```sh
amux spawn --worktree --name parser-tests claude -p "add tests for src/parser.rs"
amux spawn --worktree --name review codex -p "review the diff on branch amux/parser-tests"
amux ps --json                                  # every run: state, branch, verify, PR
```

Each `--worktree` run gets its own branch `amux/<name>`, so helpers never edit your files.

## Steer them and wait

```sh
amux send parser-tests "also cover empty input"           # types it in, presses Enter
amux send parser-tests --wait "now run the tests"         # …and waits for the turn to end
amux wait parser-tests --until idle --timeout 900         # waiting | idle | exited; 124 = timeout
amux peek parser-tests                                    # its screen right now
amux events                                               # JSON lines as states change
```

A run in state `waiting` is asking something (see `reason`). Answer with `amux send`.

## Check their work

```sh
amux diff parser-tests --stat                   # what it changed since it started
amux comment parser-tests src/parser.rs:42 "this panics on empty input"
amux comments parser-tests --send               # all comments as one message to it
amux verify parser-tests                        # recommends: test here, or push for CI
amux verify parser-tests --test                 # tests in its folder, in a test slot
amux verify parser-tests --send-failures        # failing output back to it
amux overlap                                    # which helpers would conflict, before merging
amux land parser-tests review                    # rebase, test in a slot, fast-forward, in turn
amux pr parser-tests                            # push and open a PR (gh)
amux checks parser-tests --send-failures        # failing CI back to it
```

## Rules

- Prefer `--worktree` for any helper that edits files.
- Never run `amux stop`, `amux rm` or `--force` on runs you didn't start unless the user asked.
- Keep helpers few (2–4). `amux slots` shows whether the machine has room.
- On another machine: prefix the run with the machine, e.g. `amux send gpu:parser-tests …`.
