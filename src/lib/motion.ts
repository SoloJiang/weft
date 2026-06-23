import type { Transition } from "motion/react";

// Shared motion vocabulary for the Needs-you surfaces (top dock, in-session
// permission bars, queue rows) so their enter / exit / answer-collapse stay in
// lockstep. Mirrors the curve NeedsYouView originally shipped inline.
export const needsTransition: Transition = {
  duration: 0.18,
  ease: [0.22, 1, 0.36, 1],
};

/** Per-row enter/exit for the Needs-you queue list. */
export function needsRowMotion(reduce: boolean) {
  return {
    layout: !reduce,
    initial: reduce ? false : { opacity: 0, y: 6 },
    animate: { opacity: 1, y: 0 },
    exit: reduce
      ? { opacity: 0 }
      : { opacity: 0, height: 0, marginBottom: -10, scale: 0.98 },
    transition: needsTransition,
  };
}

/**
 * Collapse + fade for a docked bar (top NeedsDock, in-session permission bar):
 * the bar grows in / collapses out so answering an ask slides the surface away
 * instead of popping.
 */
export function needsBarMotion(reduce: boolean) {
  return {
    initial: reduce ? false : { opacity: 0, height: 0 },
    animate: { opacity: 1, height: "auto" as const },
    exit: reduce ? { opacity: 0 } : { opacity: 0, height: 0 },
    transition: needsTransition,
  };
}
