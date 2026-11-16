import fs from "node:fs";
import { capture, run } from "../launch.js";
import { syncOverlay } from "../overlay.js";
import { tildify } from "../paths.js";
import { seedOverlay } from "../providers/builtin.js";
import type { ProviderSpec } from "../providers/types.js";
import { deleteToken, setToken } from "../secrets.js";
import {
  accountDir,
  accountId,
  ensureAmuxHome,
  getProvider,
  loadAccounts,
  loadConfig,
  loadProviders,
  overlayHome,
  resolveAccount,
  saveAccounts,
  saveConfig,
  type Account,
} from "../store.js";
import { need, parseArgs, readSecret, table } from "../util.js";

const NAME_RE = /^[a-z0-9][a-z0-9._-]*$/i;

/** Validates a gateway URL for `spec` and normalizes it (no trailing slash). */
export function validateBaseUrl(url: string, spec: Pick<ProviderSpec, "id" | "label" | "baseUrlEnv">): string {
  if (!spec.baseUrlEnv) {
    throw new Error(`${spec.label} has no base-URL env var configured. Add "baseUrlEnv" for "${spec.id}" in ~/.amux/config.json.`);
  }
  let parsed: URL;
  try {
    parsed = new URL(url);
  } catch {
    throw new Error(`"${url}" is not a valid URL (expected e.g. https://gateway.example.com).`);
  }
  if (parsed.protocol !== "https:" && parsed.protocol !== "http:") throw new Error("Base URL must use http or https.");
  return parsed.toString().replace(/\/$/, "");
}

/** Applies `change` to one stored account and saves the list. */
function updateAccount(acct: Account, change: (a: Account) => Account): void {
  saveAccounts(loadAccounts().map((a) => (accountId(a) === accountId(acct) ? change(a) : a)));
}

async function add(argv: string[]): Promise<number> {
  const usage = "amux account add <provider> <name> [--token] [--privacy] [--base-url <url>] [--no-login] [--note <text>]";
  const { pos, flags } = parseArgs(argv, {
    token: "bool",
    "no-login": "bool",
    privacy: "bool",
    note: "value",
    "base-url": "value",
  });
  const providerId = need(pos[0], usage);
  const name = need(pos[1], usage);
  if (!NAME_RE.test(name)) throw new Error("Account names may use letters, digits, '.', '_' and '-'.");
  const spec = getProvider(providerId);
  if (flags.privacy && !spec.privacyEnv) throw new Error(`${spec.label} has no privacy settings configured.`);
  const baseUrl = typeof flags["base-url"] === "string" ? validateBaseUrl(flags["base-url"], spec) : undefined;
  if (flags.token && !spec.tokenEnv) throw new Error(`${spec.label} has no token env var configured.`);
  const accounts = loadAccounts();
  if (accounts.some((a) => a.provider === providerId && a.name === name)) throw new Error(`${providerId}@${name} already exists.`);

  const acct: Account = {
    provider: providerId,
    name,
    auth: flags.token ? "token" : "login",
    ...(flags.privacy ? { privacy: true } : {}),
    ...(baseUrl ? { baseUrl } : {}),
    createdAt: new Date().toISOString(),
    ...(typeof flags.note === "string" ? { note: flags.note } : {}),
  };
  ensureAmuxHome();
  const home = overlayHome(spec, acct);
  syncOverlay(spec, home);
  seedOverlay(spec, spec.sharedHome, home);
  saveAccounts([...accounts, acct]);
  console.log(`Created ${accountId(acct)} → ${tildify(home)}`);

  if (acct.auth === "token") {
    const hint = providerId === "claude" ? " (create one with `claude setup-token`)" : "";
    setToken(acct, await readSecret(`Paste ${spec.tokenEnv} for ${accountId(acct)}${hint}: `));
    console.log(`Stored token for ${accountId(acct)}.`);
  } else if (!flags["no-login"]) {
    if (spec.loginArgs) {
      console.log(`Signing in: ${spec.bin} ${spec.loginArgs.join(" ")}`);
      return run(acct, spec.loginArgs);
    }
    console.log(`${spec.label} signs in on first run: amux run ${accountId(acct)}`);
  }
  return 0;
}

function list(): number {
  const accounts = loadAccounts();
  if (!accounts.length) {
    console.log("No accounts yet. Add one with: amux account add <provider> <name>");
    return 0;
  }
  const defaults = loadConfig().defaults ?? {};
  const providers = loadProviders();
  const rows = [["ACCOUNT", "AUTH", "PRIVACY", "BASE URL", "DEFAULT", "HOME", "NOTE"]];
  for (const a of accounts) {
    const spec = providers.get(a.provider);
    rows.push([
      accountId(a),
      a.auth,
      a.privacy ? "on" : "",
      a.baseUrl ?? "",
      defaults[a.provider] === a.name ? "*" : "",
      spec ? tildify(overlayHome(spec, a)) : "(unknown provider)",
      a.note ?? "",
    ]);
  }
  console.log(table(rows));
  return 0;
}

function setDefault(argv: string[]): number {
  const acct = resolveAccount(need(argv[0], "amux account default <provider>@<name>"));
  const cfg = loadConfig();
  cfg.defaults = { ...cfg.defaults, [acct.provider]: acct.name };
  saveConfig(cfg);
  console.log(`Default ${acct.provider} account: ${acct.name}`);
  return 0;
}

async function token(argv: string[]): Promise<number> {
  const acct = resolveAccount(need(argv[0], "amux account token <provider>@<name>"));
  const spec = getProvider(acct.provider);
  if (!spec.tokenEnv) throw new Error(`${spec.label} has no token env var configured.`);
  setToken(acct, await readSecret(`Paste ${spec.tokenEnv} for ${accountId(acct)}: `));
  if (acct.auth !== "token") updateAccount(acct, (a) => ({ ...a, auth: "token" }));
  console.log(`Stored token for ${accountId(acct)}.`);
  return 0;
}

function baseUrl(argv: string[]): number {
  const usage = "amux account baseurl <provider>@<name> <url|off>";
  const acct = resolveAccount(need(argv[0], usage));
  const value = need(argv[1], usage);
  const spec = getProvider(acct.provider);
  const url = ["off", "none", "-"].includes(value) ? undefined : validateBaseUrl(value, spec);
  updateAccount(acct, ({ baseUrl: _old, ...rest }) => (url ? { ...rest, baseUrl: url } : rest));
  if (url) {
    console.log(`Base URL for ${accountId(acct)}: ${url}`);
    console.log(`  Launches set ${spec.baseUrlEnv}=${url}`);
    console.log("Point this only at an endpoint you are allowed to use. Organization-managed settings are unaffected.");
  } else {
    console.log(`Base URL cleared for ${accountId(acct)}; it uses the provider default.`);
  }
  return 0;
}

function privacy(argv: string[]): number {
  const usage = "amux account privacy <provider>@<name> on|off";
  const acct = resolveAccount(need(argv[0], usage));
  const mode = need(argv[1], usage);
  if (mode !== "on" && mode !== "off") throw new Error(`Usage: ${usage}`);
  const spec = getProvider(acct.provider);
  if (mode === "on" && !spec.privacyEnv) throw new Error(`${spec.label} has no privacy settings configured.`);
  updateAccount(acct, (a) => ({ ...a, privacy: mode === "on" }));
  console.log(`Privacy ${mode} for ${accountId(acct)}.`);
  if (mode === "on") {
    for (const [k, v] of Object.entries(spec.privacyEnv ?? {})) console.log(`  ${k}=${v}`);
    console.log("Organization-managed settings still take precedence over these.");
  }
  return 0;
}

function status(argv: string[]): number {
  const targets = argv[0] ? [resolveAccount(argv[0])] : loadAccounts();
  for (const acct of targets) {
    const spec = getProvider(acct.provider);
    if (!spec.statusArgs) {
      console.log(`${accountId(acct)}: ${spec.label} has no status command`);
      continue;
    }
    const { out } = capture(acct, spec.statusArgs);
    console.log(`${accountId(acct)}:\n  ${out.split("\n").join("\n  ")}`);
  }
  return 0;
}

function remove(argv: string[]): number {
  const { pos, flags } = parseArgs(argv, { purge: "bool" });
  const acct = resolveAccount(need(pos[0], "amux account rm <provider>@<name> [--purge]"));
  saveAccounts(loadAccounts().filter((a) => accountId(a) !== accountId(acct)));
  const cfg = loadConfig();
  if (cfg.defaults?.[acct.provider] === acct.name) {
    delete cfg.defaults[acct.provider];
    saveConfig(cfg);
  }
  deleteToken(acct);
  const dir = accountDir(acct);
  if (flags.purge) {
    // rmSync unlinks symlinks without following them, so the shared home is untouched.
    fs.rmSync(dir, { recursive: true, force: true });
    console.log(`Removed ${accountId(acct)} and ${tildify(dir)}.`);
    if (acct.provider === "claude") {
      console.log('Its Claude Code Keychain login ("Claude Code-credentials-…") is left in place; run `claude auth logout` first to clear it.');
    }
  } else {
    console.log(`Removed ${accountId(acct)}. Its private files remain in ${tildify(dir)} (use --purge to delete).`);
  }
  return 0;
}

export async function accountCmd(argv: string[]): Promise<number> {
  const [sub, ...args] = argv;
  switch (sub) {
    case "add":
      return add(args);
    case "list":
    case "ls":
    case undefined:
      return list();
    case "default":
      return setDefault(args);
    case "token":
      return token(args);
    case "status":
      return status(args);
    case "privacy":
      return privacy(args);
    case "baseurl":
    case "base-url":
      return baseUrl(args);
    case "rm":
    case "remove":
      return remove(args);
    default:
      throw new Error(`Unknown subcommand "account ${sub}". See \`amux help\`.`);
  }
}
