import type { ProcessQuotaStatus } from "./types";

export const PROCESS_QUOTA_DEGRADED = "process_quota_degraded";

export type ProcessQuotaNotice = "warning" | "degraded" | "recovered";

/** Accept newer transitions and same-transition metric refreshes, but never let a
 *  late initial fetch overwrite a newer event. A same sequence with a different
 *  state is internally inconsistent and is rejected as stale. */
export function shouldApplyProcessQuotaStatus(
  previous: ProcessQuotaStatus | null,
  next: ProcessQuotaStatus,
): boolean {
  if (previous === null) return true;
  if (next.transitionSeq > previous.transitionSeq) return true;
  return next.transitionSeq === previous.transitionSeq && next.status === previous.status;
}

/** State transition to one user-facing notice. Metric-only refreshes and replayed
 *  events deliberately return null so polling/listener races cannot spam toasts. */
export function processQuotaNotice(
  previous: ProcessQuotaStatus | null,
  next: ProcessQuotaStatus,
): ProcessQuotaNotice | null {
  if (previous !== null && next.transitionSeq <= previous.transitionSeq) return null;
  if (previous?.status === next.status) return null;

  switch (next.status) {
    case "warning":
      return "warning";
    case "degraded":
      return "degraded";
    case "normal":
      return previous === null ? null : "recovered";
    default:
      // Unreachable under the closed ProcessQuotaLevel union (QUOTA_BAR_VIEW's
      // Record already forces a new status to be handled). Fail safe to "no
      // notice" so a malformed runtime value can't reach the toast path as
      // `undefined` and throw inside the event callback.
      return null;
  }
}

/** Tauri commands currently reject with strings, while tests and future adapters
 *  may surface Error or `{ code }`. Recognize the stable code without coupling UI
 *  copy to the backend message. */
export function isProcessQuotaDegradedError(error: unknown): boolean {
  if (typeof error === "string") return error.includes(PROCESS_QUOTA_DEGRADED);
  if (error instanceof Error) return error.message.includes(PROCESS_QUOTA_DEGRADED);
  if (typeof error !== "object" || error === null || !("code" in error)) return false;
  return String(error.code) === PROCESS_QUOTA_DEGRADED;
}
