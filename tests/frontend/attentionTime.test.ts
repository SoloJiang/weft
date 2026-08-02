import test from "node:test";
import assert from "node:assert/strict";
import type { ScopeApprovalAttentionItem } from "../../src/lib/types.ts";
import { attentionAge, attentionTimestampMilliseconds } from "../../src/board/attentionTime.ts";

const NOW = 1_700_007_200_000;

test("scope display time stays separate from an opaque nanos-seq revision", () => {
  const revision = "1700000000000000000-7";
  const item: ScopeApprovalAttentionItem = {
    kind: "scope_approval",
    id: `scope:12:${revision}`,
    revision,
    created_at: "1700000000",
    thread_id: 12,
    thread_title: "Issue",
  };

  assert.deepEqual(attentionAge(item.created_at, NOW), { kind: "hours", value: 2 });
  assert.equal(item.revision, revision, "age formatting leaves the OCC token byte-exact");
  assert.equal(attentionTimestampMilliseconds(item.revision, NOW), null);
});

test("legacy numeric timestamps still age while missing and opaque values fail safe", () => {
  assert.deepEqual(attentionAge("1700000000", NOW), { kind: "hours", value: 2 });
  assert.equal(attentionAge("1700000000000000000-7", NOW), null);
  assert.equal(attentionAge("1700000000000000000", NOW), null);
  assert.equal(attentionAge(null, NOW), null);
});
