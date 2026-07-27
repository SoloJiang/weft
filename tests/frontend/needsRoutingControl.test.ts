import test from "node:test";
import assert from "node:assert/strict";
import type { EngineRouteDecision, ToolStatus } from "../../src/lib/types.ts";
import { needsRoutingControlOf } from "../../src/board/needsRoutingControl.ts";

function tool(name: string, installed = true, spawnable = installed): ToolStatus {
  return {
    tool: name,
    installed,
    spawnable,
    version: null,
    path: null,
    meets_min: true,
    diagnostics: [],
  };
}

function route(overrides: Partial<EngineRouteDecision> = {}): EngineRouteDecision {
  return {
    tool: "codex",
    source: "automatic",
    reason: "normal_preference",
    hint: "normal",
    quota: "ok",
    blocked: false,
    ...overrides,
  };
}

test("loading disables approval before the initial tool probe returns", () => {
  const state = needsRoutingControlOf({
    route: route({ blocked: true, source: "blocked", tool: null }),
    picked: null,
    installedTools: [],
    defaultTool: "codex",
  });

  assert.equal(state.kind, "loading");
  assert.equal(state.approvalDisabled, true);
  assert.equal(state.pickerVisible, false);
  assert.equal(state.manualTool, undefined);
  assert.equal(state.showBlockedStatus, true);
});

test("blocked automatic routing stays disabled until the user picks an installed tool", () => {
  const state = needsRoutingControlOf({
    route: route({ blocked: true, source: "blocked", tool: null }),
    picked: null,
    installedTools: [tool("opencode")],
    defaultTool: "codex",
  });

  assert.equal(state.kind, "automaticBlocked");
  assert.equal(state.approvalDisabled, true);
  assert.equal(state.pickerVisible, true);
  assert.equal(state.pickerTool, null);
  assert.equal(state.manualTool, undefined);
});

test("an explicit installed selection enables approval and becomes the manual tool", () => {
  const state = needsRoutingControlOf({
    route: route({ blocked: true, source: "blocked", tool: null }),
    picked: "opencode",
    installedTools: [tool("opencode")],
    defaultTool: "codex",
  });

  assert.equal(state.kind, "explicitValid");
  assert.equal(state.approvalDisabled, false);
  assert.equal(state.pickerTool, "opencode");
  assert.equal(state.manualTool, "opencode");
});

test("a picked tool that is installed but no longer spawnable stays invalid and disabled", () => {
  const beforeRefresh = needsRoutingControlOf({
    route: route({ blocked: true, source: "blocked", tool: null }),
    picked: "codex",
    installedTools: [tool("codex"), tool("opencode")],
    defaultTool: "codex",
  });
  const afterRefresh = needsRoutingControlOf({
    route: route({ blocked: true, source: "blocked", tool: null }),
    picked: "codex",
    installedTools: [tool("codex", true, false), tool("opencode")],
    defaultTool: "codex",
  });

  assert.equal(beforeRefresh.kind, "explicitValid");
  assert.equal(beforeRefresh.manualTool, "codex");
  assert.equal(afterRefresh.kind, "explicitInvalid");
  assert.equal(afterRefresh.approvalDisabled, true);
  assert.equal(afterRefresh.manualTool, undefined);
  assert.equal(afterRefresh.pickerTool, "codex");
  assert.deepEqual(afterRefresh.pickerOptions.map((candidate) => candidate.tool), ["opencode"]);
  assert.equal(afterRefresh.pickerVisible, true);
});

test("automatic routing remains available without an explicit selection", () => {
  const state = needsRoutingControlOf({
    route: route(),
    picked: null,
    installedTools: [tool("codex"), tool("opencode")],
    defaultTool: "opencode",
  });

  assert.equal(state.kind, "automaticAvailable");
  assert.equal(state.approvalDisabled, false);
  assert.equal(state.manualTool, undefined);
  assert.equal(state.pickerTool, "codex");
  assert.equal(state.pickerVisible, true);
});
