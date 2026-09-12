import assert from "node:assert/strict"
import test from "node:test"

import { sidebarFooter, threadLinkStatus, workspaceFollowForThread } from "./thread-resolve.ts"

test("a workspace view does not talk about linking", () => {
  assert.equal(threadLinkStatus("workspace", false), "idle")
  assert.equal(threadLinkStatus("workspace", true), "idle")
})

test("a bound thread is silent", () => {
  assert.equal(threadLinkStatus("bound-thread", false), "idle")
})

test("an unknown thread is linking until resolve gives up", () => {
  assert.equal(threadLinkStatus("unbound-thread", false), "linking")
  assert.equal(threadLinkStatus("unbound-thread", true), "unbound")
})

test("an open failure outranks a linking status", () => {
  assert.deepEqual(sidebarFooter("thr_1", "linking"), { kind: "retry", threadId: "thr_1" })
})

test("the footer is silent when nothing is pending", () => {
  assert.deepEqual(sidebarFooter(null, "idle"), { kind: "none" })
})

test("a resolved thread in another workspace is adopted while the chat stays open", () => {
  assert.deepEqual(
    workspaceFollowForThread({
      hostView: "thread",
      currentWorkspaceId: 1,
      threadWorkspaceId: 2,
    }),
    { action: "adopt", workspaceId: 2 },
  )
})

test("the current workspace is left alone when the thread already belongs to it", () => {
  assert.deepEqual(
    workspaceFollowForThread({
      hostView: "thread",
      currentWorkspaceId: 2,
      threadWorkspaceId: 2,
    }),
    { action: "none" },
  )
})

test("a workspace view does not follow a leftover native thread", () => {
  assert.deepEqual(
    workspaceFollowForThread({
      hostView: "workspace",
      currentWorkspaceId: 1,
      threadWorkspaceId: 2,
    }),
    { action: "none" },
  )
})
