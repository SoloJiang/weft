import type { ChangeSetLane, IssueChangeSet, LaneCheckout } from "../lib/types";
import type { ReadinessFetchState } from "../lib/readinessKey";
import type { IssueDelivery } from "./issueDelivery";

/**
 * Pure derivations for the Issue Change Set panel (issue #175).
 *
 * The backend already decided every verdict on this screen. Nothing in this
 * file re-decides one: it turns the DTO into the shapes the panel renders, and
 * each multi-way state is derived ONCE here as a discriminated value rather
 * than as booleans re-tested at each call site (CLAUDE.md).
 *
 * The fetch lives in `ThreadBoard` — ONE per refresh, see `issueDelivery.ts` —
 * so this stays a pure function of its input, the same split
 * `ReadinessChip`/`EvidenceBody` already use.
 */

/** One discriminated read state for the whole panel. */
export type ChangeSetPanelState =
  | { kind: "loading" }
  | { kind: "error" }
  | { kind: "empty" }
  | { kind: "ready"; changeSet: IssueChangeSet; waves: ChangeSetWave[] };

/**
 * Collapse the board's ONE delivery read into the panel's state.
 *
 * A `ready` read whose `changeSet` is `null` came from the readiness command,
 * not the Change Set command — the tab was not open when it was issued. That
 * is a refresh still in flight, not an empty issue, so it reads `loading`:
 * calling it `empty` would tell the user this issue writes nothing, which is a
 * different and wrong claim.
 */
export function changeSetPanelState(
  read: ReadinessFetchState<IssueDelivery>,
): ChangeSetPanelState {
  if (read.kind === "loading") return { kind: "loading" };
  if (read.kind === "failed") return { kind: "error" };
  const changeSet = read.dto.changeSet;
  if (!changeSet) return { kind: "loading" };
  if (changeSet.lanes.length === 0) return { kind: "empty" };
  return { kind: "ready", changeSet, waves: laneWaves(changeSet.lanes) };
}

/** Lanes that can proceed together, in the order the dependency edges imply. */
export interface ChangeSetWave {
  lanes: ChangeSetLane[];
}

/**
 * Group lanes into dependency waves: wave 0 is everything that waits on
 * nothing in this Change Set, wave N everything whose upstreams all landed in
 * earlier waves. This answers the issue's "in what order" without asking the
 * user to reconstruct it lane by lane.
 *
 * Edges pointing outside this Change Set are ignored rather than treated as
 * unsatisfiable — an upstream lane the verdict excluded (inactive, denied) is
 * not a reason to hide its consumer. A dependency CYCLE cannot be laid out in
 * waves at all, so the remaining lanes are emitted together in their original
 * order: showing them in one honest block beats dropping them or inventing an
 * order the edges do not support. Ordering is otherwise stable — ties keep
 * their incoming order, so a refresh never reshuffles the panel.
 *
 * Lanes are tracked by POSITION, never by `direction_id`. Virtual lanes — the
 * unbound-PR row, the issue-wide ask row, every proposed lane not yet
 * materialized — all carry `direction_id === 0`, so an id-keyed collection
 * would collapse them into one and drop the rest off the screen entirely.
 */
export function laneWaves(lanes: ChangeSetLane[]): ChangeSetWave[] {
  // Only a materialized lane can be depended ON: `direction_id === 0` names no
  // lane, and the backend already drops those edges, so nothing points here.
  const positionsById = new Map<number, number[]>();
  lanes.forEach((lane, position) => {
    if (lane.direction_id === 0) return;
    const existing = positionsById.get(lane.direction_id);
    if (existing) {
      existing.push(position);
      return;
    }
    positionsById.set(lane.direction_id, [position]);
  });

  const pending = new Set<number>(lanes.map((_lane, position) => position));
  const blockers = new Map<number, Set<number>>();
  lanes.forEach((lane, position) => {
    const blocking = new Set<number>();
    for (const upstreamId of lane.depends_on) {
      for (const upstreamPosition of positionsById.get(upstreamId) ?? []) {
        // A self-edge would deadlock a lane against itself forever.
        if (upstreamPosition !== position) blocking.add(upstreamPosition);
      }
    }
    blockers.set(position, blocking);
  });

  const waves: ChangeSetWave[] = [];
  while (pending.size > 0) {
    const ready = [...pending].filter(
      (position) => (blockers.get(position)?.size ?? 0) === 0,
    );
    if (ready.length === 0) {
      // Every lane left is in or behind a cycle. Emit them as one wave.
      waves.push({ lanes: [...pending].map((position) => lanes[position]) });
      break;
    }
    waves.push({ lanes: ready.map((position) => lanes[position]) });
    for (const position of ready) {
      pending.delete(position);
    }
    for (const remaining of blockers.values()) {
      for (const position of ready) {
        remaining.delete(position);
      }
    }
  }
  return waves;
}

/** One observed checkout paired with the declared branch it is judged against. */
export interface CheckoutRowView {
  checkout: LaneCheckout;
  /**
   * Whether this row's branch is the declared one. A DISPLAY highlight that
   * points at the row worth reading — the lane's verdict stays
   * `reconciliation`, which the backend decided. An unsampled row is `null`:
   * it agrees with nothing and disagrees with nothing.
   */
  matchesDeclared: boolean | null;
}

/**
 * One discriminated checkout state per lane, derived once.
 *
 * `not_probed` and `none_registered` are deliberately separate: the backend
 * keeps `null` and `[]` apart precisely because "we did not look" and "there
 * is nothing there yet" need different next actions, and merging them here
 * would throw that away one layer above where it was preserved.
 */
export type LaneCheckoutView =
  | { kind: "not_probed" }
  | { kind: "none_registered" }
  | { kind: "matched"; rows: CheckoutRowView[] }
  | { kind: "drifted"; rows: CheckoutRowView[] }
  | { kind: "unknown"; rows: CheckoutRowView[] };

export function laneCheckoutView(lane: ChangeSetLane): LaneCheckoutView {
  const observed = lane.checkout.observed;
  if (observed === null) return { kind: "not_probed" };
  if (observed.length === 0) return { kind: "none_registered" };
  // A blank declared branch is not a branch to differ FROM. The backend
  // refuses to judge that case at all (`reconciliation_for` returns Unknown
  // for an empty `direction.branch`), so pointing at a row as "differs" here
  // would be this file inventing a comparison readiness declined to make.
  const declared = lane.checkout.declared_branch.trim();
  const rows: CheckoutRowView[] = observed.map((checkout) => ({
    checkout,
    matchesDeclared: matchesDeclaredBranch(checkout, declared),
  }));
  switch (lane.reconciliation) {
    case "matched":
      return { kind: "matched", rows };
    case "drifted":
      return { kind: "drifted", rows };
    case "unknown":
      return { kind: "unknown", rows };
  }
}

function matchesDeclaredBranch(checkout: LaneCheckout, declaredBranch: string): boolean | null {
  if (!declaredBranch) return null;
  if (!checkout.observed) return null;
  return checkout.observed.branch === declaredBranch;
}

/** Whether a lane has any evidence rows at all — `0/0/0` is "none recorded",
 *  which is not the same as "recorded and untrustworthy". */
export function hasEvidence(lane: ChangeSetLane): boolean {
  const { fresh, stale, unknown } = lane.evidence;
  return fresh + stale + unknown > 0;
}

/**
 * The lane's stored lifecycle status as ONE discriminated value.
 *
 * `direction_status` is free text in the store, so an unrecognized token must
 * still render: it maps to `unknown` rather than being printed raw, which
 * would put an untranslated backend token on screen (CLAUDE.md: user-facing
 * strings go only through the i18n files).
 */
export type LaneStatusView = "queued" | "planning" | "working" | "review" | "done" | "unknown";

const LANE_STATUSES: Record<string, LaneStatusView> = {
  queued: "queued",
  planning: "planning",
  working: "working",
  review: "review",
  done: "done",
};

export function laneStatusView(directionStatus: string): LaneStatusView {
  return LANE_STATUSES[directionStatus.trim()] ?? "unknown";
}
