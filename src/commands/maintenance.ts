import { spawnSync } from "node:child_process";
import fs from "node:fs";
import { conflictingEnv, prepare } from "../launch.js";
import { adoptStray, syncOverlay } from "../overlay.js";
import { tildify } from "../paths.js";
import { accountId, getProvider, homeEnvValue, loadAccounts, loadProviders, overlayHome, resolveAccount } from "../store.js";
import { need, parseArgs, shQuote, table } from "../util.js";

export function providersCmd(): number {
  const rows = [["PROVIDER", "NAME", "SHARED HOME", "HOME ENV", "PRIVATE PER ACCOUNT"]];
  for (const p of loadProviders().values()) {
    rows.push([p.id, p.label, tildify(p.sharedHome), p.homeEnv, p.privateEntries.join(", ")]);
  }
  console.log(table(rows));
  return 0;
}

export function envCmd(argv: string[]): number {
  const acct = resolveAccount(need(argv[0], "amux env <provider>@<name>"));
  const { spec, env } = prepare(acct);
  const keys = [
    spec.homeEnv,
    "AMUX_ACCOUNT",
    ...(acct.privacy ? Object.keys(spec.privacyEnv ?? {}) : []),
    ...(acct.auth === "token" && spec.tokenEnv ? [spec.tokenEnv] : []),
    ...(acct.baseUrl && spec.baseUrlEnv ? [spec.baseUrlEnv] : []),
  ];
  for (const k of keys) console.log(`export ${k}=${shQuote(env[k] ?? "")}`);
  // Clear vars that would otherwise keep pointing the eval'ing shell at another account.
  for (const k of conflictingEnv(spec)) if (env[k] === undefined) console.log(`unset ${k}`);
  return 0;
}

export function syncCmd(argv: string[]): number {
  const { pos, flags } = parseArgs(argv, { adopt: "bool" });
  const targets = pos[0] ? [resolveAccount(pos[0])] : loadAccounts();
  for (const acct of targets) {
    const spec = getProvider(acct.provider);
    const home = overlayHome(spec, acct);
    const r = syncOverlay(spec, home);
    const parts = [`${r.linked.length} linked`, `${r.pruned.length} pruned`];
    if (r.stray.length && flags.adopt) {
      const { adopted, conflicts } = adoptStray(spec, home, r.stray);
      if (adopted.length) parts.push(`adopted: ${adopted.join(", ")}`);
      if (conflicts.length) parts.push(`exists in both (merge by hand): ${conflicts.join(", ")}`);
    } else if (r.stray.length) {
      parts.push(`unshared: ${r.stray.join(", ")}`);
    }
    console.log(`${accountId(acct)}: ${parts.join("; ")}`);
  }
  return 0;
}

export function doctorCmd(): number {
  let problems = 0;
  const check = (ok: boolean, msg: string) => {
    console.log(`${ok ? "ok  " : "FAIL"} ${msg}`);
    if (!ok) problems++;
  };
  const providers = loadProviders();
  for (const p of providers.values()) {
    const found = spawnSync("sh", ["-c", `command -v ${shQuote(p.bin)}`], { encoding: "utf8" }).stdout.trim();
    check(!!found, `${p.id}: binary "${p.bin}" ${found ? `at ${tildify(found)}` : "not found"}`);
    check(fs.existsSync(p.sharedHome), `${p.id}: shared home ${tildify(p.sharedHome)}`);
  }
  for (const a of loadAccounts()) {
    const spec = providers.get(a.provider);
    if (!spec) {
      check(false, `${accountId(a)}: provider "${a.provider}" is not defined`);
      continue;
    }
    const home = overlayHome(spec, a);
    check(fs.existsSync(home), `${accountId(a)}: overlay ${tildify(home)} (${spec.homeEnv}=${tildify(homeEnvValue(spec, a))})`);
  }
  for (const k of ["CLAUDE_CONFIG_DIR", "CODEX_HOME", "GEMINI_CLI_HOME"]) {
    const v = process.env[k];
    if (v) console.log(`note ${k} is set in this shell (${tildify(v)}); amux overrides it per account`);
  }
  return problems ? 1 : 0;
}
