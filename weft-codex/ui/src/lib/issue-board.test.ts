import assert from "node:assert/strict"
import test from "node:test"

import { deriveIssueStatus, normalizeDirectionStatus, toIssueBoardCard } from "./issue-board.ts"
import type { BoardEntry, Direction, Issue } from "../types.ts"

function direction(partial: Partial<Direction> & Pick<Direction, "status">): Direction {
  return {
    id: partial.id ?? 1,
    issue_id: partial.issue_id ?? 1,
    name: partial.name ?? "task",
    slug: partial.slug ?? "task",
    branch: partial.branch ?? "",
    status: partial.status,
    repo_id: partial.repo_id ?? 1,
    reason: partial.reason ?? "",
    mandate: partial.mandate ?? "",
    target_branch: partial.target_branch ?? "",
    base_branch: partial.base_branch ?? "",
    spec: partial.spec ?? "",
    codex_thread_id: partial.codex_thread_id ?? "",
    attention: partial.attention ?? 0,
    attention_reason: partial.attention_reason ?? "",
    created_at: partial.created_at ?? "",
  }
}

function issue(partial: Partial<Issue> = {}): Issue {
  return {
    id: partial.id ?? 12,
    workspace_id: partial.workspace_id ?? 1,
    title: partial.title ?? "Demo",
    slug: partial.slug ?? "demo",
    kind: partial.kind ?? "feature",
    lead_codex_thread_id: partial.lead_codex_thread_id ?? "",
    lead_attention: partial.lead_attention ?? 0,
    lead_attention_reason: partial.lead_attention_reason ?? "",
    created_at: partial.created_at ?? "",
  }
}

test("empty issues stay queued", () => {
  assert.equal(deriveIssueStatus([]), "queued")
})

test("all-done issues land in done", () => {
  assert.equal(
    deriveIssueStatus([direction({ status: "done" }), direction({ id: 2, status: "done" })]),
    "done",
  )
})

test("issue status follows the most advanced open task", () => {
  assert.equal(
    deriveIssueStatus([
      direction({ status: "queued" }),
      direction({ id: 2, status: "working" }),
      direction({ id: 3, status: "done" }),
    ]),
    "working",
  )
  assert.equal(
    deriveIssueStatus([
      direction({ status: "working" }),
      direction({ id: 2, status: "review" }),
    ]),
    "review",
  )
})

test("legacy planning rows count as working", () => {
  assert.equal(normalizeDirectionStatus("planning"), "working")
  assert.equal(normalizeDirectionStatus("working"), "working")
  assert.equal(normalizeDirectionStatus("unknown"), "queued")
  assert.equal(
    deriveIssueStatus([
      { ...direction({ status: "queued" }), status: "planning" as Direction["status"] },
    ]),
    "working",
  )
})

test("issue cards summarize progress and attention", () => {
  const entry: BoardEntry = {
    issue: issue({ lead_codex_thread_id: "thread-1" }),
    directions: [
      direction({ status: "working", attention: 1 }),
      direction({ id: 2, status: "done" }),
      direction({ id: 3, status: "queued" }),
    ],
    threads: [],
    artifacts: [],
  }
  const card = toIssueBoardCard(entry)
  assert.equal(card.status, "working")
  assert.equal(card.totalTasks, 3)
  assert.equal(card.doneTasks, 1)
  assert.equal(card.attentionCount, 1)
  assert.equal(card.hasLead, true)
})
