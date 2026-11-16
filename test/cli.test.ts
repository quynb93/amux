import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { before, describe, it } from "node:test";
import { sandbox, write } from "./helpers.js";

/**
 * End-to-end: runs the compiled CLI against a sandboxed HOME with a fake
 * provider whose "CLI" prints the environment and args amux gives it.
 */

const CLI = fileURLToPath(new URL("../src/cli.js", import.meta.url));
const home = sandbox();
const amuxHome = path.join(home, ".amux");
const sharedHome = path.join(home, ".fake");
const fakeBin = path.join(home, "bin", "fake-cli");

function amux(...args: string[]) {
  const r = spawnSync(process.execPath, [CLI, ...args], {
    encoding: "utf8",
    env: { ...process.env, HOME: home, AMUX_HOME: amuxHome, AMUX_NO_UPDATE_CHECK: "1" },
  });
  return { code: r.status, out: r.stdout, err: r.stderr };
}

before(() => {
  write(
    fakeBin,
    `#!/usr/bin/env node
const pick = ["FAKE_HOME", "FAKE_BASE_URL", "FAKE_TELEMETRY", "AMUX_ACCOUNT"];
console.log(JSON.stringify({ args: process.argv.slice(2), env: Object.fromEntries(pick.map((k) => [k, process.env[k]])) }));
`,
  );
  fs.chmodSync(fakeBin, 0o755);
  write(path.join(sharedHome, "settings.json"), "{}");
  write(
    path.join(amuxHome, "config.json"),
    JSON.stringify({
      providers: [
        {
          id: "fake",
          label: "Fake CLI",
          bin: fakeBin,
          sharedHome,
          homeEnv: "FAKE_HOME",
          privateEntries: ["auth.json"],
          sharedDirs: ["sessions"],
          baseUrlEnv: "FAKE_BASE_URL",
          privacyEnv: { FAKE_TELEMETRY: "0" },
        },
      ],
    }),
  );
});

describe("amux CLI", () => {
  it("prints its version and help", () => {
    assert.match(amux("--version").out, /^\d+\.\d+\.\d+\n$/);
    const help = amux("help");
    assert.equal(help.code, 0);
    assert.match(help.out, /amux account add/);
  });

  it("adds an account with privacy and a gateway, then lists it", () => {
    const add = amux("account", "add", "fake", "one", "--no-login", "--privacy", "--base-url", "https://gw.example.com/");
    assert.equal(add.code, 0, add.err);
    assert.match(add.out, /Created fake@one/);
    const overlay = path.join(amuxHome, "accounts", "fake", "one", ".fake");
    assert.equal(fs.readlinkSync(path.join(overlay, "settings.json")), path.join(sharedHome, "settings.json"));

    const list = amux("account", "list");
    assert.match(list.out, /fake@one\s+login\s+on\s+https:\/\/gw\.example\.com/);
    assert.equal(amux("account", "add", "fake", "one", "--no-login").code, 1, "duplicates are rejected");
  });

  it("runs the provider CLI as the account, passing unknown args through", () => {
    amux("account", "default", "fake@one");
    const r = amux("fake", "--model", "opus", "hello");
    assert.equal(r.code, 0, r.err);
    const got = JSON.parse(r.out);
    assert.deepEqual(got.args, ["--model", "opus", "hello"]);
    assert.deepEqual(got.env, {
      FAKE_HOME: path.join(amuxHome, "accounts", "fake", "one", ".fake"),
      FAKE_BASE_URL: "https://gw.example.com",
      FAKE_TELEMETRY: "0",
      AMUX_ACCOUNT: "fake@one",
    });
    const mode = fs.statSync(path.join(amuxHome, "launches.jsonl")).mode & 0o777;
    assert.equal(mode, 0o600, "launch log is private");
  });

  it("prints shell exports, including privacy settings", () => {
    const out = amux("env", "fake@one").out;
    assert.match(out, /export FAKE_HOME='.*\/accounts\/fake\/one\/\.fake'/);
    assert.match(out, /export FAKE_TELEMETRY='0'/);
    assert.match(out, /export FAKE_BASE_URL='https:\/\/gw\.example\.com'/);
  });

  it("clears the gateway and privacy settings", () => {
    assert.equal(amux("account", "baseurl", "fake@one", "off").code, 0);
    assert.equal(amux("account", "privacy", "fake@one", "off").code, 0);
    const got = JSON.parse(amux("run", "fake@one").out);
    assert.equal(got.env.FAKE_BASE_URL, undefined);
    assert.equal(got.env.FAKE_TELEMETRY, undefined);
  });

  it("reports errors with a non-zero exit", () => {
    const r = amux("nope");
    assert.equal(r.code, 1);
    assert.match(r.err, /Unknown command "nope"/);
    assert.match(amux("account", "add", "fake", "bad name", "--no-login").err, /Account names may use/);
    assert.match(amux("account", "baseurl", "fake@one", "ftp://x").err, /http or https/);
  });

  it("removes an account and its overlay with --purge, leaving the shared home", () => {
    const r = amux("account", "rm", "fake@one", "--purge");
    assert.equal(r.code, 0, r.err);
    assert.equal(fs.existsSync(path.join(amuxHome, "accounts", "fake", "one")), false);
    assert.equal(fs.existsSync(path.join(sharedHome, "settings.json")), true);
    assert.match(amux("account", "list").out, /No accounts yet/);
  });
});
