import { useEffect, useRef, type RefObject } from "react";

/**
 * Dismiss a hand-rolled popover when a pointer lands outside it. Radix portals
 * (Select, dropdown menus, dialogs) get this for free; our text-anchored
 * overlays — the composer's slash palette, the diff AskBox — don't, so they
 * wire this up to close the moment a press lands anywhere outside `ref`.
 *
 * Listens on `pointerdown` in the capture phase so the popover closes on press,
 * before the click resolves and even if an inner handler stops propagation. It
 * is a no-op while `active` is false, so there is no global listener attached
 * when nothing is open.
 */
export function useClickOutside<T extends HTMLElement>(
  ref: RefObject<T | null>,
  active: boolean,
  onOutside: () => void,
): void {
  // Keep the latest callback without re-subscribing every render — the listener
  // re-binds only when `active` flips.
  const cb = useRef(onOutside);
  cb.current = onOutside;

  useEffect(() => {
    if (!active) return;
    const onDown = (e: PointerEvent) => {
      const el = ref.current;
      if (el && !el.contains(e.target as Node)) cb.current();
    };
    document.addEventListener("pointerdown", onDown, true);
    return () => document.removeEventListener("pointerdown", onDown, true);
  }, [ref, active]);
}
