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
  snapshotOfAttentionSnapshots,
  type NotifyCategoryFlags,
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
export function canOpenSystemNotificationSettings(): boolean {
  if (typeof navigator === "undefined") return false;
  const ua = navigator.userAgent;
  return ua.includes("Mac") || ua.includes("Windows");
}

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
  attentionId?: string | null;
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
    payload.attentionId ?? null,
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
    liveWorkersHydrated?: boolean;
  },
): Promise<boolean> {
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
      return false;
    }
    // Same workspace id, but a load may still be in flight (cold-start restore
    // or manual switch). Defer until that load finishes so its reset cannot
    // wipe the deep-link destination.
    if (deps.workspaceLoading) {
      stashPendingNav(payload, {
        expectedLoadSeq: deps.workspaceLoadSeq + 1,
      });
      return false;
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
    return false;
  }
  if (deps.workspaceRestoring) {
    stashPendingNav(payload);
    return false;
  }
  const needsIntent = intents.some((intent) => intent.type === "needs");
  if (needsIntent && deps.needsHydrated === false) {
    stashPendingNav(payload);
    return false;
  }
  const workerIntent = intents.some(
    (intent) => intent.type === "direction" && intent.sessionId != null,
  );
  if (workerIntent && deps.liveWorkersHydrated === false) {
    stashPendingNav(payload);
    return false;
  }
  // Global routes (quota and ownership-unknown Needs-you entries) also mutate
  // the current surface. A workspace reset that is already in flight would
  // otherwise erase Resources/Needs immediately after this handler opens it.
  if (deps.workspaceLoading) {
    stashPendingNav(payload);
    return false;
  }
  await applyNotifyIntents(payload, deps);
  return true;
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
    askId: null,
    attentionId: route.attentionId ?? null,
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
    attentionSnapshots,
    notificationOverview,
    processQuota,
    notificationHydration,
    notifyEnabled,
    notifyCategories,
    quietHours,
    activeWorkspaceId,
    needsByWorkspace,
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
  const liveWorkersHydrated = notificationHydration.liveWorkers;
  const { t } = useTranslation();
  const prev = useRef<NotifySnapshot | null>(null);
  const baselineWs = useRef<number | null>(null);
  const sourceReadyPrevious = useRef<NotifyCategoryFlags>({
    needs: false,
    review: false,
    quota: false,
  });
  const categoryEnabledPrevious = useRef<NotifyCategoryFlags>({
    needs: false,
    review: false,
    quota: false,
  });
  const [permissionState, setPermissionState] = useState<NotifyPermission>("prompt");
  const [windowFocused, setWindowFocused] = useState<boolean | null>(null);
  const lastBadge = useRef<number | null>(null);

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
    liveWorkersHydrated,
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
      liveWorkersHydrated,
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
    liveWorkersHydrated,
  ]);

  useEffect(() => {
    let unlisten: (() => void) | undefined;
    let cancelled = false;
    let takenPending: OsNotifyOpenEvent | null = null;
    const openInFlight = new Map<string, Promise<boolean>>();

    const handleOpen = (payload: OsNotifyOpenEvent): Promise<boolean> => {
      const key = notifyOpenKey(payload);
      const existing = openInFlight.get(key);
      if (existing) return existing;
      const task = navigationTailRef.current.then(async () => {
        const appliedImmediately = await handleNotifyOpen(payload, navDepsRef.current);
        if (!appliedImmediately) return false;
        // Native delivery retains the payload until the frontend confirms it was
        // handled. This also prevents a live event from being drained again by
        // the ordered cold-start check below.
        try {
          await api.osNotifyAckOpen(payload);
        } catch {
          /* pure-vite */
        }
        return true;
      });
      navigationTailRef.current = task.then(
        () => undefined,
        () => undefined,
      );
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
        const appliedImmediately = await handleOpen(pending);
        if (appliedImmediately) {
          takenPending = null;
        } else {
          // `take` removes the native slot. Keep the payload there as well as in
          // sessionStorage while deferred navigation waits for workspace state.
          await api.osNotifyRestorePendingOpen(pending);
          takenPending = null;
        }
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
    const workerIntent = planNotifyOpen(pending).some(
      (intent) => intent.type === "direction" && intent.sessionId != null,
    );
    if (workerIntent && !liveWorkersHydrated) {
      // A retained legacy worker click must wait for adopted sessions before
      // resolving the exact session route; otherwise it falls back to lead.
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
    liveWorkersHydrated,
    goToDirectionRef,
    openNeeds,
    openSettings,
    openCurator,
    pendingNavRetry,
  ]);

  // Dock / taskbar badge is the sum of canonical workspace snapshot counts.
  useEffect(() => {
    const count = Object.values(needsByWorkspace).reduce((a, b) => a + b, 0);
    if (lastBadge.current === count) return;
    lastBadge.current = count;
    void setDockBadge(count);
  }, [needsByWorkspace]);

  // Each category arms only after its authoritative source hydrates. Startup,
  // workspace switches and preference re-enables establish a silent baseline,
  // so old work is never replayed as a new OS notification.
  useEffect(() => {
    if (workspaceLoading) return;
    const workspaceMatches = notificationHydration.workspaceId === activeWorkspaceId;
    const next = snapshotOfAttentionSnapshots(
      attentionSnapshots,
      notificationOverview,
      processQuota,
    );
    const sourceReady: NotifyCategoryFlags = {
      needs: notificationHydration.workspaceNeeds,
      review: workspaceMatches && notificationHydration.overview,
      quota: notificationHydration.quota,
    };
    const categoryEnabled: NotifyCategoryFlags = {
      needs: notifyEnabled && notifyCategories.needs,
      review: notifyEnabled && notifyCategories.review,
      quota: notifyEnabled && notifyCategories.quota,
    };

    const workspaceChanged = baselineWs.current !== activeWorkspaceId;
    if (prev.current == null || workspaceChanged) {
      const previous = prev.current;
      prev.current = previous ?? emptyNotifySnapshot();
      baselineWs.current = activeWorkspaceId;
      if (previous) {
        // Needs and quota are global. A workspace switch must not reset their
        // diff baseline or suppress an action created in another workspace
        // during the switch. Review hydration is re-armed with the workspace
        // overview load below.
        sourceReadyPrevious.current.review = false;
        categoryEnabledPrevious.current.review = false;
      } else {
        sourceReadyPrevious.current.needs = false;
        sourceReadyPrevious.current.review = false;
        sourceReadyPrevious.current.quota = false;
        categoryEnabledPrevious.current.needs = false;
        categoryEnabledPrevious.current.review = false;
        categoryEnabledPrevious.current.quota = false;
      }
    }

    const baseline = prev.current;
    if (!baseline) return;
    const events = [];
    for (const kind of NOTIFY_CATEGORIES) {
      const ready = sourceReady[kind];
      const enabled = categoryEnabled[kind];
      const becameReady = ready && !sourceReadyPrevious.current[kind];
      const becameEnabled = enabled && !categoryEnabledPrevious.current[kind];
      const firstQuotaPush =
        kind === "quota" &&
        becameReady &&
        notificationHydration.quotaPushPending;
      if (ready && enabled && ((!becameReady && !becameEnabled) || firstQuotaPush)) {
        const onlyKind: NotifyCategoryFlags = {
          needs: kind === "needs",
          review: kind === "review",
          quota: kind === "quota",
        };
        events.push(...diffForNotifications(baseline, next, onlyKind));
      }
      if (ready) {
        baseline[kind] = new Map(next[kind]);
      }
      sourceReadyPrevious.current[kind] = ready;
      categoryEnabledPrevious.current[kind] = enabled;
    }
    prev.current = baseline;

    if (!notifyEnabled || permissionState !== "granted") return;
    if (
      isAppInForeground({
        windowFocused,
        documentFocused: document.hasFocus(),
      })
    ) {
      return;
    }
    if (isInQuietHours(quietHours) || events.length === 0) return;

    void (async () => {
      let sent = false;
      for (const event of events) {
        const keys = notifyCopyKeys(event.kind);
        const title = t(keys.title);
        const body =
          event.count === 1
            ? event.sample
            : t(keys.body, { count: event.count });
        try {
          await sendOsNotification(title, body, event.route);
          sent = true;
        } catch {
          /* notification delivery must never disturb the app */
        }
      }
      if (sent) void requestAttention();
    })();
  }, [
    attentionSnapshots,
    notificationOverview,
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
