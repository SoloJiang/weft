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

test("switchErrorCodeOf: every coded outcome resolves, across the shapes a reject can take", () => {
  // Tauri rejects with a bare string today; Error and { code } are the shapes
  // tests and future adapters produce. All must resolve to translated copy
  // rather than the raw code leaking into the dialog. Driven off the map, so
  // adding a fourth code without a test is not possible.
  for (const code of Object.keys(SWITCH_ERROR_I18N)) {
    assert.equal(switchErrorCodeOf(code), code, code);
    assert.equal(switchErrorCodeOf(`invoke failed: ${code}`), code, code);
    assert.equal(switchErrorCodeOf(new Error(code)), code, code);
    assert.equal(switchErrorCodeOf({ code }), code, code);
  }
});

test("switchErrorCodeOf: every code has copy, and the codes are mutually distinct", () => {
  // A code whose i18n key is missing would render as the raw key; and a code
  // that is a substring of another would make `find` return the wrong one.
  const codes = Object.keys(SWITCH_ERROR_I18N);
  for (const code of codes) {
    assert.ok(SWITCH_ERROR_I18N[code].startsWith("session."), code);
    const others = codes.filter((c) => c !== code);
    assert.ok(!others.some((c) => c.includes(code)), `${code} is a substring of another code`);
  }
});

test("switchErrorCodeOf: every other failure falls through to the raw message", () => {
  // The dialog's fallback is what keeps an unrelated error from being
  // explained as "nothing was changed, retry" — a wrong explanation is worse
  // than an untranslated one. An ordinary failed switch belongs here on
  // purpose: its own message says more than a generic sentence would.
  assert.equal(switchErrorCodeOf('unknown tool "gemini"'), null);
  assert.equal(switchErrorCodeOf("thread 7 not found"), null);
  assert.equal(switchErrorCodeOf(new Error("database is locked")), null);
  assert.equal(switchErrorCodeOf({ code: "process_quota_degraded" }), null);
  assert.equal(switchErrorCodeOf(null), null);
  assert.equal(switchErrorCodeOf(undefined), null);
  assert.equal(switchErrorCodeOf({}), null);
});
