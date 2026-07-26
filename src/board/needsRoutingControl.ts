import type { EngineRouteDecision, ToolStatus } from "../lib/types";

export type NeedsRoutingControlKind =
  | "loading"
  | "automaticAvailable"
  | "automaticBlocked"
  | "explicitValid"
  | "explicitInvalid";

type PickerVisibility = "hidden" | "multiple" | "always";

interface RoutingControlPolicy {
  pickerVisibility: PickerVisibility;
  approvalDisabled: boolean;
}

const ROUTING_CONTROL_POLICY: Record<NeedsRoutingControlKind, RoutingControlPolicy> = {
  loading: { pickerVisibility: "hidden", approvalDisabled: true },
  automaticAvailable: { pickerVisibility: "multiple", approvalDisabled: false },
  automaticBlocked: { pickerVisibility: "always", approvalDisabled: true },
  explicitValid: { pickerVisibility: "multiple", approvalDisabled: false },
  explicitInvalid: { pickerVisibility: "always", approvalDisabled: true },
};

export interface NeedsRoutingControl {
  kind: NeedsRoutingControlKind;
  route: EngineRouteDecision | null;
  pickerOptions: ToolStatus[];
  pickerTool: string | null;
  manualTool: string | undefined;
  pickerVisible: boolean;
  showBlockedStatus: boolean;
  approvalDisabled: boolean;
}

export interface NeedsRoutingControlInput {
  route?: EngineRouteDecision | null;
  picked: string | null;
  installedTools: ToolStatus[];
  defaultTool: string;
}

/**
 * The store starts with an empty list and detect_tools always returns one row
 * per supported CLI, so an empty list is the only loading signal available to
 * this component. A non-empty list is authoritative, including zero installed
 * rows.
 */
export function needsRoutingControlOf(input: NeedsRoutingControlInput): NeedsRoutingControl {
  const route = input.route ?? null;
  const pickerOptions = input.installedTools.filter((tool) => tool.installed);

  if (input.installedTools.length === 0) {
    return createRoutingControl("loading", route, pickerOptions, input.picked, input.defaultTool);
  }

  if (input.picked !== null) {
    const pickedIsInstalled = pickerOptions.some((tool) => tool.tool === input.picked);
    const kind = pickedIsInstalled ? "explicitValid" : "explicitInvalid";
    return createRoutingControl(kind, route, pickerOptions, input.picked, input.defaultTool);
  }

  const kind = route?.blocked === true ? "automaticBlocked" : "automaticAvailable";
  return createRoutingControl(kind, route, pickerOptions, input.picked, input.defaultTool);
}

function createRoutingControl(
  kind: NeedsRoutingControlKind,
  route: EngineRouteDecision | null,
  pickerOptions: ToolStatus[],
  picked: string | null,
  defaultTool: string,
): NeedsRoutingControl {
  const policy = ROUTING_CONTROL_POLICY[kind];
  const pickerVisible =
    policy.pickerVisibility === "always" ||
    (policy.pickerVisibility === "multiple" && pickerOptions.length > 1);

  return {
    kind,
    route,
    pickerOptions,
    pickerTool: pickerToolOf(kind, route, picked, defaultTool),
    manualTool: manualToolOf(kind, picked),
    pickerVisible,
    showBlockedStatus: route?.blocked === true,
    approvalDisabled: policy.approvalDisabled,
  };
}

function pickerToolOf(
  kind: NeedsRoutingControlKind,
  route: EngineRouteDecision | null,
  picked: string | null,
  defaultTool: string,
): string | null {
  switch (kind) {
    case "loading":
    case "automaticBlocked":
      return null;
    case "automaticAvailable":
      return route?.tool ?? defaultTool;
    case "explicitValid":
    case "explicitInvalid":
      return picked;
  }
}

function manualToolOf(kind: NeedsRoutingControlKind, picked: string | null): string | undefined {
  if (kind !== "explicitValid") return undefined;
  return picked ?? undefined;
}
