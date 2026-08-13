import test from "node:test";
import assert from "node:assert/strict";
import type { ChangeSetLane, IssueChangeSet, LaneCheckout } from "../../src/lib/types.ts";
import {
  deliveryFromChangeSet,
  deliveryFromReadiness,
} from "../../src/board/issueDelivery.ts";
import {
  changeSetPanelState,
  issueEvidenceView,
  waveStepNumbers,
  laneCheckoutView,
  laneEvidenceView,
  laneStatusView,
  laneWaves,
} from "../../src/board/changeSetView.ts";

function lane(overrides: Partial<ChangeSetLane> & { direction_id: number }): ChangeSetLane {
  return {
    name: `lane-${overrides.direction_id}`,
    repo_id: 1,
    repo_name: "primary",
    reason: "",
    materialized: true,
    checkout: { declared_base: "main", declared_branch: "weft/lane", observed: null },
    depends_on: [],
    direction_status: "working",
    reconciliation: "matched",
    checks: "not_applicable",
    upstream: "satisfied",
    pull_requests: [],
    evidence: { fresh: 0, stale: 0, unknown: 0, newest_observed_at: null },
    readiness: "unknown",
    reasons: [],
    ...overrides,
  };
}

function checkout(branch: string | null, repo = "primary"): LaneCheckout {
  return {
    repo_name: repo,
    path: `/tmp/${repo}`,
    observed: branch === null ? null : { branch, head_sha: "abcdef1234", dirty: false },
  };
}

function changeSet(lanes: ChangeSetLane[]): IssueChangeSet {
  return {
    readiness: "unknown",
    reasons: [],
    active_lane_count: lanes.length,
    lanes,
    issue_evidence: { fresh: 0, stale: 0, unknown: 0, newest_observed_at: null },
    evidence_scan_truncated: false,
  };
}

test("waves order lanes by their dependency edges, independents first", () => {
  const waves = laneWaves([
    lane({ direction_id: 3, depends_on: [2] }),
    lane({ direction_id: 1 }),
    lane({ direction_id: 2, depends_on: [1] }),
  ]);
  assert.deepEqual(
    waves.map((wave) => wave.lanes.map((l) => l.direction_id)),
    [[1], [2], [3]],
  );
});

test("lanes that wait on nothing share one wave and keep their incoming order", () => {
  const waves = laneWaves([
    lane({ direction_id: 5 }),
    lane({ direction_id: 4 }),
    lane({ direction_id: 9, depends_on: [4, 5] }),
  ]);
  assert.deepEqual(
    waves.map((wave) => wave.lanes.map((l) => l.direction_id)),
    [[5, 4], [9]],
  );
});

test("an edge pointing outside the change set does not hide its consumer", () => {
  // Lane 7's upstream was excluded from the verdict (inactive or denied).
  // Treating that edge as unsatisfiable would drop lane 7 from the panel.
  const waves = laneWaves([lane({ direction_id: 7, depends_on: [404] })]);
  assert.deepEqual(
    waves.map((wave) => wave.lanes.map((l) => l.direction_id)),
    [[7]],
  );
});

test("a dependency cycle is its own kind, never a parallel step", () => {
  const waves = laneWaves([
    lane({ direction_id: 1, depends_on: [2] }),
    lane({ direction_id: 2, depends_on: [1] }),
    lane({ direction_id: 3 }),
  ]);
  assert.deepEqual(
    waves.map((wave) => [wave.kind, wave.lanes.map((l) => l.direction_id)]),
    [
      ["parallel", [3]],
      ["cycle", [1, 2]],
    ],
  );
});

test("a lane blocked BEHIND a cycle is ordered after it, not beside it", () => {
  // A and B deadlock each other; C waits on A. Emitting all three together
  // would present C as free to proceed alongside its own blocked upstream.
  const waves = laneWaves([
    lane({ direction_id: 1, name: "a", depends_on: [2] }),
    lane({ direction_id: 2, name: "b", depends_on: [1] }),
    lane({ direction_id: 3, name: "c", depends_on: [1] }),
  ]);
  assert.deepEqual(
    waves.map((wave) => [wave.kind, wave.lanes.map((l) => l.name)]),
    [
      ["cycle", ["a", "b"]],
      ["parallel", ["c"]],
    ],
  );
});

test("an independent wave before a cycle keeps its own step, and the cycle takes none", () => {
  const waves = laneWaves([
    lane({ direction_id: 9, name: "independent" }),
    lane({ direction_id: 1, name: "a", depends_on: [2] }),
    lane({ direction_id: 2, name: "b", depends_on: [1] }),
  ]);
  assert.deepEqual(waves.map((w) => w.kind), ["parallel", "cycle"]);
  assert.deepEqual(waves[0].lanes.map((l) => l.name), ["independent"]);
});

test("every lane appears exactly once across the waves", () => {
  const lanes = [
    lane({ direction_id: 1 }),
    lane({ direction_id: 2, depends_on: [1] }),
    lane({ direction_id: 3, depends_on: [1, 2] }),
    lane({ direction_id: 4, depends_on: [9] }),
    lane({ direction_id: 5, depends_on: [6] }),
    lane({ direction_id: 6, depends_on: [5] }),
  ];
  const seen = laneWaves(lanes).flatMap((wave) => wave.lanes.map((l) => l.direction_id));
  assert.deepEqual([...seen].sort((a, b) => a - b), [1, 2, 3, 4, 5, 6]);
  assert.equal(new Set(seen).size, seen.length, "no lane is emitted twice");
});

test("a self-edge cannot deadlock its own lane", () => {
  const waves = laneWaves([lane({ direction_id: 8, depends_on: [8] })]);
  assert.deepEqual(
    waves.map((wave) => wave.lanes.map((l) => l.direction_id)),
    [[8]],
  );
});

test("virtual lanes sharing direction_id 0 all survive the layout", () => {
  // The unbound-PR row, the issue-wide ask row and every unmaterialized
  // proposed lane ALL carry direction_id 0. Tracking lanes by id would collapse
  // them into one and silently drop the rest off the panel.
  const waves = laneWaves([
    lane({ direction_id: 0, name: "unbound pr" }),
    lane({ direction_id: 0, name: "issue ask" }),
    lane({ direction_id: 0, name: "proposed api" }),
  ]);
  assert.deepEqual(
    waves.map((wave) => wave.lanes.map((l) => l.name)),
    [["unbound pr", "issue ask", "proposed api"]],
  );
});

test("virtual lanes still lay out beside real dependent lanes", () => {
  const waves = laneWaves([
    lane({ direction_id: 0, name: "unbound pr" }),
    lane({ direction_id: 4, name: "api" }),
    lane({ direction_id: 0, name: "issue ask" }),
    lane({ direction_id: 5, name: "ui", depends_on: [4] }),
  ]);
  assert.deepEqual(
    waves.map((wave) => wave.lanes.map((l) => l.name)),
    [["unbound pr", "api", "issue ask"], ["ui"]],
  );
});

test("a cycle among lanes that share direction_id 0 still emits each one once", () => {
  const waves = laneWaves([
    lane({ direction_id: 0, name: "virtual a" }),
    lane({ direction_id: 1, name: "x", depends_on: [2] }),
    lane({ direction_id: 0, name: "virtual b" }),
    lane({ direction_id: 2, name: "y", depends_on: [1] }),
  ]);
  const names = waves.flatMap((wave) => wave.lanes.map((l) => l.name));
  assert.deepEqual(names.sort(), ["virtual a", "virtual b", "x", "y"]);
  assert.equal(new Set(names).size, 4);
});

test("not-probed and probed-but-empty stay distinct checkout states", () => {
  assert.deepEqual(laneCheckoutView(lane({ direction_id: 1 })), { kind: "not_probed" });
  assert.deepEqual(
    laneCheckoutView(
      lane({
        direction_id: 1,
        checkout: { declared_base: "main", declared_branch: "weft/lane", observed: [] },
      }),
    ),
    { kind: "none_registered" },
  );
});

test("the checkout state comes from the backend verdict, not a re-derived comparison", () => {
  // Branches agree, but the backend said Unknown (one repo's probe failed).
  // The panel must show Unknown rather than promoting it to matched.
  const view = laneCheckoutView(
    lane({
      direction_id: 1,
      reconciliation: "unknown",
      checkout: {
        declared_base: "main",
        declared_branch: "weft/lane",
        observed: [checkout("weft/lane"), checkout(null, "second")],
      },
    }),
  );
  assert.equal(view.kind, "unknown");
  assert.equal(view.kind === "unknown" ? view.rows.length : 0, 2);
});

test("the declared-branch highlight marks the row that differs and never guesses on an unsampled one", () => {
  const view = laneCheckoutView(
    lane({
      direction_id: 1,
      reconciliation: "drifted",
      checkout: {
        declared_base: "main",
        declared_branch: "weft/lane",
        observed: [checkout("weft/lane"), checkout("weft/other", "second"), checkout(null, "third")],
      },
    }),
  );
  assert.equal(view.kind, "drifted");
  const rows = view.kind === "drifted" ? view.rows : [];
  assert.deepEqual(
    rows.map((row) => row.matchesDeclared),
    [true, false, null],
  );
});

test("panel state is one discriminated value across the fetch lifecycle", () => {
  assert.deepEqual(changeSetPanelState({ kind: "loading" }), { kind: "loading" });
  assert.deepEqual(changeSetPanelState({ kind: "failed" }), { kind: "error" });
  const emptyState = changeSetPanelState({
    kind: "ready",
    dto: deliveryFromChangeSet(changeSet([])),
  });
  assert.equal(emptyState.kind, "empty");

  const ready = changeSetPanelState({
    kind: "ready",
    dto: deliveryFromChangeSet(changeSet([lane({ direction_id: 1 })])),
  });
  assert.equal(ready.kind, "ready");
  assert.equal(ready.kind === "ready" ? ready.waves.length : 0, 1);
});

test("a readiness-only read reads as loading, never as an empty change set", () => {
  // The board issues ONE command per refresh. Right after the tab opens, the
  // read in hand may still be the readiness one, which carries no change set.
  // Calling that "empty" would claim this issue writes nothing.
  const readinessOnly = deliveryFromReadiness({
    readiness: "blocked",
    reasons: [{ code: "upstream_unmet", direction_id: 2 }],
    active_lane_count: 2,
    lanes: [
      { direction_id: 1, name: "api", readiness: "unknown", reasons: [] },
      { direction_id: 2, name: "ui", readiness: "blocked", reasons: [] },
    ],
  });
  assert.equal(readinessOnly.changeSet, null);
  assert.deepEqual(changeSetPanelState({ kind: "ready", dto: readinessOnly }), { kind: "loading" });
});

test("both reads feed the readiness chip identically, so chip and panel cannot disagree", () => {
  const cs = changeSet([lane({ direction_id: 1, readiness: "blocked" })]);
  cs.readiness = "blocked";
  cs.reasons = [{ code: "upstream_unmet", direction_id: 1 }];
  const fromChangeSet = deliveryFromChangeSet(cs);
  const fromReadiness = deliveryFromReadiness({
    readiness: cs.readiness,
    reasons: cs.reasons,
    active_lane_count: cs.active_lane_count,
    lanes: cs.lanes.map((l) => ({
      direction_id: l.direction_id,
      name: l.name,
      readiness: l.readiness,
      reasons: l.reasons,
    })),
  });
  assert.equal(fromChangeSet.readiness, fromReadiness.readiness);
  assert.deepEqual(fromChangeSet.reasons, fromReadiness.reasons);
  assert.deepEqual(fromChangeSet.lanes, fromReadiness.lanes);
});

test("a rejected read never re-shows the last payload as current", () => {
  // readinessKey.ts contract: a refresh never presents a prior verdict as
  // current evidence.
  assert.deepEqual(changeSetPanelState({ kind: "failed" }), { kind: "error" });
});

test("no evidence at all is distinct from evidence that is merely untrustworthy", () => {
  assert.deepEqual(laneEvidenceView(lane({ direction_id: 1 }), false), { kind: "none" });
  assert.deepEqual(
    laneEvidenceView(
      lane({
        direction_id: 1,
        evidence: { fresh: 0, stale: 0, unknown: 2, newest_observed_at: "1700000000" },
      }),
      false,
    ),
    { kind: "counts", fresh: 0, stale: 0, unknown: 2 },
  );
});

test("a truncated scan never lets an empty lane be reported as having no evidence", () => {
  // The scan bound is per ISSUE; a busy lane can push a quiet one's rows past
  // the cut. Calling that "no evidence recorded" turns an incomplete read into
  // a false assertion.
  assert.deepEqual(laneEvidenceView(lane({ direction_id: 1 }), true), { kind: "unscanned" });
  assert.deepEqual(
    laneEvidenceView(
      lane({
        direction_id: 1,
        evidence: { fresh: 3, stale: 1, unknown: 0, newest_observed_at: "1700000000" },
      }),
      true,
    ),
    { kind: "counts", fresh: 3, stale: 1, unknown: 0 },
  );
});

test("a blank declared branch is not something a checkout can differ from", () => {
  // `reconciliation_for` returns Unknown for an empty declared branch rather
  // than judging it. Marking a row "differs" here would invent a comparison
  // the backend declined to make.
  const view = laneCheckoutView(
    lane({
      direction_id: 1,
      reconciliation: "unknown",
      checkout: {
        declared_base: "main",
        declared_branch: "   ",
        observed: [checkout("weft/whatever")],
      },
    }),
  );
  assert.equal(view.kind, "unknown");
  assert.deepEqual(
    view.kind === "unknown" ? view.rows.map((r) => r.matchesDeclared) : [],
    [null],
  );
});

test("every stored lane status maps to a translatable value, unknown tokens included", () => {
  for (const status of ["queued", "planning", "working", "review", "done"]) {
    assert.equal(laneStatusView(lane({ direction_id: 1, direction_status: status })), status);
  }
  // Free text in the store must never reach the screen untranslated.
  assert.equal(
    laneStatusView(lane({ direction_id: 1, direction_status: "some-future-status" })),
    "unknown",
  );
  assert.equal(laneStatusView(lane({ direction_id: 1, direction_status: "" })), "unknown");
  assert.equal(laneStatusView(lane({ direction_id: 1, direction_status: "  review  " })), "review");
});

test("an unmaterialized lane never presents readiness sentinel as a lifecycle", () => {
  // `virtual_lane_facts` sets direction_status "working" purely so the verdict
  // fails closed. No worker is building anything, so labelling it "building"
  // would assert work that is not happening.
  const virtualLane = lane({
    direction_id: 0,
    name: "issue ask",
    materialized: false,
    direction_status: "working",
  });
  assert.equal(laneStatusView(virtualLane), "not_materialized");
});

test("an issue with no lanes still carries its issue-level evidence", () => {
  // Host rows for an unbound PR and issue-wide asks are recorded against the
  // issue, not a lane. Dropping the change set on the empty branch would make
  // the panel claim there is nothing to show while records exist.
  const cs = changeSet([]);
  cs.issue_evidence = { fresh: 2, stale: 1, unknown: 0, newest_observed_at: "1700000000" };
  const state = changeSetPanelState({ kind: "ready", dto: deliveryFromChangeSet(cs) });
  assert.equal(state.kind, "empty");
  assert.deepEqual(
    state.kind === "empty" ? state.changeSet.issue_evidence : null,
    { fresh: 2, stale: 1, unknown: 0, newest_observed_at: "1700000000" },
  );
});

test("issue-level evidence uses the same three-way rule as a lane", () => {
  const complete = changeSet([]);
  assert.deepEqual(issueEvidenceView(complete), { kind: "none" });

  const truncated = changeSet([]);
  truncated.evidence_scan_truncated = true;
  assert.deepEqual(
    issueEvidenceView(truncated),
    { kind: "unscanned" },
    "an all-zero count under a truncated scan is not a complete zero",
  );

  const counted = changeSet([]);
  counted.evidence_scan_truncated = true;
  counted.issue_evidence = { fresh: 1, stale: 2, unknown: 0, newest_observed_at: "1700000000" };
  assert.deepEqual(issueEvidenceView(counted), {
    kind: "counts",
    fresh: 1,
    stale: 2,
    unknown: 0,
  });
});

test("a cycle consumes no step number", () => {
  // An A<->B cycle followed by an executable wave must not render that wave as
  // "step 2 ... waits on the step above" when no step 1 was ever shown.
  assert.deepEqual(
    waveStepNumbers([
      { kind: "cycle", lanes: [] },
      { kind: "parallel", lanes: [] },
    ] as never),
    [null, 1],
  );
  assert.deepEqual(
    waveStepNumbers([
      { kind: "parallel", lanes: [] },
      { kind: "cycle", lanes: [] },
      { kind: "parallel", lanes: [] },
    ] as never),
    [1, null, 2],
    "an independent first wave must not make the wave after a cycle skip to step 3",
  );
});
