/**
 * Whether/what to show for a lane's `depends_on` edge (issue #110 T4), shared by
 * `ScopeReview`'s batch dialog and `NeedsRows`' per-lane Needs-you card — the two surfaces
 * a human approves a task split through. Before this existed, the edge that decides whether
 * a consumer's merge stays blocked was invisible at the exact moment the human approved the
 * split: a mistaken `depends_on` (a typo, or a name that resolves to the wrong producer) could
 * be approved without the human ever seeing which task would gate the consumer (Codex review,
 * PR #159 planner.rs:109).
 *
 * A pure module (no React/lucide-react import — see `needsCardView.ts` for the same
 * rationale) so this stays testable with the plain `node --test` runner and both call sites
 * share ONE answer to "is there a dependency to show" instead of re-deriving it.
 */

/**
 * The producer name to show for a lane's `depends_on`, or `null` when there is none to show.
 * Trims whitespace-only values to nothing too — the backend's own edge resolution does the
 * same (`planner::record_upstream_edges` reads `lane.depends_on.trim()`), so the UI must not
 * show a phantom dependency for a value that resolves to "no upstream" server-side either.
 */
export function dependsOnLabel(dependsOn: string | undefined | null): string | null {
  const trimmed = (dependsOn ?? "").trim();
  return trimmed === "" ? null : trimmed;
}
