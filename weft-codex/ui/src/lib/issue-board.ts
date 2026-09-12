import type { BoardEntry, Direction, DirectionStatus } from "../types.ts"
import { STATUSES } from "../types.ts"

export type IssueBoardStatus = DirectionStatus

export interface IssueBoardCard {
  entry: BoardEntry
  status: IssueBoardStatus
  totalTasks: number
  doneTasks: number
  attentionCount: number
  hasLead: boolean
  /** The lead failed to start, resume, or errored mid-turn. */
  leadAttention: boolean
}

const STATUS_RANK: Record<DirectionStatus, number> = {
  queued: 0,
  working: 1,
  review: 2,
  done: 3,
}

/** Fold the retired `planning` column into working; unknown values stay queued. */
export function normalizeDirectionStatus(status: string): DirectionStatus {
  if (status === "planning") return "working"
  for (const known of STATUSES) {
    if (known === status) return known
  }
  return "queued"
}

/**
 * Roll task statuses up to one issue-level column.
 *
 * The board is issue-primary: a card represents the issue, not a task. The
 * status is the most advanced non-done task when work remains; only an issue
 * whose every task is done lands in Done. Issues with no tasks stay in Queued
 * so empty leads remain visible.
 */
export function deriveIssueStatus(directions: readonly Direction[]): IssueBoardStatus {
  if (!directions.length) return "queued"
  const statuses = directions.map((direction) => normalizeDirectionStatus(direction.status))
  if (statuses.every((status) => status === "done")) return "done"
  let best: DirectionStatus = "queued"
  for (const status of statuses) {
    if (status === "done") continue
    if (STATUS_RANK[status] > STATUS_RANK[best]) best = status
  }
  return best
}

export function toIssueBoardCard(entry: BoardEntry): IssueBoardCard {
  const totalTasks = entry.directions.length
  const doneTasks = entry.directions.filter((direction) => direction.status === "done").length
  const attentionCount = entry.directions.filter((direction) => Boolean(direction.attention)).length
  return {
    entry,
    status: deriveIssueStatus(entry.directions),
    totalTasks,
    doneTasks,
    attentionCount,
    hasLead: Boolean(entry.issue.lead_codex_thread_id),
    leadAttention: Boolean(entry.issue.lead_attention),
  }
}

export function buildIssueBoard(board: readonly BoardEntry[]): IssueBoardCard[] {
  return board.map(toIssueBoardCard)
}

export function groupIssueBoard(
  cards: readonly IssueBoardCard[],
): Record<IssueBoardStatus, IssueBoardCard[]> {
  const groups = Object.fromEntries(STATUSES.map((status) => [status, [] as IssueBoardCard[]])) as Record<
    IssueBoardStatus,
    IssueBoardCard[]
  >
  for (const card of cards) groups[card.status].push(card)
  return groups
}
