import type {
  IssueChangeSet,
  IssueReadiness,
  IssueReadinessDto,
  LaneReadiness,
  ReadinessReason,
} from "../lib/types";

/**
 * ONE read of an issue's delivery state, whichever backend command produced it.
 *
 * `issue_readiness` and `issue_change_set` run the SAME collection — including
 * the same bounded Git signature probe of every registered worktree. Firing
 * both for one thread therefore does not just cost twice as much: the two
 * collections contend for the same probes, and a probe that loses reports
 * `unknown`. The board would then paint a chip and a panel that disagree,
 * which is precisely what `change_set`'s "one collection, two projections"
 * contract exists to rule out. Measured on the running app: 0 of 12 lane
 * probes failed when the reads were sequential, 1 of 12 when a Change Set read
 * raced a readiness poll.
 *
 * So the board issues exactly one command per refresh — the richer one when
 * the Change Set tab is open — and both consumers read this single result.
 */
export interface IssueDelivery {
  readiness: IssueReadiness;
  reasons: ReadinessReason[];
  active_lane_count: number;
  /** Just enough per lane for the board's urgency sort, in either shape. */
  lanes: { direction_id: number; readiness: LaneReadiness }[];
  /**
   * The full Change Set, present only when the read that produced this was the
   * Change Set command. `null` means "this read did not carry it", never "the
   * issue has no change set".
   */
  changeSet: IssueChangeSet | null;
}

/** Which command a refresh should issue. Derived once, from the open tab. */
export type IssueDeliveryView = "readiness" | "changeSet";

export function deliveryFromReadiness(dto: IssueReadinessDto): IssueDelivery {
  return {
    readiness: dto.readiness,
    reasons: dto.reasons,
    active_lane_count: dto.active_lane_count,
    lanes: dto.lanes.map((lane) => ({
      direction_id: lane.direction_id,
      readiness: lane.readiness,
    })),
    changeSet: null,
  };
}

export function deliveryFromChangeSet(dto: IssueChangeSet): IssueDelivery {
  return {
    readiness: dto.readiness,
    reasons: dto.reasons,
    active_lane_count: dto.active_lane_count,
    lanes: dto.lanes.map((lane) => ({
      direction_id: lane.direction_id,
      readiness: lane.readiness,
    })),
    changeSet: dto,
  };
}
