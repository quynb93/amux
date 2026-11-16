#!/usr/bin/env node
import { accountCmd } from "./commands/account.js";
import { doctorCmd, envCmd, providersCmd, syncCmd } from "./commands/maintenance.js";
import { resumeCmd, sessionsCmd } from "./commands/sessions.js";
import { run } from "./launch.js";
import { loadProviders, resolveAccount } from "./store.js";
import { checkForUpdates, fetchLatest, updateCmd, version } from "./update.js";
import { need } from "./util.js";

const HELP = `amux — many accounts, one shared home, for Claude Code, Codex, Gemini and more

Usage:
  amux <provider>[@name] [args...]         Run the CLI as that account (same as \`amux run\`)
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
  amux env <provider>@<name>               Print exports, e.g. eval "$(amux env claude@work)"
  amux sync [<provider>@<name>] [--adopt]  Refresh overlay links; --adopt moves stray files to the shared home
  amux doctor                              Check binaries, shared homes and overlays
  amux update                              Check for a newer amux release
  amux --version

Docs: https://github.com/quynb93/amux#readme
`;

async function main(argv: string[]): Promise<number> {
  const [cmd, ...rest] = argv;
  switch (cmd) {
    case "__update-check": // internal: the background refresh started by checkForUpdates()
      await fetchLatest();
      return 0;
    case "--version":
    case "-v":
    case "version":
      console.log(version());
      return 0;
    case undefined:
    case "help":
    case "-h":
    case "--help":
      console.log(HELP);
      return 0;
    case "update":
      return updateCmd();
  }

  checkForUpdates();
  switch (cmd) {
    case "account":
    case "accounts":
      return accountCmd(rest);
    case "run":
      return run(resolveAccount(need(rest[0], "amux run <provider>[@name] [args...]")), rest.slice(1));
    case "sessions":
      return sessionsCmd(rest);
    case "resume":
      return resumeCmd(rest);
    case "providers":
      return providersCmd();
    case "env":
      return envCmd(rest);
    case "sync":
      return syncCmd(rest);
    case "doctor":
      return doctorCmd();
    default: {
      // Shorthand: `amux claude@work ...`
      if (loadProviders().has(cmd.split("@")[0])) return run(resolveAccount(cmd), rest);
      throw new Error(`Unknown command "${cmd}". See \`amux help\`.`);
    }
  }
}

main(process.argv.slice(2)).then(
  (code) => process.exit(code),
  (err: Error) => {
    console.error(`amux: ${err.message}`);
    process.exit(1);
  },
);
