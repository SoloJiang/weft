import test from "node:test";
import assert from "node:assert/strict";
import type { LeadMessage } from "../../src/lib/types.ts";
import { applyLeadFinalize, mergeLeadSnapshot } from "../../src/state/leadSnapshot.ts";

function row(
  id: number,
  text: string,
  status: LeadMessage["status"] = "streaming",
  kind: LeadMessage["kind"] = "text",
): LeadMessage {
  return {
    id,
    thread_id: 1,
    role: "assistant",
    kind,
    content: JSON.stringify({ text }),
    status,
    created_at: "2026-07-16T00:00:00Z",
  } as LeadMessage;
}

test("keeps local text when it prefix-extends a still-streaming snapshot row", () => {
  const local = [row(1, "hello world, more")];
  const snap = [row(1, "hello world")];
  const merged = mergeLeadSnapshot(local, snap);
  assert.equal(merged.length, 1);
  assert.equal(merged[0], local[0]); // same object — no re-append, no copy
});

test("takes the snapshot when it finalized the row (cleaned body wins)", () => {
  const local = [row(1, "raw streamed text with sentinels")];
  const snap = [row(1, "cleaned text", "complete")];
  const merged = mergeLeadSnapshot(local, snap);
  assert.equal(merged[0], snap[0]);
});

test("takes the snapshot when local text diverges instead of extending", () => {
  const local = [row(1, "abcXYZ")];
  const snap = [row(1, "abcdef")];
  assert.equal(mergeLeadSnapshot(local, snap)[0], snap[0]);
});

test("takes the snapshot when local text is shorter", () => {
  const local = [row(1, "abc")];
  const snap = [row(1, "abcdef")];
  assert.equal(mergeLeadSnapshot(local, snap)[0], snap[0]);
});

test("keeps a locally-finalized row that extends a still-streaming snapshot", () => {
  const local = [row(1, "full final text", "complete")];
  const snap = [row(1, "full final")];
  const merged = mergeLeadSnapshot(local, snap);
  assert.equal(merged[0], local[0]);
  assert.equal(merged[0].status, "complete");
});

test("rows without a local counterpart come from the snapshot", () => {
  const snap = [row(1, "brand new")];
  assert.deepEqual(mergeLeadSnapshot([], snap), snap);
});

test("meta rows are filtered out", () => {
  const snap = [row(1, "visible"), row(2, "", "complete", "meta")];
  const merged = mergeLeadSnapshot([], snap);
  assert.deepEqual(merged.map((x) => x.id), [1]);
});

test("local-only rows are dropped (snapshot supersedes streaming state)", () => {
  const local = [row(1, "kept"), row(2, "local only")];
  const snap = [row(1, "kept")];
  assert.deepEqual(mergeLeadSnapshot(local, snap).map((x) => x.id), [1]);
});

test("a delivered queued row moves to delivery order in the same finalize update", () => {
  const first = row(1, "first", "complete");
  const queued = { ...row(2, "queued", "queued"), role: "user" as const };
  const approval = row(3, "approved", "complete", "settled");
  const tool = row(4, "ran", "complete", "tool");
  const primaryReply = row(5, "primary done", "complete");

  const finalized = applyLeadFinalize(
    [first, queued, approval, tool, primaryReply],
    queued.id,
    "complete",
    undefined,
    6,
  );

  assert.deepEqual(
    finalized.map((message) => message.id),
    [first.id, approval.id, tool.id, primaryReply.id, queued.id],
  );
  assert.equal(finalized.find((message) => message.id === queued.id)?.seq, 6);
  assert.equal(finalized.find((message) => message.id === queued.id)?.status, "complete");
});

test("an ordinary finalize preserves the current row order", () => {
  const later = row(2, "later", "streaming");
  const earlier = row(1, "earlier", "complete");

  const finalized = applyLeadFinalize([later, earlier], later.id, "complete");

  assert.deepEqual(finalized.map((message) => message.id), [later.id, earlier.id]);
  assert.equal(finalized[0]?.status, "complete");
});

test("non-text content falls back to the snapshot row", () => {
  const localTool = { ...row(3, "", "streaming", "tool"), content: '{"name":"grep"}' };
  const snapTool = { ...row(3, "", "streaming", "tool"), content: '{"name":"grep","out":"x"}' };
  assert.equal(mergeLeadSnapshot([localTool], [snapTool])[0], snapTool);
});
