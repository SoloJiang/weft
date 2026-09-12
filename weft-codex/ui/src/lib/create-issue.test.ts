import assert from "node:assert/strict"
import test from "node:test"

import { createIssueFollow } from "./create-issue.ts"

test("a spawned lead opens the native thread without covering it", () => {
  assert.deepEqual(createIssueFollow({ id: 12, codexThreadId: "thr_1" }), {
    action: "open-thread",
    route: { view: "issue", issueId: 12 },
    threadId: "thr_1",
  })
})

test("a failed spawn shows the issue page", () => {
  assert.deepEqual(createIssueFollow({ id: 12 }), {
    action: "show-issue",
    route: { view: "issue", issueId: 12 },
  })
  assert.deepEqual(createIssueFollow({ id: 12, codexThreadId: null }), {
    action: "show-issue",
    route: { view: "issue", issueId: 12 },
  })
  assert.deepEqual(createIssueFollow({ id: 12, codexThreadId: "" }), {
    action: "show-issue",
    route: { view: "issue", issueId: 12 },
  })
})
