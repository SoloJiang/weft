import assert from "node:assert/strict"
import test from "node:test"

import { apiUrl, configureApi } from "./api.ts"

test("relative paths stay relative when no origin is configured", () => {
  configureApi("")
  assert.equal(apiUrl("/api/issues"), "/api/issues")
})

test("hosted origin is prefixed without a trailing slash", () => {
  configureApi("http://127.0.0.1:47810/")
  assert.equal(apiUrl("/api/events"), "http://127.0.0.1:47810/api/events")
})

test("rejects a relative API path", () => {
  assert.throws(() => apiUrl("api/issues"), /absolute/)
})
