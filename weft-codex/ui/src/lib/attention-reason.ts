import type en from "../i18n/en.ts"

type MessageKey = keyof typeof en

/**
 * Daemon attention codes → UI copy.
 *
 * Values are MessageKeys so a new mapping without a string fails typecheck.
 * Unknown codes fall back to a generic key; the raw value stays off-screen
 * (data-attribute only). Never interpolate the code into title or body.
 */
export const LEAD_ATTENTION_KEYS: Record<string, MessageKey> = {
  "start-failed": "lead.startFailed",
  "resume-failed": "lead.resumeFailed",
  "turn-error": "lead.turnError",
}

export const DIRECTION_ATTENTION_KEYS: Record<string, MessageKey> = {
  "worker-start-failed": "dir.startFailed",
  "thread-resume-failed": "dir.resumeFailed",
  "turn failed": "dir.turnFailed",
  "quota exceeded": "dir.quotaExceeded",
}

export const DELIVERY_ATTENTION_KEYS: Record<string, MessageKey> = {
  undelivered: "dir.undelivered",
  "settlement-failed": "dir.undelivered",
}

function lookup(map: Record<string, MessageKey>, reason: string, fallback: MessageKey): MessageKey {
  if (!reason) return fallback
  return map[reason] ?? fallback
}

export function leadAttentionKey(reason: string): MessageKey {
  return lookup(LEAD_ATTENTION_KEYS, reason, "lead.failed")
}

export function directionAttentionKey(reason: string): MessageKey {
  return lookup(DIRECTION_ATTENTION_KEYS, reason, "dir.attention")
}

export function deliveryAttentionKey(reason: string): MessageKey {
  return lookup(DELIVERY_ATTENTION_KEYS, reason, "dir.undelivered")
}

export function inboxAttentionKey(
  kind: "lead" | "attention" | "delivery" | "review",
  reason: string,
): MessageKey {
  if (kind === "lead") return leadAttentionKey(reason)
  if (kind === "delivery") return deliveryAttentionKey(reason)
  if (kind === "review") return "inbox.review"
  return directionAttentionKey(reason)
}

export function issueBoardSignalKey(options: {
  leadAttention: boolean
  leadReason: string
  directionReasons: string[]
}): MessageKey {
  if (options.leadAttention) return leadAttentionKey(options.leadReason)
  const keys = options.directionReasons.map(directionAttentionKey)
  const unique = new Set(keys)
  if (unique.size === 1) {
    const only = keys[0]
    if (only) return only
  }
  return "kanban.issueNeedsYou"
}

/** Raw code for data-attributes when exactly one source is responsible. */
export function issueBoardSignalReason(options: {
  leadAttention: boolean
  leadReason: string
  directionReasons: string[]
}): string {
  if (options.leadAttention) return options.leadReason
  const unique = new Set(options.directionReasons.filter(Boolean))
  if (unique.size === 1) return options.directionReasons.find(Boolean) ?? ""
  if (options.directionReasons.length === 1) return options.directionReasons[0] ?? ""
  return ""
}
