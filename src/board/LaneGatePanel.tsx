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
  | "gate_denied_override"
  | "revoked_policy"
  | "stranded_lane";

/** Which failure arm a click maps to — derived once from the action the user
 *  actually took, not re-guessed at render time. */
function failureFor(decision: "approved" | "denied"): GateActionState {
  if (decision === "approved") return "approveFailed";
  return "denyFailed";
}

/** What a failed resolve means for the row: "stale" and "obsolete" are cards to
 *  replace, "retry" is a genuine failure to report. The backend's refusal slugs
 *  are the contract (`commands::resolve_lane_gate`); anything else is a real
 *  error and must NOT quietly remove a card the user still has to act on. */
function resolveFailure(error: unknown): "stale" | "obsolete" | "retry" {
  const text = String(error);
  if (text.includes("gate_policy_changed")) return "stale";
  if (text.includes("gate_not_actionable")) return "obsolete";
  return "retry";
}

function gateReasonKey(reason: string): GateReasonKey {
  switch (reason) {
    case "protected_branch":
    case "unreadable_policy":
    case "revoked_policy":
    case "gate_approved_override":
    case "gate_denied_override":
    case "stranded_lane":
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
type GateActionState =
  | "idle"
  | "resolving"
  | "approveFailed"
  | "denyFailed"
  | "stale"
  | "obsolete";

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
  const { dispatchDirection, directionsByThread, proposal, loadThreadChildren, sessions } =
    useStore();
  // Rows are stored WITH the thread they were fetched for. Keeping a bare array
  // left the previous thread's rows on screen for the whole duration of the new
  // thread's fetch — actionable approve/deny buttons that resolve the OLD lane,
  // since the command keys off the row's own direction id. Pairing them makes
  // that window unrepresentable rather than merely short.
  const [gates, setGates] = useState<{ threadId: number; rows: LaneGate[] } | null>(null);
  const [fetchState, setFetchState] = useState<GateFetchState>("idle");
  const [actionState, setActionState] = useState<Record<number, GateActionState>>({});
  // Bumped on every reload so a slow earlier response can never overwrite a
  // newer one — without it, resolving two rows in a row (or switching threads
  // mid-flight) can repaint an already-approved lane as still pending, or show
  // one thread's lanes while another is on screen and approve into the wrong one.
  const requestSeq = useRef(0);
  // The thread on screen RIGHT NOW. `reload` closes over the threadId it was
  // built with, so a resolution that finishes after a thread switch would call
  // the previous thread's reload — bumping the shared sequence, invalidating
  // the new thread's in-flight fetch, and then storing rows tagged for the old
  // one, which render as nothing. Completion handlers check this instead.
  const liveThreadId = useRef(threadId);
  liveThreadId.current = threadId;

  const reload = useCallback(() => {
    if (threadId == null) {
      setGates(null);
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
        setGates({ threadId, rows });
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
  // Which of this thread's lanes have a live session RIGHT NOW.
  //
  // A confirm reloads directions and clears the proposal BEFORE it starts the
  // returned workers, so this effect runs while every materialized lane is
  // still `ReadyToStart` — which the backend deliberately reports as a
  // stranded-lane card. Once dispatch registers the sessions, neither of the
  // other two signals changes: the lane ids are identical and the proposal is
  // already cleared. Without this the recovery cards stayed on screen
  // indefinitely while the workers were happily running.
  const liveSessionSignature = threadId == null
    ? ""
    : (directionsByThread[threadId] ?? [])
        .filter((d) => sessions[d.id] !== undefined)
        .map((d) => d.id)
        .join(",");

  useEffect(() => {
    reload();
  }, [reload, laneSignature, proposalSignature, liveSessionSignature]);

  async function resolve(gate: LaneGate, decision: "approved" | "denied") {
    setActionState((prev) => ({ ...prev, [gate.direction_id]: "resolving" }));
    try {
      const resolution = await api.resolveLaneGate(
        gate.direction_id,
        gate.policy_revision,
        decision,
      );
      // Drop the row NOW rather than waiting for `reload()` to replace the
      // list. Clearing only the busy flag left a settled card on screen with
      // live buttons for the whole duration of a slow refetch; a second click
      // is sequential, so it passes the backend's in-flight guard, records
      // another decision and returns the dispatch set again — opening the same
      // workers twice, since the dispatches are not awaited.
      setGates((prev) => {
        if (prev === null) return prev;
        return {
          threadId: prev.threadId,
          rows: prev.rows.filter((row) => row.direction_id !== gate.direction_id),
        };
      });
      setActionState((prev) => {
        const next = { ...prev };
        delete next[gate.direction_id];
        return next;
      });
      // Confirm deliberately left this lane AND its whole transitive dependent
      // set out of its dispatch ids, and nothing else will pick them up: this
      // lane gains a worktree (so it also drops off this list), and the
      // dependents never had a Gate of their own. The backend returns exactly
      // the set the clearance released — start all of it.
      // AWAITED before the reload. `list_lane_gates` decides "stranded" partly
      // on whether a live session exists, and `dispatchDirection` fetches
      // worktrees before opening one — so a reload racing those calls can see
      // the lane still sessionless and rebuild the very Resume card the
      // approval just cleared. Nothing re-runs this effect when the session
      // later appears, so that stale card would sit there indefinitely.
      // Failures inside dispatchDirection are already handled there; settling
      // is all this needs.
      // The approval materialized the lane, but nothing merged the returned
      // worktrees into store state — so the task card kept `writes = []`, its
      // name and Open Session action disabled and its branch hidden, while a
      // worker ran happily in a checkout the UI did not know about. Reload the
      // thread's children before dispatching so the card is whole.
      // Settled, not awaited bare. The Gate is ALREADY resolved on the backend
      // at this point, so letting a failed refresh throw sent a successful
      // approval into the catch below: the row came back with "couldn't
      // approve" on it and — worse — the dispatch never ran, leaving the lane
      // materialized, permitted, and with no worker, which nothing else starts.
      // A stale card is recoverable by reloading; a silently unstarted lane is
      // what the user walks away from.
      if (resolution.worktrees.length > 0) {
        await Promise.allSettled([loadThreadChildren(gate.thread_id)]);
      }
      await Promise.allSettled(
        resolution.dispatch_direction_ids.map((id) => dispatchDirection(id)),
      );
      if (liveThreadId.current === gate.thread_id) reload();
    } catch (error) {
      // Two backend refusals are not failed clicks — they are cards that no
      // longer describe reality — and both are answered by reloading rather
      // than by telling the user their action failed. Derived once, here, and
      // mapped exhaustively below (CLAUDE.md).
      //
      // `gate_policy_changed`: the decision was made against a superseded
      // policy revision, which the adjudicator would ignore. The row must come
      // back stamped with the rules now in force.
      //
      // `gate_not_actionable`: the lane reached a terminal or out-of-scope
      // state while this card sat open. Nothing else removes it — the reload
      // effect keys off the thread's direction ids, which a status change does
      // not alter — so without reloading here the user is left clicking a dead
      // card that reports a failure every time.
      const outcome = resolveFailure(error);
      setActionState((prev) => ({
        ...prev,
        [gate.direction_id]: outcome === "retry" ? failureFor(decision) : outcome,
      }));
      if (outcome !== "retry" && liveThreadId.current === gate.thread_id) reload();
    }
  }

  if (threadId == null) return null;
  // A rejected fetch is surfaced, never collapsed into the empty state.
  if (fetchState === "rejected") {
    return (
      <div className="flex items-center justify-between gap-2 rounded-[var(--radius-lg)] border border-danger/35 bg-danger/10 px-4 py-3 text-[11px] text-danger">
        <div className="flex items-center gap-1.5">
          <AlertTriangle size={13} />
          {t("scope.gate.loadFailed")}
        </div>
        {/* Without this the banner is terminal: the effect keys off threadId,
            the thread's lane ids and the proposal version, and none of those
            change while someone is staring at a blocked lane — so one transient
            failure hid every pending Gate until an app restart. */}
        <Button size="sm" variant="ghost" onClick={() => reload()}>
          {t("scope.gate.retry")}
        </Button>
      </div>
    );
  }
  // Anything fetched for another thread is not this board's state.
  const rows = gates?.threadId === threadId ? gates.rows : [];
  if (rows.length === 0) return null;

  return (
    <div className="flex flex-col gap-2 rounded-[var(--radius-lg)] border border-waiting/35 bg-waiting/10 px-4 py-3">
      <div className="flex items-center gap-1.5 text-[11px] font-semibold text-waiting">
        <AlertTriangle size={13} />
        {t("scope.gate.title", { count: rows.length })}
      </div>
      <div className="text-[10.5px] leading-snug text-ink-faint">{t("scope.gate.hint")}</div>
      <div className="flex flex-col gap-2">
        {rows.map((gate) => (
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
  const reasonKey = gateReasonKey(gate.verdict_reason);
  return (
    <div className="flex flex-col gap-1.5 rounded-[var(--radius-md)] border border-border bg-surface px-3 py-2">
      <div className="flex items-center justify-between gap-2">
        <div className="min-w-0 truncate text-[12.5px] font-medium text-ink">{gate.name}</div>
        <div className="flex shrink-0 items-center gap-1.5">
          <Button size="sm" variant="ghost" onClick={onApprove} disabled={busy}>
            <Check size={12} />
            {/* A stranded lane is already permitted — the action is to finish
                setting it up, not to grant something. Same command behind it. */}
            {t(reasonKey === "stranded_lane" ? "scope.gate.resume" : "scope.gate.approve")}
          </Button>
          <Button size="sm" variant="ghost" onClick={onDeny} disabled={busy}>
            <X size={12} />
            {t("scope.gate.deny")}
          </Button>
        </div>
      </div>
      <div className="text-[10.5px] text-ink-faint">
        {t("scope.gate.reasonLine", {
          reason: t(`scope.gate.reason.${reasonKey}`),
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
    case "obsolete":
      return <div className="text-[10.5px] text-ink-faint">{t("scope.gate.noLongerPending")}</div>;
  }
}
