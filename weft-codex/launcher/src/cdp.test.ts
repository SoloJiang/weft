import assert from "node:assert/strict"
import test from "node:test"

import { selectRendererTarget, type CdpTarget } from "./cdp.js"

function page(id: string, url: string): CdpTarget {
  return { id, type: "page", title: id, url, webSocketDebuggerUrl: `ws://localhost/${id}` }
}

test("selects the only renderer page", () => {
  assert.equal(selectRendererTarget([page("main", "https://example.test")]).id, "main")
})

test("uses a URL hint when multiple pages exist", () => {
  const targets = [page("other", "https://example.test"), page("codex", "file:///codex/index.html")]
  assert.equal(selectRendererTarget(targets, "codex/index.html").id, "codex")
})

test("prefers an exact URL over a matching auxiliary page", () => {
  const targets = [
    page("overlay", "app://-/index.html?initialRoute=%2Favatar-overlay"),
    page("main", "app://-/index.html"),
  ]
  assert.equal(selectRendererTarget(targets, "app://-/index.html").id, "main")
})

test("fails closed when a renderer is ambiguous", () => {
  const targets = [page("one", "https://one.test"), page("two", "https://two.test")]
  assert.throws(() => selectRendererTarget(targets), /Could not select one renderer target/)
})
