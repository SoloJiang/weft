export type ChatHistoryStatus = "loading" | "ready" | "error";

/**
 * Initial loads block the timeline, while refreshes keep an already-hydrated
 * conversation visible. This prevents a background refresh from replacing
 * useful history with a loading or error surface.
 */
export function beginChatHistoryLoad(
  current: ChatHistoryStatus | undefined,
): ChatHistoryStatus {
  if (current === "ready") return "ready";
  return "loading";
}

export function failChatHistoryLoad(
  current: ChatHistoryStatus | undefined,
): ChatHistoryStatus {
  if (current === "ready") return "ready";
  return "error";
}

/**
 * A worker conversation needs both its slot lookup and, once a session exists,
 * the parent thread's persisted history. A confirmed slot with no session is a
 * real empty conversation rather than an unfinished load.
 */
export function workerChatHistoryStatus(
  sessionId: number | null,
  sessionLookupStatus: ChatHistoryStatus,
  threadHistoryStatus: ChatHistoryStatus | undefined,
): ChatHistoryStatus {
  if (sessionId == null) return sessionLookupStatus;
  return threadHistoryStatus ?? "loading";
}
