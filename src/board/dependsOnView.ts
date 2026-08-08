/**
 * Whether/what to show for a lane's `depends_on` edge (issue #110 T4; upgraded to a join of
 * zero to many upstreams by issue #173), shared by `ScopeReview`'s batch dialog and NeedsRows'
 * per-lane Needs-you card — the two surfaces a human approves a task split through. Before this
 * existed, the edge that decides whether a consumer's merge stays blocked was invisible at the
 * exact moment the human approved the split: a mistaken `depends_on` (a typo, or a name that
 * resolves to the wrong producer) could be approved without the human ever seeing which task(s)
 * would gate the consumer (Codex review, PR #159 planner.rs:109).
 *
 * A pure module (no React/lucide-react import — see `needsCardView.ts` for the same
 * rationale) so this stays testable with the plain `node --test` runner and both call sites
 * share ONE answer to "is there a dependency to show, and what does it say" instead of
 * re-deriving it.
 */

/**
 * One display label for a lane's `depends_on` NAMES, or `null` when there is none to show.
 * Trims each name and drops empty ones — the backend's own edge resolution does the same
 * (`planner::resolve_depends_on_indices` reads each entry trimmed), so the UI must not show a
 * phantom dependency for a value that resolves to "no upstream" server-side either.
 *
 * Renders ONE label for N names (issue #173: no graph drawing, adjacency text only) by joining
 * every trimmed, non-empty name with ", " — "Waits for A" for one upstream, "Waits for A, B" for
 * a join of two. Callers pass `dependsOn.length` as the i18n `count` so the surrounding phrase
 * can pluralize ("waits for" vs "all wait for") independently of this label text.
 */
export function dependsOnLabel(dependsOn: string[] | undefined | null): string | null {
  const names = (dependsOn ?? []).map((name) => name.trim()).filter((name) => name !== "");
  return names.length === 0 ? null : names.join(", ");
}
