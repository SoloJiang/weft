import type { EngineRouteDecision } from "./types";

const ROUTE_REASON_KEYS: Record<string, string> = {
  automatic_disabled: "scope.routeReason.automatic_disabled",
  manual_pin: "scope.routeReason.manual_pin",
  normal_preference: "scope.routeReason.normal_preference",
  deep_preference: "scope.routeReason.deep_preference",
  preferred_warning: "scope.routeReason.preferred_warning",
  preferred_unavailable: "scope.routeReason.preferred_unavailable",
  quota_unknown: "scope.routeReason.quota_unknown",
  legacy_fallback: "scope.routeReason.legacy_fallback",
  no_automatic_candidate: "scope.routeReason.no_automatic_candidate",
  automatic_candidate_unavailable: "scope.routeReason.automatic_candidate_unavailable",
  both_automatic_candidates_exceeded: "scope.routeReason.both_automatic_candidates_exceeded",
  invalid_manual_tool: "scope.routeReason.invalid_manual_tool",
  manual_tool_unavailable: "scope.routeReason.manual_tool_unavailable",
};

export function routeReasonKey(reason: string): string {
  return ROUTE_REASON_KEYS[reason] ?? "scope.routeReason.automatic_candidate_unavailable";
}

export function routeLabelKey(route: EngineRouteDecision): string {
  if (route.blocked) return "scope.engineBlocked";
  if (route.source === "manual") return "scope.engineManual";
  if (route.source === "legacy") return "scope.engineLegacy";
  return "scope.engineAutomatic";
}

export function routeToolName(route: EngineRouteDecision): string {
  return route.tool ?? "—";
}
