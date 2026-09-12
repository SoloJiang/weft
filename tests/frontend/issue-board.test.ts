import assert from "node:assert/strict";
import test from "node:test";
import {
  deriveIssueStatus,
  groupIssueBoard,
  normalizeDirectionStatus,
  toIssueBoardCard,
} from "../../src/lib/issue-board.ts";

test("empty issues stay queued", () => {
  assert.equal(deriveIssueStatus([]), "queued");
});

test("all-done issues land in done", () => {
  assert.equal(deriveIssueStatus(["done", "done"]), "done");
});

test("issue status follows the most advanced open task", () => {
  assert.equal(deriveIssueStatus(["queued", "working", "done"]), "working");
  assert.equal(deriveIssueStatus(["working", "review"]), "review");
});

test("legacy planning rows count as working", () => {
  assert.equal(normalizeDirectionStatus("planning"), "working");
  assert.equal(normalizeDirectionStatus("working"), "working");
  assert.equal(normalizeDirectionStatus("unknown"), "queued");
  assert.equal(deriveIssueStatus(["planning"]), "working");
});

test("issue cards summarize progress", () => {
  const card = toIssueBoardCard({
    thread_id: 12,
    direction_ids: [1, 2, 3],
    statuses: ["working", "done", "queued"],
  });
  assert.equal(card.status, "working");
  assert.equal(card.totalTasks, 3);
  assert.equal(card.doneTasks, 1);
  assert.equal(card.threadId, 12);
});

test("groupIssueBoard fills every column", () => {
  const groups = groupIssueBoard([
    toIssueBoardCard({ thread_id: 1, direction_ids: [], statuses: [] }),
    toIssueBoardCard({ thread_id: 2, direction_ids: [1], statuses: ["review"] }),
  ]);
  assert.equal(groups.queued.length, 1);
  assert.equal(groups.review.length, 1);
  assert.equal(groups.working.length, 0);
  assert.equal(groups.done.length, 0);
});
