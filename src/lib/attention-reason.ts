/**
 * Daemon attention codes → Weft i18n paths.
 *
 * Unknown codes fall back to a generic key; the raw value stays off-screen.
 * Needs You / ScopeReview stay Weft-only — this map does not replace them.
 */

export const LEAD_ATTENTION_KEYS: Record<string, string> = {
  "start-failed": "attention.leadStartFailed",
  "resume-failed": "attention.leadResumeFailed",
  "turn-error": "attention.leadTurnError",
};

export const DIRECTION_ATTENTION_KEYS: Record<string, string> = {
  "worker-start-failed": "attention.dirStartFailed",
  "thread-resume-failed": "attention.dirResumeFailed",
  "turn failed": "attention.dirTurnFailed",
  "quota exceeded": "attention.dirQuotaExceeded",
};

export const DELIVERY_ATTENTION_KEYS: Record<string, string> = {
  undelivered: "attention.dirUndelivered",
  "settlement-failed": "attention.dirUndelivered",
};

function lookup(map: Record<string, string>, reason: string, fallback: string): string {
  if (!reason) return fallback;
  return map[reason] ?? fallback;
}

export function leadAttentionKey(reason: string): string {
  return lookup(LEAD_ATTENTION_KEYS, reason, "attention.leadFailed");
}

export function directionAttentionKey(reason: string): string {
  return lookup(DIRECTION_ATTENTION_KEYS, reason, "attention.dirAttention");
}

export function deliveryAttentionKey(reason: string): string {
  return lookup(DELIVERY_ATTENTION_KEYS, reason, "attention.dirUndelivered");
}

export function inboxAttentionKey(
  kind: "lead" | "attention" | "delivery" | "review",
  reason: string,
): string {
  if (kind === "lead") return leadAttentionKey(reason);
  if (kind === "delivery") return deliveryAttentionKey(reason);
  if (kind === "review") return "attention.inboxReview";
  return directionAttentionKey(reason);
}

export function issueBoardSignalKey(options: {
  leadAttention: boolean;
  leadReason: string;
  directionReasons: string[];
}): string {
  if (options.leadAttention) return leadAttentionKey(options.leadReason);
  const keys = options.directionReasons.map(directionAttentionKey);
  const unique = new Set(keys);
  if (unique.size === 1) {
    const only = keys[0];
    if (only) return only;
  }
  return "attention.issueNeedsYou";
}

/** Raw code for data-attributes when exactly one source is responsible. */
export function issueBoardSignalReason(options: {
  leadAttention: boolean;
  leadReason: string;
  directionReasons: string[];
}): string {
  if (options.leadAttention) return options.leadReason;
  const unique = new Set(options.directionReasons.filter(Boolean));
  if (unique.size === 1) return options.directionReasons.find(Boolean) ?? "";
  if (options.directionReasons.length === 1) return options.directionReasons[0] ?? "";
  return "";
}
