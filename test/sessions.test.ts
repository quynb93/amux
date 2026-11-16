import assert from "node:assert/strict";
import path from "node:path";
import { describe, it } from "node:test";
import type { ProviderSpec } from "../src/providers/types.js";
import { buildHandoff, listSessions, readTurns, type Turn } from "../src/sessions.js";
import { jsonl, sandbox, write } from "./helpers.js";

const specFor = (sessions: ProviderSpec["sessions"], sharedHome: string): ProviderSpec => ({
  id: sessions!,
  label: sessions!,
  bin: sessions!,
  sharedHome,
  homeEnv: "X",
  homeEnvPoints: "home",
  privateEntries: [],
  sharedDirs: [],
  sessions,
});

describe("Claude sessions", () => {
  const home = sandbox();
  write(
    path.join(home, "projects", "-work-app", "11111111-aaaa.jsonl"),
    jsonl([
      { type: "user", cwd: "/work/app", message: { role: "user", content: "<command-name>/clear</command-name>" } },
      { type: "user", cwd: "/work/app", message: { role: "user", content: "Fix the login bug" } },
      { type: "assistant", message: { content: [{ type: "text", text: "Looking at auth.ts" }, { type: "tool_use", name: "Read" }] } },
      { type: "user", isMeta: true, message: { role: "user", content: "meta" } },
    ]),
  );
  write(path.join(home, "projects", "-work-app", "subagents", "x.jsonl"), jsonl([{ type: "user", message: { role: "user", content: "sub" } }]));

  it("lists sessions with cwd and the first real prompt, skipping subagents", () => {
    const [s, ...rest] = listSessions(specFor("claude", home));
    assert.equal(rest.length, 0);
    assert.equal(s.id, "11111111-aaaa");
    assert.equal(s.cwd, "/work/app");
    assert.equal(s.title, "Fix the login bug");
  });

  it("reads user and assistant text turns", () => {
    const [s] = listSessions(specFor("claude", home));
    assert.deepEqual(readTurns(s, "claude"), [
      { role: "user", text: "Fix the login bug" },
      { role: "assistant", text: "Looking at auth.ts" },
    ]);
  });
});

describe("Codex sessions", () => {
  const home = sandbox();
  const prompt = { type: "response_item", payload: { type: "message", role: "user", content: [{ type: "input_text", text: "Add tests" }] } };
  write(
    path.join(home, "sessions", "2026", "01", "02", "rollout-x.jsonl"),
    jsonl([
      { type: "session_meta", payload: { id: "codex-1", cwd: "/work/api" } },
      { type: "response_item", payload: { type: "message", role: "user", content: [{ type: "input_text", text: "# AGENTS.md\nrules" }] } },
      prompt,
      prompt,
      { type: "response_item", payload: { type: "message", role: "assistant", content: [{ type: "output_text", text: "Done" }] } },
    ]),
  );

  it("lists sessions and drops injected context and duplicate prompts", () => {
    const [s] = listSessions(specFor("codex", home));
    assert.equal(s.id, "codex-1");
    assert.equal(s.cwd, "/work/api");
    assert.equal(s.title, "Add tests");
    assert.deepEqual(readTurns(s, "codex"), [
      { role: "user", text: "Add tests" },
      { role: "assistant", text: "Done" },
    ]);
  });
});

describe("buildHandoff", () => {
  const info = { provider: "codex", id: "codex-1", cwd: "/work/api", title: "t", updatedAt: new Date(0), file: "/f.jsonl" };

  it("keeps the first prompt and the most recent turns within the budget", () => {
    const turns: Turn[] = [{ role: "user", text: "ORIGINAL REQUEST" }];
    for (let i = 0; i < 50; i++) turns.push({ role: i % 2 ? "user" : "assistant", text: `turn ${i} ${"x".repeat(200)}` });
    const out = buildHandoff(info, turns, 2_000);
    assert.match(out, /ORIGINAL REQUEST/);
    assert.match(out, /earlier turns omitted/);
    assert.match(out, /turn 49 /);
    assert.doesNotMatch(out, /turn 0 /);
  });

  it("keeps the tail of an oversized last turn instead of dropping it", () => {
    const out = buildHandoff(info, [{ role: "user", text: "ask" }, { role: "assistant", text: "a".repeat(10_000) + "THE END" }], 3_000);
    assert.match(out, /THE END/);
  });
});
