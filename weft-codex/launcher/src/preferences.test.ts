import assert from "node:assert/strict"
import { mkdtemp, readFile } from "node:fs/promises"
import { tmpdir } from "node:os"
import path from "node:path"
import test from "node:test"

import { readHostPreferences, writeHostPreferences } from "./preferences.js"

test("host preferences default to Weft mode", async () => {
  const directory = await mkdtemp(path.join(tmpdir(), "weft-host-pref-"))
  assert.deepEqual(await readHostPreferences(path.join(directory, "missing.json")), {
    version: 1,
    mode: "weft",
  })
})

test("host preferences persist one validated mode atomically", async () => {
  const directory = await mkdtemp(path.join(tmpdir(), "weft-host-pref-"))
  const filePath = path.join(directory, "host.json")
  await writeHostPreferences({ version: 1, mode: "codex" }, filePath)
  assert.equal((await readHostPreferences(filePath)).mode, "codex")
  assert.match(await readFile(filePath, "utf8"), /"mode": "codex"/)
})
