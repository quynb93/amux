import fs from "node:fs";
import path from "node:path";
import { fillArgs, run } from "../launch.js";
import { HANDOFF_DIR, tildify } from "../paths.js";
import { buildHandoff, listSessions, readTurns, type SessionInfo } from "../sessions.js";
import { accountId, getProvider, loadProviders, resolveAccount } from "../store.js";
import { ago, need, parseArgs, table } from "../util.js";

function allSessions(providerFilter?: string): SessionInfo[] {
  const out: SessionInfo[] = [];
  for (const spec of loadProviders().values()) {
    if (providerFilter && spec.id !== providerFilter) continue;
    out.push(...listSessions(spec));
  }
  return out.sort((a, b) => b.updatedAt.getTime() - a.updatedAt.getTime());
}

export function sessionsCmd(argv: string[]): number {
  const { pos, flags } = parseArgs(argv, { here: "bool", n: "value", limit: "value" });
  let list = allSessions(pos[0]);
  if (flags.here) list = list.filter((s) => s.cwd === process.cwd());
  const limit = Number(flags.n ?? flags.limit ?? 20);
  if (!Number.isInteger(limit) || limit < 1) throw new Error("-n must be a positive whole number.");
  const rows = [["PROVIDER", "SESSION", "UPDATED", "DIR", "FIRST PROMPT"]];
  for (const s of list.slice(0, limit)) {
    rows.push([s.provider, s.id, ago(s.updatedAt), s.cwd ? tildify(s.cwd) : "?", s.title]);
  }
  console.log(rows.length > 1 ? table(rows) : "No sessions found.");
  if (list.length > limit) console.log(`… ${list.length - limit} more (use -n)`);
  return 0;
}

function findSession(idPrefix: string): SessionInfo {
  const matches = allSessions().filter((s) => s.id.startsWith(idPrefix));
  if (!matches.length) throw new Error(`No session matches "${idPrefix}".`);
  if (matches.length > 1) throw new Error(`"${idPrefix}" matches ${matches.length} sessions; use more characters.`);
  return matches[0];
}

export async function resumeCmd(argv: string[]): Promise<number> {
  const { pos, flags } = parseArgs(argv, { as: "value" });
  const session = findSession(need(pos[0], "amux resume <session-id> [--as <provider>@<name>]"));
  const acct = resolveAccount(typeof flags.as === "string" && flags.as ? flags.as : session.provider);
  const target = getProvider(acct.provider);
  const cwd = session.cwd && fs.existsSync(session.cwd) ? session.cwd : process.cwd();
  const extra = pos.slice(1);

  if (acct.provider === session.provider) {
    if (!target.resumeArgs) throw new Error(`${target.label} has no resume command configured.`);
    console.error(`amux: resuming ${session.provider} session ${session.id} as ${accountId(acct)} in ${tildify(cwd)}`);
    return run(acct, [...fillArgs(target.resumeArgs, { id: session.id }), ...extra], { cwd });
  }

  if (!target.promptArgs) throw new Error(`${target.label} cannot start with an initial prompt.`);
  const source = getProvider(session.provider);
  const handoff = buildHandoff(session, readTurns(session, source.sessions));
  fs.mkdirSync(HANDOFF_DIR, { recursive: true, mode: 0o700 });
  // Session ids come from transcript contents; keep them from escaping the handoff dir.
  const safe = (s: string) => s.replace(/[^\w.-]/g, "_");
  const file = path.join(HANDOFF_DIR, `${safe(session.provider)}-${safe(session.id)}-to-${safe(acct.provider)}.md`);
  fs.writeFileSync(file, handoff, { mode: 0o600 });
  console.error(`amux: handing ${session.provider} session ${session.id} to ${accountId(acct)} (${tildify(file)})`);
  const prompt = `Read the handoff file ${file} first. It contains the transcript of a previous ${source.label} session in this directory. Then continue that work.`;
  return run(acct, [...fillArgs(target.promptArgs, { prompt }), ...extra], { cwd });
}
