import type { GrantSnapshot, ReadOnlyGrants } from "./types";

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

/** A session's read-only auto-allow scope (issue #103) — a single discriminated
 *  value so every surface that renders "is this read-only-trusted, and how do I
 *  revoke it" maps the SAME three states instead of re-deriving booleans.
 *  "issue" wins over "session" when both are set (revoking "session" alone
 *  would be a no-op while the issue-wide grant still covers it — the UI must
 *  point at the grant that's actually doing the work). */
export type ReadOnlyScope = "issue" | "session" | "none";

/** The single source of truth for a (thread, dir) session's read-only scope —
 *  used by the session-info panel to show the right copy and by its revoke
 *  button to target the right grant. */
export function readOnlyScopeOf(
  grants: ReadOnlyGrants,
  thread: number,
  dir: string,
): ReadOnlyScope {
  if (grants.issue.includes(thread)) return "issue";
  if (grants.session.some((g) => g.thread === thread && g.dir === dir)) return "session";
  return "none";
}

/** The `dir` argument `revokeReadOnlyGrant` needs for a given scope: "issue"
 *  revokes the WHOLE issue's propagation (dir=null); "session" revokes just
 *  this one session (dir=dir). Single source so every caller maps the same
 *  `ReadOnlyScope` the same way, instead of inlining the choice at each call
 *  site (which nests a second ternary inside whatever conditional renders the
 *  revoke button in the first place). */
export function readOnlyRevokeDir(scope: ReadOnlyScope, dir: string): string | null {
  return scope === "issue" ? null : dir;
}
