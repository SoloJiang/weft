import test from "node:test";
import assert from "node:assert/strict";
import {
  modelSupported,
  switchErrorCodeOf,
  switchKindOf,
  SWITCH_ERROR_I18N,
} from "../../src/session/engineSwitch.ts";

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

test("switchErrorCodeOf: every code resolves, across the shapes a reject can take", () => {
  // Tauri rejects with a bare string today; Error and { code } are the shapes
  // tests and future adapters produce. Driven off the map, so a third code
  // cannot be added untested.
  for (const code of Object.keys(SWITCH_ERROR_I18N)) {
    assert.equal(switchErrorCodeOf(code), code, code);
    assert.equal(switchErrorCodeOf(`invoke failed: ${code}`), code, code);
    assert.equal(switchErrorCodeOf(new Error(code)), code, code);
    assert.equal(switchErrorCodeOf({ code }), code, code);
  }
});

test("switchErrorCodeOf: a prefix code never shadows the longer one", () => {
  // `switch_failed` IS a prefix of `switch_failed_interrupted`, and matching is
  // by substring — so without longest-first ordering an interrupted switch
  // would silently render the un-interrupted copy, telling the user nothing was
  // affected right after their turn was killed. The Rust side pins the same
  // prefix relationship.
  assert.equal(switchErrorCodeOf("switch_failed_interrupted"), "switch_failed_interrupted");
  assert.equal(switchErrorCodeOf("switch_failed"), "switch_failed");
  for (const code of Object.keys(SWITCH_ERROR_I18N)) {
    assert.ok(SWITCH_ERROR_I18N[code].startsWith("session."), code);
  }
});

test("switchErrorCodeOf: every other failure falls through to the raw message", () => {
  // The dialog's fallback keeps an unrelated error from being explained with
  // the wrong sentence — a wrong explanation is worse than an untranslated one.
  assert.equal(switchErrorCodeOf('unknown tool "gemini"'), null);
  assert.equal(switchErrorCodeOf("thread 7 not found"), null);
  assert.equal(switchErrorCodeOf(new Error("database is locked")), null);
  assert.equal(switchErrorCodeOf({ code: "process_quota_degraded" }), null);
  assert.equal(switchErrorCodeOf(null), null);
  assert.equal(switchErrorCodeOf(undefined), null);
  assert.equal(switchErrorCodeOf({}), null);
});
