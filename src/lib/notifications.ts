import { useCallback, useEffect, useRef, useState } from "react";
import { openUrl } from "@tauri-apps/plugin-opener";
import { getCurrentWindow, UserAttentionType } from "@tauri-apps/api/window";
import { listen } from "@tauri-apps/api/event";
import { useTranslation } from "react-i18next";
import { useStore } from "../state/store";
import { api } from "./api";
import {
  diffForNotifications,
  emptyNotifySnapshot,
  isAppInForeground,
  isInQuietHours,
  NOTIFY_CATEGORIES,
  notifyCopyKeys,
  planNotifyOpen,
  snapshotOf,
  type NotifyCategory,
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
let notifyPermissionInFlight: Promise<NotifyPermission> | null = null;

export function ensureNotifyPermission(): Promise<NotifyPermission> {
  if (notifyPermissionInFlight) return notifyPermissionInFlight;

  const request = (async () => {
    const p = await notifyPermission();
    if (p !== "prompt") return p;
    try {
      return asNotifyPermission(await api.osNotifyRequestPermission());
    } catch {
      return "denied";
    }
  })();
  notifyPermissionInFlight = request;
  void request.then(
    () => {
      if (notifyPermissionInFlight === request) notifyPermissionInFlight = null;
    },
    () => {
      if (notifyPermissionInFlight === request) notifyPermissionInFlight = null;
    },
  );
  return request;
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

function notifyOpenKey(payload: OsNotifyOpenEvent): string {
  return JSON.stringify([
    payload.kind,
    payload.threadId ?? null,
    payload.directionId ?? null,
    payload.repoId ?? null,
    payload.sessionId ?? null,
    payload.askId ?? null,
    payload.workspaceId ?? null,
    payload.openNeeds ?? null,
    payload.openCurator ?? null,
  ]);
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

/** Restore a deferred route only when a newer click has not replaced it. */
function putPendingNavIfUnchanged(pending: PendingNav): void {
  try {
    const raw = sessionStorage.getItem(PENDING_NAV_KEY);
    if (raw) {
      const current = JSON.parse(raw) as PendingNav;
      if (notifyOpenKey(current) !== notifyOpenKey(pending)) return;
    }
    sessionStorage.setItem(PENDING_NAV_KEY, JSON.stringify(pending));
  } catch {
    /* ignore malformed storage / private mode */
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
    workspaceRestoring?: boolean;
    workspaceLoadReady?: boolean;
    needsHydrated?: boolean;
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
      // the deep-link destination. The store coalesces the direct call with the
      // activeWorkspaceId effect, so one successful load advances the marker.
      stashPendingNav(payload, {
        expectedLoadSeq: deps.workspaceLoadSeq + 1,
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
  if (
    deps.activeWorkspaceId != null &&
    deps.workspaceLoadReady === false
  ) {
    stashPendingNav(payload, {
      expectedLoadSeq: deps.workspaceLoadSeq + 1,
    });
    try {
      await deps.selectWorkspace(deps.activeWorkspaceId);
    } catch {
      /* Keep the deferred route; a later successful load can apply it. */
    }
    return;
  }
  if (deps.workspaceRestoring) {
    stashPendingNav(payload);
    return;
  }
  const needsIntent = intents.some((intent) => intent.type === "needs");
  if (needsIntent && deps.needsHydrated === false) {
    stashPendingNav(payload);
    return;
  }
  // Global routes (quota and ownership-unknown Needs-you entries) also mutate
  // the current surface. A workspace reset that is already in flight would
  // otherwise erase Resources/Needs immediately after this handler opens it.
  if (deps.workspaceLoading) {
    stashPendingNav(payload);
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
    workspaceRestoring,
    workspaceLoadReady,
    selectWorkspace,
    goToDirectionRef,
    openNeeds,
    openSettings,
    openCurator,
  } = useStore();
  const needsHydrated =
    notificationHydration.workspaceId === activeWorkspaceId &&
    notificationHydration.needs;
  const { t } = useTranslation();
  const prev = useRef<NotifySnapshot | null>(null);
  const baselineWs = useRef<number | null>(null);
  const baselineReady = useRef<Set<NotifyCategory>>(new Set());
  const sourceReadyPrevious = useRef<Record<NotifyCategory, boolean>>({
    needs: false,
    review: false,
    stalled: false,
    quota: false,
  });
  const liveWorkersReadyPrevious = useRef(false);
  const asksReadyPrevious = useRef(false);
  const workspaceNeedsReadyPrevious = useRef(false);
  const [permissionState, setPermissionState] = useState<NotifyPermission>("prompt");
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
      setPermissionState(p);
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
          if (cancelled) return;
          setWindowFocused(payload);
          if (payload && notifyEnabled) {
            void notifyPermission().then((p) => {
              if (!cancelled) setPermissionState(p);
            });
          }
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
  }, [notifyEnabled]);

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
    workspaceRestoring,
    workspaceLoadReady,
    needsHydrated,
  });
  const navigationTailRef = useRef<Promise<void>>(Promise.resolve());
  const navigationSelectWorkspace = useCallback(
    async (id: number) => {
      // React's passive effects can lag a notification event by one tick. Keep
      // the navigation snapshot synchronously honest while the store starts its
      // async workspace load so a following click cannot open over the reset.
      navDepsRef.current.activeWorkspaceId = id;
      navDepsRef.current.workspaceLoading = true;
      navDepsRef.current.workspaceRestoring = false;
      navDepsRef.current.workspaceLoadReady = false;
      await selectWorkspace(id);
    },
    [selectWorkspace],
  );
  useEffect(() => {
    navDepsRef.current = {
      selectWorkspace: navigationSelectWorkspace,
      goToDirectionRef,
      openNeeds,
      openSettings,
      openCurator,
      activeWorkspaceId,
      workspaceLoadSeq,
      workspaceLoading,
      workspaceRestoring,
      workspaceLoadReady,
      needsHydrated,
    };
  }, [
    navigationSelectWorkspace,
    goToDirectionRef,
    openNeeds,
    openSettings,
    openCurator,
    activeWorkspaceId,
    workspaceLoadSeq,
    workspaceLoading,
    workspaceRestoring,
    workspaceLoadReady,
    needsHydrated,
  ]);

  useEffect(() => {
    let unlisten: (() => void) | undefined;
    let cancelled = false;
    let takenPending: OsNotifyOpenEvent | null = null;
    const openInFlight = new Map<string, Promise<void>>();

    const handleOpen = (payload: OsNotifyOpenEvent): Promise<void> => {
      const key = notifyOpenKey(payload);
      const existing = openInFlight.get(key);
      if (existing) return existing;
      const task = navigationTailRef.current.then(async () => {
        await handleNotifyOpen(payload, navDepsRef.current);
        // Native delivery retains the payload until the frontend confirms it was
        // handled. This also prevents a live event from being drained again by
        // the ordered cold-start check below.
        try {
          await api.osNotifyAckOpen(payload);
        } catch {
          /* pure-vite */
        }
      });
      navigationTailRef.current = task.catch(() => undefined);
      openInFlight.set(key, task);
      void task.then(
        () => {
          if (openInFlight.get(key) === task) openInFlight.delete(key);
        },
        () => {
          if (openInFlight.get(key) === task) openInFlight.delete(key);
        },
      );
      return task;
    };

    void (async () => {
      try {
        unlisten = await listen<OsNotifyOpenEvent>("notify://open", (event) => {
          void handleOpen(event.payload).catch(() => undefined);
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
        if (takenPending) {
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
  const pendingAppliedKey = useRef<string | null>(null);
  const pendingApplyingKey = useRef<string | null>(null);
  const pendingNavRetryTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const [pendingNavRetry, setPendingNavRetry] = useState(0);
  useEffect(() => {
    return () => {
      if (pendingNavRetryTimer.current != null) {
        clearTimeout(pendingNavRetryTimer.current);
        pendingNavRetryTimer.current = null;
      }
    };
  }, []);
  useEffect(() => {
    void pendingNavRetry;
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
    if (workspaceRestoring) {
      putPendingNav(pending);
      return;
    }
    if (activeWorkspaceId != null && !workspaceLoadReady) {
      // A failed workspace load must never be treated as a route prerequisite;
      // leave the click pending until a later selection commits successfully.
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
    const applicationKey = `${workspaceLoadSeq}:${notifyOpenKey(pending)}`;
    if (
      pendingAppliedKey.current === applicationKey ||
      pendingApplyingKey.current === applicationKey
    ) {
      // Already applied for this load.
      return;
    }
    // If a workspace switch just started, wait for its load seq bump.
    if (pending.workspaceId != null && workspaceLoadSeq === 0) {
      putPendingNav(pending);
      return;
    }
    const needsIntent = planNotifyOpen(pending).some(
      (intent) => intent.type === "needs",
    );
    if (needsIntent && !needsHydrated) {
      // selectWorkspace clears the old workspace's Needs rows; wait until the
      // target workspace has completed its authoritative Needs refresh.
      putPendingNav(pending);
      return;
    }
    pendingApplyingKey.current = applicationKey;
    const application = navigationTailRef.current.then(() =>
      applyNotifyIntents(pending, {
        goToDirectionRef,
        openNeeds,
        openSettings,
        openCurator,
      }),
    );
    navigationTailRef.current = application.catch(() => undefined);
    void application.then(
      () => {
        pendingApplyingKey.current = null;
        pendingAppliedKey.current = applicationKey;
        void api.osNotifyAckOpen(pending).catch(() => {
          /* pure-vite / older backend */
        });
      },
      () => {
        pendingApplyingKey.current = null;
        putPendingNavIfUnchanged(pending);
        if (pendingNavRetryTimer.current == null) {
          pendingNavRetryTimer.current = setTimeout(() => {
            pendingNavRetryTimer.current = null;
            setPendingNavRetry((n) => n + 1);
          }, 1_000);
        }
      }
    );
  }, [
    activeWorkspaceId,
    workspaceLoadSeq,
    workspaceLoading,
    workspaceRestoring,
    workspaceLoadReady,
    needsHydrated,
    goToDirectionRef,
    openNeeds,
    openSettings,
    openCurator,
    pendingNavRetry,
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
  // Each category keeps its own baseline so a ready source can accumulate real
  // events while an unrelated source is still retrying.
  useEffect(() => {
    const sourceWorkspaceMatches =
      notificationHydration.workspaceId === activeWorkspaceId;
    const asksReady = sourceWorkspaceMatches && notificationHydration.asks;
    const workspaceNeedsReady =
      sourceWorkspaceMatches && notificationHydration.workspaceNeeds;
    const liveWorkersReady =
      sourceWorkspaceMatches && notificationHydration.liveWorkers;
    const sessionRefs: Record<
      number,
      {
        info: { session_id: number };
        status: (typeof sessions)[number]["status"];
        directionId: number;
        repoId: number;
        threadId: number;
        eventDriven?: boolean;
        workspaceId?: number;
      }
    > = {};
    for (const [sid, s] of Object.entries(sessions)) {
      if (!liveWorkersReady && !s.eventDriven) continue;
      sessionRefs[Number(sid)] = {
        info: { session_id: s.info.session_id },
        status: s.status,
        directionId: s.directionId,
        repoId: s.repoId,
        threadId: s.threadId,
        eventDriven: s.eventDriven,
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
    const sourceReady: Record<NotifyCategory, boolean> = {
      needs: asksReady || workspaceNeedsReady,
      review: sourceWorkspaceMatches && notificationHydration.overview,
      // Lead-turn pushes and locally observed sessions are authoritative even
      // while the adopted-worker snapshot is retrying.
      stalled: sourceWorkspaceMatches,
      quota: sourceWorkspaceMatches && notificationHydration.quota,
    };
    const eventStalledKeys = new Set<string>();
    for (const s of Object.values(sessionRefs)) {
      if (s.eventDriven && s.status === "stalled") {
        eventStalledKeys.add(`stall:worker:${s.info.session_id}`);
      }
    }
    for (const [tidStr, turn] of Object.entries(leadTurn)) {
      if (turn.state !== "stalled") continue;
      const tid = Number(tidStr);
      const kind = threadsById.current[tid]?.kind;
      eventStalledKeys.add(
        kind === "curator" ? `stall:curator:${tid}` : `stall:lead:${tid}`,
      );
    }
    const sameWorkspace =
      baselineWs.current === activeWorkspaceId && prev.current != null;
    const needsSourceBecameUnready =
      (asksReadyPrevious.current && !asksReady) ||
      (workspaceNeedsReadyPrevious.current && !workspaceNeedsReady);
    const liveWorkersBecameReady =
      liveWorkersReady && !liveWorkersReadyPrevious.current;
    const sourceBecameUnready =
      needsSourceBecameUnready ||
      NOTIFY_CATEGORIES.some(
        (kind) => sourceReadyPrevious.current[kind] && !sourceReady[kind],
      );
    if (!sameWorkspace || sourceBecameUnready) {
      baselineWs.current = activeWorkspaceId;
      prev.current = emptyNotifySnapshot();
      baselineReady.current.clear();
      liveWorkersReadyPrevious.current = false;
      asksReadyPrevious.current = false;
      workspaceNeedsReadyPrevious.current = false;
    }
    sourceReadyPrevious.current = sourceReady;
    liveWorkersReadyPrevious.current = liveWorkersReady;
    const asksBecameReady = asksReady && !asksReadyPrevious.current;
    const workspaceNeedsBecameReady =
      workspaceNeedsReady && !workspaceNeedsReadyPrevious.current;
    asksReadyPrevious.current = asksReady;
    workspaceNeedsReadyPrevious.current = workspaceNeedsReady;
    const base = prev.current;
    if (!base) return;
    const isNeedsKeyReady = (key: string): boolean => {
      if (key.startsWith("ask:")) return asksReady;
      return workspaceNeedsReady;
    };
    for (const kind of NOTIFY_CATEGORIES) {
      if (sourceReady[kind] && !baselineReady.current.has(kind)) {
        if (kind === "needs") {
          base.needs = new Map(
            [...next.needs].filter(([key]) => isNeedsKeyReady(key)),
          );
        } else {
          base[kind] = new Map(next[kind]);
        }
        baselineReady.current.add(kind);
      }
    }
    if (asksBecameReady) {
      for (const [key, entry] of next.needs) {
        if (key.startsWith("ask:")) base.needs.set(key, entry);
      }
    }
    if (workspaceNeedsBecameReady) {
      for (const [key, entry] of next.needs) {
        if (!key.startsWith("ask:")) base.needs.set(key, entry);
      }
    }
    if (liveWorkersBecameReady) {
      // Initial adopted-worker rows are not transitions. Keep lead/local event
      // keys in the diff baseline so stalls observed before hydration survive.
      for (const [key, entry] of next.stalled) {
        if (!eventStalledKeys.has(key)) base.stalled.set(key, entry);
      }
    }
    const allSourcesHydrated = NOTIFY_CATEGORIES.every(
      (kind) => sourceReady[kind],
    );
    if (!allSourcesHydrated) {
      // Keep ready categories' old baselines; newly arrived entries will be
      // compared once the unrelated source also becomes authoritative.
      return;
    }
    const nextForDiff = {
      ...next,
      needs: new Map(
        [...next.needs].filter(([key]) => isNeedsKeyReady(key)),
      ),
    };
    const events = diffForNotifications(base, nextForDiff, notifyCategories);
    // Advance every category before the foreground/quiet-hours gate, preserving
    // the existing behavior that suppressed events are not replayed later.
    const baselineNeeds = new Map(
      [...base.needs].filter(([key]) => !isNeedsKeyReady(key)),
    );
    for (const [key, entry] of next.needs) {
      if (isNeedsKeyReady(key)) baselineNeeds.set(key, entry);
    }
    prev.current = { ...next, needs: baselineNeeds };
    if (workspaceLoading) return;
    if (!notifyEnabled || permissionState !== "granted") return;
    if (
      isAppInForeground({
        windowFocused,
        documentFocused: document.hasFocus(),
      })
    ) {
      return;
    }
    if (isInQuietHours(quietHours)) return;

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
    permissionState,
  ]);
}
