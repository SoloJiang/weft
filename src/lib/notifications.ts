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
  openCurator?: boolean | null;
}

/** Apply a notification click to the in-app navigation surface. */
const PENDING_NAV_KEY = "weft-notify-pending-nav";
const NAV_RUNTIME_GENERATION = `${Date.now()}-${Math.random().toString(36).slice(2)}`;

type PendingNav = OsNotifyOpenEvent & {
  /** Absolute workspaceLoadSeq that must be reached before applying intents. */
  expectedLoadSeq?: number;
  /** Runtime token used to rebase sequence markers after a WebView reload. */
  loadGeneration?: string;
};

function stashPendingNav(
  payload: OsNotifyOpenEvent,
  opts?: { expectedLoadSeq?: number },
): void {
  try {
    const pending: PendingNav = { ...payload };
    pending.loadGeneration = NAV_RUNTIME_GENERATION;
    if (opts?.expectedLoadSeq != null) {
      pending.expectedLoadSeq = opts.expectedLoadSeq;
    }
    sessionStorage.setItem(PENDING_NAV_KEY, JSON.stringify(pending));
  } catch {
    /* ignore quota / private mode */
  }
}

export function takePendingNav(): PendingNav | null {
  try {
    const raw = sessionStorage.getItem(PENDING_NAV_KEY);
    if (!raw) return null;
    sessionStorage.removeItem(PENDING_NAV_KEY);
    return JSON.parse(raw) as PendingNav;
  } catch {
    return null;
  }
}

function putPendingNav(pending: PendingNav): void {
  try {
    sessionStorage.setItem(PENDING_NAV_KEY, JSON.stringify(pending));
  } catch {
    /* ignore */
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
    openCurator: () => void;
  },
): Promise<void> {
  const intents = planNotifyOpen(payload).filter((i) => i.type !== "workspace");
  for (const intent of intents) {
    if (intent.type === "resources") {
      deps.openSettings("resources");
      continue;
    }
    if (intent.type === "curator") {
      deps.openCurator();
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
    openCurator: () => void;
    activeWorkspaceId: number | null;
    workspaceLoadSeq: number;
    workspaceLoading?: boolean;
  },
): Promise<void> {
  await focusMainWindow();
  const intents = planNotifyOpen(payload);
  const workspaceIntent = intents.find((i) => i.type === "workspace");
  if (workspaceIntent && workspaceIntent.type === "workspace") {
    const sameWorkspace =
      workspaceIntent.workspaceId === deps.activeWorkspaceId;
    if (!sameWorkspace) {
      // Defer the rest until the store finishes the workspace switch effect.
      // A second selectWorkspace(activeWorkspaceId) otherwise races and clears
      // the deep-link destination. Capture the sequence before starting either
      // load: the direct call plus the [activeWorkspaceId] effect each bump it.
      stashPendingNav(payload, {
        expectedLoadSeq: deps.workspaceLoadSeq + 2,
      });
      await deps.selectWorkspace(workspaceIntent.workspaceId);
      return;
    }
    // Same workspace id, but a load may still be in flight (cold-start restore
    // or manual switch). Defer until that load finishes so its reset cannot
    // wipe the deep-link destination.
    if (deps.workspaceLoading) {
      stashPendingNav(payload, {
        expectedLoadSeq: deps.workspaceLoadSeq + 1,
      });
      return;
    }
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
    openCurator: route.openCurator ?? null,
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
    notificationHydration,
    threads,
    notifyEnabled,
    notifyCategories,
    quietHours,
    activeWorkspaceId,
    needsByWorkspace,
    threadWorkspaceById,
    workspaceLoadSeq,
    workspaceLoading,
    selectWorkspace,
    goToDirectionRef,
    openNeeds,
    openSettings,
    openCurator,
  } = useStore();
  const { t } = useTranslation();
  const prev = useRef<NotifySnapshot | null>(null);
  const baselineWs = useRef<number | null>(null);
  const granted = useRef<boolean | null>(null);
  const [windowFocused, setWindowFocused] = useState<boolean | null>(null);
  const lastBadge = useRef<number | null>(null);

  const threadsById = useRef<
    Record<number, { title: string; workspaceId?: number; kind?: string }>
  >({});
  useEffect(() => {
    const m: Record<
      number,
      { title: string; workspaceId?: number; kind?: string }
    > = {
      ...threadsById.current,
    };
    for (const [idStr, ws] of Object.entries(threadWorkspaceById)) {
      const id = Number(idStr);
      const prevMeta = m[id];
      m[id] = {
        title: prevMeta?.title ?? `#${id}`,
        workspaceId: ws,
        kind: prevMeta?.kind,
      };
    }
    for (const th of threads) {
      m[th.id] = {
        title: th.title,
        workspaceId: th.workspace_id,
        kind: th.kind,
      };
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
          if (!cancelled) setWindowFocused(payload);
        });
        if (cancelled) {
          unFocus();
          unFocus = undefined;
        }
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
    openCurator,
    activeWorkspaceId,
    workspaceLoadSeq,
    workspaceLoading,
  });
  useEffect(() => {
    navDepsRef.current = {
      selectWorkspace,
      goToDirectionRef,
      openNeeds,
      openSettings,
      openCurator,
      activeWorkspaceId,
      workspaceLoadSeq,
      workspaceLoading,
    };
  }, [
    selectWorkspace,
    goToDirectionRef,
    openNeeds,
    openSettings,
    openCurator,
    activeWorkspaceId,
    workspaceLoadSeq,
    workspaceLoading,
  ]);

  useEffect(() => {
    let unlisten: (() => void) | undefined;
    let cancelled = false;
    let takenPending: OsNotifyOpenEvent | null = null;

    const handleOpen = async (payload: OsNotifyOpenEvent) => {
      await handleNotifyOpen(payload, navDepsRef.current);
      // Native delivery retains the payload until the frontend confirms it was
      // handled. This also prevents a live event from being drained again by
      // the ordered cold-start check below.
      try {
        await api.osNotifyAckOpen();
      } catch {
        /* pure-vite */
      }
    };

    void (async () => {
      try {
        unlisten = await listen<OsNotifyOpenEvent>("notify://open", (event) => {
          void handleOpen(event.payload);
        });
        if (cancelled) {
          unlisten?.();
          return;
        }
        // Register the listener before taking the retained payload. A click
        // arriving during async listen() setup is then either delivered live
        // or consumed by this drain, never lost between the two effects.
        const pending = await api.osNotifyTakePendingOpen();
        if (!pending) return;
        takenPending = pending;
        if (cancelled) {
          await api.osNotifyRestorePendingOpen(pending);
          takenPending = null;
          return;
        }
        await handleOpen(pending);
        takenPending = null;
      } catch {
        if (cancelled && takenPending) {
          try {
            await api.osNotifyRestorePendingOpen(takenPending);
            takenPending = null;
          } catch {
            /* pure-vite / older backend */
          }
        }
        /* pure-vite */
      }
    })();
    return () => {
      cancelled = true;
      unlisten?.();
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
      putPendingNav(pending);
      return;
    }
    if (pending.loadGeneration !== NAV_RUNTIME_GENERATION) {
      // sessionStorage survives a WebView reload, but workspaceLoadSeq does
      // not. The original absolute marker belongs to the old runtime; after a
      // reload only the current workspace load needs to settle.
      pending.loadGeneration = NAV_RUNTIME_GENERATION;
      pending.expectedLoadSeq = workspaceLoadSeq + (workspaceLoading ? 1 : 0);
    }
    if (workspaceLoading) {
      // Active workspace id may already match, but selectWorkspace is still
      // mid-reset/fetch. Wait for it to finish.
      putPendingNav(pending);
      return;
    }
    if (
      pending.expectedLoadSeq != null &&
      workspaceLoadSeq < pending.expectedLoadSeq
    ) {
      putPendingNav(pending);
      return;
    }
    if (pendingAppliedSeq.current === workspaceLoadSeq) {
      // Already applied for this load.
      return;
    }
    // If a workspace switch just started, wait for its load seq bump.
    if (pending.workspaceId != null && workspaceLoadSeq === 0) {
      putPendingNav(pending);
      return;
    }
    pendingAppliedSeq.current = workspaceLoadSeq;
    void applyNotifyIntents(pending, {
      goToDirectionRef,
      openNeeds,
      openSettings,
      openCurator,
    });
  }, [
    activeWorkspaceId,
    workspaceLoadSeq,
    workspaceLoading,
    goToDirectionRef,
    openNeeds,
    openSettings,
    openCurator,
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

  // Notification sources hydrate asynchronously after mount / workspace switch.
  // The store marks each source only after its authoritative request completes;
  // do not arm diffs from a fixed timeout while those requests are still empty.
  const sourcesHydrated =
    notificationHydration.workspaceId === activeWorkspaceId &&
    notificationHydration.needs &&
    notificationHydration.overview &&
    notificationHydration.quota &&
    notificationHydration.liveWorkers;
  useEffect(() => {
    const sessionRefs: Record<
      number,
      {
        info: { session_id: number };
        status: (typeof sessions)[number]["status"];
        directionId: number;
        repoId: number;
        threadId: number;
        workspaceId?: number;
      }
    > = {};
    for (const [sid, s] of Object.entries(sessions)) {
      sessionRefs[Number(sid)] = {
        info: { session_id: s.info.session_id },
        status: s.status,
        directionId: s.directionId,
        repoId: s.repoId,
        threadId: s.threadId,
        workspaceId: s.workspaceId,
      };
    }
    const next = snapshotOf(
      needs,
      asks,
      writeTriggers,
      overview,
      sessionRefs,
      leadTurn,
      processQuota,
      threadsById.current,
      activeWorkspaceId,
    );
    const sameWorkspace = baselineWs.current === activeWorkspaceId;
    const base = sameWorkspace ? prev.current : null;
    prev.current = next;
    baselineWs.current = activeWorkspaceId;

    // First pass for a workspace, while it is loading, or before every initial
    // source has completed: only establish/update the silent baseline.
    if (!base || workspaceLoading || !sourcesHydrated) {
      return;
    }
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
    notificationHydration,
    notifyEnabled,
    notifyCategories,
    quietHours,
    activeWorkspaceId,
    workspaceLoading,
    windowFocused,
    t,
    sourcesHydrated,
  ]);
}
