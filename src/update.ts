import { spawn } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { AMUX_HOME } from "./paths.js";

/**
 * Update check in the style of Claude Code and Codex: commands never wait on the
 * network. Each run prints a one-line notice from a cached result and refreshes
 * that cache in a detached background process at most once a day.
 */

interface PackageJson {
  version: string;
  repository?: string | { url?: string };
}

const CACHE_FILE = path.join(AMUX_HOME, "update-check.json");
const DAY_MS = 24 * 60 * 60 * 1000;

interface Cache {
  checkedAt: number;
  latest?: string;
}

let pkgCache: PackageJson | undefined;

/** amux's package.json (the one that defines the `amux` bin), found next to the compiled output: dist/ → ../package.json. */
function pkg(): PackageJson {
  if (pkgCache) return pkgCache;
  for (const rel of ["../package.json", "../../package.json"]) {
    try {
      const file = fileURLToPath(new URL(rel, import.meta.url));
      const data = JSON.parse(fs.readFileSync(file, "utf8")) as PackageJson & { bin?: Record<string, string> };
      if (data.bin?.amux) return (pkgCache = data);
    } catch {
      // Try the next location.
    }
  }
  return (pkgCache = { version: "0.0.0" });
}

export const version = () => pkg().version;

/** "owner/repo" from a package.json "repository" value such as "github:owner/repo" or a GitHub URL. */
export function parseRepo(repository: PackageJson["repository"]): string | null {
  const raw = typeof repository === "string" ? repository : (repository?.url ?? "");
  const m = raw.match(/(?:^github:|github\.com[/:])([\w.-]+\/[\w.-]+?)(?:\.git)?$/);
  return m ? m[1] : null;
}

const repo = () => parseRepo(pkg().repository);
const releasesUrl = (r: string) => `https://github.com/${r}/releases/latest`;

/** True when version `a` is newer than `b`. Compares "1.2.3" numerically; a leading "v" and any "-pre" suffix are ignored. */
export function isNewer(a: string, b: string): boolean {
  const parse = (v: string) =>
    v
      .replace(/^v/, "")
      .split("-")[0]
      .split(".")
      .map((n) => Number(n) || 0);
  const [x, y] = [parse(a), parse(b)];
  for (let i = 0; i < Math.max(x.length, y.length); i++) {
    if ((x[i] ?? 0) !== (y[i] ?? 0)) return (x[i] ?? 0) > (y[i] ?? 0);
  }
  return false;
}

function readCache(): Cache | null {
  try {
    return JSON.parse(fs.readFileSync(CACHE_FILE, "utf8")) as Cache;
  } catch {
    return null;
  }
}

function disabled(): boolean {
  return !!(process.env.AMUX_NO_UPDATE_CHECK || process.env.CI) || !process.stderr.isTTY || !repo();
}

/** Asks GitHub for the latest release tag and caches it. Returns the version without "v", or null. */
export async function fetchLatest(): Promise<string | null> {
  const r = repo();
  if (!r) return null;
  let latest: string | undefined;
  try {
    const res = await fetch(`https://api.github.com/repos/${r}/releases/latest`, {
      headers: { Accept: "application/vnd.github+json", "User-Agent": `amux/${version()}` },
      signal: AbortSignal.timeout(5000),
    });
    if (res.ok) latest = ((await res.json()) as { tag_name?: string }).tag_name?.replace(/^v/, "");
  } catch {
    // Offline or blocked: record the attempt so the next try is tomorrow, not on every run.
  }
  fs.mkdirSync(AMUX_HOME, { recursive: true, mode: 0o700 });
  fs.writeFileSync(CACHE_FILE, JSON.stringify({ checkedAt: Date.now(), latest } satisfies Cache) + "\n", { mode: 0o600 });
  return latest ?? null;
}

/** Called on every run: prints the cached notice and starts a background refresh when the cache is stale. */
export function checkForUpdates(): void {
  const r = repo();
  if (disabled() || !r) return;
  const cache = readCache();
  if (cache?.latest && isNewer(cache.latest, version())) {
    process.stderr.write(`\x1b[33mUpdate available: amux ${version()} → ${cache.latest}\x1b[0m  Run \`amux update\` or see ${releasesUrl(r)}\n`);
  }
  if (!cache || Date.now() - cache.checkedAt > DAY_MS) {
    const child = spawn(process.execPath, [fileURLToPath(new URL("./cli.js", import.meta.url)), "__update-check"], {
      detached: true,
      stdio: "ignore",
      env: { ...process.env, AMUX_NO_UPDATE_CHECK: "1" },
    });
    child.unref();
  }
}

/**
 * The command that upgrades this copy of amux, based on where it is installed:
 * Homebrew keeps formulae under a Cellar directory, and everything else gets the release package.
 */
export function upgradeCommand(installPath: string, r: string, latest: string): string {
  if (/[/\\]Cellar[/\\]amux[/\\]/.test(installPath)) return "brew upgrade amux";
  return `npm install -g https://github.com/${r}/releases/download/v${latest}/amux.tgz`;
}

/** `amux update`: checks now and prints how to upgrade. */
export async function updateCmd(): Promise<number> {
  const r = repo();
  if (!r) {
    console.log(`amux ${version()}. No GitHub repo is set in package.json "repository", so there is nothing to check.`);
    return 1;
  }
  const latest = await fetchLatest();
  if (!latest) {
    console.log(`amux ${version()}. Could not read the latest release from ${releasesUrl(r)}.`);
    return 1;
  }
  if (!isNewer(latest, version())) {
    console.log(`amux ${version()} is up to date.`);
    return 0;
  }
  console.log(`Update available: amux ${version()} → ${latest}`);
  console.log(`  ${upgradeCommand(fs.realpathSync(fileURLToPath(import.meta.url)), r, latest)}`);
  console.log(`  Release notes: ${releasesUrl(r)}`);
  return 0;
}
