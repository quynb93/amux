import assert from "node:assert/strict";
import { describe, it } from "node:test";
import { isolate } from "./helpers.js";

isolate();
const { isNewer, parseRepo, upgradeCommand, version } = await import("../src/update.js");

describe("upgradeCommand", () => {
  it("uses brew for Homebrew installs and the release package otherwise", () => {
    assert.equal(upgradeCommand("/opt/homebrew/Cellar/amux/0.1.1/libexec/lib/node_modules/@quynb/amux/dist/update.js", "o/r", "0.2.0"), "brew upgrade amux");
    assert.equal(upgradeCommand("/home/linuxbrew/.linuxbrew/Cellar/amux/0.1.1/libexec/x.js", "o/r", "0.2.0"), "brew upgrade amux");
    assert.equal(
      upgradeCommand("/usr/local/lib/node_modules/@quynb/amux/dist/update.js", "o/r", "0.2.0"),
      "npm install -g https://github.com/o/r/releases/download/v0.2.0/amux.tgz",
    );
  });
});

describe("isNewer", () => {
  it("compares versions numerically", () => {
    assert.equal(isNewer("0.2.0", "0.1.0"), true);
    assert.equal(isNewer("0.10.0", "0.9.9"), true);
    assert.equal(isNewer("v1.0.0", "1.0.0"), false);
    assert.equal(isNewer("1.0.0", "1.0.1"), false);
    assert.equal(isNewer("1.1", "1.0.9"), true);
    assert.equal(isNewer("1.0.0-beta.1", "0.9.0"), true);
  });
});

describe("parseRepo", () => {
  it("reads the GitHub repo from package.json repository forms", () => {
    assert.equal(parseRepo("github:quynb93/amux"), "quynb93/amux");
    assert.equal(parseRepo({ url: "git+https://github.com/quynb93/amux.git" }), "quynb93/amux");
    assert.equal(parseRepo("git@github.com:quynb93/amux.git"), "quynb93/amux");
    assert.equal(parseRepo("https://gitlab.com/x/y"), null);
    assert.equal(parseRepo(undefined), null);
  });
});

describe("version", () => {
  it("comes from package.json", () => {
    assert.match(version(), /^\d+\.\d+\.\d+/);
    assert.notEqual(version(), "0.0.0");
  });
});
