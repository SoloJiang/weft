import test from "node:test";
import assert from "node:assert/strict";
import { modelSupported, switchKindOf } from "../../src/session/engineSwitch.ts";

test("modelSupported: only claude/codex accept the --model override", () => {
  assert.equal(modelSupported("claude"), true);
  assert.equal(modelSupported("codex"), true);
  assert.equal(modelSupported("opencode"), false);
  assert.equal(modelSupported("unknown"), false);
  assert.equal(modelSupported(""), false);
});

test("switchKindOf: same non-empty tool reads as a reload", () => {
  assert.equal(switchKindOf("claude", "claude"), "reload");
  assert.equal(switchKindOf("codex", "codex"), "reload");
});

test("switchKindOf: a different tool reads as a switch", () => {
  assert.equal(switchKindOf("claude", "codex"), "switch");
  assert.equal(switchKindOf("codex", "opencode"), "switch");
});

test("switchKindOf: an unresolved prior tool (empty string) never reads as a reload", () => {
  // Nothing to "reload" from when the old identity was never known — this
  // must not collapse to reload just because "" === "" would be true for any
  // other pair of equal strings.
  assert.equal(switchKindOf("", ""), "switch");
  assert.equal(switchKindOf("", "claude"), "switch");
});
