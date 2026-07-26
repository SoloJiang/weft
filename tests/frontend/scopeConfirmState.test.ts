import test from "node:test";
import assert from "node:assert/strict";
import { SCOPE_CONFIRM_DISABLED, scopeConfirmStateOf } from "../../src/board/scopeConfirmState.ts";

test("scope confirmation requires an explicit override for a blocked route", () => {
  const blockedAutomatic = scopeConfirmStateOf({
    dirCount: 1,
    confirming: false,
    routeBlocked: true,
    manualTool: null,
  });
  assert.equal(blockedAutomatic, "blockedRoute");
  assert.equal(SCOPE_CONFIRM_DISABLED[blockedAutomatic], true);

  const overridden = scopeConfirmStateOf({
    dirCount: 1,
    confirming: false,
    routeBlocked: true,
    manualTool: "opencode",
  });
  assert.equal(overridden, "ready");
  assert.equal(SCOPE_CONFIRM_DISABLED[overridden], false);
});
