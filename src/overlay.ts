import fs from "node:fs";
import path from "node:path";
import type { ProviderSpec } from "./providers/types.js";

export interface SyncReport {
  linked: string[];
  pruned: string[];
  /** Real files/dirs in the overlay that are not private: the CLI wrote them before they were shared. */
  stray: string[];
}

export function isPrivate(spec: ProviderSpec, name: string): boolean {
  return spec.privateEntries.some((pat) => (pat.endsWith("*") ? name.startsWith(pat.slice(0, -1)) : name === pat));
}

function lstatOrNull(p: string): fs.Stats | null {
  try {
    return fs.lstatSync(p);
  } catch {
    return null;
  }
}

/**
 * Makes `overlay` mirror `shared` with symlinks, except private entries.
 * Safe to run on every launch: it only adds missing links, repoints wrong
 * ones and removes links whose target is gone. It never deletes real files.
 */
export function syncOverlay(spec: ProviderSpec, overlay: string): SyncReport {
  const shared = spec.sharedHome;
  const report: SyncReport = { linked: [], pruned: [], stray: [] };

  fs.mkdirSync(shared, { recursive: true });
  fs.mkdirSync(overlay, { recursive: true, mode: 0o700 });
  for (const dir of spec.sharedDirs) fs.mkdirSync(path.join(shared, dir), { recursive: true });

  for (const name of fs.readdirSync(shared)) {
    if (isPrivate(spec, name)) continue;
    const target = path.join(shared, name);
    const link = path.join(overlay, name);
    const st = lstatOrNull(link);
    if (!st) {
      fs.symlinkSync(target, link);
      report.linked.push(name);
    } else if (st.isSymbolicLink()) {
      if (fs.readlinkSync(link) !== target) {
        fs.unlinkSync(link);
        fs.symlinkSync(target, link);
        report.linked.push(name);
      }
    } else {
      report.stray.push(name);
    }
  }

  for (const name of fs.readdirSync(overlay)) {
    const link = path.join(overlay, name);
    const st = lstatOrNull(link);
    if (st?.isSymbolicLink() && isPrivate(spec, name) && fs.readlinkSync(link) === path.join(shared, name)) {
      // Linked before the entry became private (e.g. a provider spec update): give the account its own copy.
      fs.unlinkSync(link);
      report.pruned.push(name);
    } else if (st?.isSymbolicLink() && !fs.existsSync(link)) {
      fs.unlinkSync(link);
      report.pruned.push(name);
    } else if (st && !st.isSymbolicLink() && !isPrivate(spec, name) && !fs.existsSync(path.join(shared, name))) {
      report.stray.push(name);
    }
  }
  return report;
}

/**
 * Moves stray overlay entries into the shared home (when the shared home has
 * no entry by that name) and replaces them with links. Entries that exist in
 * both places are left alone and returned.
 */
export function adoptStray(spec: ProviderSpec, overlay: string, names: string[]): { adopted: string[]; conflicts: string[] } {
  const adopted: string[] = [];
  const conflicts: string[] = [];
  for (const name of names) {
    const src = path.join(overlay, name);
    const dest = path.join(spec.sharedHome, name);
    if (fs.existsSync(dest)) {
      conflicts.push(name);
      continue;
    }
    fs.renameSync(src, dest);
    fs.symlinkSync(dest, src);
    adopted.push(name);
  }
  return { adopted, conflicts };
}
