import { useEffect, useRef, useState } from "react";
import {
  isPermissionGranted,
  requestPermission,
  sendNotification,
} from "@tauri-apps/plugin-notification";
import { openUrl } from "@tauri-apps/plugin-opener";
import { useTranslation } from "react-i18next";
import { api } from "./api";
import { useStore } from "../state/store";
import type { NeedItem, PermissionAsk, ThreadOverview, WriteTrigger } from "./types";

/** Three-state OS permission. macOS prompts exactly once — after a refusal,
 *  requestPermission returns "denied" without a dialog and the only remedy is
 *  the OS settings pane, so callers must tell "denied" apart from "prompt". */
export type NotifyPermission = "granted" | "denied" | "prompt";

export async function notifyPermission(): Promise<NotifyPermission> {
  try {
    if (await isPermissionGranted()) return "granted";
    const perm = (window as { Notification?: { permission?: string } }).Notification
      ?.permission;
    return perm === "denied" ? "denied" : "prompt";
  } catch {
    return "denied"; // pure-vite dev: plugin unavailable
  }
}

/** Resolve to a settled state, asking the OS only from "prompt" (a dismissed
 *  dialog stays "prompt" so the user can be asked again later). */
export async function ensureNotifyPermission(): Promise<NotifyPermission> {
  const p = await notifyPermission();
  if (p !== "prompt") return p;
  try {
    const r = await requestPermission();
    if (r === "granted") return "granted";
    if (r === "denied") return "denied";
    return "prompt";
  } catch {
    return "denied";
  }
}

/** Jump to the OS notification settings. macOS / Windows have stable URLs;
 *  Linux has no portable one — returns false and the caller's copy stands. */
export async function openSystemNotificationSettings(): Promise<boolean> {
  const ua = navigator.userAgent;
  const url = ua.includes("Mac")
    ? "x-apple.systempreferences:com.apple.preference.notifications"
    : ua.includes("Windows")
      ? "ms-settings:notifications"
      : null;
  if (!url) return false;
  try {
    await openUrl(url);
    return true;
  } catch {
    return false;
  }
}

/**
 * OS notifications for the two things worth pulling the human back (spec
 * 2026-06-11): a new Needs-you item, and a sub-task reaching review. Desktop
 * notifications carry no click callback (Tauri v2 platform limit), so the body
 * carries the context and the in-app badges take over once focused.
 */

/** Notify-relevant state reduced to stable identity keys → context line. */
export interface NotifySnapshot {
  needs: Map<string, string>;
  review: Map<string, string>;
}

export function snapshotOf(
  needs: NeedItem[],
  asks: PermissionAsk[],
  triggers: WriteTrigger[],
  overview: ThreadOverview[],
): NotifySnapshot {
  const n = new Map<string, string>();
  for (const it of needs) {
    n.set(`need:${it.ask_id}`, `${it.thread_title} · ${it.direction_name}`);
  }
  for (const a of asks) {
    n.set(`ask:${a.id}`, `${a.thread_title} · ${a.dir_name}`);
  }
  for (const w of triggers) {
    n.set(`wt:${w.thread_id}:${w.index}`, `${w.thread_title} · ${w.name}`);
  }
  const r = new Map<string, string>();
  for (const o of overview) {
    o.statuses.forEach((s, i) => {
      if (s === "review") r.set(`rev:${o.direction_ids[i]}`, o.title);
    });
  }
  return { needs: n, review: r };
}

export interface NotifyEvent {
  kind: "needs" | "review";
  count: number;
  /** Context of the first new item, used as the body when count === 1. */
  sample: string;
}

/** New keys in `next` that weren't in `prev` — the things worth a ping. */
export function diffForNotifications(
  prev: NotifySnapshot,
  next: NotifySnapshot,
): NotifyEvent[] {
  const out: NotifyEvent[] = [];
  for (const kind of ["needs", "review"] as const) {
    const fresh = [...next[kind]].filter(([k]) => !prev[kind].has(k));
    if (fresh.length > 0) {
      out.push({ kind, count: fresh.length, sample: fresh[0][1] });
    }
  }
  return out;
}

const OVERVIEW_POLL_MS = 10_000;

/**
 * Mounted once in App. Reuses the store's Needs-you aggregation (4s poll +
 * push); polls workspace_overview itself for review transitions (the board
 * only fetches it on mount, and the per-issue direction poll covers only the
 * open issue). First load and workspace switches rebuild the baseline silently.
 */
export function useSystemNotifications() {
  const { needs, asks, writeTriggers, notifyEnabled, activeWorkspaceId } = useStore();
  const { t } = useTranslation();
  const [overview, setOverview] = useState<ThreadOverview[]>([]);
  const prev = useRef<NotifySnapshot | null>(null);
  const baselineWs = useRef<number | null>(null);
  const granted = useRef<boolean | null>(null);

  // OS permission, settled once per enable. Only a never-asked "prompt" state
  // raises the system dialog; a past refusal stays silent (Settings shows the
  // denied hint with the System-Settings jump).
  useEffect(() => {
    if (!notifyEnabled) return;
    void ensureNotifyPermission().then((p) => {
      granted.current = p === "granted";
    });
  }, [notifyEnabled]);

  // Review-transition source: our own modest overview poll.
  useEffect(() => {
    if (!notifyEnabled || activeWorkspaceId == null) {
      setOverview([]);
      return;
    }
    let alive = true;
    const tick = async () => {
      try {
        const o = await api.workspaceOverview(activeWorkspaceId);
        if (alive) setOverview(o);
      } catch {
        /* backend unavailable */
      }
    };
    void tick();
    const h = setInterval(tick, OVERVIEW_POLL_MS);
    return () => {
      alive = false;
      clearInterval(h);
    };
  }, [notifyEnabled, activeWorkspaceId]);

  useEffect(() => {
    const next = snapshotOf(needs, asks, writeTriggers, overview);
    const base = baselineWs.current === activeWorkspaceId ? prev.current : null;
    prev.current = next;
    baselineWs.current = activeWorkspaceId;
    if (!base) return; // first load / workspace switch: baseline only
    if (!notifyEnabled || granted.current !== true) return;
    if (document.hasFocus()) return; // already looking at the app
    for (const ev of diffForNotifications(base, next)) {
      try {
        sendNotification({
          title: ev.kind === "needs" ? t("notify.needsTitle") : t("notify.reviewTitle"),
          body:
            ev.count === 1
              ? ev.sample
              : t(ev.kind === "needs" ? "notify.needsBody" : "notify.reviewBody", {
                  count: ev.count,
                }),
        });
      } catch {
        /* never let a failed ping disturb the app */
      }
    }
  }, [needs, asks, writeTriggers, overview, notifyEnabled, activeWorkspaceId, t]);
}
