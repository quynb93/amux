#!/usr/bin/env node
// Runs the prebuilt amux binary for this platform. The release workflow puts one
// binary per platform under native/<target>/amux; nothing is downloaded at install.
"use strict";
const { spawnSync } = require("node:child_process");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");

const TARGETS = {
  "darwin-arm64": "aarch64-apple-darwin",
  "darwin-x64": "x86_64-apple-darwin",
  "linux-arm64": "aarch64-unknown-linux-musl",
  "linux-x64": "x86_64-unknown-linux-musl",
};

const key = `${process.platform}-${process.arch}`;
const target = TARGETS[key];
if (!target) {
  console.error(`amux: there is no prebuilt binary for ${key}. See https://github.com/quynb93/amux#install`);
  process.exit(1);
}
const bin = path.join(__dirname, "..", "native", target, "amux");
try {
  fs.accessSync(bin, fs.constants.X_OK);
} catch {
  try {
    fs.chmodSync(bin, 0o755);
  } catch {
    // Reported by spawnSync below if it still can't run.
  }
}

// The binary owns the terminal; don't let Ctrl-C kill this wrapper out from under it.
for (const sig of ["SIGINT", "SIGQUIT", "SIGTERM", "SIGHUP"]) process.on(sig, () => {});
const r = spawnSync(bin, process.argv.slice(2), { stdio: "inherit" });
if (r.error) {
  console.error(`amux: cannot run ${bin}: ${r.error.message}`);
  process.exit(1);
}
process.exit(r.status ?? 128 + ((r.signal && os.constants.signals[r.signal]) || 1));
