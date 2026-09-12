import { currentHost } from "../host.ts"

export interface ThreadOpenState {
  openingThreadId: string | null
  failedThreadId: string | null
}

export const idleThreadOpen: ThreadOpenState = {
  openingThreadId: null,
  failedThreadId: null,
}

export function beginThreadOpen(threadId: string): ThreadOpenState {
  return { openingThreadId: threadId, failedThreadId: null }
}

export function finishThreadOpen(
  current: ThreadOpenState,
  threadId: string,
  ok: boolean,
): ThreadOpenState {
  if (current.openingThreadId !== threadId) return current
  if (ok) return idleThreadOpen
  return { openingThreadId: null, failedThreadId: threadId }
}

export function clearThreadOpenFailure(current: ThreadOpenState): ThreadOpenState {
  if (!current.failedThreadId) return current
  return { ...current, failedThreadId: null }
}

export function isOpeningThread(current: ThreadOpenState, threadId: string): boolean {
  return current.openingThreadId === threadId
}

export function codexThreadHref(threadId: string): string {
  return `codex://threads/${encodeURIComponent(threadId)}`
}

export function openCodexThread(threadId: string): Promise<void> {
  if (!threadId) return Promise.reject(new Error("Thread id is required"))
  const host = currentHost()
  if (host) return host.openThread(threadId)
  window.location.assign(codexThreadHref(threadId))
  return Promise.resolve()
}
