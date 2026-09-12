export const ISSUE_BOARD_STATUSES = ["queued", "working", "review", "done"] as const;
export type IssueBoardStatus = (typeof ISSUE_BOARD_STATUSES)[number];

export interface IssueBoardInput {
  thread_id: number;
  direction_ids: readonly number[];
  statuses: readonly string[];
}

export interface IssueBoardCard {
  threadId: number;
  status: IssueBoardStatus;
  totalTasks: number;
  doneTasks: number;
}

const STATUS_RANK: Record<IssueBoardStatus, number> = {
  queued: 0,
  working: 1,
  review: 2,
  done: 3,
};

/** Fold the retired `planning` column into working; unknown values stay queued. */
export function normalizeDirectionStatus(status: string): IssueBoardStatus {
  if (status === "planning") return "working";
  for (const known of ISSUE_BOARD_STATUSES) {
    if (known === status) return known;
  }
  return "queued";
}

/**
 * Roll task statuses up to one issue-level column.
 *
 * The board is issue-primary: a card represents the issue, not a task. The
 * status is the most advanced non-done task when work remains; only an issue
 * whose every task is done lands in Done. Issues with no tasks stay in Queued
 * so empty leads remain visible.
 */
export function deriveIssueStatus(statuses: readonly string[]): IssueBoardStatus {
  if (!statuses.length) return "queued";
  const normalized = statuses.map((status) => normalizeDirectionStatus(status));
  if (normalized.every((status) => status === "done")) return "done";
  let best: IssueBoardStatus = "queued";
  for (const status of normalized) {
    if (status === "done") continue;
    if (STATUS_RANK[status] > STATUS_RANK[best]) best = status;
  }
  return best;
}

export function toIssueBoardCard(entry: IssueBoardInput): IssueBoardCard {
  const totalTasks = entry.direction_ids.length;
  const doneTasks = entry.statuses.filter(
    (status) => normalizeDirectionStatus(status) === "done",
  ).length;
  return {
    threadId: entry.thread_id,
    status: deriveIssueStatus(entry.statuses),
    totalTasks,
    doneTasks,
  };
}

export function buildIssueBoard(board: readonly IssueBoardInput[]): IssueBoardCard[] {
  return board.map(toIssueBoardCard);
}

export function groupIssueBoard(
  cards: readonly IssueBoardCard[],
): Record<IssueBoardStatus, IssueBoardCard[]> {
  const groups = Object.fromEntries(
    ISSUE_BOARD_STATUSES.map((status) => [status, [] as IssueBoardCard[]]),
  ) as Record<IssueBoardStatus, IssueBoardCard[]>;
  for (const card of cards) groups[card.status].push(card);
  return groups;
}
