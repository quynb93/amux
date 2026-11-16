import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import { beforeEach, describe, it } from "node:test";
import { adoptStray, isPrivate, syncOverlay } from "../src/overlay.js";
import type { ProviderSpec } from "../src/providers/types.js";
import { sandbox, write } from "./helpers.js";

let shared: string;
let overlay: string;
let spec: ProviderSpec;

beforeEach(() => {
  const root = sandbox();
  shared = path.join(root, "shared");
  overlay = path.join(root, "overlay");
  spec = {
    id: "fake",
    label: "Fake",
    bin: "fake",
    sharedHome: shared,
    homeEnv: "FAKE_HOME",
    homeEnvPoints: "home",
    privateEntries: ["auth.json", "state*"],
    sharedDirs: ["sessions"],
  };
  write(path.join(shared, "settings.json"), "{}");
  write(path.join(shared, "skills", "a.md"), "skill");
  write(path.join(shared, "auth.json"), "shared-login");
  write(path.join(shared, "state.db"), "x");
});

const linkOf = (name: string) => fs.readlinkSync(path.join(overlay, name));

describe("isPrivate", () => {
  it("matches exact names and trailing-* prefixes", () => {
    assert.equal(isPrivate(spec, "auth.json"), true);
    assert.equal(isPrivate(spec, "state.db.signature"), true);
    assert.equal(isPrivate(spec, "settings.json"), false);
  });
});

describe("syncOverlay", () => {
  it("links shared entries and skips private ones", () => {
    const r = syncOverlay(spec, overlay);
    assert.deepEqual(r.linked.sort(), ["sessions", "settings.json", "skills"]);
    assert.equal(linkOf("skills"), path.join(shared, "skills"));
    assert.equal(fs.existsSync(path.join(overlay, "auth.json")), false);
    assert.equal(fs.existsSync(path.join(overlay, "state.db")), false);
  });

  it("is idempotent", () => {
    syncOverlay(spec, overlay);
    const r = syncOverlay(spec, overlay);
    assert.deepEqual(r, { linked: [], pruned: [], stray: [] });
  });

  it("prunes links whose target is gone", () => {
    syncOverlay(spec, overlay);
    fs.rmSync(path.join(shared, "settings.json"));
    const r = syncOverlay(spec, overlay);
    assert.deepEqual(r.pruned, ["settings.json"]);
    assert.equal(fs.existsSync(path.join(overlay, "settings.json")), false);
  });

  it("repoints links that point somewhere else", () => {
    fs.mkdirSync(overlay, { recursive: true });
    fs.symlinkSync("/nonexistent", path.join(overlay, "skills"));
    syncOverlay(spec, overlay);
    assert.equal(linkOf("skills"), path.join(shared, "skills"));
  });

  it("reports real files as stray but never deletes them", () => {
    write(path.join(overlay, "notes.txt"), "mine");
    write(path.join(overlay, "auth.json"), "private-login");
    const r = syncOverlay(spec, overlay);
    assert.deepEqual(r.stray, ["notes.txt"]);
    assert.equal(fs.readFileSync(path.join(overlay, "notes.txt"), "utf8"), "mine");
    assert.equal(fs.readFileSync(path.join(overlay, "auth.json"), "utf8"), "private-login");
  });

  it("unlinks entries that were shared before they became private", () => {
    fs.mkdirSync(overlay, { recursive: true });
    fs.symlinkSync(path.join(shared, "state.db"), path.join(overlay, "state.db"));
    const r = syncOverlay(spec, overlay);
    assert.deepEqual(r.pruned, ["state.db"]);
    assert.equal(fs.existsSync(path.join(overlay, "state.db")), false);
    assert.equal(fs.existsSync(path.join(shared, "state.db")), true, "shared copy is kept");
  });
});

describe("adoptStray", () => {
  it("moves stray entries into the shared home and links them back", () => {
    write(path.join(overlay, "notes.txt"), "mine");
    write(path.join(overlay, "settings.json"), "conflict");
    const { adopted, conflicts } = adoptStray(spec, overlay, ["notes.txt", "settings.json"]);
    assert.deepEqual(adopted, ["notes.txt"]);
    assert.deepEqual(conflicts, ["settings.json"]);
    assert.equal(fs.readFileSync(path.join(shared, "notes.txt"), "utf8"), "mine");
    assert.equal(linkOf("notes.txt"), path.join(shared, "notes.txt"));
    assert.equal(fs.readFileSync(path.join(overlay, "settings.json"), "utf8"), "conflict");
  });
});
