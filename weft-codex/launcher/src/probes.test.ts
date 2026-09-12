import assert from "node:assert/strict"
import test from "node:test"
import { readFileSync } from "node:fs"
import { dirname, join } from "node:path"
import { fileURLToPath } from "node:url"

import {
  buildProbeExpression,
  classifyCompatibility,
  reportFromSnapshot,
  TOKEN_PROBES,
  type CapabilityProbe,
} from "./probes.js"

const SELECTOR_IDS = [
  "renderer.root",
  "renderer.main",
  "sidebar.scroll",
  "sidebar.section",
  "sidebar.heading",
  "sidebar.projectCreate",
  "sidebar.threadRow",
  "sidebar.threadRoute",
  "sidebar.threadActive",
] as const

const TOKEN_VALUES = [
  "--vscode-sideBar-background", "--color-token-main-surface-primary",
  "--color-token-dropdown-background", "--color-token-foreground",
  "--color-token-text-secondary", "--color-token-border",
  "--color-token-border-heavy", "--color-token-primary",
  "--vscode-button-foreground", "--color-token-text-link-foreground",
  "--vscode-input-background", "--color-token-input-border",
  "--color-token-list-hover-background", "--font-sans", "--font-mono",
  "--radius-lg", "--radius-md", "--radius-sm",
]

function healthySnapshot(missing: string[] = []) {
  return {
    selectors: Object.fromEntries(SELECTOR_IDS.map((id) => [id, !missing.includes(id)])),
    tokens: Object.fromEntries(TOKEN_VALUES.map((token) => [token, "#000"])),
    modeSwitcher: true,
    modeSwitcherId: true,
    threadRowCount: 24,
    titlebarDragRegion: true,
    headerActionSlot: true,
    locale: "en-GB",
  }
}

function probe(
  id: string,
  ok: boolean,
  requiredFor: CapabilityProbe["requiredFor"],
): CapabilityProbe {
  return { id, ok, detail: id, requiredFor }
}

test("a required failure enters safe mode", () => {
  const probes = [probe("renderer", false, "base"), probe("mode", true, "optional")]
  assert.equal(classifyCompatibility(probes), "safe-mode")
})

test("an optional failure still reaches Weft mode", () => {
  const probes = [
    probe("renderer", true, "base"),
    probe("slot", false, "optional"),
  ]
  assert.equal(classifyCompatibility(probes), "weft-mode")
})

test("all capabilities enable Weft mode", () => {
  const probes = [
    probe("renderer", true, "base"),
    probe("token", true, "base"),
    probe("mode", true, "base"),
  ]
  assert.equal(classifyCompatibility(probes), "weft-mode")
})

test("renderer probe requires an interactive visible main route", () => {
  const expression = buildProbeExpression()
  assert.match(expression, /visibleMainRoute\(\)/)
  assert.match(expression, /closest\('\[inert\], \[aria-hidden="true"\], \[hidden\]'\)/)
  assert.match(expression, /style\.visibility === "hidden"/)
  assert.match(expression, /style\.pointerEvents === "none"/)
  assert.match(expression, /id === "renderer\.main" \? Boolean\(mainRoute\)/)
})

test("a healthy renderer reaches Weft mode with no failure reasons", () => {
  const report = reportFromSnapshot(healthySnapshot())
  assert.equal(report.tier, "weft-mode")
  assert.ok(report.probes.every((entry) => entry.ok))
  assert.ok(report.probes.every((entry) => entry.reason === undefined))
})

for (const id of [
  "sidebar.threadRoute",
  "sidebar.threadActive",
  "sidebar.threadRow",
] as const) {
  test(`losing ${id} when conversations exist cannot enter Weft`, () => {
    const report = reportFromSnapshot(healthySnapshot([id]))
    assert.equal(report.tier, "safe-mode")
    const failed = report.probes.find((entry) => entry.id === id)
    assert.equal(failed?.ok, false)
    assert.equal(failed?.requiredFor, "base")
  })
}

test("losing project create does not block Weft", () => {
  const report = reportFromSnapshot(healthySnapshot(["sidebar.projectCreate"]))
  assert.equal(report.tier, "weft-mode")
  assert.equal(report.probes.find((entry) => entry.id === "sidebar.projectCreate")?.requiredFor, "optional")
})

test("a profile with no conversations still reaches Weft mode", () => {
  const snapshot = healthySnapshot([
    "sidebar.threadRow",
    "sidebar.threadRoute",
    "sidebar.threadActive",
  ])
  snapshot.threadRowCount = 0

  const report = reportFromSnapshot(snapshot)
  assert.equal(report.tier, "weft-mode")
  for (const id of ["sidebar.threadRow", "sidebar.threadRoute", "sidebar.threadActive"]) {
    const entry = report.probes.find((probe) => probe.id === id)
    assert.equal(entry?.ok, true, `${id} must not fail when there is nothing to check`)
    assert.match(entry?.detail ?? "", /Not applicable/)
  }
})

test("once conversations exist a missing thread anchor still blocks Weft", () => {
  const snapshot = healthySnapshot(["sidebar.threadActive"])
  snapshot.threadRowCount = 24
  const report = reportFromSnapshot(snapshot)
  assert.equal(report.tier, "safe-mode")
  assert.equal(
    report.probes.find((probe) => probe.id === "sidebar.threadActive")?.ok,
    false,
  )
})

test("a mode trigger without an id fails the probe instead of reporting ok", () => {
  const report = reportFromSnapshot({ ...healthySnapshot(), modeSwitcherId: false })
  assert.equal(report.tier, "safe-mode")
  const entry = report.probes.find((probe) => probe.id === "mode.switcher")
  assert.equal(entry?.ok, false)
  assert.match(entry?.detail ?? "", /no id/)
  assert.ok(entry?.reason)
})

test("a missing mode trigger is reported distinctly from a trigger without an id", () => {
  const report = reportFromSnapshot({ ...healthySnapshot(), modeSwitcher: false, modeSwitcherId: false })
  const entry = report.probes.find((probe) => probe.id === "mode.switcher")
  assert.equal(entry?.ok, false)
  assert.match(entry?.detail ?? "", /Missing one semantic/)
})

test("failure reasons are user-facing and never leak a selector", () => {
  const report = reportFromSnapshot(healthySnapshot([...SELECTOR_IDS]))
  const failures = report.probes.filter((entry) => !entry.ok)
  assert.ok(failures.length > 0)
  for (const failure of failures) {
    if (!failure.reason) continue
    assert.doesNotMatch(failure.reason, /\[data-|#root|querySelector/)
  }
})

test("a base anchor failure enters safe mode", () => {
  const report = reportFromSnapshot(healthySnapshot(["sidebar.scroll"]))
  assert.equal(report.tier, "safe-mode")
})

/** Spec §8.3: core surfaces, foreground and fonts are the tokens Weft needs. */
for (const id of [
  "theme.sidebarSurface",
  "theme.mainSurface",
  "theme.dropdownSurface",
  "theme.foreground",
  "theme.fontSans",
  "theme.fontMono",
] as const) {
  test(`losing ${id} cannot enter Weft`, () => {
    const snapshot = healthySnapshot()
    snapshot.tokens[TOKEN_PROBES[id]] = ""
    const report = reportFromSnapshot(snapshot)
    assert.equal(report.tier, "safe-mode")
    assert.equal(report.probes.find((probe) => probe.id === id)?.requiredFor, "base")
  })
}

/**
 * A cosmetic token falls back at every consumption site, so losing one may not
 * cost the whole product. Build 6662 deleted two of them and locked everyone
 * out; this is the assertion that stops that from being possible again.
 */
for (const id of ["theme.buttonForeground", "theme.inputBackground", "theme.primary"] as const) {
  test(`losing ${id} still reaches Weft mode`, () => {
    const snapshot = healthySnapshot()
    snapshot.tokens[TOKEN_PROBES[id]] = ""
    const report = reportFromSnapshot(snapshot)
    assert.equal(report.tier, "weft-mode")
    const entry = report.probes.find((probe) => probe.id === id)
    assert.equal(entry?.ok, false, "the probe must still report the loss")
    assert.equal(entry?.requiredFor, "optional")
  })
}

test("the healthy fixture supplies every token the probes read", () => {
  const unsupplied = Object.values(TOKEN_PROBES).filter((token) => !TOKEN_VALUES.includes(token))
  assert.deepEqual(unsupplied, [], "TOKEN_VALUES drifted from TOKEN_PROBES; the fixture is no longer healthy")
})

/**
 * Build 6662 deleted these two aliases while keeping the `--vscode-*`
 * variables behind them, which stranded every user on that build in safe mode.
 * Nothing else in the suite would notice their return: the fixture supplies
 * whatever the probes ask for, so a revert here reads as perfectly healthy.
 */
test("the aliases build 6662 deleted are not probed again", () => {
  const probed: string[] = Object.values(TOKEN_PROBES)
  for (const deleted of ["--color-token-button-foreground", "--color-token-input-background"]) {
    assert.ok(!probed.includes(deleted), `${deleted} does not exist on Codex 6662`)
  }
})

test("a missing titlebar drag region cannot enter Weft", () => {
  const snapshot = { ...healthySnapshot(), titlebarDragRegion: false }
  const report = reportFromSnapshot(snapshot)
  assert.equal(report.tier, "safe-mode")
  assert.equal(report.probes.find((entry) => entry.id === "titlebar.dragRegion")?.ok, false)
})

test("a missing header action slot still reaches Weft mode", () => {
  const snapshot = { ...healthySnapshot(), headerActionSlot: false }
  const report = reportFromSnapshot(snapshot)
  assert.equal(report.tier, "weft-mode")
})

function matrixRows(): { build: number; anchor: string; requiredFor: string }[] {
  const here = dirname(fileURLToPath(import.meta.url))
  const matrix = readFileSync(join(here, "..", "..", "docs", "compat", "codex-builds.md"), "utf8")
  const rows: { build: number; anchor: string; requiredFor: string }[] = []
  for (const line of matrix.split("\n")) {
    const match = /^\|\s*(\d{4,})\s*\|\s*`([^`]+)`\s*\|\s*(base|additive|subtractive|optional)\s*\|/.exec(line)
    if (!match) continue
    const [, build, anchor, requiredFor] = match
    if (!build || !anchor || !requiredFor) continue
    rows.push({ build: Number(build), anchor, requiredFor })
  }
  return rows
}

test("the compatibility matrix documents every probe of the newest build", () => {
  const rows = matrixRows()
  assert.ok(rows.length > 0, "no anchor rows parsed from docs/compat/codex-builds.md")
  const newest = Math.max(...rows.map((row) => row.build))
  const documented = new Map(
    rows.filter((row) => row.build === newest).map((row) => [row.anchor, row.requiredFor]),
  )
  const probes = reportFromSnapshot(healthySnapshot()).probes

  const undocumented = probes.filter((probe) => !documented.has(probe.id)).map((probe) => probe.id)
  assert.deepEqual(undocumented, [], `probes missing from the matrix for build ${newest}`)

  const stale = [...documented.keys()].filter((id) => !probes.some((probe) => probe.id === id))
  assert.deepEqual(stale, [], `matrix rows for build ${newest} with no matching probe`)

  const currentRequiredFor = (value: string | undefined): string | undefined => {
    if (value === "additive") return "base"
    return value
  }
  const mismatched = probes
    .filter((probe) => currentRequiredFor(documented.get(probe.id)) !== probe.requiredFor)
    .map((probe) => `${probe.id}: code=${probe.requiredFor} matrix=${documented.get(probe.id)}`)
  assert.deepEqual(mismatched, [], "requiredFor disagrees between code and matrix")
})
