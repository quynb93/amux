import assert from "node:assert/strict";
import { describe, it } from "node:test";
import { isolate } from "./helpers.js";

isolate();
const { buildEnv, fillArgs } = await import("../src/launch.js");
const { getProvider, homeEnvValue } = await import("../src/store.js");
const { validateBaseUrl } = await import("../src/commands/account.js");

const base = { provider: "claude", name: "work", auth: "login" as const, createdAt: "2026-01-01T00:00:00Z" };

describe("buildEnv", () => {
  it("points the CLI at the account overlay and drops conflicting env", () => {
    process.env.CLAUDE_CONFIG_DIR = "/somewhere/else";
    process.env.CLAUDE_CODE_OAUTH_TOKEN = "leaked";
    const spec = getProvider("claude");
    const env = buildEnv(spec, base);
    assert.equal(env.CLAUDE_CONFIG_DIR, homeEnvValue(spec, base));
    assert.equal(env.CLAUDE_CODE_OAUTH_TOKEN, undefined);
    assert.equal(env.AMUX_ACCOUNT, "claude@work");
    assert.equal(env.DISABLE_TELEMETRY, undefined, "privacy is off by default");
    delete process.env.CLAUDE_CONFIG_DIR;
    delete process.env.CLAUDE_CODE_OAUTH_TOKEN;
  });

  it("drops the provider's inherited token var for a login account", () => {
    process.env.OPENAI_API_KEY = "personal-key";
    const env = buildEnv(getProvider("codex"), { ...base, provider: "codex" });
    assert.equal(env.OPENAI_API_KEY, undefined);
    delete process.env.OPENAI_API_KEY;
  });

  it("adds the privacy opt-outs and base URL when set", () => {
    const env = buildEnv(getProvider("claude"), { ...base, privacy: true, baseUrl: "https://gw.example.com" });
    assert.equal(env.DISABLE_TELEMETRY, "1");
    assert.equal(env.DISABLE_ERROR_REPORTING, "1");
    assert.equal(env.ANTHROPIC_BASE_URL, "https://gw.example.com");
  });

  it("refuses a base URL for a provider without baseUrlEnv", () => {
    assert.throws(
      () => buildEnv(getProvider("gemini"), { ...base, provider: "gemini", baseUrl: "https://x.example.com" }),
      /no base-URL env var/,
    );
  });
});

describe("fillArgs", () => {
  it("substitutes {placeholders}", () => {
    assert.deepEqual(fillArgs(["--resume", "{id}", "{missing}"], { id: "abc" }), ["--resume", "abc", ""]);
  });
});

describe("validateBaseUrl", () => {
  const claude = getProvider("claude");
  it("normalizes valid http(s) URLs", () => {
    assert.equal(validateBaseUrl("https://gw.example.com/v1/", claude), "https://gw.example.com/v1");
  });
  it("rejects bad input", () => {
    assert.throws(() => validateBaseUrl("not a url", claude), /not a valid URL/);
    assert.throws(() => validateBaseUrl("ftp://gw.example.com", claude), /http or https/);
    assert.throws(() => validateBaseUrl("https://x.example.com", getProvider("gemini")), /no base-URL env var/);
  });
});
