import assert from "node:assert/strict"
import test from "node:test"

import { canPickFolders, type WeftHost } from "./host.ts"

function host(origin: string): WeftHost {
  return { weftdOrigin: origin } as WeftHost
}

test("the folder picker is only offered when weftd is a real origin", () => {
  assert.equal(canPickFolders(host("")), false)
  assert.equal(canPickFolders(host("http://127.0.0.1:47810")), true)
})
