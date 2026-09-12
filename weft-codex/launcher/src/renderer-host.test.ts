import assert from "node:assert/strict"
import test from "node:test"

import { buildRendererAgentSource } from "./renderer-agent.js"
import {
  attachFailureReason,
  installReady,
  waitForAgent,
  waitForSurfaces,
  type RendererAgentStatus,
} from "./renderer-host.js"

function status(partial: Partial<RendererAgentStatus> = {}): RendererAgentStatus {
  return {
    version: 1,
    mode: "weft",
    view: "workspace",
    tier: "weft-mode",
    cspBypass: false,
    uiMounted: false,
    uiReady: false,
    nativeModeSwitcher: false,
    ...partial,
  }
}

const MOUNTED = { uiMounted: true }
const READY = { uiMounted: true, uiReady: true }

function scripted(steps: RendererAgentStatus[]): () => Promise<RendererAgentStatus | null> {
  let index = 0
  return async () => steps[Math.min(index++, steps.length - 1)] ?? null
}

/**
 * Work and Codex never mount the Weft shell, so readiness there means "the
 * agent answered" and nothing more. Requiring `uiReady` in those modes made
 * install give up and dispose the agent, which took the mode menu's third item
 * down with it and left no way into Weft at all — the launcher could only ever
 * enter Weft via `--mode=weft`.
 */
test("an agent that never mounts a shell still counts as attached", async () => {
  const result = await waitForAgent(scripted([status({ mode: "codex" })]), 400)
  assert.equal(result?.mode, "codex")
  assert.equal(result?.uiReady, false)
})

test("a renderer that never answers is not attached", async () => {
  const result = await waitForAgent(async () => null, 300)
  assert.equal(result, null)
})

test("waiting for the agent stops at the first answer, not at the budget", async () => {
  const started = Date.now()
  await waitForAgent(scripted([status({ mode: "work" })]), 5000)
  assert.ok(Date.now() - started < 1000, "should not wait out a budget it does not need")
})

test("work and codex are ready once the agent answers, even without a shell", () => {
  assert.equal(installReady(false, status({ mode: "codex", uiReady: false })), true)
  assert.equal(installReady(false, status({ mode: "work", uiReady: false })), true)
  assert.equal(installReady(false, null), false)
})

test("weft is ready only after the shell handshakes", () => {
  assert.equal(installReady(true, status({ uiMounted: true, uiReady: false })), false)
  assert.equal(installReady(true, status(READY)), true)
  assert.equal(installReady(true, null), false)
})

test("failure reasons name the phase that actually failed", () => {
  assert.match(attachFailureReason(false, null), /agent did not attach/)
  assert.match(attachFailureReason(true, status(MOUNTED)), /finish mounting/)
  assert.match(attachFailureReason(true, status()), /did not render its shell/)
})

test("host hydration does not spend the handshake budget", async () => {
  const steps = [...Array(6)].map(() => status()).concat([status(MOUNTED), status(READY)])
  const result = await waitForSurfaces(scripted(steps), { mountMs: 4000, handshakeMs: 200 })
  assert.equal(result?.uiReady, true)
})

test("a mounted but silent surface still gives up", async () => {
  const result = await waitForSurfaces(scripted([status(MOUNTED)]), { mountMs: 4000, handshakeMs: 200 })
  assert.equal(result?.uiReady, false)
  assert.equal(result?.uiMounted, true, "mounted state must survive so the caller can name the phase")
})

test("a host that never renders its shell gives up on the mount budget", async () => {
  const result = await waitForSurfaces(scripted([status()]), { mountMs: 200, handshakeMs: 200 })
  assert.equal(result?.uiMounted, false)
})

test("a transient poll failure keeps the last status", async () => {
  let call = 0
  const result = await waitForSurfaces(async () => {
    call += 1
    if (call === 1) return status(MOUNTED)
    if (call < 4) throw new Error("no execution context")
    return status(READY)
  }, { mountMs: 1000, handshakeMs: 1000 })
  assert.equal(result?.uiReady, true)
})

test("surfaces already ready return without waiting out the mount budget", async () => {
  const started = Date.now()
  const result = await waitForSurfaces(scripted([status(READY)]), { mountMs: 5000, handshakeMs: 5000 })
  assert.equal(result?.uiReady, true)
  assert.ok(Date.now() - started < 500, "must not sleep once the surfaces are ready")
})

test("renderer host agent exposes the lifecycle methods used by reconnect and CSP fallback", () => {
  const source = buildRendererAgentSource({
    webBaseUrl: "http://127.0.0.1:47810/",
    bindingName: "weftCodexHost",
    initialMode: "weft",
    cspBypass: false,
  })
  for (const method of [
    "status",
    "setMode",
    "setCspBypass",
    "reloadFrames",
    "deliverActionResult",
    "dispose",
  ]) {
    assert.match(source, new RegExp(`\\b${method}\\b`))
  }
})
