import test from "node:test";
import assert from "node:assert/strict";
import type { EngineRouteDecision } from "../../src/lib/types.ts";
import { routeLabelKey, routeReasonKey, routeToolName } from "../../src/lib/engineRoutingDisplay.ts";

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

test("route display keeps automatic, manual, legacy, and blocked states distinct", () => {
  assert.equal(routeLabelKey(route()), "scope.engineAutomatic");
  assert.equal(routeLabelKey(route({ source: "manual" })), "scope.engineManual");
  assert.equal(routeLabelKey(route({ source: "legacy" })), "scope.engineLegacy");
  assert.equal(routeLabelKey(route({ source: "blocked", blocked: true, tool: null })), "scope.engineBlocked");
});

test("route display maps stable server reason codes and safe fallbacks", () => {
  assert.equal(routeReasonKey("deep_preference"), "scope.routeReason.deep_preference");
  assert.equal(routeReasonKey("future_reason"), "scope.routeReason.automatic_candidate_unavailable");
  assert.equal(routeToolName(route()), "codex");
  assert.equal(routeToolName(route({ tool: null, blocked: true })), "—");
});
