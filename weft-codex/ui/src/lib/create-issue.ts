import type { SurfaceRoute } from "../route.ts"

export type CreateIssueFollow =
  | { action: "open-thread"; route: SurfaceRoute; threadId: string }
  | { action: "show-issue"; route: SurfaceRoute }

/**
 * Where to go after creating an issue.
 *
 * A spawned lead already has a native thread: remember the issue route so
 * coming back lands on the detail page, but do not call showWorkspace —
 * that would cover the chat. A failed spawn has nowhere else to go, so the
 * issue page is the surface.
 */
export function createIssueFollow(created: {
  id: number
  codexThreadId?: string | null
}): CreateIssueFollow {
  const route: SurfaceRoute = { view: "issue", issueId: created.id }
  const threadId = created.codexThreadId
  if (threadId) return { action: "open-thread", route, threadId }
  return { action: "show-issue", route }
}
