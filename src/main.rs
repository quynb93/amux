mod commands {
    pub mod account;
    pub mod maintenance;
    pub mod sessions;
}
mod fsx;
mod launch;
mod overlay;
mod paths;
mod providers;
mod secrets;
mod sessions;
mod store;
#[cfg(test)]
mod testutil;
mod time;
mod update;
mod util;

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
  amux update                              Check for a newer amux release
  amux --version

Docs: https://github.com/quynb93/amux#readme
";

fn main_inner(argv: &[String]) -> Result<i32> {
    let cmd = argv.first().map(String::as_str);
    let rest = argv.get(1..).unwrap_or_default();
    match cmd {
        // Internal: the background refresh started by check_for_updates().
        Some("__update-check") => {
            if rest.first().map(String::as_str) == Some("--detach") {
                update::spawn_refresh();
            } else {
                update::fetch_latest();
            }
            return Ok(0);
        }
        Some("--version" | "-v" | "version") => {
            println!("{}", update::version());
            return Ok(0);
        }
        None | Some("help" | "-h" | "--help") => {
            println!("{HELP}");
            return Ok(0);
        }
        Some("update") => return Ok(update::update_cmd()),
        _ => {}
    }

    update::check_for_updates();
    let cmd = cmd.unwrap_or_default();
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
        _ => {
            // Shorthand: `amux claude@work ...`
            let provider = cmd.split('@').next().unwrap_or_default();
            if store::load_providers()?.iter().any(|p| p.id == provider) {
                return launch::run(&store::resolve_account(cmd)?, rest, None);
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
