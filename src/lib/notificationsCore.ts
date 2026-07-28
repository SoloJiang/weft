import type {
  NeedItem,
  PermissionAsk,
  ProcessQuotaStatus,
  SessionStatus,
  ThreadOverview,
  TurnState,
  WriteTrigger,
} from "./types";

/** Notification preference helpers (localStorage-backed). Kept separate from
 *  the React hook so the store can import without a cycle through notifications. */

export type NotifyCategory = "needs" | "review" | "stalled" | "quota";

export const NOTIFY_CATEGORIES: readonly NotifyCategory[] = [
  "needs",
  "review",
  "stalled",
  "quota",
] as const;

export type NotifyCategoryFlags = Record<NotifyCategory, boolean>;

export const DEFAULT_NOTIFY_CATEGORIES: NotifyCategoryFlags = {
  needs: true,
  review: true,
  stalled: true,
  quota: true,
};

/** Quiet hours window in local wall-clock minutes. End may wrap past midnight. */
export interface QuietHours {
  enabled: boolean;
  /** Inclusive start, minutes since local midnight (0–1439). */
  startMin: number;
  /** Exclusive end, minutes since local midnight (0–1439). */
  endMin: number;
}

export const DEFAULT_QUIET_HOURS: QuietHours = {
  enabled: false,
  startMin: 22 * 60,
  endMin: 8 * 60,
};

function clampMinute(n: number): number {
  if (!Number.isFinite(n)) return 0;
  const m = Math.trunc(n) % (24 * 60);
  return m < 0 ? m + 24 * 60 : m;
}

/** Parse category flags stored as JSON; unknown / partial shapes fall back to
 *  defaults so a future category lands ON without a migration. */
export function parseNotifyCategories(raw: string | null): NotifyCategoryFlags {
  const out: NotifyCategoryFlags = { ...DEFAULT_NOTIFY_CATEGORIES };
  if (!raw) return out;
  try {
    const parsed = JSON.parse(raw) as Partial<Record<string, unknown>>;
    for (const key of NOTIFY_CATEGORIES) {
      if (typeof parsed[key] === "boolean") out[key] = parsed[key];
    }
  } catch {
    /* keep defaults */
  }
  return out;
}

export function serializeNotifyCategories(flags: NotifyCategoryFlags): string {
  return JSON.stringify(flags);
}

export function parseQuietHours(raw: string | null): QuietHours {
  if (!raw) return { ...DEFAULT_QUIET_HOURS };
  try {
    const parsed = JSON.parse(raw) as Partial<QuietHours>;
    const startMin =
      typeof parsed.startMin === "number" && Number.isFinite(parsed.startMin)
        ? clampMinute(parsed.startMin)
        : DEFAULT_QUIET_HOURS.startMin;
    const endMin =
      typeof parsed.endMin === "number" && Number.isFinite(parsed.endMin)
        ? clampMinute(parsed.endMin)
        : DEFAULT_QUIET_HOURS.endMin;
    return {
      enabled: parsed.enabled === true,
      startMin,
      endMin,
    };
  } catch {
    return { ...DEFAULT_QUIET_HOURS };
  }
}

export function serializeQuietHours(qh: QuietHours): string {
  return JSON.stringify({
    enabled: qh.enabled,
    startMin: clampMinute(qh.startMin),
    endMin: clampMinute(qh.endMin),
  });
}

/** `HH:MM` for Settings inputs; always two-digit. */
export function formatQuietTime(min: number): string {
  const m = clampMinute(min);
  const hh = String(Math.floor(m / 60)).padStart(2, "0");
  const mm = String(m % 60).padStart(2, "0");
  return `${hh}:${mm}`;
}

/** Parse an `<input type="time">` value; invalid → null. */
export function parseQuietTime(value: string): number | null {
  const m = /^(\d{1,2}):(\d{2})$/.exec(value.trim());
  if (!m) return null;
  const hh = Number(m[1]);
  const mm = Number(m[2]);
  if (!Number.isInteger(hh) || !Number.isInteger(mm)) return null;
  if (hh < 0 || hh > 23 || mm < 0 || mm > 59) return null;
  return hh * 60 + mm;
}

/**
 * Quiet-hours membership. When start === end the window is empty (never quiet).
 * When start < end it is a same-day range; otherwise it wraps past midnight.
 */
export function isInQuietHours(
  qh: QuietHours,
  now: Date = new Date(),
): boolean {
  if (!qh.enabled) return false;
  if (qh.startMin === qh.endMin) return false;
  const cur = now.getHours() * 60 + now.getMinutes();
  if (qh.startMin < qh.endMin) {
    return cur >= qh.startMin && cur < qh.endMin;
  }
  return cur >= qh.startMin || cur < qh.endMin;
}


/** Minimal session shape for stalled detection — avoids importing store. */
export interface NotifySessionRef {
  info: { session_id: number };
  status: SessionStatus;
  directionId: number;
  threadId: number;
}

/** Deep-link fields carried through the OS notification `user_info`. */
export interface NotifyRoute {
  kind: NotifyCategory;
  threadId?: number;
  directionId?: number;
  askId?: number;
  workspaceId?: number;
}

/** One snapshot entry: human sample line + optional deep-link route. */
export interface NotifyEntry {
  sample: string;
  route: NotifyRoute;
}

/** Notify-relevant state reduced to stable identity keys → entry. */
export interface NotifySnapshot {
  needs: Map<string, NotifyEntry>;
  review: Map<string, NotifyEntry>;
  stalled: Map<string, NotifyEntry>;
  quota: Map<string, NotifyEntry>;
}

export function emptyNotifySnapshot(): NotifySnapshot {
  return {
    needs: new Map(),
    review: new Map(),
    stalled: new Map(),
    quota: new Map(),
  };
}

export function snapshotOf(
  needs: NeedItem[],
  asks: PermissionAsk[],
  triggers: WriteTrigger[],
  overview: ThreadOverview[],
  sessions: Record<number, NotifySessionRef>,
  leadTurn: Record<number, { state: TurnState; queue: unknown[] }>,
  processQuota: ProcessQuotaStatus | null,
  threadsById: Record<number, { title: string } | undefined> = {},
  workspaceId: number | null = null,
): NotifySnapshot {
  const n = new Map<string, NotifyEntry>();
  for (const it of needs) {
    // Notices are display-only (or self-clearing); only real questions ping Needs-you.
    if (it.kind !== "question") continue;
    n.set(`need:${it.ask_id}`, {
      sample: `${it.thread_title} · ${it.direction_name}`,
      route: {
        kind: "needs",
        threadId: it.thread_id,
        directionId: it.direction_id,
        askId: it.ask_id,
        workspaceId: workspaceId ?? undefined,
      },
    });
  }
  for (const a of asks) {
    const directionId = Number(a.dir);
    n.set(`ask:${a.id}`, {
      sample: `${a.thread_title} · ${a.dir_name}`,
      route: {
        kind: "needs",
        threadId: a.thread,
        directionId: Number.isFinite(directionId) ? directionId : undefined,
        askId: a.id,
        workspaceId: workspaceId ?? undefined,
      },
    });
  }
  for (const w of triggers) {
    n.set(`wt:${w.thread_id}:${w.index}`, {
      sample: `${w.thread_title} · ${w.name}`,
      route: {
        kind: "needs",
        threadId: w.thread_id,
        workspaceId: workspaceId ?? undefined,
      },
    });
  }

  const r = new Map<string, NotifyEntry>();
  for (const o of overview) {
    o.statuses.forEach((s, i) => {
      if (s !== "review") return;
      const directionId = o.direction_ids[i];
      r.set(`rev:${directionId}`, {
        sample: o.title,
        route: {
          kind: "review",
          threadId: o.thread_id,
          directionId,
          workspaceId: workspaceId ?? undefined,
        },
      });
    });
  }

  const stalled = new Map<string, NotifyEntry>();
  for (const s of Object.values(sessions)) {
    if (s.status !== "stalled") continue;
    const title = threadsById[s.threadId]?.title ?? `thread ${s.threadId}`;
    stalled.set(`stall:worker:${s.info.session_id}`, {
      sample: `${title} · dir ${s.directionId}`,
      route: {
        kind: "stalled",
        threadId: s.threadId,
        directionId: s.directionId,
        workspaceId: workspaceId ?? undefined,
      },
    });
  }
  for (const [tidStr, turn] of Object.entries(leadTurn)) {
    if (turn.state !== "stalled") continue;
    const tid = Number(tidStr);
    const title = threadsById[tid]?.title ?? `thread ${tid}`;
    stalled.set(`stall:lead:${tid}`, {
      sample: title,
      route: {
        kind: "stalled",
        threadId: tid,
        workspaceId: workspaceId ?? undefined,
      },
    });
  }

  const quota = new Map<string, NotifyEntry>();
  if (processQuota?.status === "degraded") {
    // Identity is the transition sequence so a later re-degrade re-notifies.
    const key = `quota:degraded:${processQuota.transitionSeq}`;
    const limit = processQuota.processLimit ?? 0;
    const sample =
      limit > 0
        ? `${processQuota.processCount} / ${limit}`
        : `${processQuota.processCount}`;
    quota.set(key, {
      sample,
      route: {
        kind: "quota",
        workspaceId: workspaceId ?? undefined,
      },
    });
  }

  return { needs: n, review: r, stalled, quota };
}

export interface NotifyEvent {
  kind: NotifyCategory;
  count: number;
  /** Context of the first new item, used as the body when count === 1. */
  sample: string;
  /** Deep-link of the first new item (best-effort; multi-item pings open the first). */
  route: NotifyRoute;
}

/** New keys in `next` that weren't in `prev` — the things worth a ping. */
export function diffForNotifications(
  prev: NotifySnapshot,
  next: NotifySnapshot,
  enabled: NotifyCategoryFlags = DEFAULT_NOTIFY_CATEGORIES,
): NotifyEvent[] {
  const out: NotifyEvent[] = [];
  for (const kind of NOTIFY_CATEGORIES) {
    if (!enabled[kind]) continue;
    const fresh = [...next[kind]].filter(([k]) => !prev[kind].has(k));
    if (fresh.length > 0) {
      const first = fresh[0][1];
      out.push({
        kind,
        count: fresh.length,
        sample: first.sample,
        route: first.route,
      });
    }
  }
  return out;
}

/** Title / body i18n keys for each category. Plural forms use i18next `_one/_other`. */
export function notifyCopyKeys(kind: NotifyCategory): {
  title: string;
  body: string;
} {
  switch (kind) {
    case "needs":
      return { title: "notify.needsTitle", body: "notify.needsBody" };
    case "review":
      return { title: "notify.reviewTitle", body: "notify.reviewBody" };
    case "stalled":
      return { title: "notify.stalledTitle", body: "notify.stalledBody" };
    case "quota":
      return { title: "notify.quotaTitle", body: "notify.quotaBody" };
    default: {
      const _exhaustive: never = kind;
      return _exhaustive;
    }
  }
}

/** Pending Needs-you badge count (actionable only). Exported for Dock badge. */
export function badgeCountFrom(
  needs: NeedItem[],
  asks: PermissionAsk[],
  writeTriggers: WriteTrigger[],
): number {
  return needs.filter((n) => n.kind === "question").length + asks.length + writeTriggers.length;
}

/**
 * Focus / visibility gate. Prefer Tauri window focus when available; fall back
 * to document focus so a visible-but-unfocused window still receives pings.
 */
export function isAppInForeground(
  opts: {
    windowFocused?: boolean | null;
    documentFocused?: boolean;
  } = {},
): boolean {
  if (opts.windowFocused === true) return true;
  if (opts.windowFocused === false) return false;
  if (typeof opts.documentFocused === "boolean") return opts.documentFocused;
  if (typeof document !== "undefined") return document.hasFocus();
  return false;
}


/** Pure navigation intent for a notification click. UI layer applies it. */
export type NotifyOpenIntent =
  | { type: "workspace"; workspaceId: number }
  | { type: "direction"; threadId: number; direction: string }
  | { type: "needs" }
  | { type: "resources" };

export function planNotifyOpen(payload: {
  kind: string;
  threadId?: number | null;
  directionId?: number | null;
  workspaceId?: number | null;
}): NotifyOpenIntent[] {
  const out: NotifyOpenIntent[] = [];
  if (payload.workspaceId != null) {
    out.push({ type: "workspace", workspaceId: payload.workspaceId });
  }
  if (payload.kind === "quota") {
    out.push({ type: "resources" });
    return out;
  }
  if (payload.threadId != null) {
    const direction =
      payload.directionId != null ? String(payload.directionId) : "lead";
    out.push({ type: "direction", threadId: payload.threadId, direction });
    return out;
  }
  if (payload.kind === "needs") {
    out.push({ type: "needs" });
  }
  return out;
}
