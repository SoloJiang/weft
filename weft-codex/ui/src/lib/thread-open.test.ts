import assert from "node:assert/strict"
import test from "node:test"

import { installHost, type WeftHost } from "../host.ts"
import {
  beginThreadOpen,
  clearThreadOpenFailure,
  finishThreadOpen,
  idleThreadOpen,
  isOpeningThread,
  openCodexThread,
} from "./thread-open.ts"

function host(openThread: WeftHost["openThread"]): WeftHost {
  return { weftdOrigin: "http://127.0.0.1:47810", openThread } as WeftHost
}

test("beginning an open clears a previous failure", () => {
  const next = beginThreadOpen("t-2")
  assert.deepEqual(next, { openingThreadId: "t-2", failedThreadId: null })
})

test("a successful finish returns to idle", () => {
  const opening = beginThreadOpen("t-1")
  assert.deepEqual(finishThreadOpen(opening, "t-1", true), idleThreadOpen)
})

test("a failed finish keeps the thread id for retry", () => {
  const opening = beginThreadOpen("t-1")
  assert.deepEqual(finishThreadOpen(opening, "t-1", false), {
    openingThreadId: null,
    failedThreadId: "t-1",
  })
})

test("a stale finish does not clobber a newer open", () => {
  const first = beginThreadOpen("t-1")
  const second = beginThreadOpen("t-2")
  assert.deepEqual(finishThreadOpen(second, "t-1", false), second)
  assert.equal(isOpeningThread(first, "t-1"), true)
  assert.equal(isOpeningThread(second, "t-1"), false)
})

test("dismissing a failure leaves an in-flight open alone", () => {
  const opening = beginThreadOpen("t-1")
  assert.deepEqual(clearThreadOpenFailure(opening), opening)
})

test("dismissing a failure clears only the failed id", () => {
  const failed = finishThreadOpen(beginThreadOpen("t-1"), "t-1", false)
  assert.deepEqual(clearThreadOpenFailure(failed), idleThreadOpen)
})

test("an empty thread id is rejected before the host is called", async () => {
  await assert.rejects(() => openCodexThread(""), /Thread id is required/)
})

test("openCodexThread uses the installed host", async () => {
  const opened: string[] = []
  installHost(host(async (threadId) => {
    opened.push(threadId)
  }))
  try {
    await openCodexThread("t-9")
    assert.deepEqual(opened, ["t-9"])
  } finally {
    installHost(null)
  }
})

test("a host rejection surfaces so the session can offer retry", async () => {
  installHost(host(async () => {
    throw new Error("missing row")
  }))
  try {
    await assert.rejects(() => openCodexThread("t-9"), /missing row/)
  } finally {
    installHost(null)
  }
})
