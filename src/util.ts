import { spawnSync } from "node:child_process";

export type Flags = Record<string, string | boolean>;

/**
 * Splits known flags (`--x`, `--x value`, `--x=value`) from positionals. Unknown
 * flags stay positional so they pass through to the provider CLI untouched.
 */
export function parseArgs(argv: string[], known: Record<string, "bool" | "value">): { pos: string[]; flags: Flags } {
  const pos: string[] = [];
  const flags: Flags = {};
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    let name: string | null = null;
    let inline: string | undefined;
    if (a.startsWith("--")) {
      const eq = a.indexOf("=");
      name = eq === -1 ? a.slice(2) : a.slice(2, eq);
      if (eq !== -1) inline = a.slice(eq + 1);
    } else if (a === "-n") {
      name = "n";
    }
    if (name && known[name]) {
      flags[name] = known[name] === "value" ? (inline ?? argv[++i] ?? "") : true;
    } else {
      pos.push(a);
    }
  }
  return { pos, flags };
}

/** Returns `v`, or throws a usage error when it is missing. */
export function need<T>(v: T | undefined, usage: string): T {
  if (v === undefined || v === "") throw new Error(`Usage: ${usage}`);
  return v;
}

/** Left-aligned columns; the last column is not padded. */
export function table(rows: string[][]): string {
  const widths = rows[0].map((_, i) => Math.max(...rows.map((r) => r[i].length)));
  return rows.map((r) => r.map((c, i) => (i === r.length - 1 ? c : c.padEnd(widths[i]))).join("  ")).join("\n");
}

export function ago(d: Date, now = Date.now()): string {
  const s = (now - d.getTime()) / 1000;
  if (s < 3600) return `${Math.max(1, Math.round(s / 60))}m ago`;
  if (s < 86400) return `${Math.round(s / 3600)}h ago`;
  return `${Math.round(s / 86400)}d ago`;
}

/** Single-quotes a string for POSIX shells. */
export const shQuote = (s: string) => `'${s.replace(/'/g, `'\\''`)}'`;

/** Reads a secret from stdin when piped, or from the terminal with echo off. */
export async function readSecret(prompt: string): Promise<string> {
  if (!process.stdin.isTTY) {
    const chunks: Buffer[] = [];
    for await (const c of process.stdin) chunks.push(c as Buffer);
    return Buffer.concat(chunks).toString("utf8").trim();
  }
  process.stderr.write(prompt);
  spawnSync("stty", ["-echo"], { stdio: "inherit" });
  // Ctrl-C would exit before `finally` runs and leave the terminal with echo off.
  const onInt = () => {
    spawnSync("stty", ["echo"], { stdio: "inherit" });
    process.stderr.write("\n");
    process.exit(130);
  };
  process.once("SIGINT", onInt);
  try {
    const value = await new Promise<string>((resolve) => {
      process.stdin.once("data", (d) => resolve(d.toString().trim()));
    });
    process.stdin.pause();
    return value;
  } finally {
    process.off("SIGINT", onInt);
    spawnSync("stty", ["echo"], { stdio: "inherit" });
    process.stderr.write("\n");
  }
}
