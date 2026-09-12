import assert from "node:assert/strict"
import test from "node:test"

import { buildRendererAgentSource, validateRendererAgentConfig } from "./renderer-agent.js"

const baseConfig = {
  webBaseUrl: "http://127.0.0.1:47810/",
  bindingName: "weftCodexHost",
  initialMode: "weft" as const,
  cspBypass: false,
}

test("accepts a loopback renderer URL and normalizes it", () => {
  const config = validateRendererAgentConfig({ ...baseConfig, webBaseUrl: "http://localhost:47810" })
  assert.equal(config.webBaseUrl, "http://localhost:47810/")
})

test("rejects a non-loopback renderer URL", () => {
  assert.throws(
    () => validateRendererAgentConfig({ ...baseConfig, webBaseUrl: "https://example.com" }),
    /loopback HTTP/,
  )
})

test("rejects a malformed binding identifier", () => {
  assert.throws(() => validateRendererAgentConfig({ ...baseConfig, bindingName: "bad-name" }), /binding name/)
})

test("builds an idempotent document-start script without raw tag injection", () => {
  const source = buildRendererAgentSource(baseConfig)
  assert.match(source, /__weftCodexAgentV1/)
  assert.match(source, /WeftCodex\.mountWeft/)
  assert.match(source, /attachShadow\(\{ mode: "open" \}\)/)
  assert.match(source, /const mainRoute = visibleMainRoute\(\)/)
  assert.match(source, /THREAD_OPEN_RETRY_DELAYS = \[0, 80, 160, 320, 640, 1000, 1800\]/)
  assert.match(source, /async function openNativeThread\(threadId\)/)
  assert.doesNotMatch(source, /const mainRoute = document\.querySelector\("main"\)/)
  assert.doesNotMatch(source, /<\/script>/i)
  assert.doesNotMatch(source, /createElement\("iframe"\)/)
  assert.doesNotMatch(source, /weft:host-context/)
  assert.doesNotMatch(source, /BroadcastChannel/)
})

const SUBTRACTIVE_RULES = [
  /\[data-weft-codex-tier="weft-mode"\]\[data-weft-codex-mode="weft"\] \[data-app-action-sidebar-scroll\] > :not\(#weft-codex-sidebar-root\)/,
  /\[data-weft-codex-tier="weft-mode"\]\[data-weft-codex-mode="weft"\] \[data-app-action-sidebar-scroll\] \{/,
  /\[data-weft-codex-tier="weft-mode"\]\[data-weft-codex-mode="weft"\] \[data-weft-codex-native-header-action\]/,
  /\[data-weft-codex-tier="weft-mode"\]\[data-weft-codex-mode="weft"\]\[data-weft-codex-view="workspace"\] \[data-testid="app-shell-header-context-menu-surface"\]/,
]

test("the workspace never claims the whole main again", () => {
  const source = buildRendererAgentSource(baseConfig)
  // `inset-inline: 0` plus `bottom: 0` is the rule that swallowed the dock and
  // the side panel: it pinned the workspace to every edge of main regardless of
  // what Codex had open there.
  assert.doesNotMatch(source, /inset-inline: 0/)
  assert.match(source, /root\.style\.right !== rightValue/)
  assert.match(source, /root\.style\.bottom !== bottomValue/)
})

test("the space Codex owns is measured, never assumed", () => {
  const source = buildRendererAgentSource(baseConfig)
  assert.match(source, /nativePanelSize\(mainRoute, "right-panel", "width"\)/)
  assert.match(source, /nativePanelSize\(mainRoute, "bottom-panel", "height"\)/)
  // Scoped to the visible main, so the inert duplicate route cannot answer.
  assert.match(source, /mainRoute\.querySelector\('\[data-app-shell-focus-area="' \+ area \+ '"\]'\)/)
  assert.match(source, /getBoundingClientRect\(\)/)
  // Absent or collapsed both have to read as "Codex is using nothing here".
  assert.match(source, /if \(!\(panel instanceof HTMLElement\)\) return 0;/)
})

test("the native panels are observed so their animation is followed to rest", () => {
  const source = buildRendererAgentSource(baseConfig)
  assert.match(source, /state\.observedPanels\.has\(panel\)/)
  assert.match(source, /state\.resizeObserver\.observe\(panel\)/)
})

test("every subtractive rule is gated on the weft-mode tier", () => {
  const source = buildRendererAgentSource(baseConfig)
  for (const rule of SUBTRACTIVE_RULES) assert.match(source, rule)
  assert.doesNotMatch(source, /html\[data-weft-codex-mode="weft"\] \[data-app-action-sidebar-scroll\] > :not\(/)
  assert.doesNotMatch(source, /html\[data-weft-codex-mode="weft"\] \[data-weft-codex-native-header-action\]/)
})

/**
 * The agent is assembled from nested template literals — the CSS block is a
 * template literal inside the one that builds the script — so a stray backtick
 * anywhere in it silently closes the wrong string. The regex assertions here
 * all still passed while the renderer refused the script outright; only
 * parsing catches that.
 */
test("the generated agent source parses", () => {
  assert.doesNotThrow(() => new Function(buildRendererAgentSource(baseConfig)))
})

test("the stale thread title is hidden on the workspace stage, not collapsed", () => {
  const source = buildRendererAgentSource(baseConfig)
  // Collapsing it would take the header's drag band with it, so the workspace
  // root would compute a top of 0 and slide under the window controls.
  assert.match(
    source,
    /\[data-testid="app-shell-header-context-menu-surface"\] \{\s*visibility: hidden;/,
  )
  assert.doesNotMatch(
    source,
    /\[data-testid="app-shell-header-context-menu-surface"\] \{\s*display: none/,
  )
})

test("the agent has no additive product path", () => {
  const source = buildRendererAgentSource(baseConfig)
  assert.doesNotMatch(source, /data-weft-codex-tier="additive"/)
  assert.doesNotMatch(source, /weft-codex-fallback-button/)
})

test("the native header actions inside the mode row are marked for hiding", () => {
  const source = buildRendererAgentSource(baseConfig)
  assert.match(source, /function actionSlot\(button\)/)
  assert.match(source, /const slot = actionSlot\(button\);[\s\S]{0,400}weftCodexNativeHeaderAction = ""/)
  assert.match(source, /if \(child\.hasAttribute\(HEADER_ACTION_ATTR\)\) continue/)
})

test("the action slot is located structurally, never by locale text", () => {
  const source = buildRendererAgentSource(baseConfig)
  assert.match(source, /siblings\.length === 1 \? siblings\[0\] : null/)
  assert.doesNotMatch(source, /aria-label[^\n]*[Ss]earch/)
  assert.doesNotMatch(source, /View activity/)
})

test("the injected Weft entries are gated on both tier and mode", () => {
  const source = buildRendererAgentSource(baseConfig)
  assert.match(
    source,
    /html:not\(\[data-weft-codex-tier="weft-mode"\]\) \[data-weft-codex-header-action\],\s*html:not\(\[data-weft-codex-mode="weft"\]\) \[data-weft-codex-header-action\]/,
  )
})

test("a missing action slot falls back instead of dropping the entries", () => {
  const source = buildRendererAgentSource(baseConfig)
  assert.match(source, /headerActions: state\.headerActionsMounted \? "native" : "fallback"/)
  assert.match(source, /removeHeaderActions\(\);\s*state\.headerActionsMounted = false;/)
})

test("dispose removes the injected header entries", () => {
  const source = buildRendererAgentSource(baseConfig)
  assert.match(source, /restoreModeButton\(\);\s*removeHeaderActions\(\);/)
})

test("rem-bearing radius tokens are resolved to pixels before writing onto the host", () => {
  const source = buildRendererAgentSource(baseConfig)
  assert.match(source, /function usedLength\(value, rootFontSize\)/)
  assert.match(source, /token\.startsWith\("--radius"\)/)
  assert.match(source, /value\.includes\("%"\)\) return value/)
  assert.match(source, /state\.usedLengths\.set\(key, used\)/)
})

test("every document attribute the agent sets is removed on dispose", () => {
  const source = buildRendererAgentSource(baseConfig)
  const valuesBlock = /const values = \{([\s\S]*?)\};/.exec(source)
  assert.ok(valuesBlock, "could not locate the setDocumentState values block")
  const written = new Set(
    [...(valuesBlock[1] ?? "").matchAll(/^\s*(weftCodex[A-Za-z]+):/gm)].map((m) => m[1]),
  )
  const deleted = new Set(
    [...source.matchAll(/delete root\.dataset\.(weftCodex[A-Za-z]+)/g)].map((m) => m[1]),
  )
  assert.ok(written.size >= 4, `expected several document attributes, saw ${written.size}`)
  const leaked = [...written].filter((name) => !deleted.has(name))
  assert.deepEqual(leaked, [], "document attributes written but never deleted on dispose")
})

test("dispose also removes the injected roots and stylesheet", () => {
  const source = buildRendererAgentSource(baseConfig)
  assert.match(source, /if \(state\.sidebarRoot\) state\.sidebarRoot\.remove\(\)/)
  assert.match(source, /if \(state\.workspaceRoot\) state\.workspaceRoot\.remove\(\)/)
  assert.match(source, /if \(state\.overlayRoot\) state\.overlayRoot\.remove\(\)/)
  assert.match(source, /getElementById\(STYLE_ID\)[\s\S]{0,60}style\.remove\(\)/)
})

test("installing twice disposes the previous agent first", () => {
  const source = buildRendererAgentSource(baseConfig)
  assert.match(
    source,
    /const previous = window\[GLOBAL_KEY\];[\s\S]{0,160}previous\.dispose\(\)/,
  )
})

test("opening another thread while already in thread view still notifies", () => {
  const source = buildRendererAgentSource(baseConfig)
  assert.match(
    source,
    /if \(state.view === nextView && state.notifiedThreadId === threadId\) return/,
  )
  assert.doesNotMatch(source, /if \(state.view === nextView\) return;/)
  assert.match(source, /function syncThreadView\(\)/)
  assert.match(source, /state.mode !== "weft" \|\| state.view !== "thread"/)
  assert.match(source, /syncModeMenus\(\);\s*syncThreadView\(\);/)
})

test("dialogs live in the overlay shadow, not a third iframe", () => {
  const source = buildRendererAgentSource(baseConfig)
  assert.match(source, /root\.id = OVERLAY_ROOT_ID/)
  assert.match(source, /#weft-codex-overlay-root \{[\s\S]*?position: fixed;[\s\S]*?inset: 0;/)
  assert.doesNotMatch(source, /createFrame\("modal"\)/)
  assert.doesNotMatch(source, /#weft-codex-modal-root/)
})

test("a failed Weft bundle is removed so the next mount can retry", () => {
  const source = buildRendererAgentSource(baseConfig)
  assert.match(source, /script\.onerror = \(\) => \{\s*script\.remove\(\)/)
  assert.match(source, /if \(existing\) existing\.remove\(\)/)
})

test("work and codex start without mounting the Weft shell", () => {
  for (const initialMode of ["work", "codex"] as const) {
    const source = buildRendererAgentSource({ ...baseConfig, initialMode })
    assert.doesNotThrow(() => new Function(source))
    assert.match(source, /if \(state\.disposed \|\| state\.mode !== "weft"\) return false/)
    assert.match(source, /if \(state\.mode === "weft"\) void ensureWeftMounted\(\)/)
    assert.match(source, /if \(nextMode === "weft"\) void ensureWeftMounted\(\)/)
  }
})

test("picking Weft from the menu follows the live host button, not a stale nativeMode", () => {
  const source = buildRendererAgentSource({ ...baseConfig, initialMode: "work" })
  // nativeMode used to default to "codex" even on ChatGPT, so the first Weft
  // click skipped the native switch and mounted on the everyday-work base.
  assert.doesNotMatch(
    source,
    /nativeCodex instanceof HTMLElement && state\.nativeMode !== "codex"/,
  )
  assert.match(source, /inferNativeMode\(state\.modeButton\) === "codex"/)
  assert.match(source, /nativeCodex\.click\(\)/)
  assert.match(source, /setMode\("weft"\)/)
})

test("work and codex keep nativeMode in sync with the host button", () => {
  const source = buildRendererAgentSource({ ...baseConfig, initialMode: "work" })
  assert.match(source, /nativeMode: config\.initialMode === "work" \? "work" : "codex"/)
  assert.match(source, /state\.nativeMode = inferred/)
})
