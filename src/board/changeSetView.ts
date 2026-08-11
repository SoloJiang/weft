import type {
  ChangeSetLane,
  IssueChangeSet,
  LaneCheckout,
} from "../lib/types";

/**
 * Pure derivations for the Issue Change Set panel (issue #175).
 *
 * The backend already decided every verdict on this screen. Nothing in this
 * file re-decides one: it turns the DTO into the shapes the panel renders, and
 * each multi-way state is derived ONCE here as a discriminated value rather
 * than as booleans re-tested at each call site (CLAUDE.md).
 *
 * The fetch lives in the panel component; this stays a pure function of its
 * input, the same split `ReadinessChip`/`EvidenceBody` already use.
 */

/** One discriminated read state for the whole panel. */
export type ChangeSetPanelState =
  | { kind: "loading" }
  | { kind: "error" }
  | { kind: "empty" }
  | { kind: "ready"; changeSet: IssueChangeSet; waves: ChangeSetWave[] };

export function changeSetPanelState(
  fetchStatus: "loading" | "resolved" | "rejected",
  changeSet: IssueChangeSet | null,
): ChangeSetPanelState {
  if (fetchStatus === "loading") return { kind: "loading" };
  if (fetchStatus === "rejected") return { kind: "error" };
  if (!changeSet || changeSet.lanes.length === 0) return { kind: "empty" };
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
 */
export function laneWaves(lanes: ChangeSetLane[]): ChangeSetWave[] {
  const present = new Set(lanes.map((lane) => lane.direction_id));
  const pending = new Map<number, ChangeSetLane>();
  const blockers = new Map<number, Set<number>>();
  for (const lane of lanes) {
    pending.set(lane.direction_id, lane);
    blockers.set(
      lane.direction_id,
      new Set(lane.depends_on.filter((id) => id !== lane.direction_id && present.has(id))),
    );
  }

  const waves: ChangeSetWave[] = [];
  while (pending.size > 0) {
    const ready = [...pending.values()].filter(
      (lane) => (blockers.get(lane.direction_id)?.size ?? 0) === 0,
    );
    if (ready.length === 0) {
      // Every lane left is in or behind a cycle. Emit them as one wave.
      waves.push({ lanes: [...pending.values()] });
      break;
    }
    waves.push({ lanes: ready });
    for (const lane of ready) {
      pending.delete(lane.direction_id);
    }
    for (const remaining of blockers.values()) {
      for (const lane of ready) {
        remaining.delete(lane.direction_id);
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
  const rows: CheckoutRowView[] = observed.map((checkout) => ({
    checkout,
    matchesDeclared: checkout.observed
      ? checkout.observed.branch === lane.checkout.declared_branch
      : null,
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

/** Whether a lane has any evidence rows at all — `0/0/0` is "none recorded",
 *  which is not the same as "recorded and untrustworthy". */
export function hasEvidence(lane: ChangeSetLane): boolean {
  const { fresh, stale, unknown } = lane.evidence;
  return fresh + stale + unknown > 0;
}
