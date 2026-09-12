import assert from "node:assert/strict"
import test from "node:test"

import {
  buildInbox,
  inboxFollow,
  inboxIssueIds,
  inboxRowMeta,
  parseDeliveryFailure,
  rememberDeliveryFailure,
  searchBoard,
  searchFollow,
} from "./sidebar-entries.ts"
import type {
  ArtifactSummary,
  BoardEntry,
  Direction,
  Issue,
  Repo,
  ThreadBinding,
} from "../types.ts"

function issue(partial: Partial<Issue> = {}): Issue {
  return {
    id: partial.id ?? 12,
    workspace_id: partial.workspace_id ?? 1,
    title: partial.title ?? "Ship the importer",
    slug: partial.slug ?? "ship-the-importer",
    kind: partial.kind ?? "feature",
    lead_codex_thread_id: partial.lead_codex_thread_id ?? "",
    lead_attention: partial.lead_attention ?? 0,
    lead_attention_reason: partial.lead_attention_reason ?? "",
    created_at: partial.created_at ?? "",
  }
}

function direction(partial: Partial<Direction> = {}): Direction {
  return {
    id: partial.id ?? 1,
    issue_id: partial.issue_id ?? 12,
    name: partial.name ?? "parser",
    slug: partial.slug ?? "parser",
    branch: partial.branch ?? "feat/parser",
    status: partial.status ?? "working",
    repo_id: partial.repo_id ?? 7,
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

function thread(partial: Partial<ThreadBinding> = {}): ThreadBinding {
  return {
    thread_id: partial.thread_id ?? "t-1",
    issue_id: partial.issue_id ?? 12,
    direction_id: partial.direction_id ?? null,
    title: partial.title ?? "",
    is_primary: partial.is_primary ?? 0,
  } as ThreadBinding
}

function artifact(partial: Partial<ArtifactSummary> = {}): ArtifactSummary {
  return {
    id: partial.id ?? 3,
    issue_id: partial.issue_id ?? 12,
    kind: partial.kind ?? "plan",
    title: partial.title ?? "Rollout plan",
    format: partial.format ?? "markdown",
    revision: partial.revision ?? 1,
    status: partial.status ?? "draft",
    stale_reason: partial.stale_reason ?? "",
    updated_at: partial.updated_at ?? "",
  }
}

function repo(partial: Partial<Repo> = {}): Repo {
  return {
    id: partial.id ?? 7,
    workspace_id: partial.workspace_id ?? 1,
    name: partial.name ?? "weft-codex",
    path: partial.path ?? "/tmp/weft-codex",
    base_ref: partial.base_ref ?? "main",
    remote_url: partial.remote_url ?? "",
    base_ref_is_default: partial.base_ref_is_default ?? 1,
    created_at: partial.created_at ?? "",
  }
}

function board(partial: Partial<BoardEntry> = {}): BoardEntry[] {
  return [{
    issue: partial.issue ?? issue(),
    directions: partial.directions ?? [direction()],
    threads: partial.threads ?? [],
    artifacts: partial.artifacts ?? [],
  }]
}

test("an empty query matches nothing rather than everything", () => {
  assert.deepEqual(searchBoard(board(), [repo()], "   "), [])
})

test("issues match on title and on their own number", () => {
  assert.equal(searchBoard(board(), [repo()], "importer")[0]?.kind, "issue")
  assert.equal(searchBoard(board(), [repo()], "12")[0]?.issueId, 12)
})

test("a direction matches on its repository name, which lives outside the board", () => {
  const hits = searchBoard(board(), [repo({ name: "weft-codex" })], "weft-codex")
  assert.equal(hits.length, 1)
  assert.equal(hits[0]?.kind, "direction")
  assert.match(hits[0]?.meta ?? "", /weft-codex/)
})

test("a direction hit carries the thread that opening it should land on", () => {
  const hits = searchBoard(
    board({ threads: [thread({ direction_id: 1, is_primary: 1, thread_id: "t-primary" })] }),
    [repo()],
    "parser",
  )
  assert.equal(hits[0]?.threadId, "t-primary")
})

test("issues outrank the directions and artifacts under them", () => {
  const hits = searchBoard(
    board({
      issue: issue({ title: "parser rewrite" }),
      artifacts: [artifact({ title: "parser notes" })],
    }),
    [repo()],
    "parser",
  )
  assert.deepEqual(hits.map((hit) => hit.kind), ["issue", "direction", "artifact"])
})

// A primary thread is already reachable through its issue or direction row.
test("primary threads are not listed twice", () => {
  const hits = searchBoard(
    board({
      threads: [
        thread({ thread_id: "t-primary", title: "parser", is_primary: 1 }),
        thread({ thread_id: "t-fork", title: "parser fork" }),
      ],
    }),
    [repo()],
    "parser fork",
  )
  assert.deepEqual(hits.map((hit) => hit.threadId), ["t-fork"])
})

test("the inbox lists directions flagged for attention, with their reason", () => {
  const items = buildInbox(
    board({ directions: [direction({ attention: 1, attention_reason: "needs a decision" })] }),
    [],
  )
  assert.equal(items.length, 1)
  assert.equal(items[0]?.kind, "attention")
  assert.equal(items[0]?.reason, "needs a decision")
  assert.equal(items[0]?.issueTitle, "Ship the importer")
})

test("a delivery failure surfaces against the direction it was bound for", () => {
  const items = buildInbox(board(), [{ issueId: 12, party: "1", reason: "settlement-failed" }])
  assert.equal(items.length, 1)
  assert.equal(items[0]?.kind, "delivery")
  assert.equal(items[0]?.directionId, 1)
})

// The attention flag says the same thing and gives the human somewhere to go.
test("a delivery failure on an already-flagged direction does not double up", () => {
  const items = buildInbox(
    board({ directions: [direction({ attention: 1, attention_reason: "stuck" })] }),
    [{ issueId: 12, party: "1", reason: "settlement-failed" }],
  )
  assert.deepEqual(items.map((item) => item.kind), ["attention"])
})

test("a delivery failure for an issue no longer on the board is dropped", () => {
  assert.deepEqual(buildInbox(board(), [{ issueId: 999, party: "1", reason: "gone" }]), [])
})

// A lead that will not start blocks everything under it, so it has to reach the
// inbox rather than living only in a toast that disappears.
test("a stalled lead reaches the inbox ahead of its tasks", () => {
  const items = buildInbox(
    board({
      issue: issue({ lead_attention: 1, lead_attention_reason: "start-failed" }),
      directions: [direction({ attention: 1, attention_reason: "stuck" })],
    }),
    [],
  )
  assert.deepEqual(items.map((item) => item.kind), ["lead", "attention"])
  assert.equal(items[0]?.reason, "start-failed")
})

test("an issue with a healthy lead contributes nothing to the inbox", () => {
  assert.deepEqual(buildInbox(board(), []), [])
})

test("a task waiting for accept reaches the inbox", () => {
  const items = buildInbox(
    board({ directions: [direction({ status: "review", attention: 0 })] }),
    [],
  )
  assert.equal(items.length, 1)
  assert.equal(items[0]?.kind, "review")
  assert.equal(inboxFollow(items[0]!).action, "show-issue")
})

test("a failed review turn is listed once as attention, not again as review", () => {
  const items = buildInbox(
    board({
      directions: [direction({ status: "review", attention: 1, attention_reason: "turn failed" })],
    }),
    [],
  )
  assert.deepEqual(items.map((item) => item.kind), ["attention"])
})

test("a stalled lead with a thread opens that chat instead of covering it", () => {
  const items = buildInbox(
    board({
      issue: issue({
        lead_attention: 1,
        lead_attention_reason: "turn-error",
        lead_codex_thread_id: "thr_lead",
      }),
      directions: [],
    }),
    [],
  )
  assert.deepEqual(inboxFollow(items[0]!), {
    action: "open-thread",
    threadId: "thr_lead",
    issueId: 12,
  })
})

test("a stalled lead without a thread shows the issue page", () => {
  const items = buildInbox(
    board({
      issue: issue({ lead_attention: 1, lead_attention_reason: "start-failed" }),
      directions: [],
    }),
    [],
  )
  assert.deepEqual(inboxFollow(items[0]!), { action: "show-issue", issueId: 12 })
})

test("task inbox rows name the parent issue; lead rows do not repeat it", () => {
  const items = buildInbox(
    board({
      issue: issue({ lead_attention: 1, lead_attention_reason: "start-failed" }),
      directions: [direction({ status: "review", attention: 0 })],
    }),
    [],
  )
  assert.equal(items[0]?.issueTitle, undefined)
  assert.equal(items[1]?.issueTitle, "Ship the importer")
  assert.equal(inboxRowMeta(items[1]?.issueTitle, "Ready for you to accept"), "Ship the importer · Ready for you to accept")
  assert.deepEqual([...inboxIssueIds(items)], [12])
})

test("a direction search hit without a thread opens the issue page", () => {
  const hits = searchBoard(board(), [repo()], "parser")
  assert.equal(hits[0]?.kind, "direction")
  assert.deepEqual(searchFollow(hits[0]!), { action: "show-issue", issueId: 12 })
})

test("a direction search hit with a thread opens that chat", () => {
  const hits = searchBoard(
    board({ threads: [thread({ direction_id: 1, is_primary: 1, thread_id: "t-primary" })] }),
    [repo()],
    "parser",
  )
  assert.deepEqual(searchFollow(hits[0]!), {
    action: "open-thread",
    threadId: "t-primary",
    issueId: 12,
  })
})

test("an issue search hit still opens the lead", () => {
  const hits = searchBoard(board(), [repo()], "importer")
  assert.deepEqual(searchFollow(hits[0]!), { action: "open-issue", issueId: 12 })
})

test("an undelivered bus payload becomes a delivery failure", () => {
  const failure = parseDeliveryFailure(JSON.stringify({
    issueId: 12,
    party: "1",
    reason: "settlement-failed",
  }))
  assert.deepEqual(failure, { issueId: 12, party: "1", reason: "settlement-failed" })
})

test("a malformed undelivered payload is ignored", () => {
  assert.equal(parseDeliveryFailure("{"), null)
  assert.equal(parseDeliveryFailure(JSON.stringify({ party: "1" })), null)
})

test("the same delivery failure is remembered once", () => {
  const first = rememberDeliveryFailure([], { issueId: 12, party: "1", reason: "a" })
  const second = rememberDeliveryFailure(first, { issueId: 12, party: "1", reason: "b" })
  assert.equal(second.length, 1)
  assert.equal(second[0]?.reason, "a")
})
