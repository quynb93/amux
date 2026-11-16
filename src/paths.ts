import os from "node:os";
import path from "node:path";

export const HOME = os.homedir();

/** Root of amux's own state. Override with AMUX_HOME. */
export const AMUX_HOME = process.env.AMUX_HOME
  ? path.resolve(process.env.AMUX_HOME)
  : path.join(HOME, ".amux");

export const CONFIG_FILE = path.join(AMUX_HOME, "config.json");
export const ACCOUNTS_FILE = path.join(AMUX_HOME, "accounts.json");
export const ACCOUNTS_DIR = path.join(AMUX_HOME, "accounts");
export const HANDOFF_DIR = path.join(AMUX_HOME, "handoffs");
export const LAUNCH_LOG = path.join(AMUX_HOME, "launches.jsonl");

export function expandHome(p: string): string {
  if (p === "~") return HOME;
  if (p.startsWith("~/")) return path.join(HOME, p.slice(2));
  return path.resolve(p);
}

export function tildify(p: string): string {
  return p.startsWith(HOME + path.sep) ? "~" + p.slice(HOME.length) : p;
}
