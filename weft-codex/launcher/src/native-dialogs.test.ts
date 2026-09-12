import assert from "node:assert/strict"
import test from "node:test"

import { parseSelectedDirectories } from "./native-dialogs.js"

test("parses, normalizes, and deduplicates selected directories", () => {
  assert.deepEqual(
    parseSelectedDirectories("/repo/one/\n/repo/two\n/repo/one/\nrelative\n"),
    ["/repo/one", "/repo/two"],
  )
})
