import { useEffect, useRef, useState } from "react";
import { openUrl } from "@tauri-apps/plugin-opener";
import { getCurrentWindow, UserAttentionType } from "@tauri-apps/api/window";
import { listen } from "@tauri-apps/api/event";
import { useTranslation } from "react-i18next";
import { useStore } from "../state/store";
import { api } from "./api";
import {
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
    // Backend is authoritative for denied/granted (user-notify). Only use the
    // Web Notification bit as a secondary signal when backend is still prompt.
    if (p === "granted" || p === "denied") return p;
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
  repoId?: number | null;
  sessionId?: number | null;
  askId?: number | null;
  workspaceId?: number | null;
  openNeeds?: boolean | null;
}

/** Apply a notification click to the in-app navigation surface. */
const PENDING_NAV_KEY = "weft-notify-pending-nav";

function stashPendingNav(payload: OsNotifyOpenEvent): void {
  try {
    sessionStorage.setItem(PENDING_NAV_KEY, JSON.stringify(payload));
  } catch {
    /* ignore quota / private mode */
  }
}

export function takePendingNav(): OsNotifyOpenEvent | null {
  try {
    const raw = sessionStorage.getItem(PENDING_NAV_KEY);
    if (!raw) return null;
    sessionStorage.removeItem(PENDING_NAV_KEY);
    return JSON.parse(raw) as OsNotifyOpenEvent;
  } catch {
    return null;
  }
}

export async function applyNotifyIntents(
  payload: OsNotifyOpenEvent,
  deps: {
    goToDirectionRef: (
      thread: number,
      dir: string,
      opts?: { repoId?: number; sessionId?: number },
    ) => Promise<void>;
    openNeeds: () => void;
    openSettings: (page?: "resources" | "general" | "appearance" | "automation" | "skills" | "im" | "backup") => void;
  },
): Promise<void> {
  const intents = planNotifyOpen(payload).filter((i) => i.type !== "workspace");
  for (const intent of intents) {
    if (intent.type === "resources") {
      deps.openSettings("resources");
      continue;
    }
    if (intent.type === "direction") {
      await deps.goToDirectionRef(intent.threadId, intent.direction, {
        repoId: intent.repoId,
        sessionId: intent.sessionId,
      });
      continue;
    }
    if (intent.type === "needs") {
      deps.openNeeds();
    }
  }
}

/** Apply a notification click to the in-app navigation surface. */
export async function handleNotifyOpen(
  payload: OsNotifyOpenEvent,
  deps: {
    selectWorkspace: (id: number) => Promise<void> | void;
    goToDirectionRef: (
      thread: number,
      dir: string,
      opts?: { repoId?: number; sessionId?: number },
    ) => Promise<void>;
    openNeeds: () => void;
    openSettings: (page?: "resources" | "general" | "appearance" | "automation" | "skills" | "im" | "backup") => void;
    activeWorkspaceId: number | null;
  },
): Promise<void> {
  await focusMainWindow();
  const intents = planNotifyOpen(payload);
  const workspaceIntent = intents.find((i) => i.type === "workspace");
  if (
    workspaceIntent &&
    workspaceIntent.type === "workspace" &&
    workspaceIntent.workspaceId !== deps.activeWorkspaceId
  ) {
    // Defer the rest until the store finishes the workspace switch effect.
    // A second selectWorkspace(activeWorkspaceId) otherwise races and clears
    // the deep-link destination.
    stashPendingNav(payload);
    await deps.selectWorkspace(workspaceIntent.workspaceId);
    return;
  }
  await applyNotifyIntents(payload, deps);
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
    repoId: route.repoId ?? null,
    sessionId: route.sessionId ?? null,
    askId: route.askId ?? null,
    workspaceId: route.workspaceId ?? null,
    openNeeds: route.openNeeds ?? null,
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
    needsByWorkspace,
    threadWorkspaceById,
    workspaceLoadSeq,
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

  const threadsById = useRef<Record<number, { title: string; workspaceId?: number }>>({});
  useEffect(() => {
    const m: Record<number, { title: string; workspaceId?: number }> = {
      ...threadsById.current,
    };
    for (const [idStr, ws] of Object.entries(threadWorkspaceById)) {
      const id = Number(idStr);
      const prev = m[id];
      m[id] = { title: prev?.title ?? `#${id}`, workspaceId: ws };
    }
    for (const th of threads) {
      m[th.id] = { title: th.title, workspaceId: th.workspace_id };
    }
    threadsById.current = m;
  }, [threads, threadWorkspaceById]);

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

  // Live click / action deep-link from the native bridge. Keep this effect
  // dependency-light so we do not re-drain pending opens on every navigation.
  const navDepsRef = useRef({
    selectWorkspace,
    goToDirectionRef,
    openNeeds,
    openSettings,
    activeWorkspaceId,
  });
  useEffect(() => {
    navDepsRef.current = {
      selectWorkspace,
      goToDirectionRef,
      openNeeds,
      openSettings,
      activeWorkspaceId,
    };
  }, [
    selectWorkspace,
    goToDirectionRef,
    openNeeds,
    openSettings,
    activeWorkspaceId,
  ]);

  useEffect(() => {
    let unlisten: (() => void) | undefined;
    let cancelled = false;
    void (async () => {
      try {
        unlisten = await listen<OsNotifyOpenEvent>("notify://open", (event) => {
          void (async () => {
            await handleNotifyOpen(event.payload, navDepsRef.current);
            // Live delivery was handled — drop any retained pending copy.
            try {
              await api.osNotifyAckOpen();
            } catch {
              /* pure-vite */
            }
          })();
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
  }, []);

  // Cold-start only: drain a pending open once after mount.
  useEffect(() => {
    let cancelled = false;
    void (async () => {
      try {
        const pending = await api.osNotifyTakePendingOpen();
        if (!cancelled && pending) {
          await handleNotifyOpen(pending, navDepsRef.current);
        }
      } catch {
        /* pure-vite */
      }
    })();
    return () => {
      cancelled = true;
    };
  }, []);

  // Finish a deep link that had to switch workspaces first. Wait until
  // selectWorkspace has finished loading (workspaceLoadSeq) so a concurrent
  // selection reset cannot clear the destination right after we navigate.
  const pendingAppliedSeq = useRef<number | null>(null);
  useEffect(() => {
    const pending = takePendingNav();
    if (!pending) return;
    if (
      pending.workspaceId != null &&
      pending.workspaceId !== activeWorkspaceId
    ) {
      // Not settled yet — put it back.
      try {
        sessionStorage.setItem(
          "weft-notify-pending-nav",
          JSON.stringify(pending),
        );
      } catch {
        /* ignore */
      }
      return;
    }
    if (pendingAppliedSeq.current === workspaceLoadSeq) {
      // Already applied for this load; put back only if still relevant.
      return;
    }
    // If a workspace switch just started, wait for its load seq bump.
    // workspaceLoadSeq is 0 before any selection completes; still allow apply
    // once activeWorkspaceId already matches the target after a completed load.
    if (pending.workspaceId != null && workspaceLoadSeq === 0) {
      try {
        sessionStorage.setItem(
          "weft-notify-pending-nav",
          JSON.stringify(pending),
        );
      } catch {
        /* ignore */
      }
      return;
    }
    pendingAppliedSeq.current = workspaceLoadSeq;
    void applyNotifyIntents(pending, {
      goToDirectionRef,
      openNeeds,
      openSettings,
    });
  }, [
    activeWorkspaceId,
    workspaceLoadSeq,
    goToDirectionRef,
    openNeeds,
    openSettings,
  ]);

  // Dock / taskbar badge tracks actionable Needs-you across all workspaces.
  // needsByWorkspace already includes questions + asks + writeTriggers +
  // action-required notices per workspace (self-clearing notices stay out).
  useEffect(() => {
    const count = Object.values(needsByWorkspace).reduce((a, b) => a + b, 0);
    if (lastBadge.current === count) return;
    lastBadge.current = count;
    void setDockBadge(count);
  }, [needsByWorkspace]);

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
