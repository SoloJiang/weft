import { normalizeDirectionStatus } from "./issue-board.ts";

export type DirectionCardPrimary = "handle" | "viewChanges" | "openSession";

/** One primary card action. Needs You stays first; review opens the diff. */
export function directionCardPrimary(hasNeed: boolean, status: string): DirectionCardPrimary {
  if (hasNeed) return "handle";
  if (status === "review") return "viewChanges";
  return "openSession";
}

/** Same rule as `weft_scheduler::can_complete`: only review → done. */
export function canComplete(status: string): boolean {
  return normalizeDirectionStatus(status) === "review";
}

/** Continue is offered after the worker has a result to talk about. */
export function canContinueDirection(status: string): boolean {
  const normalized = normalizeDirectionStatus(status);
  return normalized === "review" || normalized === "done";
}
