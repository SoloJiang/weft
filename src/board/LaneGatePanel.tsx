import { useCallback, useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { AlertTriangle, Check, X } from "lucide-react";
import type { LaneGate } from "../lib/types";
import { api } from "../lib/api";
import { Button } from "../components/ui/Button";

/** A stable, machine-readable Gate reason (mirrors Rust `authority::
 *  VerdictReason`, restricted to the values `list_lane_gates` can actually
 *  surface) — a single discriminated value mapped exhaustively to an i18n
 *  key, never re-derived per call site (CLAUDE.md). An unrecognized string
 *  (a future reason this build doesn't know about yet) falls back to the
 *  generic "awaiting_gate_decision" copy rather than showing nothing. */
type GateReasonKey =
  | "protected_branch"
  | "awaiting_gate_decision"
  | "gate_approved_override"
  | "gate_denied_override";

function gateReasonKey(reason: string): GateReasonKey {
  switch (reason) {
    case "protected_branch":
    case "gate_approved_override":
    case "gate_denied_override":
      return reason;
    default:
      return "awaiting_gate_decision";
  }
}

/** Fetch/resolve status for one Gate row's approve/deny action — a single
 *  discriminated value per row (keyed by direction_id) instead of scattered
 *  booleans, mapped exhaustively where it's rendered. */
type GateActionState = "idle" | "resolving" | "failed";

/**
 * Issue #172: the pending-Gate list for one thread. A `needs_gate` Lane has
 * no worktree yet and its latest decision Evidence names a specific rule —
 * this panel is the ONLY card a Gate produces; a `allowed_by_policy` Lane
 * materializes silently and never appears here. Renders nothing while there
 * is nothing to decide (fetched but empty), so a policy-allowed batch shows
 * no extra UI at all.
 */
export function LaneGatePanel({ threadId }: { threadId: number | null }) {
  const { t } = useTranslation();
  const [gates, setGates] = useState<LaneGate[]>([]);
  const [actionState, setActionState] = useState<Record<number, GateActionState>>({});

  const reload = useCallback(() => {
    if (threadId == null) {
      setGates([]);
      return;
    }
    api
      .listLaneGates(threadId)
      .then(setGates)
      .catch(() => setGates([]));
  }, [threadId]);

  useEffect(() => {
    reload();
  }, [reload]);

  async function resolve(gate: LaneGate, decision: "approved" | "denied") {
    setActionState((prev) => ({ ...prev, [gate.direction_id]: "resolving" }));
    try {
      await api.resolveLaneGate(gate.direction_id, gate.policy_revision, decision);
      setActionState((prev) => {
        const next = { ...prev };
        delete next[gate.direction_id];
        return next;
      });
      reload();
    } catch {
      setActionState((prev) => ({ ...prev, [gate.direction_id]: "failed" }));
    }
  }

  if (threadId == null || gates.length === 0) return null;

  return (
    <div className="flex flex-col gap-2 rounded-[var(--radius-lg)] border border-waiting/35 bg-waiting/10 px-4 py-3">
      <div className="flex items-center gap-1.5 text-[11px] font-semibold text-waiting">
        <AlertTriangle size={13} />
        {t("scope.gate.title", { count: gates.length })}
      </div>
      <div className="text-[10.5px] leading-snug text-ink-faint">{t("scope.gate.hint")}</div>
      <div className="flex flex-col gap-2">
        {gates.map((gate) => (
          <LaneGateRow
            key={gate.direction_id}
            gate={gate}
            state={actionState[gate.direction_id] ?? "idle"}
            onApprove={() => void resolve(gate, "approved")}
            onDeny={() => void resolve(gate, "denied")}
          />
        ))}
      </div>
    </div>
  );
}

function LaneGateRow({
  gate,
  state,
  onApprove,
  onDeny,
}: {
  gate: LaneGate;
  state: GateActionState;
  onApprove: () => void;
  onDeny: () => void;
}) {
  const { t } = useTranslation();
  const busy = state === "resolving";
  return (
    <div className="flex flex-col gap-1.5 rounded-[var(--radius-md)] border border-border bg-surface px-3 py-2">
      <div className="flex items-center justify-between gap-2">
        <div className="min-w-0 truncate text-[12.5px] font-medium text-ink">{gate.name}</div>
        <div className="flex shrink-0 items-center gap-1.5">
          <Button size="sm" variant="ghost" onClick={onApprove} disabled={busy}>
            <Check size={12} />
            {t("scope.gate.approve")}
          </Button>
          <Button size="sm" variant="ghost" onClick={onDeny} disabled={busy}>
            <X size={12} />
            {t("scope.gate.deny")}
          </Button>
        </div>
      </div>
      <div className="text-[10.5px] text-ink-faint">
        {t("scope.gate.reasonLabel")}: {t(`scope.gate.reason.${gateReasonKey(gate.verdict_reason)}`)}
      </div>
      {gate.hit_rule ? (
        <div className="truncate text-[10.5px] text-ink-faint">
          {t("scope.gate.hitRuleLabel", { rule: gate.hit_rule })}
        </div>
      ) : null}
      {state === "resolving" ? (
        <div className="text-[10.5px] text-ink-faint">{t("scope.gate.resolving")}</div>
      ) : null}
      {state === "failed" ? (
        <div className="text-[10.5px] text-danger">{t("scope.gate.approveFailed")}</div>
      ) : null}
    </div>
  );
}
