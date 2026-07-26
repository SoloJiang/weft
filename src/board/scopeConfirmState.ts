type ScopeConfirmStateBase = {
  selectedTool: string | null;
  showBlockedRoute: boolean;
};

export type ScopeConfirmState =
  | (ScopeConfirmStateBase & { kind: "empty"; manualTool: undefined })
  | (ScopeConfirmStateBase & { kind: "confirming"; manualTool: string | undefined })
  | (ScopeConfirmStateBase & { kind: "automaticAvailable"; manualTool: undefined })
  | (ScopeConfirmStateBase & { kind: "automaticBlocked"; manualTool: undefined })
  | (ScopeConfirmStateBase & { kind: "explicitValid"; manualTool: string })
  | (ScopeConfirmStateBase & { kind: "explicitInvalid"; manualTool: undefined });

type ScopeConfirmInputs = {
  dirCount: number;
  confirming: boolean;
  routeBlocked: boolean;
  manualTool: string | null;
  spawnableToolNames: readonly string[];
};

export function scopeConfirmStateOf({
  dirCount,
  confirming,
  routeBlocked,
  manualTool,
  spawnableToolNames,
}: ScopeConfirmInputs): ScopeConfirmState {
  const selectedTool = manualTool;
  const spawnableManualTool = spawnableManualToolOf(manualTool, spawnableToolNames);

  if (confirming) {
    return {
      kind: "confirming",
      selectedTool,
      manualTool: spawnableManualTool,
      showBlockedRoute: routeBlocked && spawnableManualTool === undefined,
    };
  }
  if (dirCount === 0) {
    return {
      kind: "empty",
      selectedTool,
      manualTool: undefined,
      showBlockedRoute: false,
    };
  }
  if (manualTool !== null) {
    if (spawnableManualTool !== undefined) {
      return {
        kind: "explicitValid",
        selectedTool,
        manualTool: spawnableManualTool,
        showBlockedRoute: false,
      };
    }
    return {
      kind: "explicitInvalid",
      selectedTool,
      manualTool: undefined,
      showBlockedRoute: routeBlocked,
    };
  }
  if (routeBlocked) {
    return {
      kind: "automaticBlocked",
      selectedTool: null,
      manualTool: undefined,
      showBlockedRoute: true,
    };
  }
  return {
    kind: "automaticAvailable",
    selectedTool: null,
    manualTool: undefined,
    showBlockedRoute: false,
  };
}

function spawnableManualToolOf(
  manualTool: string | null,
  spawnableToolNames: readonly string[],
): string | undefined {
  if (manualTool === null) return undefined;
  if (!spawnableToolNames.includes(manualTool)) return undefined;
  return manualTool;
}

export const SCOPE_CONFIRM_DISABLED: Record<ScopeConfirmState["kind"], boolean> = {
  automaticAvailable: false,
  automaticBlocked: true,
  explicitValid: false,
  explicitInvalid: true,
  empty: true,
  confirming: true,
};
