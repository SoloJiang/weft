/** Bounded waits while a native thread is matched to a Weft binding. */
export const THREAD_RESOLVE_DELAYS_MS = [0, 250, 750] as const

export type ThreadLinkStatus = "idle" | "linking" | "unbound"

/**
 * What the sidebar should say about the native thread that is currently open.
 *
 * Bound threads and the workspace view are silent. An unknown thread is
 * "linking" until the resolve loop has spent its retries, then "unbound".
 */
export function threadLinkStatus(
  kind: "workspace" | "bound-thread" | "unbound-thread",
  resolveExhausted: boolean,
): ThreadLinkStatus {
  if (kind !== "unbound-thread") return "idle"
  if (resolveExhausted) return "unbound"
  return "linking"
}

export type SidebarFooter =
  | { kind: "retry"; threadId: string }
  | { kind: "linking" }
  | { kind: "unbound" }
  | { kind: "none" }

export function sidebarFooter(
  failedThreadId: string | null,
  linkStatus: ThreadLinkStatus,
): SidebarFooter {
  if (failedThreadId) return { kind: "retry", threadId: failedThreadId }
  if (linkStatus === "linking") return { kind: "linking" }
  if (linkStatus === "unbound") return { kind: "unbound" }
  return { kind: "none" }
}

export type WorkspaceFollow =
  | { action: "none" }
  | { action: "adopt"; workspaceId: number }

/**
 * How to move the sidebar onto a thread's workspace.
 *
 * In thread view the native chat is the main surface: switch the workspace
 * under the sidebar, but do not call showWorkspace — that would cover the
 * chat with the kanban. In workspace view a leftover native active row must
 * not steal the workspace the person is already looking at.
 */
export function workspaceFollowForThread(options: {
  hostView: "workspace" | "thread"
  currentWorkspaceId: number | null
  threadWorkspaceId: number | null
}): WorkspaceFollow {
  const threadWorkspaceId = options.threadWorkspaceId
  if (!threadWorkspaceId) return { action: "none" }
  if (threadWorkspaceId === options.currentWorkspaceId) return { action: "none" }
  if (options.hostView !== "thread") return { action: "none" }
  return { action: "adopt", workspaceId: threadWorkspaceId }
}
