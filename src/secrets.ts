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
  if (!token) throw new Error(`Empty token for ${accountId(a)}; nothing stored.`);
  if (useKeychain) {
    // Pass the command on stdin (`security -i`) rather than argv, so the token
    // never shows up in `ps`. -U updates an existing item. Interactive mode splits
    // on whitespace and quotes, so only accept tokens without them.
    if (/[\s"'\\]/.test(token)) throw new Error("Tokens can't contain spaces, quotes or backslashes.");
    const r = spawnSync("security", ["-i"], {
      input: `add-generic-password -U -s "${SERVICE}" -a "${accountId(a)}" -w "${token}"\n`,
      stdio: ["pipe", "ignore", "pipe"],
    });
    // `security -i` can exit 0 even when the inner command fails, so read it back.
    if (r.status !== 0 || getToken(a) !== token) throw new Error(`Keychain write failed: ${r.stderr.toString().trim() || "token not stored"}`);
    return;
  }
  fs.mkdirSync(accountDir(a), { recursive: true, mode: 0o700 });
  // Write a fresh 0600 file and rename it over the old one: `mode` only applies on
  // create, and writing in place would follow a symlink or keep looser permissions.
  const file = fallbackFile(a);
  const tmp = `${file}.${process.pid}.tmp`;
  fs.writeFileSync(tmp, token, { mode: 0o600, flag: "wx" });
  fs.renameSync(tmp, file);
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
