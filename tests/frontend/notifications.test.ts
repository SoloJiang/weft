import test from "node:test";
import assert from "node:assert/strict";
import {
  badgeCountFrom,
  DEFAULT_NOTIFY_CATEGORIES,
  DEFAULT_QUIET_HOURS,
  diffForNotifications,
  emptyNotifySnapshot,
  formatQuietTime,
  isAppInForeground,
  isInQuietHours,
  notifyCopyKeys,
  planNotifyOpen,
  parseNotifyCategories,
  parseQuietHours,
  parseQuietTime,
  serializeNotifyCategories,
  serializeQuietHours,
  snapshotOf,
  snapshotOfAttentionSnapshots,
} from "../../src/lib/notificationsCore.ts";
import type {
  AttentionItem,
  AttentionSnapshot,
  ProcessQuotaStatus,
  ThreadOverview,
} from "../../src/lib/types.ts";

const question: AttentionItem = {
  kind: "question",
  id: "question:7",
  revision: "1",
  created_at: "100",
  request_id: 7,
  thread_id: 1,
  thread_title: "Issue A",
  direction_id: 10,
  direction_name: "task-1",
  text: "Which release?",
};

const permission: AttentionItem = {
  kind: "permission",
  id: "permission:3",
  revision: "1",
  created_at: "101",
  ask: {
    id: 3,
    thread: 1,
    dir: "10",
    tool: "Bash",
    summary: "rm",
    detail: "rm -rf",
    risk: "write",
    ts: 1,
    thread_title: "Issue A",
    dir_name: "task-1",
    workspace_id: 9,
  },
};

function overview(statuses: string[], ids: number[]): ThreadOverview {
  return {
    thread_id: 1,
    title: "Issue A",
    kind: "feature",
    direction_ids: ids,
    statuses,
    write_repos: [],
  };
}

function quota(status: ProcessQuotaStatus["status"], seq: number): ProcessQuotaStatus {
  return {
    status,
    processCount: 900,
    processLimit: 1000,
    usagePercent: 90,
    warningPercent: 80,
    degradedPercent: 90,
    recoveryPercent: 70,
    transitionSeq: seq,
  };
}

test("canonical attention identity drives notification keys, route, and badge once", () => {
  const items = [question, permission];
  const snap = snapshotOf(items, [overview(["working", "review"], [11, 12])], null, 9);
  assert.deepEqual([...snap.needs.keys()], ["question:7", "permission:3"]);
  assert.equal(snap.needs.get("question:7")?.route.attentionId, "question:7");
  assert.equal(snap.needs.get("question:7")?.route.openNeeds, true);
  assert.equal(snap.needs.get("permission:3")?.route.workspaceId, 9);
  assert.equal(snap.review.get("rev:12")?.route.directionId, 12);
  assert.equal(snap.review.has("rev:11"), false);
  assert.equal(badgeCountFrom(items), 2);
});

test("stable attention ids dedupe refreshes and notify only a genuinely new action", () => {
  const prev = snapshotOf([question], [], null, 9);
  const sameRevision = snapshotOf([{ ...question, revision: "2" }], [], null, 9);
  assert.deepEqual(diffForNotifications(prev, sameRevision), []);

  const nextQuestion = { ...question, id: "question:8", request_id: 8 };
  const next = snapshotOf([question, nextQuestion], [], null, 9);
  const events = diffForNotifications(prev, next);
  assert.equal(events.length, 1);
  assert.equal(events[0]?.kind, "needs");
  assert.equal(events[0]?.count, 1);
  assert.equal(events[0]?.route.attentionId, "question:8");
});

test("inactive workspace actions participate in the global Needs notification diff", () => {
  const snapshots = (backgroundItems: AttentionItem[]): AttentionSnapshot[] => [
    { workspace_id: 9, count: 1, items: [question] },
    { workspace_id: 10, count: backgroundItems.length, items: backgroundItems },
  ];
  const prev = snapshotOfAttentionSnapshots(snapshots([]), [], null);
  const background = {
    ...question,
    id: "question:80",
    request_id: 80,
    thread_id: 8,
    thread_title: "Background issue",
  };
  const next = snapshotOfAttentionSnapshots(snapshots([background]), [], null);

  const events = diffForNotifications(prev, next);
  assert.equal(events.length, 1);
  assert.equal(events[0]?.route.workspaceId, 10);
  assert.equal(events[0]?.route.threadId, 8);
  assert.equal(events[0]?.route.attentionId, "question:80");
});

test("quota notification is transition-keyed and warnings remain silent", () => {
  const degraded = snapshotOf([], [], quota("degraded", 4));
  assert.equal(degraded.quota.has("quota:degraded:4"), true);
  assert.equal(snapshotOf([], [], quota("warning", 5)).quota.size, 0);
});

test("category parsing has no removed stalled category", () => {
  assert.deepEqual(parseNotifyCategories(null), DEFAULT_NOTIFY_CATEGORIES);
  const partial = parseNotifyCategories(JSON.stringify({ needs: false, stalled: true }));
  assert.deepEqual(partial, { needs: false, review: true, quota: true });
  assert.equal(serializeNotifyCategories(partial), JSON.stringify(partial));
  assert.equal(notifyCopyKeys("needs").title, "notify.needsTitle");
  assert.equal(notifyCopyKeys("review").title, "notify.reviewTitle");
  assert.equal(notifyCopyKeys("quota").title, "notify.quotaTitle");
});

test("notification clicks route canonical Needs to its workspace queue", () => {
  assert.deepEqual(
    planNotifyOpen({ kind: "needs", workspaceId: 9, attentionId: "question:7" }),
    [{ type: "workspace", workspaceId: 9 }, { type: "needs" }],
  );
  assert.deepEqual(planNotifyOpen({ kind: "quota", workspaceId: 9 }), [
    { type: "resources" },
  ]);
  assert.deepEqual(
    planNotifyOpen({ kind: "review", threadId: 1, directionId: 10 }),
    [{ type: "direction", threadId: 1, direction: "10", repoId: undefined, sessionId: undefined }],
  );
});

test("quiet-hour and foreground helpers remain deterministic", () => {
  assert.equal(formatQuietTime(8 * 60 + 5), "08:05");
  assert.equal(parseQuietTime("08:05"), 485);
  assert.equal(parseQuietTime("25:00"), null);
  const wrapped = { enabled: true, startMin: 22 * 60, endMin: 8 * 60 };
  assert.equal(isInQuietHours(wrapped, new Date(2026, 0, 1, 23, 0)), true);
  assert.equal(isInQuietHours(wrapped, new Date(2026, 0, 1, 12, 0)), false);
  assert.deepEqual(parseQuietHours(serializeQuietHours(wrapped)), wrapped);
  assert.deepEqual(parseQuietHours(null), DEFAULT_QUIET_HOURS);
  assert.equal(isAppInForeground({ windowFocused: true, documentFocused: false }), true);
  assert.equal(isAppInForeground({ windowFocused: false, documentFocused: true }), false);
  assert.deepEqual(emptyNotifySnapshot().needs.size, 0);
});
