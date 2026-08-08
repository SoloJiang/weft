import test from "node:test";
import assert from "node:assert/strict";
import { dependsOnLabel } from "../../src/board/dependsOnView.ts";

// Codex review, PR #159 planner.rs:109: the human approving a task split (via ScopeReview's
// batch dialog, or NeedsRows' per-lane Needs-you card) could not see which producer gates a
// consumer's merge — the edge was already decided in the data, just never rendered. Both
// surfaces share this ONE function to decide whether/what to show, so these tests cover the
// actual decision both call sites render from. Issue #173 upgraded `depends_on` from one name
// to a list (a join dependency), so this covers the multi-name shape too.

test("a single-name depends_on returns the trimmed name", () => {
  assert.equal(dependsOnLabel(["producer"]), "producer");
  assert.equal(dependsOnLabel(["  producer  "]), "producer", "surrounding whitespace is trimmed");
});

test("an empty depends_on array returns null — no dependency to show", () => {
  assert.equal(dependsOnLabel([]), null);
});

test("a whitespace-only single entry returns null too", () => {
  // The backend's own edge resolution trims before deciding "is this empty"
  // (`planner::resolve_depends_on_indices` reads each entry trimmed) — the UI must not show a
  // phantom dependency for a value that resolves to "no upstream" server-side.
  assert.equal(dependsOnLabel(["   "]), null);
});

test("undefined or null (a defensive fallback, e.g. stale cached state) returns null", () => {
  assert.equal(dependsOnLabel(undefined), null);
  assert.equal(dependsOnLabel(null), null);
});

test("a name containing internal whitespace is preserved, only the ends are trimmed", () => {
  assert.equal(dependsOnLabel(["  the producer task  "]), "the producer task");
});

// Issue #173 (R1-03): a join dependency — a Lane can wait on 2+ upstreams.
test("two or more names join into one label, comma-separated", () => {
  assert.equal(dependsOnLabel(["interface repo", "sdk repo"]), "interface repo, sdk repo");
  assert.equal(
    dependsOnLabel(["interface repo", "sdk repo", "release repo"]),
    "interface repo, sdk repo, release repo"
  );
});

test("a join dependency drops blank entries but keeps the rest, trimmed", () => {
  assert.equal(dependsOnLabel(["  a  ", "", "   ", "b"]), "a, b");
});

test("a join dependency where every entry is blank returns null", () => {
  assert.equal(dependsOnLabel(["", "   ", ""]), null);
});
