import fs from "node:fs";
import os from "node:os";
import path from "node:path";

/** A fresh temp dir. amux reads HOME/AMUX_HOME at import time, so tests set them before importing src modules. */
export function sandbox(prefix = "amux-test-"): string {
  return fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), prefix)));
}

/** Points HOME and AMUX_HOME at a sandbox so no test touches the real ~/.claude, ~/.codex or ~/.amux. */
export function isolate(): { home: string; amuxHome: string } {
  const home = sandbox();
  const amuxHome = path.join(home, ".amux");
  process.env.HOME = home;
  process.env.AMUX_HOME = amuxHome;
  process.env.AMUX_NO_UPDATE_CHECK = "1";
  return { home, amuxHome };
}

export function write(file: string, content: string): void {
  fs.mkdirSync(path.dirname(file), { recursive: true });
  fs.writeFileSync(file, content);
}

export const jsonl = (records: unknown[]) => records.map((r) => JSON.stringify(r)).join("\n") + "\n";
