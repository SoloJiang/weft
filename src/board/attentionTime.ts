const MAX_CLOCK_SKEW_MS = 5 * 60 * 1000;
const ISO_DATE_TIME = /^\d{4}-\d{2}-\d{2}(?:T|\s)/;

export type AttentionAge =
  | { kind: "just_now" }
  | { kind: "minutes"; value: number }
  | { kind: "hours"; value: number }
  | { kind: "days"; value: number };

/**
 * Parse only the wall-clock formats used by persisted attention sources:
 * Unix seconds (including fractional seconds) or an ISO date-time. Opaque OCC
 * revisions such as `unix_nanos-seq` deliberately fail closed.
 */
export function attentionTimestampMilliseconds(
  timestamp: string | null | undefined,
  nowMilliseconds = Date.now(),
): number | null {
  const value = timestamp?.trim();
  if (!value) return null;

  const numeric = Number(value);
  let milliseconds: number;
  if (Number.isFinite(numeric)) {
    milliseconds = numeric * 1000;
  } else if (ISO_DATE_TIME.test(value)) {
    milliseconds = Date.parse(value);
  } else {
    return null;
  }

  if (!Number.isFinite(milliseconds) || milliseconds < 0) return null;
  if (milliseconds > nowMilliseconds + MAX_CLOCK_SKEW_MS) return null;
  return milliseconds;
}

export function attentionAge(
  timestamp: string | null | undefined,
  nowMilliseconds = Date.now(),
): AttentionAge | null {
  const milliseconds = attentionTimestampMilliseconds(timestamp, nowMilliseconds);
  if (milliseconds == null) return null;

  const seconds = Math.max(0, Math.floor((nowMilliseconds - milliseconds) / 1000));
  if (seconds < 60) return { kind: "just_now" };
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return { kind: "minutes", value: minutes };
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return { kind: "hours", value: hours };
  return { kind: "days", value: Math.floor(hours / 24) };
}
