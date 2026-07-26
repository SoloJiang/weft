import type { ThreadActivityView } from "../../state/threadActivity";

export type ThreadActivityTone = "running" | "stalled";

export interface ThreadActivityMarker {
  kind: ThreadActivityTone;
  count: number;
}

/** Map the canonical view to the compact markers shared by thread surfaces. */
export function threadActivityMarkers(activity: ThreadActivityView): ThreadActivityMarker[] {
  switch (activity.kind) {
    case "idle":
      return [];
    case "running":
      return [{ kind: "running", count: activity.running }];
    case "stalled":
      return [{ kind: "stalled", count: activity.stalled }];
    case "mixed":
      return [
        { kind: "running", count: activity.running },
        { kind: "stalled", count: activity.stalled },
      ];
  }
}
