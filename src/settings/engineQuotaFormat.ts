// Pure helpers for Settings → Resources' engine-quota group (issue #97) — kept
// JSX-free so this logic is directly unit-testable (node:test can't import a
// .tsx file), the same split `session/engineSwitch.ts` uses for the switch
// marker's own pure verdicts.

/** Which of the three reset-copy variants a countdown should render as — ONE
 *  discriminated value mapped exhaustively by the caller (Resources.tsx's
 *  `RESET_LABEL_KEY`), not re-derived booleans at the render site. */
export type ResetGranularity = "days" | "hours" | "minutes";

function resetGranularityOf(days: number, hours: number): ResetGranularity {
  if (days > 0) return "days";
  if (hours > 0) return "hours";
  return "minutes";
}

export interface ResetParts {
  granularity: ResetGranularity;
  days: number;
  hours: number;
  minutes: number;
}

/** Breaks a `resetsAt` (unix SECONDS — matches the Rust `engine_quota`
 *  snapshot) down against `nowMs` (caller-supplied so this stays pure/testable
 *  instead of reaching for `Date.now()` internally) into the pieces the
 *  Resources panel's three reset-copy variants need. `null` when the reset
 *  time has already passed (a stale snapshot, or the window just rolled over
 *  and a fresher reading hasn't arrived yet) — nothing useful to show then.
 */
export function resetParts(resetsAt: number, nowMs: number): ResetParts | null {
  const deltaSec = resetsAt - Math.floor(nowMs / 1000);
  if (deltaSec <= 0) return null;
  const days = Math.floor(deltaSec / 86400);
  const hours = Math.floor((deltaSec % 86400) / 3600);
  const minutes = Math.floor((deltaSec % 3600) / 60);
  return { granularity: resetGranularityOf(days, hours), days, hours, minutes };
}
