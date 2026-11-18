mod commands {
    pub mod account;
    #[cfg(unix)]
    pub mod allow;
    #[cfg(unix)]
    pub mod attempts;
    #[cfg(unix)]
    pub mod autopilot;
    #[cfg(unix)]
    pub mod control;
    #[cfg(unix)]
    pub mod explain;
    #[cfg(unix)]
    pub mod gc;
    #[cfg(unix)]
    pub mod github;
    #[cfg(unix)]
    pub mod intake;
    #[cfg(unix)]
    pub mod login_item;
    pub mod maintenance;
    #[cfg(unix)]
    pub mod notes;
    #[cfg(unix)]
    pub mod overlap;
    #[cfg(unix)]
    pub mod phone;
    #[cfg(unix)]
    pub mod plugin;
    #[cfg(unix)]
    pub mod review;
    #[cfg(unix)]
    pub mod runs;
    #[cfg(unix)]
    pub mod schedule;
    pub mod sessions;
    pub mod skill;
    #[cfg(unix)]
    pub mod slot;
    #[cfg(unix)]
    pub mod team;
    #[cfg(unix)]
    pub mod theme;
    #[cfg(unix)]
    pub mod usage;
    #[cfg(unix)]
    pub mod verify;
}
mod fsx;
mod git;
mod launch;
mod overlay;
#[cfg(unix)]
mod palettes;
mod paths;
#[cfg(unix)]
mod plugins;
mod providers;
#[cfg(unix)]
mod remote;
#[cfg(unix)]
mod runs;
mod secrets;
#[cfg(unix)]
mod serve;
mod sessions;
mod store;
#[cfg(test)]
mod testutil;
mod time;
#[cfg(unix)]
mod tls;
#[cfg(unix)]
mod ui;
mod update;
mod util;
#[cfg(unix)]
mod work;

use anyhow::{bail, Result};
use commands::{account, maintenance, sessions as session_cmds};

const HELP: &str = "amux — many accounts, one shared home, for Claude Code, Codex, Gemini and more

Usage:
  amux <provider>[@name] [args...]         Run the CLI as that account (same as `amux run`)
  amux run <provider>[@name] [args...]

Accounts:
  amux account add <provider> <name> [--token] [--privacy] [--base-url <url>] [--no-login] [--note <text>]
  amux account list
  amux account default <provider>@<name>
  amux account token <provider>@<name>     Store or replace a token (read from stdin)
  amux account privacy <provider>@<name> on|off      Turn the CLI's telemetry opt-outs on or off
  amux account baseurl <provider>@<name> <url|off>   Send the CLI's API requests to a gateway
  amux account status [<provider>@<name>]  Ask each CLI who is signed in
  amux account rm <provider>@<name> [--purge]

Parallel runs (each CLI in the background, in its own terminal):
  amux spawn [--on <machine>[:<dir>]] [--stall <30m>] [--max-time <4h>] [--name <run>] [--worktree [--branch <b>] [--base <ref>]] [--attach] <provider>[@name] [args...]
      --no-auto-approve: every permission prompt waits for you, whatever `amux allow` says
      --issue <n>: work on a GitHub issue (its own worktree; the PR says it fixes it)
      --autopilot[=merge]: open the PR when it commits, hand back failing CI and review
        comments, and say when it's ready to merge (=merge: merge it)
      Start the CLI in the background. --worktree gives it its own git worktree and branch.
      --on starts it on another machine; then name it <machine>:<run> anywhere a run goes.
  amux ps [--json]                         List runs and whether each is working or idle
  amux explain <run> [--json]              Why a run shows its state, with the evidence
  amux attach <run>                        Connect this terminal to a run (Ctrl-] detaches)
  amux logs <run>                          Print everything a run has output
  amux peek <run>                          Print what is on a run's screen now
  amux send <run> [--wait] <text...> | -   Type a prompt into a run and press Enter
  amux wait <run>... [--until waiting|idle|exited] [--timeout <secs>]
      Block until the runs need you, finish their turn (default) or exit; 124 on timeout
  amux events [<run>...] [--text]          Stream state, verify and PR changes as JSON lines
  amux diff <run> [--stat]                 What a run changed since its worktree started
  amux try <n> <acct> | <acct>,<acct>... -p <task> --name <task>
      Several attempts at one task, each in its own worktree (accounts or CLIs can differ)
  amux compare <task> [--no-test]          Test each attempt in a slot and rank them
  amux pick <run> [--land]                 Keep the best attempt (and land it); clear the others
  amux lead [--workers <n>] [--worker <acct>]... <provider>[@name] -p <goal>
      A lead agent that splits the goal, gives each part to a worker in its own worktree
      (`amux spawn` from inside it), waits for them, reviews and lands their work
  amux team [<lead>] [--json] | amux team wait [<lead>]
      A team's workers: state, changes, tests and reports; wait blocks until one finishes
  amux report <text...>                    From inside a run: what it did, for its lead
  amux note <text...>                      A decision every new run on this repo starts with
  amux notes [--rm <n>]                    List (or remove) this repo's notes
  amux broadcast <text...> [--note]        Tell every run working on this repo
  amux overlap [<run>...] [--into <b>]     Which runs touch the same files, and which would conflict
  amux land <run>... [--into <b>] [--no-test] [--dry-run]
      Merge queue: rebase each run onto the branch, test it in a slot, fast-forward.
      A run that conflicts or fails is skipped and its agent told what to fix.
  amux usage [--json]                      Each account's 5-hour and 7-day usage, limits, runs
  amux usage --track on|off                Record Claude Code's usage through its status line
  amux failover <run> [--to <acct>]        Resume a run's session under another account
                                           (same worktree); automatic with limits.onLimit
  amux pr <run> [--draft] [--base <b>]     Push a run's branch and open its pull request (gh)
  amux checks <run> [--send-failures]      Its PR's CI checks; failures can go to the agent
  amux comment <run> <file>:<line> <note>  A review comment on a line the run changed
  amux comments <run> [--send | --rm <n> | --clear]
      List a run's review comments, or send the pending ones to its agent as one message
  amux review <run> [--hunks] [--drop <file>[:<n>]]... [--no-tell] [--force]
      List a run's hunks, numbered per file, or drop some from its worktree and tell
      the agent. Refuses while the agent is working (--force skips that check)
  amux stop <run> [--force]                End a run (SIGTERM, or SIGKILL with --force)
  amux rm <run>... [--force] | --done      Remove runs and their clean worktrees
  amux revive [<run>...] [--all] [--fresh] Bring back runs lost in a restart: same name, worktree
                                           and session (--resume); --at-login does it after each restart
  amux gc [--dry-run]                      End processes finished runs left behind (dev servers…)
  amux allow [<rule>... | --deny <rule> | --rm <rule> | --starter | --check <rule> | --log]
      Answer permission prompts by themselves when they ask for what the rules allow:
      \"bash: cargo test*\", \"edit\" (inside the run's folder); deny rules win
  amux schedule add <name> \"<when>\" <amux command...> | rm | run | pause | resume | log
      Run an amux command by itself: \"every 1h\", \"weekdays 9:00\", or cron (\"0 9 * * 1-5\")
  amux intake [--label <l>] [--limit <n>] <provider>[@name]
      A run for each new open GitHub issue (with the label), e.g. on a schedule
  amux autopilot [<run> [--merge] [--retries <n>] | <run> --off | <run> --log]
      Runs on autopilot and how far each has got; turn it on or off for a run
  amux ui                                  Watch, attach to, start and stop runs in one screen
  amux theme [<name>]                      List themes (Catppuccin, Tokyo Night, Dracula, Gruvbox,
                                           Nord, Solarized…) or choose one for amux ui and the phone app
  amux verify <run> [--test [--command <cmd>] | --push | --send-failures]
      Check a finished run's work: run its tests here (waiting for a test slot),
      or push its branch so CI runs them. With neither, says which is better now.

Sharing the machine (tests, builds and dev servers from many worktrees):
  amux slot [--class test|build|dev|<name>] [--timeout <secs>] [--no-wait] -- <command...>
      Run a heavy command when a slot of its class is free and the machine isn't overloaded.
      Exits 75 if no slot came free in time: try later, or push and let CI run it.
  amux slots [--hook]                      Show slots, who holds them and who waits;
                                           --hook prints a Claude Code hook that enforces them

Sessions (shared across accounts):
  amux sessions [provider] [--here] [-n <count>]
  amux resume <session-id> [--as <provider>@<name>]
      Same provider: native resume under any account.
      Other provider: writes a transcript handoff and starts a new session with it.

Maintenance:
  amux providers                           List providers and their shared homes
  amux env <provider>@<name>               Print exports, e.g. eval \"$(amux env claude@work)\"
  amux sync [<provider>@<name>] [--adopt]  Refresh overlay links; --adopt moves stray files to the shared home
  amux doctor                              Check binaries, shared homes and overlays
  amux machine add <name> <user@host> | list | rm <name> | check <name>
                                           Machines reached over SSH, with amux installed
  amux serve [--listen <host:port>] [--tls | --tailscale]
                                           amux on your phone: runs, screens, replies (web app);
                                           --tailscale serves it over HTTPS on your tailnet
  amux phone ntfy|pushover|webhook|test|off   Notifications on your phone when a run needs you,
                                           stalls, hits a limit, ends, or its tests/CI finish
  amux skill [--install]                   The agent skill for running other agents with amux
  amux plugin list | new <name> | run <name>   Plugins (`amux <name>`) and hooks (~/.amux/hooks)
  amux update [--check]                    Install the latest amux release (--check: only look)
  amux --version

Docs: https://github.com/quynb93/amux#readme
";

fn main_inner(argv: &[String]) -> Result<i32> {
    let cmd = argv.first().map(String::as_str);
    let rest = argv.get(1..).unwrap_or_default();
    match cmd {
        // Internal: the background refresh started by check_for_updates().
        Some("__update-check") => return Ok(update::internal_cmd(rest)),
        #[cfg(unix)]
        Some("__run-host") => return Ok(runs::host::main(rest)),
        // Internal: carries a remote run's socket over SSH (remote.rs).
        #[cfg(unix)]
        Some("__pipe") => return Ok(remote::pipe_main(rest)),
        // Internal: the Claude Code hook that `amux slots --hook` prints.
        // Internal: sends one phone notification (runs/push.rs).
        #[cfg(unix)]
        Some("__push") => return Ok(runs::push::main(rest)),
        // Internal: starts schedules when they're due (commands/schedule.rs).
        #[cfg(unix)]
        Some("__scheduler") => return Ok(commands::schedule::main()),
        #[cfg(unix)]
        Some("__slot-hook") => return Ok(commands::slot::hook_main()),
        // Internal: Claude Code's status line while `amux usage --track` is on.
        #[cfg(unix)]
        Some("__statusline") => return Ok(commands::usage::statusline_main()),
        Some("--version" | "-v" | "version") => {
            println!("{}", update::version());
            return Ok(0);
        }
        None | Some("help" | "-h" | "--help") => {
            println!("{HELP}");
            return Ok(0);
        }
        Some("update") => return Ok(update::update_cmd(rest)),
        _ => {}
    }

    update::check_for_updates();
    #[cfg(unix)]
    commands::schedule::ensure_running();
    let cmd = cmd.unwrap_or_default();
    // `amux <cmd> <machine>:<run>`: the same command, on that machine.
    #[cfg(unix)]
    if let Some(r) = remote::forward(cmd, rest) {
        return r;
    }
    match cmd {
        "account" | "accounts" => account::account_cmd(rest),
        "run" => {
            let r = util::need(rest.first(), "amux run <provider>[@name] [args...]")?;
            launch::run(&store::resolve_account(r)?, &rest[1..], None)
        }
        "sessions" => session_cmds::sessions_cmd(rest),
        "resume" => session_cmds::resume_cmd(rest),
        "providers" => maintenance::providers_cmd(),
        "env" => maintenance::env_cmd(rest),
        "sync" => maintenance::sync_cmd(rest),
        "doctor" => maintenance::doctor_cmd(),
        "skill" => commands::skill::skill_cmd(rest),
        #[cfg(unix)]
        "machine" | "machines" => remote::machine_cmd(rest),
        #[cfg(unix)]
        "serve" => serve::serve_cmd(rest),
        #[cfg(unix)]
        "plugin" | "plugins" => commands::plugin::plugin_cmd(rest),
        #[cfg(unix)]
        "spawn" => commands::runs::spawn_cmd(rest),
        #[cfg(unix)]
        "ps" => commands::runs::ps_cmd(rest),
        #[cfg(unix)]
        "attach" => commands::runs::attach_cmd(rest),
        #[cfg(unix)]
        "logs" => commands::runs::logs_cmd(rest),
        #[cfg(unix)]
        "peek" => commands::runs::peek_cmd(rest),
        #[cfg(unix)]
        "stop" => commands::runs::stop_cmd(rest),
        #[cfg(unix)]
        "rm" => commands::runs::rm_cmd(rest),
        #[cfg(unix)]
        "ui" => ui::ui_cmd(rest),
        #[cfg(unix)]
        "explain" => commands::explain::explain_cmd(rest),
        #[cfg(unix)]
        "slot" => commands::slot::slot_cmd(rest),
        #[cfg(unix)]
        "slots" => commands::slot::slots_cmd(rest),
        #[cfg(unix)]
        "verify" => commands::verify::verify_cmd(rest),
        #[cfg(unix)]
        "send" => commands::control::send_cmd(rest),
        #[cfg(unix)]
        "wait" => commands::control::wait_cmd(rest),
        #[cfg(unix)]
        "events" => commands::control::events_cmd(rest),
        #[cfg(unix)]
        "diff" => commands::control::diff_cmd(rest),
        #[cfg(unix)]
        "pr" => commands::github::pr_cmd(rest),
        #[cfg(unix)]
        "checks" => commands::github::checks_cmd(rest),
        #[cfg(unix)]
        "overlap" => commands::overlap::overlap_cmd(rest),
        #[cfg(unix)]
        "usage" | "limits" => commands::usage::usage_cmd(rest),
        #[cfg(unix)]
        "failover" => commands::runs::failover_cmd(rest),
        #[cfg(unix)]
        "gc" => commands::gc::gc_cmd(rest),
        #[cfg(unix)]
        "revive" => commands::runs::revive_cmd(rest),
        #[cfg(unix)]
        "phone" => commands::phone::phone_cmd(rest),
        #[cfg(unix)]
        "lead" => commands::team::lead_cmd(rest),
        #[cfg(unix)]
        "team" => commands::team::team_cmd(rest),
        #[cfg(unix)]
        "allow" => commands::allow::allow_cmd(rest),
        #[cfg(unix)]
        "autopilot" => commands::autopilot::autopilot_cmd(rest),
        #[cfg(unix)]
        "schedule" | "schedules" => commands::schedule::schedule_cmd(rest),
        #[cfg(unix)]
        "intake" => commands::intake::intake_cmd(rest),
        #[cfg(unix)]
        "report" => commands::team::report_cmd(rest),
        #[cfg(unix)]
        "theme" | "themes" => commands::theme::theme_cmd(rest),
        #[cfg(unix)]
        "note" => commands::notes::note_cmd(rest),
        #[cfg(unix)]
        "notes" => commands::notes::notes_cmd(rest),
        #[cfg(unix)]
        "broadcast" => commands::notes::broadcast_cmd(rest),
        #[cfg(unix)]
        "try" => commands::attempts::try_cmd(rest),
        #[cfg(unix)]
        "compare" => commands::attempts::compare_cmd(rest),
        #[cfg(unix)]
        "pick" => commands::attempts::pick_cmd(rest),
        #[cfg(unix)]
        "land" => commands::overlap::land_cmd(rest),
        #[cfg(unix)]
        "comment" => commands::review::comment_cmd(rest),
        #[cfg(unix)]
        "comments" => commands::review::comments_cmd(rest),
        #[cfg(unix)]
        "review" => commands::review::review_cmd(rest),
        #[cfg(not(unix))]
        "spawn" | "ps" | "attach" | "logs" | "peek" | "stop" | "rm" | "ui" | "explain" | "slot"
        | "slots" | "verify" | "send" | "wait" | "events" | "diff" | "pr" | "checks"
        | "overlap" | "land" | "comment" | "comments" | "review" | "usage" | "failover" | "gc"
        | "try" | "compare" | "pick" | "note" | "notes" | "broadcast" | "revive" | "phone"
        | "theme" | "themes" => {
            bail!("Parallel runs need macOS or Linux.")
        }
        _ => {
            // Shorthand: `amux claude@work ...`
            let provider = cmd.split('@').next().unwrap_or_default();
            if store::load_providers()?.iter().any(|p| p.id == provider) {
                return launch::run(&store::resolve_account(cmd)?, rest, None);
            }
            #[cfg(unix)]
            if let Some(p) = plugins::find(cmd) {
                let run = std::env::var("AMUX_RUN").ok();
                return plugins::run(&p, rest, run.as_deref());
            }
            bail!("Unknown command \"{cmd}\". See `amux help`.")
        }
    }
}

fn main() {
    let argv: Vec<String> = std::env::args_os()
        .skip(1)
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let code = match main_inner(&argv) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("amux: {e:#}");
            1
        }
    };
    std::process::exit(code);
}
