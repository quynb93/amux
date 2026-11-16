import fs from "node:fs";
import path from "node:path";
import { ACCOUNTS_DIR, ACCOUNTS_FILE, AMUX_HOME, CONFIG_FILE, expandHome } from "./paths.js";
import { BUILTIN_PROVIDERS } from "./providers/builtin.js";
import type { ProviderSpec } from "./providers/types.js";

export interface Config {
  /** Extra providers, or partial overrides of built-ins matched by id. */
  providers?: Array<Partial<ProviderSpec> & { id: string }>;
  /** Default account per provider id, e.g. { "claude": "work" }. */
  defaults?: Record<string, string>;
}

export interface Account {
  provider: string;
  name: string;
  /** "login": the CLI's own sign-in inside the overlay. "token": a stored secret injected as the provider's tokenEnv. */
  auth: "login" | "token";
  /** Applies the provider's privacyEnv on every launch. */
  privacy?: boolean;
  /** Overrides the provider's API endpoint via its baseUrlEnv on every launch (a gateway / Bedrock / Vertex / relay). */
  baseUrl?: string;
  createdAt: string;
  note?: string;
}

export const accountId = (a: Pick<Account, "provider" | "name">) => `${a.provider}@${a.name}`;

function readJson<T>(file: string, fallback: T): T {
  try {
    return JSON.parse(fs.readFileSync(file, "utf8")) as T;
  } catch (err) {
    if ((err as NodeJS.ErrnoException).code === "ENOENT") return fallback;
    throw new Error(`Cannot parse ${file}: ${(err as Error).message}`);
  }
}

function writeJson(file: string, data: unknown): void {
  fs.mkdirSync(path.dirname(file), { recursive: true, mode: 0o700 });
  const tmp = `${file}.${process.pid}.tmp`;
  fs.writeFileSync(tmp, JSON.stringify(data, null, 2) + "\n", { mode: 0o600 });
  fs.renameSync(tmp, file);
}

export function loadConfig(): Config {
  return readJson<Config>(CONFIG_FILE, {});
}

export function saveConfig(cfg: Config): void {
  writeJson(CONFIG_FILE, cfg);
}

export function loadProviders(cfg = loadConfig()): Map<string, ProviderSpec> {
  const map = new Map<string, ProviderSpec>(BUILTIN_PROVIDERS.map((p) => [p.id, { ...p }]));
  for (const custom of cfg.providers ?? []) {
    const base = map.get(custom.id);
    const merged = { ...base, ...custom } as ProviderSpec;
    const missing = (["bin", "sharedHome", "homeEnv"] as const).filter((k) => !merged[k]);
    if (missing.length) throw new Error(`Provider "${custom.id}" in ${CONFIG_FILE} is missing: ${missing.join(", ")}`);
    merged.label ??= custom.id;
    merged.homeEnvPoints ??= "home";
    merged.privateEntries ??= [];
    merged.sharedDirs ??= [];
    map.set(custom.id, merged);
  }
  for (const p of map.values()) p.sharedHome = expandHome(p.sharedHome);
  return map;
}

export function getProvider(id: string): ProviderSpec {
  const p = loadProviders().get(id);
  if (!p) throw new Error(`Unknown provider "${id}". Run \`amux providers\` to see the list.`);
  return p;
}

export function loadAccounts(): Account[] {
  return readJson<{ accounts: Account[] }>(ACCOUNTS_FILE, { accounts: [] }).accounts;
}

export function saveAccounts(accounts: Account[]): void {
  writeJson(ACCOUNTS_FILE, { accounts });
}

/** Parses "claude@work", or "claude" (uses the provider's default account). */
export function resolveAccount(ref: string): Account {
  const accounts = loadAccounts();
  let [provider, name] = ref.split("@");
  if (!name) {
    name = loadConfig().defaults?.[provider] ?? "";
    if (!name) {
      const own = accounts.filter((a) => a.provider === provider);
      if (own.length === 1) name = own[0].name;
      else throw new Error(`"${ref}" is ambiguous. Use ${provider}@<name> or set a default with \`amux account default ${provider}@<name>\`.`);
    }
  }
  const acct = accounts.find((a) => a.provider === provider && a.name === name);
  if (!acct) throw new Error(`No account "${provider}@${name}". Add it with \`amux account add ${provider} ${name}\`.`);
  return acct;
}

/** amux's per-account dir: ~/.amux/accounts/<provider>/<name> */
export function accountDir(a: Pick<Account, "provider" | "name">): string {
  return path.join(ACCOUNTS_DIR, a.provider, a.name);
}

/** The relocated home dir the CLI sees for this account, e.g. ~/.amux/accounts/claude/work/.claude */
export function overlayHome(spec: ProviderSpec, a: Pick<Account, "provider" | "name">): string {
  return path.join(accountDir(a), path.basename(spec.sharedHome));
}

/** Value to put in spec.homeEnv. */
export function homeEnvValue(spec: ProviderSpec, a: Pick<Account, "provider" | "name">): string {
  return spec.homeEnvPoints === "parent" ? accountDir(a) : overlayHome(spec, a);
}

export function ensureAmuxHome(): void {
  fs.mkdirSync(AMUX_HOME, { recursive: true, mode: 0o700 });
}
