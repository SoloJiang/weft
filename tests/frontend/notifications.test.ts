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
  type NotifySnapshot,
} from "../../src/lib/notificationsCore.ts";
import type {
  NeedItem,
  PermissionAsk,
  ProcessQuotaStatus,
  ThreadOverview,
  WriteTrigger,
} from "../../src/lib/types.ts";

function need(partial: Partial<NeedItem> & Pick<NeedItem, "ask_id">): NeedItem {
  return {
    ask_id: partial.ask_id,
    thread_id: partial.thread_id ?? 1,
    direction_id: partial.direction_id ?? 10,
    thread_title: partial.thread_title ?? "Issue A",
    direction_name: partial.direction_name ?? "task-1",
    text: partial.text ?? "?",
    ts: partial.ts ?? 0,
    kind: partial.kind ?? "question",
  };
}

function ask(id: number): PermissionAsk {
  return {
    id,
    thread: 1,
    dir: "10",
    tool: "Bash",
    summary: "rm",
    detail: "rm -rf",
    risk: "write",
    ts: 0,
    thread_title: "Issue A",
    dir_name: "task-1",
  };
}

function wt(index: number): WriteTrigger {
  return {
    thread_id: 1,
    index,
    name: "lane",
    repo_name: "weft",
    reason: "x",
    thread_title: "Issue A",
    base_branch: "",
  };
}

function overview(statuses: string[], ids: number[] = statuses.map((_, i) => i + 1)): ThreadOverview {
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

test("snapshotOf keys cover needs / asks / writeTriggers / review", () => {
  const snap = snapshotOf(
    [need({ ask_id: 7 })],
    [ask(3)],
    [wt(0)],
    [overview(["working", "review"], [11, 12])],
    {},
    {},
    null,
  );
  assert.equal(snap.needs.get("need:7")?.sample, "Issue A · task-1");
  assert.equal(snap.needs.get("need:7")?.route.threadId, 1);
  assert.equal(snap.needs.get("need:7")?.route.askId, 7);
  assert.equal(snap.needs.get("ask:3")?.sample, "Issue A · task-1");
  assert.equal(snap.needs.get("wt:1:0")?.sample, "Issue A · lane");
  assert.equal(snap.review.get("rev:12")?.sample, "Issue A");
  assert.equal(snap.review.get("rev:12")?.route.directionId, 12);
  assert.equal(snap.review.has("rev:11"), false);
});

test("snapshotOf skips self-clearing notices but keeps action-required notices", () => {
  const silent = snapshotOf(
    [need({ ask_id: 1, kind: "notice" })],
    [],
    [],
    [],
    {},
    {},
    null,
  );
  assert.equal(silent.needs.size, 0);
  const action = snapshotOf(
    [need({ ask_id: 2, kind: "notice_action_required" })],
    [],
    [],
    [],
    {},
    {},
    null,
  );
  assert.equal(action.needs.get("need:2")?.route.openNeeds, true);
});

test("snapshotOf captures stalled workers and leads", () => {
  const snap = snapshotOf(
    [],
    [],
    [],
    [],
    {
      99: {
        info: { session_id: 99 },
        status: "stalled",
        directionId: 5,
        repoId: 8,
        threadId: 2,
      },
    },
    { 2: { state: "stalled", queue: [] }, 3: { state: "busy", queue: [] } },
    null,
    { 2: { title: "Issue B" } },
  );
  assert.equal(snap.stalled.get("stall:worker:99")?.sample, "Issue B · dir 5");
  assert.equal(snap.stalled.get("stall:worker:99")?.route.directionId, 5);
  assert.equal(snap.stalled.get("stall:worker:99")?.route.repoId, 8);
  assert.equal(snap.stalled.get("stall:worker:99")?.route.sessionId, 99);
  assert.equal(snap.stalled.get("stall:lead:2")?.sample, "Issue B");
  assert.equal(snap.stalled.has("stall:lead:3"), false);
});

test("snapshotOf only surfaces degraded quota keyed by transitionSeq", () => {
  const snap = snapshotOf([], [], [], [], {}, {}, quota("degraded", 4));
  assert.equal(snap.quota.get("quota:degraded:4")?.sample, "900 / 1000");
  assert.equal(snap.quota.get("quota:degraded:4")?.route.kind, "quota");
  const warning = snapshotOf([], [], [], [], {}, {}, quota("warning", 5));
  assert.equal(warning.quota.size, 0);
});

test("diffForNotifications emits only new keys and merges per category", () => {
  const entry = (sample: string, kind: "needs" | "review" | "stalled" | "quota" = "needs") => ({
    sample,
    route: { kind },
  });
  const prev: NotifySnapshot = {
    needs: new Map([["need:1", entry("a")]]),
    review: new Map(),
    stalled: new Map(),
    quota: new Map(),
  };
  const next: NotifySnapshot = {
    needs: new Map([
      ["need:1", entry("a")],
      ["need:2", entry("b")],
      ["ask:9", entry("c")],
    ]),
    review: new Map([["rev:1", entry("Issue", "review")]]),
    stalled: new Map(),
    quota: new Map([["quota:degraded:1", entry("1 / 1", "quota")]]),
  };
  const events = diffForNotifications(prev, next);
  assert.deepEqual(
    events.map((e) => e.kind),
    ["needs", "review", "quota"],
  );
  const needs = events.find((e) => e.kind === "needs")!;
  assert.equal(needs.count, 2);
  assert.equal(needs.sample, "b");
});

test("diffForNotifications respects category mutes", () => {
  const prev = emptyNotifySnapshot();
  const entry = (sample: string, kind: "needs" | "review" | "stalled" | "quota") => ({
    sample,
    route: { kind },
  });
  const next: NotifySnapshot = {
    needs: new Map([["need:1", entry("a", "needs")]]),
    review: new Map([["rev:1", entry("b", "review")]]),
    stalled: new Map([["stall:lead:1", entry("c", "stalled")]]),
    quota: new Map([["quota:degraded:1", entry("d", "quota")]]),
  };
  const events = diffForNotifications(prev, next, {
    needs: true,
    review: false,
    stalled: true,
    quota: false,
  });
  assert.deepEqual(
    events.map((e) => e.kind),
    ["needs", "stalled"],
  );
});

test("quiet hours same-day and wrap-past-midnight", () => {
  const sameDay = { enabled: true, startMin: 9 * 60, endMin: 17 * 60 };
  assert.equal(isInQuietHours(sameDay, new Date(2026, 0, 1, 10, 0)), true);
  assert.equal(isInQuietHours(sameDay, new Date(2026, 0, 1, 8, 0)), false);
  assert.equal(isInQuietHours(sameDay, new Date(2026, 0, 1, 17, 0)), false);

  const wrap = { enabled: true, startMin: 22 * 60, endMin: 8 * 60 };
  assert.equal(isInQuietHours(wrap, new Date(2026, 0, 1, 23, 0)), true);
  assert.equal(isInQuietHours(wrap, new Date(2026, 0, 1, 7, 0)), true);
  assert.equal(isInQuietHours(wrap, new Date(2026, 0, 1, 12, 0)), false);

  assert.equal(isInQuietHours({ ...wrap, enabled: false }, new Date(2026, 0, 1, 23, 0)), false);
  assert.equal(isInQuietHours({ enabled: true, startMin: 10, endMin: 10 }, new Date()), false);
});

test("quiet hours parse/serialize and time helpers", () => {
  const raw = serializeQuietHours({ enabled: true, startMin: 22 * 60 + 30, endMin: 7 * 60 });
  const parsed = parseQuietHours(raw);
  assert.deepEqual(parsed, { enabled: true, startMin: 22 * 60 + 30, endMin: 7 * 60 });
  assert.equal(formatQuietTime(8 * 60 + 5), "08:05");
  assert.equal(parseQuietTime("23:59"), 23 * 60 + 59);
  assert.equal(parseQuietTime("24:00"), null);
  assert.deepEqual(parseQuietHours(null), DEFAULT_QUIET_HOURS);
});

test("category flags parse with defaults for missing keys", () => {
  assert.deepEqual(parseNotifyCategories(null), DEFAULT_NOTIFY_CATEGORIES);
  const partial = parseNotifyCategories(JSON.stringify({ needs: false }));
  assert.equal(partial.needs, false);
  assert.equal(partial.review, true);
  assert.equal(partial.stalled, true);
  assert.equal(partial.quota, true);
  assert.equal(JSON.parse(serializeNotifyCategories(partial)).needs, false);
});

test("foreground gate prefers explicit window focus", () => {
  assert.equal(isAppInForeground({ windowFocused: true, documentFocused: false }), true);
  assert.equal(isAppInForeground({ windowFocused: false, documentFocused: true }), false);
  assert.equal(isAppInForeground({ windowFocused: null, documentFocused: true }), true);
  assert.equal(isAppInForeground({ windowFocused: null, documentFocused: false }), false);
});

test("badgeCountFrom counts only actionable needs + asks + writeTriggers", () => {
  const n = badgeCountFrom(
    [
      need({ ask_id: 1, kind: "question" }),
      need({ ask_id: 2, kind: "notice" }),
      need({ ask_id: 3, kind: "notice_action_required" }),
    ],
    [ask(1)],
    [wt(0), wt(1)],
  );
  assert.equal(n, 1 + 1 + 1 + 2);
});

test("notifyCopyKeys covers every category", () => {
  assert.equal(notifyCopyKeys("needs").title, "notify.needsTitle");
  assert.equal(notifyCopyKeys("review").title, "notify.reviewTitle");
  assert.equal(notifyCopyKeys("stalled").title, "notify.stalledTitle");
  assert.equal(notifyCopyKeys("quota").title, "notify.quotaTitle");
});


test("planNotifyOpen routes needs/review to direction and switches workspace", () => {
  assert.deepEqual(
    planNotifyOpen({
      kind: "needs",
      workspaceId: 2,
      threadId: 9,
      directionId: 11,
    }),
    [
      { type: "workspace", workspaceId: 2 },
      { type: "direction", threadId: 9, direction: "11" },
    ],
  );
});

test("planNotifyOpen routes quota to resources and lead-only stalled to lead", () => {
  assert.deepEqual(planNotifyOpen({ kind: "quota", workspaceId: 1 }), [
    { type: "workspace", workspaceId: 1 },
    { type: "resources" },
  ]);
  assert.deepEqual(planNotifyOpen({ kind: "stalled", threadId: 4 }), [
    { type: "direction", threadId: 4, direction: "lead" },
  ]);
});

test("planNotifyOpen falls back to needs list when only kind is present", () => {
  assert.deepEqual(planNotifyOpen({ kind: "needs" }), [{ type: "needs" }]);
});

test("planNotifyOpen routes write-trigger style openNeeds to Needs-you", () => {
  assert.deepEqual(
    planNotifyOpen({
      kind: "needs",
      threadId: 9,
      openNeeds: true,
      workspaceId: 2,
    }),
    [
      { type: "workspace", workspaceId: 2 },
      { type: "needs" },
    ],
  );
});

test("planNotifyOpen carries repo/session for stalled workers", () => {
  assert.deepEqual(
    planNotifyOpen({
      kind: "stalled",
      threadId: 4,
      directionId: 5,
      repoId: 8,
      sessionId: 99,
    }),
    [
      {
        type: "direction",
        threadId: 4,
        direction: "5",
        repoId: 8,
        sessionId: 99,
      },
    ],
  );
});
