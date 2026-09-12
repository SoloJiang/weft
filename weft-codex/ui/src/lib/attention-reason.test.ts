import assert from "node:assert/strict"
import test from "node:test"

import {
  DELIVERY_ATTENTION_KEYS,
  DIRECTION_ATTENTION_KEYS,
  LEAD_ATTENTION_KEYS,
  deliveryAttentionKey,
  directionAttentionKey,
  inboxAttentionKey,
  issueBoardSignalKey,
  issueBoardSignalReason,
  leadAttentionKey,
} from "./attention-reason.ts"

test("each known lead failure has its own copy key", () => {
  assert.equal(leadAttentionKey("start-failed"), "lead.startFailed")
  assert.equal(leadAttentionKey("resume-failed"), "lead.resumeFailed")
  assert.equal(leadAttentionKey("turn-error"), "lead.turnError")
})

test("the four direction failures are distinct keys", () => {
  assert.equal(directionAttentionKey("worker-start-failed"), "dir.startFailed")
  assert.equal(directionAttentionKey("thread-resume-failed"), "dir.resumeFailed")
  assert.equal(directionAttentionKey("turn failed"), "dir.turnFailed")
  assert.equal(directionAttentionKey("quota exceeded"), "dir.quotaExceeded")
  const keys = [
    "worker-start-failed",
    "thread-resume-failed",
    "turn failed",
    "quota exceeded",
  ].map(directionAttentionKey)
  assert.equal(new Set(keys).size, 4)
})

test("an unknown or empty reason uses generic copy, never the raw code", () => {
  assert.equal(leadAttentionKey("mystery"), "lead.failed")
  assert.equal(leadAttentionKey(""), "lead.failed")
  assert.equal(directionAttentionKey("needs a decision"), "dir.attention")
  assert.equal(directionAttentionKey(""), "dir.attention")
  assert.equal(deliveryAttentionKey("gone"), "dir.undelivered")
  assert.notEqual(leadAttentionKey("mystery"), "mystery")
  assert.notEqual(directionAttentionKey("needs a decision"), "needs a decision")
})

test("inbox rows pick the map that matches their kind", () => {
  assert.equal(inboxAttentionKey("lead", "start-failed"), "lead.startFailed")
  assert.equal(inboxAttentionKey("attention", "quota exceeded"), "dir.quotaExceeded")
  assert.equal(inboxAttentionKey("delivery", "settlement-failed"), "dir.undelivered")
  assert.equal(inboxAttentionKey("review", "review"), "inbox.review")
})

test("an issue card prefers the lead reason over its tasks", () => {
  assert.equal(
    issueBoardSignalKey({
      leadAttention: true,
      leadReason: "resume-failed",
      directionReasons: ["quota exceeded"],
    }),
    "lead.resumeFailed",
  )
})

test("one shared task reason is what the issue card shows", () => {
  assert.equal(
    issueBoardSignalKey({
      leadAttention: false,
      leadReason: "",
      directionReasons: ["quota exceeded", "quota exceeded"],
    }),
    "dir.quotaExceeded",
  )
})

test("mixed task reasons collapse to the generic needs-you line", () => {
  assert.equal(
    issueBoardSignalKey({
      leadAttention: false,
      leadReason: "",
      directionReasons: ["quota exceeded", "turn failed"],
    }),
    "kanban.issueNeedsYou",
  )
})

test("the data-attribute carries a code only when one source owns the signal", () => {
  assert.equal(
    issueBoardSignalReason({
      leadAttention: true,
      leadReason: "start-failed",
      directionReasons: ["quota exceeded"],
    }),
    "start-failed",
  )
  assert.equal(
    issueBoardSignalReason({
      leadAttention: false,
      leadReason: "",
      directionReasons: ["quota exceeded", "quota exceeded"],
    }),
    "quota exceeded",
  )
  assert.equal(
    issueBoardSignalReason({
      leadAttention: false,
      leadReason: "",
      directionReasons: ["quota exceeded", "turn failed"],
    }),
    "",
  )
})

test("every mapped value is a message key, not a daemon code", () => {
  const values = [
    ...Object.values(LEAD_ATTENTION_KEYS),
    ...Object.values(DIRECTION_ATTENTION_KEYS),
    ...Object.values(DELIVERY_ATTENTION_KEYS),
  ]
  for (const value of values) {
    assert.match(value, /^(lead|dir)\.[A-Za-z]+$/)
  }
})
