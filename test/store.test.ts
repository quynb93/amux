import assert from "node:assert/strict";
import path from "node:path";
import { beforeEach, describe, it } from "node:test";
import { isolate, write } from "./helpers.js";

const { home, amuxHome } = isolate();
const store = await import("../src/store.js");
const { loadProviders, resolveAccount, saveAccounts, saveConfig, homeEnvValue, overlayHome, getProvider } = store;

const acct = (provider: string, name: string) => ({ provider, name, auth: "login" as const, createdAt: "2026-01-01T00:00:00Z" });

beforeEach(() => {
  saveConfig({});
  saveAccounts([]);
});

describe("loadProviders", () => {
  it("has the built-in providers with expanded homes", () => {
    const p = loadProviders();
    assert.deepEqual([...p.keys()].sort(), ["claude", "codex", "gemini"]);
    assert.equal(p.get("claude")!.sharedHome, path.join(home, ".claude"));
  });

  it("merges config overrides into built-ins", () => {
    saveConfig({ providers: [{ id: "claude", sharedHome: "~/.claude-work" }] });
    const claude = loadProviders().get("claude")!;
    assert.equal(claude.sharedHome, path.join(home, ".claude-work"));
    assert.equal(claude.homeEnv, "CLAUDE_CONFIG_DIR", "keeps built-in fields");
  });

  it("adds custom providers with defaults and rejects incomplete ones", () => {
    saveConfig({ providers: [{ id: "my", bin: "mycli", sharedHome: "~/.my", homeEnv: "MY_HOME" }] });
    const my = loadProviders().get("my")!;
    assert.equal(my.label, "my");
    assert.equal(my.homeEnvPoints, "home");
    assert.deepEqual(my.privateEntries, []);

    saveConfig({ providers: [{ id: "broken", bin: "x" }] });
    assert.throws(() => loadProviders(), /missing: sharedHome, homeEnv/);
  });

  it("reports a config file it cannot parse", () => {
    write(path.join(amuxHome, "config.json"), "{ not json");
    assert.throws(() => loadProviders(), /Cannot parse/);
  });
});

describe("resolveAccount", () => {
  it("resolves provider@name", () => {
    saveAccounts([acct("claude", "work"), acct("claude", "home")]);
    assert.equal(resolveAccount("claude@home").name, "home");
  });

  it("uses the configured default, or the only account", () => {
    saveAccounts([acct("claude", "work"), acct("claude", "home"), acct("codex", "solo")]);
    saveConfig({ defaults: { claude: "work" } });
    assert.equal(resolveAccount("claude").name, "work");
    assert.equal(resolveAccount("codex").name, "solo");
  });

  it("explains ambiguous and unknown references", () => {
    saveAccounts([acct("claude", "work"), acct("claude", "home")]);
    assert.throws(() => resolveAccount("claude"), /ambiguous/);
    assert.throws(() => resolveAccount("claude@nope"), /No account "claude@nope"/);
  });
});

describe("homeEnvValue", () => {
  it("points at the overlay home, or its parent for gemini", () => {
    const a = acct("claude", "work");
    assert.equal(homeEnvValue(getProvider("claude"), a), path.join(amuxHome, "accounts", "claude", "work", ".claude"));
    assert.equal(overlayHome(getProvider("claude"), a), homeEnvValue(getProvider("claude"), a));
    const g = acct("gemini", "main");
    assert.equal(homeEnvValue(getProvider("gemini"), g), path.join(amuxHome, "accounts", "gemini", "main"));
  });
});
