import { useEffect, useRef, useState } from "react";
import { openUrl } from "@tauri-apps/plugin-opener";
import { getCurrentWindow, UserAttentionType } from "@tauri-apps/api/window";
import { listen } from "@tauri-apps/api/event";
import { useTranslation } from "react-i18next";
import { useStore } from "../state/store";
import { api } from "./api";
import {
  badgeCountFrom,
  diffForNotifications,
  isAppInForeground,
  isInQuietHours,
  notifyCopyKeys,
  planNotifyOpen,
  snapshotOf,
  type NotifyRoute,
  type NotifySnapshot,
} from "./notificationsCore";

export * from "./notificationsCore";

/** Three-state OS permission. macOS prompts exactly once — after a refusal,
 *  requestPermission returns "denied" without a dialog and the only remedy is
 *  the OS settings pane, so callers must tell "denied" apart from "prompt". */
export type NotifyPermission = "granted" | "denied" | "prompt";

function asNotifyPermission(raw: string): NotifyPermission {
  if (raw === "granted" || raw === "denied" || raw === "prompt") return raw;
  return "denied";
}

export async function notifyPermission(): Promise<NotifyPermission> {
  try {
    const p = asNotifyPermission(await api.osNotifyPermission());
    if (p === "granted") return "granted";
    // user-notify collapses Denied + NotDetermined to false. Prefer the Web
    // Notification permission bit when present so Settings can show the
    // "open System Settings" recovery instead of re-prompting forever.
    const web = (window as { Notification?: { permission?: string } }).Notification
      ?.permission;
    if (web === "denied") return "denied";
    if (web === "granted") return "granted";
    return "prompt";
  } catch {
    return "denied"; // pure-vite dev: command unavailable
  }
}

/** Resolve to a settled state, asking the OS only from "prompt". */
export async function ensureNotifyPermission(): Promise<NotifyPermission> {
  const p = await notifyPermission();
  if (p !== "prompt") return p;
  try {
    return asNotifyPermission(await api.osNotifyRequestPermission());
  } catch {
    return "denied";
  }
}

/** Jump to the OS notification settings. macOS / Windows have stable URLs;
 *  Linux has no portable one — returns false and the caller's copy stands. */
export async function openSystemNotificationSettings(): Promise<boolean> {
  const ua = navigator.userAgent;
  let url: string | null = null;
  if (ua.includes("Mac")) {
    url = "x-apple.systempreferences:com.apple.preference.notifications";
  } else if (ua.includes("Windows")) {
    url = "ms-settings:notifications";
  }
  if (!url) return false;
  try {
    await openUrl(url);
    return true;
  } catch {
    return false;
  }
}

async function setDockBadge(count: number): Promise<void> {
  try {
    const win = getCurrentWindow();
    await win.setBadgeCount(count > 0 ? count : undefined);
  } catch {
    /* pure-vite / unsupported platform */
  }
}

async function requestAttention(): Promise<void> {
  try {
    const win = getCurrentWindow();
    await win.requestUserAttention(UserAttentionType.Informational);
  } catch {
    /* pure-vite / unsupported */
  }
}

async function focusMainWindow(): Promise<void> {
  try {
    const win = getCurrentWindow();
    await win.unminimize();
    await win.show();
    await win.setFocus();
  } catch {
    /* pure-vite */
  }
}

export interface OsNotifyOpenEvent {
  kind: string;
  threadId?: number | null;
  directionId?: number | null;
  askId?: number | null;
  workspaceId?: number | null;
}

/** Apply a notification click to the in-app navigation surface. */
export async function handleNotifyOpen(
  payload: OsNotifyOpenEvent,
  deps: {
    selectWorkspace: (id: number) => Promise<void> | void;
    goToDirectionRef: (thread: number, dir: string) => Promise<void>;
    openNeeds: () => void;
    openSettings: (page?: "resources" | "general" | "appearance" | "automation" | "skills" | "im" | "backup") => void;
    activeWorkspaceId: number | null;
  },
): Promise<void> {
  await focusMainWindow();
  const intents = planNotifyOpen(payload);
  for (const intent of intents) {
    if (intent.type === "workspace") {
      if (intent.workspaceId !== deps.activeWorkspaceId) {
        await deps.selectWorkspace(intent.workspaceId);
      }
      continue;
    }
    if (intent.type === "resources") {
      deps.openSettings("resources");
      continue;
    }
    if (intent.type === "direction") {
      await deps.goToDirectionRef(intent.threadId, intent.direction);
      continue;
    }
    if (intent.type === "needs") {
      deps.openNeeds();
    }
  }
}

async function sendOsNotification(
  title: string,
  body: string,
  route: NotifyRoute,
): Promise<void> {
  await api.osNotifySend({
    title,
    body,
    kind: route.kind,
    threadId: route.threadId ?? null,
    directionId: route.directionId ?? null,
    askId: route.askId ?? null,
    workspaceId: route.workspaceId ?? null,
  });
}

/**
 * Mounted once in App. Reuses the store's Needs-you aggregation, overview,
 * live session/lead turn state, and process-quota snapshot. First load and
 * workspace switches rebuild the baseline silently.
 *
 * Delivery uses the community `user-notify` bridge (not the official Tauri
 * notification plugin) so desktop clicks can deep-link via `notify://open`.
 */
export function useSystemNotifications() {
  const {
    needs,
    asks,
    writeTriggers,
    overview,
    sessions,
    leadTurn,
    processQuota,
    threads,
    notifyEnabled,
    notifyCategories,
    quietHours,
    activeWorkspaceId,
    selectWorkspace,
    goToDirectionRef,
    openNeeds,
    openSettings,
  } = useStore();
  const { t } = useTranslation();
  const prev = useRef<NotifySnapshot | null>(null);
  const baselineWs = useRef<number | null>(null);
  const granted = useRef<boolean | null>(null);
  const [windowFocused, setWindowFocused] = useState<boolean | null>(null);
  const lastBadge = useRef<number | null>(null);

  const threadsById = useRef<Record<number, { title: string }>>({});
  useEffect(() => {
    const m: Record<number, { title: string }> = {};
    for (const th of threads) m[th.id] = { title: th.title };
    threadsById.current = m;
  }, [threads]);

  // OS permission, settled once per enable.
  useEffect(() => {
    if (!notifyEnabled) return;
    void ensureNotifyPermission().then((p) => {
      granted.current = p === "granted";
    });
  }, [notifyEnabled]);

  // Track OS-level window focus.
  useEffect(() => {
    let unFocus: (() => void) | undefined;
    let cancelled = false;
    void (async () => {
      try {
        const win = getCurrentWindow();
        const focused = await win.isFocused();
        if (!cancelled) setWindowFocused(focused);
        unFocus = await win.onFocusChanged(({ payload }) => {
          setWindowFocused(payload);
        });
      } catch {
        if (!cancelled) setWindowFocused(null);
      }
    })();
    return () => {
      cancelled = true;
      unFocus?.();
    };
  }, []);

  // Click / action deep-link from the native bridge.
  useEffect(() => {
    let unlisten: (() => void) | undefined;
    let cancelled = false;
    void (async () => {
      try {
        unlisten = await listen<OsNotifyOpenEvent>("notify://open", (event) => {
          void handleNotifyOpen(event.payload, {
            selectWorkspace,
            goToDirectionRef,
            openNeeds,
            openSettings,
            activeWorkspaceId,
          });
        });
      } catch {
        /* pure-vite */
      }
      if (cancelled) unlisten?.();
    })();
    return () => {
      cancelled = true;
      unlisten?.();
    };
  }, [
    selectWorkspace,
    goToDirectionRef,
    openNeeds,
    openSettings,
    activeWorkspaceId,
  ]);

  // Dock / taskbar badge tracks actionable Needs-you count.
  useEffect(() => {
    const count = badgeCountFrom(needs, asks, writeTriggers);
    if (lastBadge.current === count) return;
    lastBadge.current = count;
    void setDockBadge(count);
  }, [needs, asks, writeTriggers]);

  useEffect(() => {
    const next = snapshotOf(
      needs,
      asks,
      writeTriggers,
      overview,
      sessions,
      leadTurn,
      processQuota,
      threadsById.current,
      activeWorkspaceId,
    );
    const base = baselineWs.current === activeWorkspaceId ? prev.current : null;
    prev.current = next;
    baselineWs.current = activeWorkspaceId;
    if (!base) return; // first load / workspace switch: baseline only
    if (!notifyEnabled || granted.current !== true) return;
    if (
      isAppInForeground({
        windowFocused,
        documentFocused: document.hasFocus(),
      })
    ) {
      return;
    }
    if (isInQuietHours(quietHours)) return;

    const events = diffForNotifications(base, next, notifyCategories);
    if (events.length === 0) return;

    void (async () => {
      let sent = false;
      for (const ev of events) {
        const keys = notifyCopyKeys(ev.kind);
        const title = t(keys.title);
        const body =
          ev.count === 1 ? ev.sample : t(keys.body, { count: ev.count });
        try {
          await sendOsNotification(title, body, ev.route);
          sent = true;
        } catch {
          /* never let a failed ping disturb the app */
        }
      }
      if (sent) void requestAttention();
    })();
  }, [
    needs,
    asks,
    writeTriggers,
    overview,
    sessions,
    leadTurn,
    processQuota,
    notifyEnabled,
    notifyCategories,
    quietHours,
    activeWorkspaceId,
    windowFocused,
    t,
  ]);
}
