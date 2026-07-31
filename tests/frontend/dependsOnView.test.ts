import test from "node:test";
import assert from "node:assert/strict";
import { dependsOnLabel } from "../../src/board/dependsOnView.ts";

// Codex review, PR #159 planner.rs:109: the human approving a task split (via ScopeReview's
// batch dialog, or NeedsRows' per-lane Needs-you card) could not see which producer gates a
// consumer's merge — the edge was already decided in the data, just never rendered. Both
// surfaces share this ONE function to decide whether/what to show, so these tests cover the
// actual decision both call sites render from.

test("a non-empty depends_on returns the trimmed name", () => {
  assert.equal(dependsOnLabel("producer"), "producer");
  assert.equal(dependsOnLabel("  producer  "), "producer", "surrounding whitespace is trimmed");
});

test("an empty depends_on returns null — no dependency to show", () => {
  assert.equal(dependsOnLabel(""), null);
});

test("a whitespace-only depends_on returns null too", () => {
  // The backend's own edge resolution trims before deciding "is this empty"
  // (planner::record_upstream_edges reads `lane.depends_on.trim()`) — the UI must not show a
  // phantom dependency for a value that resolves to "no upstream" server-side.
  assert.equal(dependsOnLabel("   "), null);
});

test("undefined or null (a defensive fallback, e.g. stale cached state) returns null", () => {
  assert.equal(dependsOnLabel(undefined), null);
  assert.equal(dependsOnLabel(null), null);
});

test("a name containing internal whitespace is preserved, only the ends are trimmed", () => {
  assert.equal(dependsOnLabel("  the producer task  "), "the producer task");
});
