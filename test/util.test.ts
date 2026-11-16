import assert from "node:assert/strict";
import { describe, it } from "node:test";
import { ago, need, parseArgs, shQuote, table } from "../src/util.js";

describe("parseArgs", () => {
  const known = { privacy: "bool", note: "value", n: "value" } as const;

  it("splits known flags from positionals", () => {
    const { pos, flags } = parseArgs(["claude", "work", "--privacy", "--note", "hi"], known);
    assert.deepEqual(pos, ["claude", "work"]);
    assert.deepEqual(flags, { privacy: true, note: "hi" });
  });

  it("accepts --flag=value and -n", () => {
    const { flags } = parseArgs(["--note=a=b", "-n", "5"], known);
    assert.deepEqual(flags, { note: "a=b", n: "5" });
  });

  it("leaves unknown flags in place for the provider CLI", () => {
    const { pos, flags } = parseArgs(["--model", "opus", "--privacy"], known);
    assert.deepEqual(pos, ["--model", "opus"]);
    assert.deepEqual(flags, { privacy: true });
  });
});

describe("need", () => {
  it("returns present values and throws a usage error otherwise", () => {
    assert.equal(need("x", "usage"), "x");
    assert.throws(() => need(undefined, "amux foo <bar>"), /Usage: amux foo <bar>/);
    assert.throws(() => need("", "amux foo"), /Usage/);
  });
});

describe("table", () => {
  it("pads every column except the last", () => {
    assert.equal(
      table([
        ["A", "BB", "C"],
        ["aaa", "b", "c"],
      ]),
      "A    BB  C\naaa  b   c",
    );
  });
});

describe("shQuote", () => {
  it("quotes single quotes safely", () => {
    assert.equal(shQuote("it's"), `'it'\\''s'`);
  });
});

describe("ago", () => {
  it("formats minutes, hours and days", () => {
    const now = Date.UTC(2026, 0, 2);
    assert.equal(ago(new Date(now - 5 * 60_000), now), "5m ago");
    assert.equal(ago(new Date(now - 3 * 3_600_000), now), "3h ago");
    assert.equal(ago(new Date(now - 2 * 86_400_000), now), "2d ago");
  });
});
