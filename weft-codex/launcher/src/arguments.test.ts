import assert from "node:assert/strict"
import test from "node:test"

import { hostCommand } from "./arguments.js"

test("no command starts the complete managed Codex flow", () => {
  assert.equal(hostCommand(["node", "cli.js"]), "start")
})

test("a leading option still means start", () => {
  assert.equal(hostCommand(["weft-codex", "cli.js", "--safe-mode"]), "start")
})

test("standalone help and version flags remain commands", () => {
  assert.equal(hostCommand(["weft-codex", "cli.js", "--version"]), "--version")
  assert.equal(hostCommand(["weft-codex", "cli.js", "--help"]), "--help")
})

test("an explicit diagnostic command is preserved", () => {
  assert.equal(hostCommand(["weft-codex", "cli.js", "doctor"]), "doctor")
})
