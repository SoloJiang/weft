import type {
  AttentionItem,
  ProcessQuotaStatus,
  ThreadOverview,
} from "./types";

/** Notification preferences remain independent from the React hook. */
export type NotifyCategory = "needs" | "review" | "quota";

export const NOTIFY_CATEGORIES: readonly NotifyCategory[] = [
  "needs",
  "review",
  "quota",
] as const;

export type NotifyCategoryFlags = Record<NotifyCategory, boolean>;
export type NotificationOverview = ThreadOverview & { workspace_id?: number };

export const DEFAULT_NOTIFY_CATEGORIES: NotifyCategoryFlags = {
  needs: true,
  review: true,
  quota: true,
};

export interface QuietHours {
  enabled: boolean;
  startMin: number;
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
    let startMin = DEFAULT_QUIET_HOURS.startMin;
    if (typeof parsed.startMin === "number" && Number.isFinite(parsed.startMin)) {
      startMin = clampMinute(parsed.startMin);
    }
    let endMin = DEFAULT_QUIET_HOURS.endMin;
    if (typeof parsed.endMin === "number" && Number.isFinite(parsed.endMin)) {
      endMin = clampMinute(parsed.endMin);
    }
    return { enabled: parsed.enabled === true, startMin, endMin };
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

export function formatQuietTime(min: number): string {
  const m = clampMinute(min);
  const hh = String(Math.floor(m / 60)).padStart(2, "0");
  const mm = String(m % 60).padStart(2, "0");
  return `${hh}:${mm}`;
}

export function parseQuietTime(value: string): number | null {
  const match = /^(\d{1,2}):(\d{2})$/.exec(value.trim());
  if (!match) return null;
  const hh = Number(match[1]);
  const mm = Number(match[2]);
  if (!Number.isInteger(hh) || !Number.isInteger(mm)) return null;
  if (hh < 0 || hh > 23 || mm < 0 || mm > 59) return null;
  return hh * 60 + mm;
}

export function isInQuietHours(qh: QuietHours, now: Date = new Date()): boolean {
  if (!qh.enabled || qh.startMin === qh.endMin) return false;
  const cur = now.getHours() * 60 + now.getMinutes();
  if (qh.startMin < qh.endMin) {
    return cur >= qh.startMin && cur < qh.endMin;
  }
  return cur >= qh.startMin || cur < qh.endMin;
}

export interface NotifyRoute {
  kind: NotifyCategory;
  threadId?: number;
  directionId?: number;
  repoId?: number;
  sessionId?: number;
  workspaceId?: number;
  attentionId?: string;
  openNeeds?: boolean;
  openCurator?: boolean;
}

export interface NotifyEntry {
  sample: string;
  route: NotifyRoute;
}

export interface NotifySnapshot {
  needs: Map<string, NotifyEntry>;
  review: Map<string, NotifyEntry>;
  quota: Map<string, NotifyEntry>;
}

export function emptyNotifySnapshot(): NotifySnapshot {
  return { needs: new Map(), review: new Map(), quota: new Map() };
}

function attentionContext(item: AttentionItem): {
  sample: string;
  threadId?: number;
  directionId?: number;
  workspaceId?: number;
} {
  switch (item.kind) {
    case "permission": {
      const directionId = Number(item.ask.dir);
      return {
        sample: [item.ask.thread_title, item.ask.dir_name].filter(Boolean).join(" · "),
        threadId: item.ask.thread,
        directionId: Number.isFinite(directionId) ? directionId : undefined,
        workspaceId: item.ask.workspace_id ?? undefined,
      };
    }
    case "question":
      return {
        sample: [item.thread_title, item.direction_name].filter(Boolean).join(" · "),
        threadId: item.thread_id,
        directionId: item.direction_id > 0 ? item.direction_id : undefined,
      };
    case "plan_approval":
    case "repo_action":
      return {
        sample: [item.thread_title, item.title].filter(Boolean).join(" · "),
        threadId: item.thread_id,
      };
    case "scope_approval":
      return { sample: item.thread_title, threadId: item.thread_id };
    case "pr_tracking_retry":
      return {
        sample: [item.thread_title, item.direction_name].filter(Boolean).join(" · "),
        threadId: item.thread_id,
        directionId: item.direction_id,
      };
  }
}

/** Build notification state from the same canonical attention rows as the page. */
export function snapshotOf(
  attentionItems: AttentionItem[],
  overview: NotificationOverview[],
  processQuota: ProcessQuotaStatus | null,
  workspaceId: number | null = null,
): NotifySnapshot {
  const needs = new Map<string, NotifyEntry>();
  for (const item of attentionItems) {
    const context = attentionContext(item);
    needs.set(item.id, {
      sample: context.sample,
      route: {
        kind: "needs",
        threadId: context.threadId,
        directionId: context.directionId,
        workspaceId: context.workspaceId ?? workspaceId ?? undefined,
        attentionId: item.id,
        openNeeds: true,
      },
    });
  }

  const review = new Map<string, NotifyEntry>();
  for (const row of overview) {
    row.statuses.forEach((status, index) => {
      if (status !== "review") return;
      const directionId = row.direction_ids[index];
      review.set(`rev:${directionId}`, {
        sample: row.title,
        route: {
          kind: "review",
          threadId: row.thread_id,
          directionId,
          workspaceId: row.workspace_id ?? workspaceId ?? undefined,
        },
      });
    });
  }

  const quota = new Map<string, NotifyEntry>();
  if (processQuota?.status === "degraded") {
    const key = `quota:degraded:${processQuota.transitionSeq}`;
    const sample = processQuota.processLimit
      ? `${processQuota.processCount} / ${processQuota.processLimit}`
      : `${processQuota.processCount}`;
    quota.set(key, { sample, route: { kind: "quota" } });
  }

  return { needs, review, quota };
}

export interface NotifyEvent {
  kind: NotifyCategory;
  count: number;
  sample: string;
  route: NotifyRoute;
}

export function diffForNotifications(
  prev: NotifySnapshot,
  next: NotifySnapshot,
  enabled: NotifyCategoryFlags = DEFAULT_NOTIFY_CATEGORIES,
): NotifyEvent[] {
  const out: NotifyEvent[] = [];
  for (const kind of NOTIFY_CATEGORIES) {
    if (!enabled[kind]) continue;
    const fresh = [...next[kind]].filter(([key]) => !prev[kind].has(key));
    const first = fresh[0]?.[1];
    if (!first) continue;
    out.push({ kind, count: fresh.length, sample: first.sample, route: first.route });
  }
  return out;
}

export function notifyCopyKeys(kind: NotifyCategory): { title: string; body: string } {
  switch (kind) {
    case "needs":
      return { title: "notify.needsTitle", body: "notify.needsBody" };
    case "review":
      return { title: "notify.reviewTitle", body: "notify.reviewBody" };
    case "quota":
      return { title: "notify.quotaTitle", body: "notify.quotaBody" };
  }
}

export function badgeCountFrom(items: AttentionItem[]): number {
  return items.length;
}

export function isAppInForeground(
  opts: { windowFocused?: boolean | null; documentFocused?: boolean } = {},
): boolean {
  if (opts.windowFocused === true) return true;
  if (opts.windowFocused === false) return false;
  if (typeof opts.documentFocused === "boolean") return opts.documentFocused;
  if (typeof document !== "undefined") return document.hasFocus();
  return false;
}

export type NotifyOpenIntent =
  | { type: "workspace"; workspaceId: number }
  | {
      type: "direction";
      threadId: number;
      direction: string;
      repoId?: number;
      sessionId?: number;
    }
  | { type: "needs" }
  | { type: "resources" }
  | { type: "curator" };

export function planNotifyOpen(payload: {
  kind: string;
  threadId?: number | null;
  directionId?: number | null;
  repoId?: number | null;
  sessionId?: number | null;
  workspaceId?: number | null;
  attentionId?: string | null;
  openNeeds?: boolean | null;
  openCurator?: boolean | null;
}): NotifyOpenIntent[] {
  if (payload.kind === "quota") return [{ type: "resources" }];
  const out: NotifyOpenIntent[] = [];
  if (payload.workspaceId != null) {
    out.push({ type: "workspace", workspaceId: payload.workspaceId });
  }
  if (payload.openCurator) {
    out.push({ type: "curator" });
    return out;
  }
  if (payload.kind === "needs" || payload.openNeeds || payload.attentionId) {
    out.push({ type: "needs" });
    return out;
  }
  if (payload.threadId != null) {
    const direction = payload.directionId == null ? "lead" : String(payload.directionId);
    out.push({
      type: "direction",
      threadId: payload.threadId,
      direction,
      repoId: payload.repoId ?? undefined,
      sessionId: payload.sessionId ?? undefined,
    });
  }
  return out;
}
