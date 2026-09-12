import assert from "node:assert/strict"
import test from "node:test"

import { pickWorkspaceId } from "./workspace-id.ts"

test("a preferred id wins when it still exists", () => {
  assert.equal(pickWorkspaceId([1, 2, 3], { preferredId: 2, currentId: 1, persistedId: 3 }), 2)
})

test("the current id beats a persisted one", () => {
  assert.equal(pickWorkspaceId([1, 2, 3], { currentId: 2, persistedId: 3 }), 2)
})

test("a persisted id is used when nothing else is selected", () => {
  assert.equal(pickWorkspaceId([1, 2, 3], { persistedId: 3 }), 3)
})

test("a stale persisted id falls through to the first workspace", () => {
  assert.equal(pickWorkspaceId([1, 2], { persistedId: 9 }), 1)
})

test("an empty list has no workspace", () => {
  assert.equal(pickWorkspaceId([], { persistedId: 1 }), null)
})
