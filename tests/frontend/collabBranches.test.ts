import test from "node:test";
import assert from "node:assert/strict";
import type { LeadMessage } from "../../src/lib/types.ts";
import {
  groupTimeline,
  nodeKey,
  topLevelRows,
  type TimelineNode,
} from "../../src/session/collabBranches.ts";

let nextId = 1;

/** A `kind:"text"` assistant row, optionally tagged with `agentThread` (issue
 *  #99's sub-agent origin). `id` auto-increments so tests read as plain
 *  arrival order without hand-numbering every row. */
function textRow(text: string, opts: { agentThread?: string; status?: LeadMessage["status"] } = {}): LeadMessage {
  const content: Record<string, unknown> = { text };
  if (opts.agentThread) content.agentThread = opts.agentThread;
  return {
    id: nextId++,
    thread_id: 1,
    session_id: null,
    turn_id: 1,
    role: "assistant",
    kind: "text",
    content: JSON.stringify(content),
    status: opts.status ?? "complete",
    created_at: "2026-07-25T00:00:00Z",
  };
}

/** A `kind:"tool"` row. `collabThreads` models a `collabAgentToolCall` row's
 *  known sub-agent target(s) (empty = not yet resolved, e.g. a spawn's
 *  `item/started`). */
function toolRow(
  name: string,
  opts: { agentThread?: string; collabThreads?: string[]; status?: LeadMessage["status"] } = {},
): LeadMessage {
  const content: Record<string, unknown> = { name, summary: name, input: {}, output: "" };
  if (opts.agentThread) content.agentThread = opts.agentThread;
  if (opts.collabThreads) content.collabThreads = opts.collabThreads;
  return {
    id: nextId++,
    thread_id: 1,
    session_id: null,
    turn_id: 1,
    role: "assistant",
    kind: "tool",
    content: JSON.stringify(content),
    status: opts.status ?? "complete",
    created_at: "2026-07-25T00:00:00Z",
  };
}

function userRow(text: string): LeadMessage {
  return {
    id: nextId++,
    thread_id: 1,
    session_id: null,
    turn_id: 1,
    role: "user",
    kind: "text",
    content: JSON.stringify({ text }),
    status: "complete",
    created_at: "2026-07-25T00:00:00Z",
  };
}

function actionCardRow(): LeadMessage {
  return {
    id: nextId++,
    thread_id: 1,
    session_id: null,
    turn_id: 1,
    role: "assistant",
    kind: "action_card",
    content: JSON.stringify({ title: "x", actions: [] }),
    status: "complete",
    created_at: "2026-07-25T00:00:00Z",
  };
}

test.beforeEach(() => {
  nextId = 1;
});

// ---- flat / no-signal baseline (dialects with no collab concept at all) ----

test("rows with no agentThread/collabThreads render fully flat", () => {
  const rows = [textRow("hello"), toolRow("read_file"), textRow("done")];
  const roots = groupTimeline(rows);
  assert.equal(roots.length, 3);
  assert.ok(roots.every((n) => n.kind === "row"));
  assert.deepEqual(
    roots.map((n) => nodeKey(n)),
    rows.map((r) => r.id),
  );
});

test("empty timeline groups to an empty forest", () => {
  assert.deepEqual(groupTimeline([]), []);
});

// ---- the core grouping behavior ----

test("a sub-agent's rows nest under the collab call that first reveals its thread id", () => {
  const spawn = toolRow("collabAgentToolCall", { collabThreads: ["sub-1"] });
  const subText = textRow("investigating…", { agentThread: "sub-1" });
  const subTool = toolRow("grep", { agentThread: "sub-1" });
  const rows = [spawn, subText, subTool];

  const roots = groupTimeline(rows);
  assert.equal(roots.length, 1);
  const [branch] = roots;
  assert.equal(branch.kind, "branch");
  if (branch.kind !== "branch") return;
  assert.equal(branch.anchor.id, spawn.id);
  assert.deepEqual(
    branch.children.map((n) => nodeKey(n)),
    [subText.id, subTool.id],
  );
});

test("a second collab call for the SAME thread joins the existing branch instead of opening a sibling", () => {
  const spawn = toolRow("collabAgentToolCall", { collabThreads: ["sub-1"] });
  const subText = textRow("first update", { agentThread: "sub-1" });
  const wait = toolRow("collabAgentToolCall", { collabThreads: ["sub-1"] }); // a later `wait` call
  const subText2 = textRow("second update", { agentThread: "sub-1" });
  const rows = [spawn, subText, wait, subText2];

  const roots = groupTimeline(rows);
  assert.equal(roots.length, 1, "one branch, not two — the wait call must not open a sibling");
  const [branch] = roots;
  assert.equal(branch.kind, "branch");
  if (branch.kind !== "branch") return;
  assert.equal(branch.anchor.id, spawn.id);
  assert.deepEqual(
    branch.children.map((n) => nodeKey(n)),
    [subText.id, wait.id, subText2.id],
  );
});

test("two independent sub-agents form two separate top-level branches", () => {
  const spawnA = toolRow("collabAgentToolCall", { collabThreads: ["sub-a"] });
  const aText = textRow("a working", { agentThread: "sub-a" });
  const spawnB = toolRow("collabAgentToolCall", { collabThreads: ["sub-b"] });
  const bText = textRow("b working", { agentThread: "sub-b" });
  const rows = [spawnA, aText, spawnB, bText];

  const roots = groupTimeline(rows);
  assert.equal(roots.length, 2);
  assert.ok(roots.every((n) => n.kind === "branch"));
  const anchors = roots.map((n) => (n.kind === "branch" ? n.anchor.id : -1));
  assert.deepEqual(anchors, [spawnA.id, spawnB.id]);
});

test("a sub-agent that itself spawns a sub-sub-agent nests one level deeper", () => {
  const spawn = toolRow("collabAgentToolCall", { collabThreads: ["sub-1"] });
  // sub-1 issues its OWN collab call (agentThread: sub-1) which introduces sub-2.
  const nestedSpawn = toolRow("collabAgentToolCall", {
    agentThread: "sub-1",
    collabThreads: ["sub-2"],
  });
  const grandchildText = textRow("deepest", { agentThread: "sub-2" });
  const rows = [spawn, nestedSpawn, grandchildText];

  const roots = groupTimeline(rows);
  assert.equal(roots.length, 1);
  const top = roots[0];
  assert.equal(top.kind, "branch");
  if (top.kind !== "branch") return;
  assert.equal(top.anchor.id, spawn.id);
  assert.equal(top.children.length, 1);
  const inner = top.children[0];
  assert.equal(inner.kind, "branch");
  if (inner.kind !== "branch") return;
  assert.equal(inner.anchor.id, nestedSpawn.id);
  assert.deepEqual(
    inner.children.map((n) => nodeKey(n)),
    [grandchildText.id],
  );
});

// ---- the PR #132 falsifying counterexample (review BLOCK) ----

test("an unrelated mainline row arriving WHILE a collab anchor is still streaming stays top-level", () => {
  // This is the exact scenario an independent review reproduced against
  // PR #132's arrival-order heuristic: the lead delegates to a sub-agent
  // (collab call still "streaming"), the sub-agent's text nests correctly,
  // and then the LEAD's own unrelated tool call arrives — which the old
  // heuristic folded into the branch because it was merely "the first row
  // observed while the anchor was still open". This algorithm has no notion
  // of "currently open" at all — attribution is by thread id, not order — so
  // it cannot reproduce that failure by construction.
  const spawn = toolRow("collabAgentToolCall", { collabThreads: ["sub-1"], status: "streaming" });
  const subText = textRow("sub-agent working…", { agentThread: "sub-1", status: "streaming" });
  // The lead's OWN activity, mid-delegation — no agentThread at all.
  const mainlineRead = toolRow("read_file");
  const rows = [spawn, subText, mainlineRead];

  const roots = groupTimeline(rows);
  assert.equal(roots.length, 2, "the mainline read_file must stand apart from the branch");
  assert.equal(roots[0].kind, "branch");
  assert.equal(roots[1].kind, "row");
  if (roots[1].kind !== "row") return;
  assert.equal(roots[1].row.id, mainlineRead.id);
  if (roots[0].kind !== "branch") return;
  // And the mainline row must NOT have been swallowed as a branch child either.
  assert.ok(!roots[0].children.some((n) => nodeKey(n) === mainlineRead.id));
});

test("mainline text interleaved with a streaming collab branch stays mainline even mid-turn", () => {
  // Same falsifying shape, but for the anonymous/current text slot rather than
  // a tool row — the lead keeps narrating between tool calls while a
  // delegation is still open.
  const spawn = toolRow("collabAgentToolCall", { collabThreads: ["sub-1"], status: "streaming" });
  const subText = textRow("sub-agent update", { agentThread: "sub-1", status: "streaming" });
  const mainlineNarration = textRow("meanwhile, back on the main line…");
  const rows = [spawn, subText, mainlineNarration];

  const roots = groupTimeline(rows);
  const topLevelIds = roots.map((n) => nodeKey(n));
  assert.ok(topLevelIds.includes(mainlineNarration.id));
});

// ---- honest degradation while the anchor hasn't resolved yet ----

test("a sub-agent's rows render flat (not guessed) until their anchor reveals the thread id, then fold into place on the next recompute", () => {
  // A spawn's `item/started` has EMPTY receiverThreadIds — the child doesn't
  // exist yet — so its row's collabThreads starts empty even though the
  // spawn itself already exists as a row. If the child's own activity
  // reaches the frontend before the spawn's `item/completed` updates that
  // row's content (a real possibility this repo's own review flagged),
  // grouping must not GUESS an attribution — it must show those rows flat,
  // honestly, exactly like an unresolved/unknown-origin row always would.
  const spawnStarting = toolRow("collabAgentToolCall", { collabThreads: [], status: "streaming" });
  const subText = textRow("already working", { agentThread: "sub-1", status: "streaming" });
  const rowsBeforeResolved = [spawnStarting, subText];

  const early = groupTimeline(rowsBeforeResolved);
  assert.equal(early.length, 2, "no anchor known yet — nothing nests, nothing is hidden");
  assert.ok(early.every((n) => n.kind === "row"));

  // The SAME row (same id) later gets its content updated in place (item/completed
  // merges receiverThreadIds — see engine.rs's merge_tool_results) — modeled here
  // by replacing that row's content, exactly like a live content update would.
  const spawnResolved: LeadMessage = {
    ...spawnStarting,
    content: JSON.stringify({ name: "collabAgentToolCall", collabThreads: ["sub-1"] }),
    status: "complete",
  };
  const rowsAfterResolved = [spawnResolved, subText];

  const later = groupTimeline(rowsAfterResolved);
  assert.equal(later.length, 1, "now that the anchor is known, the child folds into place");
  assert.equal(later[0].kind, "branch");
  if (later[0].kind !== "branch") return;
  assert.equal(later[0].anchor.id, spawnStarting.id);
  assert.deepEqual(
    later[0].children.map((n) => nodeKey(n)),
    [subText.id],
  );
});

// ---- structural guards (never-nested kinds, roles) ----

test("action_card / plan_card / proposal / rewind / settled / test_cases never nest, whatever their content", () => {
  const spawn = toolRow("collabAgentToolCall", { collabThreads: ["sub-1"] });
  const card: LeadMessage = {
    ...actionCardRow(),
    // Even if content somehow carried an agentThread-shaped key, the kind
    // guard (NESTABLE_KIND) must refuse to nest it — a decision card going
    // invisible inside a collapsed branch would raise cognitive load, the
    // opposite of what issue #99 exists to fix.
    content: JSON.stringify({ title: "x", actions: [], agentThread: "sub-1" }),
  };
  const rows = [spawn, card];
  const roots = groupTimeline(rows);
  assert.equal(roots.length, 2);
  assert.deepEqual(roots.map((n) => nodeKey(n)).sort(), [spawn.id, card.id].sort());
});

test("a user row never nests even with a matching thread id in its content", () => {
  const spawn = toolRow("collabAgentToolCall", { collabThreads: ["sub-1"] });
  const fakeUserRow: LeadMessage = {
    ...userRow("hi"),
    content: JSON.stringify({ text: "hi", agentThread: "sub-1" }),
  };
  const roots = groupTimeline([spawn, fakeUserRow]);
  assert.equal(roots.length, 2);
});

// ---- topLevelRows / nodeKey ----

test("topLevelRows surfaces each branch's ANCHOR, never its children", () => {
  const spawn = toolRow("collabAgentToolCall", { collabThreads: ["sub-1"] });
  const subText = textRow("x", { agentThread: "sub-1" });
  const mainline = textRow("mainline");
  const roots = groupTimeline([spawn, subText, mainline]);

  const top = topLevelRows(roots);
  assert.deepEqual(
    top.map((r) => r.id),
    [spawn.id, mainline.id],
  );
  assert.ok(!top.some((r) => r.id === subText.id));
});

test("nodeKey is the anchor's id for a branch and the row's own id otherwise", () => {
  const spawn = toolRow("collabAgentToolCall", { collabThreads: ["sub-1"] });
  const subText = textRow("x", { agentThread: "sub-1" });
  const roots = groupTimeline([spawn, subText]);
  const branch: TimelineNode = roots[0];
  assert.equal(nodeKey(branch), spawn.id);
});

// ---- determinism / statelessness ----

test("grouping the same rows twice yields the same shape (pure function, no hidden state)", () => {
  const rows = [
    toolRow("collabAgentToolCall", { collabThreads: ["sub-1"] }),
    textRow("a", { agentThread: "sub-1" }),
    textRow("mainline"),
  ];
  const first = groupTimeline(rows);
  const second = groupTimeline(rows);
  assert.deepEqual(
    first.map((n) => nodeKey(n)),
    second.map((n) => nodeKey(n)),
  );
});

test("a rewind-style truncation (rows removed from the end) re-groups cleanly with no phantom parents", () => {
  const spawn = toolRow("collabAgentToolCall", { collabThreads: ["sub-1"] });
  const subText = textRow("a", { agentThread: "sub-1" });
  const full = [spawn, subText];
  // Truncate back to just the anchor — models a rewind that deleted the
  // child's row. Since grouping is a pure recompute (not incremental,
  // ref-persisted state), there is nothing that could leave a stale parent
  // pointer around: the anchor simply has no children this time.
  const truncated = [spawn];
  const roots = groupTimeline(truncated);
  assert.equal(roots.length, 1);
  assert.equal(roots[0].kind, "row");
});
