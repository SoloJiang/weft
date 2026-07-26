import test from "node:test";
import assert from "node:assert/strict";
import { SCOPE_CONFIRM_DISABLED, scopeConfirmStateOf } from "../../src/board/scopeConfirmState.ts";

test("scope confirmation requires an explicit override for a blocked route", () => {
  const blockedAutomatic = scopeConfirmStateOf({
    dirCount: 1,
    confirming: false,
    routeBlocked: true,
    manualTool: null,
    installedToolNames: ["opencode"],
  });
  assert.equal(blockedAutomatic.kind, "automaticBlocked");
  assert.equal(SCOPE_CONFIRM_DISABLED[blockedAutomatic.kind], true);

  const overridden = scopeConfirmStateOf({
    dirCount: 1,
    confirming: false,
    routeBlocked: true,
    manualTool: "opencode",
    installedToolNames: ["opencode"],
  });
  assert.equal(overridden.kind, "explicitValid");
  assert.equal(overridden.manualTool, "opencode");
  assert.equal(SCOPE_CONFIRM_DISABLED[overridden.kind], false);
});

test("a stale explicit selection becomes invalid after the installed tools refresh", () => {
  const state = scopeConfirmStateOf({
    dirCount: 1,
    confirming: false,
    routeBlocked: true,
    manualTool: "opencode",
    installedToolNames: ["codex"],
  });

  assert.equal(state.kind, "explicitInvalid");
  assert.equal(state.selectedTool, "opencode");
  assert.equal(state.manualTool, undefined);
  assert.equal(SCOPE_CONFIRM_DISABLED[state.kind], true);
});

test("automatic routing remains available without an explicit selection", () => {
  const state = scopeConfirmStateOf({
    dirCount: 1,
    confirming: false,
    routeBlocked: false,
    manualTool: null,
    installedToolNames: ["codex"],
  });

  assert.equal(state.kind, "automaticAvailable");
  assert.equal(state.manualTool, undefined);
  assert.equal(SCOPE_CONFIRM_DISABLED[state.kind], false);
});
