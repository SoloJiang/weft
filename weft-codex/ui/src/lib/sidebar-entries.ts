import type { BoardEntry, Repo } from "@/types"

/**
 * What the sidebar's two header entries show.
 *
 * Both work off the board the sidebar already holds. `/api/issues` returns
 * issues, directions, threads and artifacts in one payload, so workspace search
 * needs no endpoint of its own — reaching for the network here would add a
 * round trip to answer a question the client can already answer.
 */

export type SearchHitKind = "issue" | "direction" | "artifact" | "thread"

export interface SearchHit {
  key: string
  kind: SearchHitKind
  issueId: number
  title: string
  meta: string
  threadId?: string
  artifactId?: number
}

export type InboxItemKind = "attention" | "delivery" | "lead" | "review"

export interface InboxItem {
  key: string
  kind: InboxItemKind
  issueId: number
  title: string
  /** Daemon code. The panel maps it; the raw value never becomes copy. */
  reason: string
  /** Parent issue title on task rows, so the inbox names the work without opening it. */
  issueTitle?: string
  directionId?: number
  /** Present when opening the row should land on a native thread. */
  threadId?: string
  /** Present on delivery items: identifies the failure to drop once acted on. */
  failureKey?: string
}

export type InboxFollow =
  | { action: "open-thread"; threadId: string; issueId: number }
  | { action: "show-issue"; issueId: number }

/**
 * Where an inbox row should go.
 *
 * Failures with a live thread open that chat and only remember the issue
 * route — showWorkspace would cover the conversation. Review is the opposite:
 * Accept lives on the issue page, so the Weft surface has to come forward.
 */
export function inboxFollow(item: InboxItem): InboxFollow {
  if (item.kind === "review") return { action: "show-issue", issueId: item.issueId }
  if (item.threadId) {
    return { action: "open-thread", threadId: item.threadId, issueId: item.issueId }
  }
  return { action: "show-issue", issueId: item.issueId }
}

export function inboxIssueIds(items: readonly InboxItem[]): Set<number> {
  return new Set(items.map((item) => item.issueId))
}

/** Task rows name the issue; lead rows already are the issue. */
export function inboxRowMeta(issueTitle: string | undefined, reasonText: string): string {
  if (!issueTitle) return reasonText
  return `${issueTitle} · ${reasonText}`
}

export type SearchFollow =
  | { action: "open-thread"; threadId: string; issueId: number }
  | { action: "show-artifact"; issueId: number; artifactId: number }
  | { action: "open-issue"; issueId: number }
  | { action: "show-issue"; issueId: number }

/**
 * A direction hit without a thread has nowhere to talk. The issue page is
 * where retry / accept live. An issue hit still opens the lead.
 */
export function searchFollow(hit: SearchHit): SearchFollow {
  if (hit.threadId) {
    return { action: "open-thread", threadId: hit.threadId, issueId: hit.issueId }
  }
  if (hit.artifactId !== undefined) {
    return { action: "show-artifact", issueId: hit.issueId, artifactId: hit.artifactId }
  }
  if (hit.kind === "direction") return { action: "show-issue", issueId: hit.issueId }
  return { action: "open-issue", issueId: hit.issueId }
}

function nonEmptyThread(value: string | undefined): string | undefined {
  if (!value) return undefined
  return value
}

/** A `bus.undelivered` event seen this session, keyed by issue and party. */
export interface DeliveryFailure {
  issueId: number
  party: string
  reason: string
}

export function normalizeQuery(value: string): string {
  return value.trim().toLocaleLowerCase()
}

function matches(query: string, ...values: (string | number)[]): boolean {
  return values.some((value) => String(value).toLocaleLowerCase().includes(query))
}

function repoName(repos: Repo[], repoId: number): string {
  return repos.find((repo) => repo.id === repoId)?.name ?? ""
}

/**
 * Ranked by kind, not by score: an issue is the coarsest handle on a piece of
 * work and the one a human most often means, so issues lead regardless of where
 * in the string the match landed. Within a kind, board order is preserved.
 */
const KIND_ORDER: Record<SearchHitKind, number> = {
  issue: 0,
  direction: 1,
  artifact: 2,
  thread: 3,
}

export function searchBoard(
  board: BoardEntry[],
  repos: Repo[],
  rawQuery: string,
  limit = 40,
): SearchHit[] {
  const query = normalizeQuery(rawQuery)
  if (!query) return []

  const hits: SearchHit[] = []
  for (const entry of board) {
    const issue = entry.issue
    if (matches(query, issue.title, issue.id, issue.slug, issue.kind)) {
      hits.push({
        key: `issue:${issue.id}`,
        kind: "issue",
        issueId: issue.id,
        title: issue.title,
        meta: `#${issue.id}`,
      })
    }

    for (const direction of entry.directions) {
      const repo = repoName(repos, direction.repo_id)
      if (!matches(query, direction.name, direction.branch, repo)) continue
      const primary = entry.threads.find(
        (binding) => binding.direction_id === direction.id && binding.is_primary === 1,
      )
      hits.push({
        key: `direction:${direction.id}`,
        kind: "direction",
        issueId: issue.id,
        title: direction.name,
        meta: [repo, direction.branch].filter(Boolean).join(" · ") || issue.title,
        ...(primary ? { threadId: primary.thread_id } : {}),
      })
    }

    for (const artifact of entry.artifacts ?? []) {
      if (!matches(query, artifact.title, artifact.kind)) continue
      hits.push({
        key: `artifact:${artifact.id}`,
        kind: "artifact",
        issueId: issue.id,
        title: artifact.title || artifact.kind,
        meta: issue.title,
        artifactId: artifact.id,
      })
    }

    for (const binding of entry.threads) {
      // A primary thread is already reachable through its issue or direction
      // row; listing it again would just push distinct results off the limit.
      if (binding.is_primary === 1) continue
      if (!matches(query, binding.title)) continue
      hits.push({
        key: `thread:${binding.thread_id}`,
        kind: "thread",
        issueId: issue.id,
        title: binding.title,
        meta: issue.title,
        threadId: binding.thread_id,
      })
    }
  }

  hits.sort((left, right) => KIND_ORDER[left.kind] - KIND_ORDER[right.kind])
  return hits.slice(0, limit)
}

/**
 * `bus.parked` is deliberately absent.
 *
 * Parking means a human is mid-turn on that thread and the backlog flushes by
 * itself when the turn ends — nothing is asked of anyone. Listing it would fill
 * the inbox with entries that resolve while you read them, which is how an
 * inbox stops being read at all. `bus.undelivered` is the opposite: delivery
 * actually failed and the message is sitting there.
 */
function leadThreadId(entry: BoardEntry): string | undefined {
  const primary = entry.threads.find(
    (binding) => binding.direction_id == null && binding.is_primary === 1,
  )
  return nonEmptyThread(primary?.thread_id || entry.issue.lead_codex_thread_id)
}

function directionThreadId(entry: BoardEntry, directionId: number, fallback: string): string | undefined {
  const primary = entry.threads.find(
    (binding) => binding.direction_id === directionId && binding.is_primary === 1,
  )
  return nonEmptyThread(primary?.thread_id || fallback)
}

export function buildInbox(board: BoardEntry[], failures: DeliveryFailure[]): InboxItem[] {
  const items: InboxItem[] = []
  for (const entry of board) {
    // A stalled lead blocks everything under it, so it leads the list.
    if (entry.issue.lead_attention) {
      items.push({
        key: `lead:${entry.issue.id}`,
        kind: "lead",
        issueId: entry.issue.id,
        title: entry.issue.title,
        reason: entry.issue.lead_attention_reason,
        threadId: leadThreadId(entry),
      })
    }
    for (const direction of entry.directions) {
      if (!direction.attention) continue
      items.push({
        key: `attention:${direction.id}`,
        kind: "attention",
        issueId: entry.issue.id,
        title: direction.name,
        reason: direction.attention_reason,
        issueTitle: entry.issue.title,
        directionId: direction.id,
        threadId: directionThreadId(entry, direction.id, direction.codex_thread_id),
      })
    }
    // A finished turn waiting for Accept is the happy-path Needs-you. Failed
    // turns also land in review, but those already have an attention row.
    for (const direction of entry.directions) {
      if (direction.status !== "review" || direction.attention) continue
      items.push({
        key: `review:${direction.id}`,
        kind: "review",
        issueId: entry.issue.id,
        title: direction.name,
        reason: "review",
        issueTitle: entry.issue.title,
        directionId: direction.id,
      })
    }
  }

  for (const failure of failures) {
    const entry = board.find((candidate) => candidate.issue.id === failure.issueId)
    if (!entry) continue
    const directionId = Number.parseInt(failure.party, 10)
    const direction = Number.isNaN(directionId)
      ? undefined
      : entry.directions.find((candidate) => candidate.id === directionId)
    // A direction already flagged for attention says the same thing in a more
    // actionable way; the raw delivery failure would only duplicate the row.
    if (direction?.attention) continue
    items.push({
      key: `delivery:${deliveryFailureKey(failure)}`,
      kind: "delivery",
      issueId: failure.issueId,
      title: direction?.name ?? entry.issue.title,
      reason: failure.reason,
      issueTitle: direction ? entry.issue.title : undefined,
      failureKey: deliveryFailureKey(failure),
      ...(direction
        ? {
            directionId: direction.id,
            threadId: directionThreadId(entry, direction.id, direction.codex_thread_id),
          }
        : { threadId: leadThreadId(entry) }),
    })
  }

  return items
}

export function deliveryFailureKey(failure: DeliveryFailure): string {
  return `${failure.issueId}:${failure.party}`
}

export function parseDeliveryFailure(data: string): DeliveryFailure | null {
  let payload: unknown
  try {
    payload = JSON.parse(data)
  } catch {
    return null
  }
  if (!payload || typeof payload !== "object") return null
  const body = payload as Record<string, unknown>
  if (typeof body.issueId !== "number" || typeof body.party !== "string") return null
  return {
    issueId: body.issueId,
    party: body.party,
    reason: typeof body.reason === "string" ? body.reason : "undelivered",
  }
}

export function rememberDeliveryFailure(
  current: DeliveryFailure[],
  failure: DeliveryFailure,
): DeliveryFailure[] {
  const key = deliveryFailureKey(failure)
  if (current.some((existing) => deliveryFailureKey(existing) === key)) return current
  return [...current, failure]
}
