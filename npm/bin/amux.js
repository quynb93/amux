#!/usr/bin/env node
// Runs the prebuilt amux binary for this platform. The release workflow puts one
// binary per platform under native/<target>/amux; nothing is downloaded at install.
"use strict";
const { spawn } = require("node:child_process");
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
    // Reported by spawn below if it still can't run.
  }
}

const child = spawn(bin, process.argv.slice(2), { stdio: "inherit" });
// The binary owns the terminal. Ctrl-C and Ctrl-\ already reach it through the
// foreground process group, so this wrapper just ignores them. A SIGTERM or SIGHUP
// sent to the wrapper alone (kill, timeout, a process manager) is passed on.
for (const sig of ["SIGINT", "SIGQUIT"]) process.on(sig, () => {});
for (const sig of ["SIGTERM", "SIGHUP"]) {
  process.on(sig, () => {
    try {
      child.kill(sig);
    } catch {
      // Already gone.
    }
  });
}
child.on("error", (err) => {
  console.error(`amux: cannot run ${bin}: ${err.message}`);
  process.exit(1);
});
child.on("exit", (code, signal) => {
  process.exit(code ?? 128 + ((signal && os.constants.signals[signal]) || 1));
});
