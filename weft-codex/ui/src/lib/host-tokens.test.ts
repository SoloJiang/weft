import assert from "node:assert/strict"
import test from "node:test"
import { readFileSync } from "node:fs"
import { dirname, join } from "node:path"
import { fileURLToPath } from "node:url"

const css = readFileSync(join(dirname(fileURLToPath(import.meta.url)), "..", "index.css"), "utf8")

/**
 * Host variables are read on the host's terms, not ours.
 *
 * Codex retires and renames them between builds — 6662 retired six at once,
 * see docs/compat/codex-builds.md §8.1 — and a read with no fallback turns
 * that into an unset property rather than a slightly-off colour. The one place
 * that ever shipped without one was the inbox badge in `renderer-agent`, whose
 * text colour collapsed to `inherit` on top of the accent fill.
 */
test("every host variable the stylesheet reads has a fallback", () => {
  const bare = css.match(/var\(--(?:color-token|vscode)-[a-zA-Z0-9-]+\)/g) ?? []
  assert.deepEqual([...new Set(bare)].sort(), [])
})
