import test from "node:test";
import assert from "node:assert/strict";
import {
  isSwitchMarkerError,
  modelSupported,
  switchKindOf,
  SWITCH_MARKER_ERROR_CODE,
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

test("isSwitchMarkerError: matches the stable code across the shapes a reject can take", () => {
  // Tauri rejects with a bare string today; Error and { code } are the shapes
  // tests and future adapters produce. All three must resolve to translated
  // copy rather than the raw code leaking into the dialog.
  assert.equal(isSwitchMarkerError(SWITCH_MARKER_ERROR_CODE), true);
  assert.equal(isSwitchMarkerError(`invoke failed: ${SWITCH_MARKER_ERROR_CODE}`), true);
  assert.equal(isSwitchMarkerError(new Error(SWITCH_MARKER_ERROR_CODE)), true);
  assert.equal(isSwitchMarkerError({ code: SWITCH_MARKER_ERROR_CODE }), true);
});

test("isSwitchMarkerError: every other failure falls through to the raw message", () => {
  // The dialog's fallback is what keeps an unrelated error from being
  // explained as "nothing was changed, retry" — a wrong explanation is worse
  // than an untranslated one.
  assert.equal(isSwitchMarkerError('unknown tool "gemini"'), false);
  assert.equal(isSwitchMarkerError("thread 7 not found"), false);
  assert.equal(isSwitchMarkerError(new Error("database is locked")), false);
  assert.equal(isSwitchMarkerError({ code: "process_quota_degraded" }), false);
  assert.equal(isSwitchMarkerError(null), false);
  assert.equal(isSwitchMarkerError(undefined), false);
  assert.equal(isSwitchMarkerError({}), false);
});
