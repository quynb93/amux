import { spawnSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import { accountDir, accountId, type Account } from "./store.js";

const SERVICE = "amux";
const useKeychain = process.platform === "darwin";

function fallbackFile(a: Account): string {
  return path.join(accountDir(a), "token");
}

export function setToken(a: Account, token: string): void {
  if (useKeychain) {
    // -U updates an existing item. The token is passed in argv only to the
    // short-lived `security` process.
    const r = spawnSync("security", ["add-generic-password", "-U", "-s", SERVICE, "-a", accountId(a), "-w", token], {
      stdio: ["ignore", "ignore", "pipe"],
    });
    if (r.status !== 0) throw new Error(`Keychain write failed: ${r.stderr.toString().trim()}`);
    return;
  }
  fs.mkdirSync(accountDir(a), { recursive: true, mode: 0o700 });
  fs.writeFileSync(fallbackFile(a), token, { mode: 0o600 });
}

export function getToken(a: Account): string | null {
  if (useKeychain) {
    const r = spawnSync("security", ["find-generic-password", "-s", SERVICE, "-a", accountId(a), "-w"], {
      stdio: ["ignore", "pipe", "ignore"],
    });
    return r.status === 0 ? r.stdout.toString().trimEnd() : null;
  }
  try {
    return fs.readFileSync(fallbackFile(a), "utf8").trimEnd();
  } catch {
    return null;
  }
}

export function deleteToken(a: Account): void {
  if (useKeychain) {
    spawnSync("security", ["delete-generic-password", "-s", SERVICE, "-a", accountId(a)], { stdio: "ignore" });
    return;
  }
  fs.rmSync(fallbackFile(a), { force: true });
}
