import { useCallback, useSyncExternalStore } from "react";

/** Persisted user choice. "system" follows the OS appearance live. */
export type ThemePref = "system" | "light" | "dark";
/** Concrete appearance reflected on <html data-theme>. */
export type ResolvedTheme = "light" | "dark";

const KEY = "weft-theme";

function darkQuery(): MediaQueryList | null {
  try {
    return window.matchMedia("(prefers-color-scheme: dark)");
  } catch {
    return null;
  }
}

/** The OS-reported appearance right now (falls back to dark). */
export function systemTheme(): ResolvedTheme {
  return darkQuery()?.matches ? "dark" : "light";
}

/** The concrete light/dark a preference resolves to. */
export function resolvePref(pref: ThemePref): ResolvedTheme {
  return pref === "system" ? systemTheme() : pref;
}

/** Cycle order for the quick toggle: System → Light → Dark → System. */
export function nextPref(pref: ThemePref): ThemePref {
  return pref === "system" ? "light" : pref === "light" ? "dark" : "system";
}

/** Saved preference, else "system". Tolerates legacy "dark"/"light" values. */
export function readPref(): ThemePref {
  try {
    const saved = localStorage.getItem(KEY);
    if (saved === "system" || saved === "dark" || saved === "light") return saved;
  } catch {
    /* private mode / no storage */
  }
  return "system";
}

/** Reflect a resolved theme on <html>. */
export function applyResolved(r: ResolvedTheme) {
  document.documentElement.dataset.theme = r;
}

// --- shared module-level store: every useTheme() consumer stays in sync ---

let state: { pref: ThemePref; resolved: ResolvedTheme } = (() => {
  const pref = readPref();
  return { pref, resolved: resolvePref(pref) };
})();
applyResolved(state.resolved);

const listeners = new Set<() => void>();
function emit() {
  for (const l of listeners) l();
}

function commit(pref: ThemePref) {
  state = { pref, resolved: resolvePref(pref) };
  applyResolved(state.resolved);
  emit();
}

function setPrefGlobal(pref: ThemePref) {
  try {
    localStorage.setItem(KEY, pref);
  } catch {
    /* private mode / no storage */
  }
  commit(pref);
}

// Follow live OS appearance changes while in system mode (registered once).
// Guard the listener API: older WebKit/WebKitGTK WebViews (Tauri's Linux
// runtime) expose only the legacy MediaQueryList.addListener, so an unguarded
// addEventListener would throw at import and blank the app for those users.
const systemQuery = darkQuery();
const onSystemChange = () => {
  if (state.pref === "system") commit("system");
};
if (systemQuery) {
  if (typeof systemQuery.addEventListener === "function") {
    systemQuery.addEventListener("change", onSystemChange);
  } else if (typeof systemQuery.addListener === "function") {
    systemQuery.addListener(onSystemChange);
  }
}

function subscribe(cb: () => void): () => void {
  listeners.add(cb);
  return () => {
    listeners.delete(cb);
  };
}
function getSnapshot() {
  return state;
}

/**
 * Theme preference + resolved appearance.
 * - `pref` is the persisted choice (system/light/dark).
 * - `resolved` is the applied light/dark, reflected on <html data-theme>.
 * In system mode `resolved` live-tracks the OS; explicit modes pin it.
 */
export function useTheme(): {
  pref: ThemePref;
  resolved: ResolvedTheme;
  setPref: (p: ThemePref) => void;
  cycle: () => void;
} {
  const snapshot = useSyncExternalStore(subscribe, getSnapshot, getSnapshot);
  const setPref = useCallback((p: ThemePref) => setPrefGlobal(p), []);
  const cycle = useCallback(() => setPrefGlobal(nextPref(state.pref)), []);
  return { pref: snapshot.pref, resolved: snapshot.resolved, setPref, cycle };
}
