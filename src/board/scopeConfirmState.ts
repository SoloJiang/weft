export type ScopeConfirmState = "ready" | "empty" | "confirming" | "blockedRoute";

type ScopeConfirmInputs = {
  dirCount: number;
  confirming: boolean;
  routeBlocked: boolean;
  manualTool: string | null;
};

export function scopeConfirmStateOf({
  dirCount,
  confirming,
  routeBlocked,
  manualTool,
}: ScopeConfirmInputs): ScopeConfirmState {
  if (confirming) return "confirming";
  if (dirCount === 0) return "empty";
  if (routeBlocked && manualTool === null) return "blockedRoute";
  return "ready";
}

export const SCOPE_CONFIRM_DISABLED: Record<ScopeConfirmState, boolean> = {
  ready: false,
  empty: true,
  confirming: true,
  blockedRoute: true,
};
