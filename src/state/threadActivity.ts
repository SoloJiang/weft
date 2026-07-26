import type { SessionStatus, TurnState } from "../lib/types";

/** The worker session fields needed to derive thread-level activity. */
export interface ThreadActivitySession {
  directionId: number;
  status: SessionStatus;
}

/** A canonical, exhaustive view of the activity owned by one thread. */
export type ThreadActivityView =
  | { kind: "idle"; running: 0; stalled: 0 }
  | { kind: "running"; running: number; stalled: 0 }
  | { kind: "stalled"; running: 0; stalled: number }
  | { kind: "mixed"; running: number; stalled: number };

export interface ThreadActivityInput {
  workerSessions: Iterable<ThreadActivitySession>;
  directionIds: readonly number[];
  leadState: TurnState | undefined;
}

function viewForCounts(running: number, stalled: number): ThreadActivityView {
  if (running === 0 && stalled === 0) {
    return { kind: "idle", running: 0, stalled: 0 };
  }
  if (stalled === 0) {
    return { kind: "running", running, stalled: 0 };
  }
  if (running === 0) {
    return { kind: "stalled", running: 0, stalled };
  }
  return { kind: "mixed", running, stalled };
}

/**
 * Derive thread/lead/worker liveness without depending on React or the store.
 * Idle and exited worker sessions are deliberately ignored: they are not
 * evidence that a thread is still active.
 */
export function selectThreadActivity({
  workerSessions,
  directionIds,
  leadState,
}: ThreadActivityInput): ThreadActivityView {
  const threadDirections = new Set(directionIds);
  let running = 0;
  let stalled = 0;

  for (const session of workerSessions) {
    if (!threadDirections.has(session.directionId)) continue;
    switch (session.status) {
      case "running":
        running += 1;
        break;
      case "stalled":
        stalled += 1;
        break;
      case "idle":
      case "exited":
        break;
    }
  }

  switch (leadState) {
    case "busy":
      running += 1;
      break;
    case "stalled":
      stalled += 1;
      break;
    case "idle":
    case "stopped":
    case undefined:
      break;
  }

  return viewForCounts(running, stalled);
}
