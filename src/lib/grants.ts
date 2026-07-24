import type { GrantSnapshot } from "./types";

/** Which standing authorization an issue inherited: Full access, always-allow
 *  rules, or both. Drives the board's "Inherited access" chip copy. */
export type InheritedKind = "full" | "always" | "both";

export interface InheritedAccess {
  kind: InheritedKind;
  /** how many always-allow rules the issue's tasks hold (0 when kind is "full") */
  alwaysCount: number;
}

/** The single source of truth for "does this issue carry inherited access, and
 *  of what kind" — used by the kanban card to gate the chip and by the chip to
 *  pick accurate copy. Grants key on thread id, so only this thread's entries
 *  count. Returns null when the issue holds no standing grants. */
export function inheritedAccessOf(
  grants: GrantSnapshot,
  threadId: number,
): InheritedAccess | null {
  const hasFull = grants.full.some((g) => g.thread === threadId);
  const alwaysCount = grants.always.filter((g) => g.thread === threadId).length;
  if (hasFull && alwaysCount > 0) return { kind: "both", alwaysCount };
  if (hasFull) return { kind: "full", alwaysCount: 0 };
  if (alwaysCount > 0) return { kind: "always", alwaysCount };
  return null;
}
