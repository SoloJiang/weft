import { useEffect } from "react";
import { useStore } from "./store";

/**
 * The app-wide keyboard layer. weft owns the ⌘ prefix (§4.3 key ownership), so
 * these run in the capture phase — they fire before any focused input sees the
 * key, and only ever on ⌘/Ctrl-chords, so plain typing (and copy/paste) passes
 * straight through. ⌘K is the palette's (handled there).
 *
 *   ⌘\        toggle the sidebar
 *   ⌘1/2      workspace board / repo map
 *   ⌘[        go up one level (session → board → workspace)
 *
 * No chrome — keeps the "精简" home headerless while making the whole app
 * navigable from the keyboard.
 */
export function useAppShortcuts() {
  const {
    navCollapsed,
    setNavCollapsed,
    backToWorkspace,
    setHomeTab,
    openRepoMap,
    activeSessionId,
    viewing,
    showNeeds,
    activeThreadId,
    backToBoard,
    closeObserve,
  } = useStore();

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (!(e.metaKey || e.ctrlKey) || e.altKey) return;
      switch (e.key) {
        case "\\":
          e.preventDefault();
          setNavCollapsed(!navCollapsed);
          break;
        case "1":
          e.preventDefault();
          backToWorkspace();
          setHomeTab("board");
          break;
        case "2":
          e.preventDefault();
          openRepoMap();
          break;
        case "[":
          // Up one level, mirroring the route hierarchy in App's Main().
          e.preventDefault();
          if (activeSessionId != null) backToBoard();
          else if (viewing != null) closeObserve();
          else if (showNeeds || activeThreadId != null) backToWorkspace();
          break;
        default:
          break;
      }
    };
    window.addEventListener("keydown", onKey, { capture: true });
    return () => window.removeEventListener("keydown", onKey, { capture: true });
  }, [
    navCollapsed,
    setNavCollapsed,
    backToWorkspace,
    setHomeTab,
    openRepoMap,
    activeSessionId,
    viewing,
    showNeeds,
    activeThreadId,
    backToBoard,
    closeObserve,
  ]);
}
