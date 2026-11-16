import { spawn, spawnSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import { LAUNCH_LOG } from "./paths.js";
import { syncOverlay } from "./overlay.js";
import { getToken } from "./secrets.js";
import { accountId, ensureAmuxHome, getProvider, homeEnvValue, overlayHome, type Account } from "./store.js";
import type { ProviderSpec } from "./providers/types.js";

/** Env vars that would point the CLI at another account if inherited from the shell. */
const CONFLICTING_ENV: Record<string, string[]> = {
  claude: ["CLAUDE_CONFIG_DIR", "CLAUDE_CODE_OAUTH_TOKEN"],
  codex: ["CODEX_HOME"],
  gemini: ["GEMINI_CLI_HOME"],
};

export function buildEnv(spec: ProviderSpec, acct: Account): NodeJS.ProcessEnv {
  const env: NodeJS.ProcessEnv = { ...process.env };
  for (const k of CONFLICTING_ENV[spec.id] ?? []) delete env[k];
  env[spec.homeEnv] = homeEnvValue(spec, acct);
  env.AMUX_ACCOUNT = accountId(acct);
  if (acct.privacy) Object.assign(env, spec.privacyEnv);
  if (acct.baseUrl) {
    if (!spec.baseUrlEnv) throw new Error(`Provider "${spec.id}" has no base-URL env var configured; set "baseUrlEnv" in ~/.amux/config.json.`);
    env[spec.baseUrlEnv] = acct.baseUrl;
  }
  if (acct.auth === "token") {
    if (!spec.tokenEnv) throw new Error(`Provider "${spec.id}" has no tokenEnv; use login auth instead.`);
    const token = getToken(acct);
    if (!token) throw new Error(`No stored token for ${accountId(acct)}. Run \`amux account token ${accountId(acct)}\`.`);
    env[spec.tokenEnv] = token;
  }
  return env;
}

/** Prepares the overlay and returns the env to run the provider CLI as `acct`. */
export function prepare(acct: Account): { spec: ProviderSpec; env: NodeJS.ProcessEnv } {
  const spec = getProvider(acct.provider);
  ensureAmuxHome();
  const report = syncOverlay(spec, overlayHome(spec, acct));
  if (report.stray.length) {
    process.stderr.write(
      `amux: ${accountId(acct)} has unshared entries: ${report.stray.join(", ")} (run \`amux sync ${accountId(acct)} --adopt\`)\n`,
    );
  }
  return { spec, env: buildEnv(spec, acct) };
}

function logLaunch(acct: Account, args: string[], cwd: string): void {
  try {
    // Args can contain prompts, so the log is readable by the owner only.
    fs.appendFileSync(LAUNCH_LOG, JSON.stringify({ ts: new Date().toISOString(), account: accountId(acct), cwd, args }) + "\n", {
      mode: 0o600,
    });
    fs.chmodSync(LAUNCH_LOG, 0o600);
  } catch {
    // Logging is best-effort.
  }
}

/** Runs the provider CLI in the foreground and resolves with its exit code. */
export function run(acct: Account, args: string[], opts: { cwd?: string } = {}): Promise<number> {
  const { spec, env } = prepare(acct);
  const cwd = opts.cwd ?? process.cwd();
  logLaunch(acct, args, cwd);
  return new Promise((resolve, reject) => {
    const child = spawn(spec.bin, args, { stdio: "inherit", env, cwd });
    // The child owns the terminal; let it handle Ctrl-C itself.
    const ignore = () => {};
    process.on("SIGINT", ignore);
    child.on("error", (err) => {
      process.off("SIGINT", ignore);
      reject((err as NodeJS.ErrnoException).code === "ENOENT" ? new Error(`"${spec.bin}" not found on PATH.`) : err);
    });
    child.on("exit", (code, signal) => {
      process.off("SIGINT", ignore);
      resolve(code ?? 128 + (signal ? (os.constants.signals[signal] ?? 1) : 1));
    });
  });
}

/** Runs the provider CLI and captures output (for status checks). */
export function capture(acct: Account, args: string[]): { status: number; out: string } {
  const { spec, env } = prepare(acct);
  const r = spawnSync(spec.bin, args, { env, encoding: "utf8", timeout: 20_000 });
  return { status: r.status ?? 1, out: `${r.stdout ?? ""}${r.stderr ?? ""}`.trim() };
}

export function fillArgs(template: string[], vars: Record<string, string>): string[] {
  return template.map((a) => a.replace(/\{(\w+)\}/g, (_, k) => vars[k] ?? ""));
}
