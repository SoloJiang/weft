import { useCallback, useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { AlertTriangle, Check, X } from "lucide-react";
import type { LaneGate } from "../lib/types";
import { api } from "../lib/api";
import { useStore } from "../state/store";
import { Button } from "../components/ui/Button";

/** A stable, machine-readable Gate reason (mirrors Rust `authority::
 *  VerdictReason`, restricted to the values `list_lane_gates` can actually
 *  surface) — a single discriminated value mapped exhaustively to an i18n
 *  key, never re-derived per call site (CLAUDE.md). An unrecognized string
 *  (a future reason this build doesn't know about yet) falls back to the
 *  generic "awaiting_gate_decision" copy rather than showing nothing. */
type GateReasonKey =
  | "protected_branch"
  | "unreadable_policy"
  | "awaiting_gate_decision"
  | "gate_approved_override"
  | "gate_denied_override";

/** Which failure arm a click maps to — derived once from the action the user
 *  actually took, not re-guessed at render time. */
function failureFor(decision: "approved" | "denied"): GateActionState {
  if (decision === "approved") return "approveFailed";
  return "denyFailed";
}

function gateReasonKey(reason: string): GateReasonKey {
  switch (reason) {
    case "protected_branch":
    case "unreadable_policy":
    case "gate_approved_override":
    case "gate_denied_override":
      return reason;
    default:
      return "awaiting_gate_decision";
  }
}

/** Fetch/resolve status for one Gate row's approve/deny action — a single
 *  discriminated value per row (keyed by direction_id) instead of scattered
 *  booleans, mapped exhaustively where it's rendered. The two failure arms are
 *  distinct on purpose: telling someone "couldn't approve" after they clicked
 *  Deny reads, on a permission surface, as if they had just approved. */
type GateActionState = "idle" | "resolving" | "approveFailed" | "denyFailed" | "stale";

/** Whether the whole list has loaded. A failed fetch must NOT look like "no
 *  Gates pending" — that is the state in which a user concludes everything is
 *  running and walks away from a lane that is actually blocked. */
type GateFetchState = "idle" | "loading" | "resolved" | "rejected";

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
  const { dispatchDirection, directionsByThread, proposal } = useStore();
  const [gates, setGates] = useState<LaneGate[]>([]);
  const [fetchState, setFetchState] = useState<GateFetchState>("idle");
  const [actionState, setActionState] = useState<Record<number, GateActionState>>({});
  // Bumped on every reload so a slow earlier response can never overwrite a
  // newer one — without it, resolving two rows in a row (or switching threads
  // mid-flight) can repaint an already-approved lane as still pending, or show
  // one thread's lanes while another is on screen and approve into the wrong one.
  const requestSeq = useRef(0);

  const reload = useCallback(() => {
    if (threadId == null) {
      setGates([]);
      setFetchState("idle");
      return;
    }
    requestSeq.current += 1;
    const seq = requestSeq.current;
    setFetchState("loading");
    api
      .listLaneGates(threadId)
      .then((rows) => {
        if (seq !== requestSeq.current) return;
        setGates(rows);
        setFetchState("resolved");
      })
      .catch(() => {
        if (seq !== requestSeq.current) return;
        setFetchState("rejected");
      });
  }, [threadId]);

  // Confirming a proposal on the ALREADY-ACTIVE thread is what raises most
  // Gates, and it changes neither `threadId` nor this component's identity — it
  // reloads the thread's children and clears the proposal. Depending on those
  // two signals is what makes the card appear on that confirm instead of only
  // after a thread switch or an app reload.
  const laneSignature = threadId == null
    ? ""
    : (directionsByThread[threadId] ?? []).map((d) => d.id).join(",");
  const proposalSignature = proposal ? `${proposal.status}:${proposal.created_at}` : "";

  useEffect(() => {
    reload();
  }, [reload, laneSignature, proposalSignature]);

  async function resolve(gate: LaneGate, decision: "approved" | "denied") {
    setActionState((prev) => ({ ...prev, [gate.direction_id]: "resolving" }));
    try {
      const worktrees = await api.resolveLaneGate(gate.direction_id, gate.policy_revision, decision);
      setActionState((prev) => {
        const next = { ...prev };
        delete next[gate.direction_id];
        return next;
      });
      // Confirm deliberately left this lane out of its dispatch set (it had no
      // worktree then), and nothing else will pick it up: approving created the
      // worktree, which also drops the lane from `list_lane_gates`, so without
      // this it would vanish from the UI and sit idle forever.
      if (decision === "approved" && worktrees.length > 0) {
        void dispatchDirection(gate.direction_id);
      }
      reload();
    } catch (error) {
      // The backend rejects a decision made against a superseded policy
      // revision rather than recording one the adjudicator would ignore. That
      // is not a failed click, it is a card the user must re-read — say so, and
      // reload so the row comes back stamped with the rules now in force.
      const stale = String(error).includes("gate_policy_changed");
      setActionState((prev) => ({
        ...prev,
        [gate.direction_id]: stale ? "stale" : failureFor(decision),
      }));
      if (stale) reload();
    }
  }

  if (threadId == null) return null;
  // A rejected fetch is surfaced, never collapsed into the empty state.
  if (fetchState === "rejected") {
    return (
      <div className="flex items-center gap-1.5 rounded-[var(--radius-lg)] border border-danger/35 bg-danger/10 px-4 py-3 text-[11px] text-danger">
        <AlertTriangle size={13} />
        {t("scope.gate.loadFailed")}
      </div>
    );
  }
  if (gates.length === 0) return null;

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
        {t("scope.gate.reasonLine", {
          reason: t(`scope.gate.reason.${gateReasonKey(gate.verdict_reason)}`),
        })}
      </div>
      {gate.hit_rule ? (
        <div className="truncate text-[10.5px] text-ink-faint">
          {t("scope.gate.hitRuleLabel", { rule: gate.hit_rule })}
        </div>
      ) : null}
      <GateRowStatus state={state} />
    </div>
  );
}

/** The row's single status line: one discriminated value mapped exhaustively,
 *  so a new arm cannot be added without deciding what it renders. */
function GateRowStatus({ state }: { state: GateActionState }) {
  const { t } = useTranslation();
  switch (state) {
    case "idle":
      return null;
    case "resolving":
      return <div className="text-[10.5px] text-ink-faint">{t("scope.gate.resolving")}</div>;
    case "approveFailed":
      return <div className="text-[10.5px] text-danger">{t("scope.gate.approveFailed")}</div>;
    case "denyFailed":
      return <div className="text-[10.5px] text-danger">{t("scope.gate.denyFailed")}</div>;
    case "stale":
      return <div className="text-[10.5px] text-danger">{t("scope.gate.policyChanged")}</div>;
  }
}
