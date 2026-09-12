import assert from "node:assert/strict"
import test from "node:test"

import { codexLaunchArgs, reserveLoopbackPort, resolveRuntimeLayout } from "./processes.js"

test("Codex launch arguments isolate the profile and bind CDP to loopback", () => {
  assert.deepEqual(codexLaunchArgs("/tmp/weft profile", 9412), [
    "--user-data-dir=/tmp/weft profile",
    "--remote-debugging-address=127.0.0.1",
    "--remote-debugging-port=9412",
  ])
})

test("Codex launch arguments reject an invalid port", () => {
  assert.throws(() => codexLaunchArgs("/tmp/profile", 0), /port is invalid/)
  assert.throws(() => codexLaunchArgs("/tmp/profile", 70000), /port is invalid/)
})

test("reserves a usable loopback port", async () => {
  const port = await reserveLoopbackPort()
  assert.ok(port > 0 && port < 65536)
})

test("resolves a tar archive layout from the launcher executable", () => {
  const files = new Set([
    "/opt/weft-codex/bin/weftd",
    "/opt/weft-codex/share/weft-codex/web/index.html",
  ])
  assert.deepEqual(resolveRuntimeLayout({
    executablePath: "/opt/weft-codex/bin/weft-codex",
    moduleUrl: "file:///ignored/processes.js",
    exists: (candidate) => files.has(candidate),
  }), {
    weftdPath: "/opt/weft-codex/bin/weftd",
    webDir: "/opt/weft-codex/share/weft-codex/web",
    skillsDir: "/opt/weft-codex/share/weft-codex/skills",
    source: "bundle",
  })
})

test("an explicit runtime root wins even before files are built", () => {
  assert.deepEqual(resolveRuntimeLayout({
    executablePath: "/ignored/bin/node",
    moduleUrl: "file:///ignored/processes.js",
    runtimeRoot: "/tmp/custom-runtime",
    exists: () => false,
  }), {
    weftdPath: "/tmp/custom-runtime/bin/weftd",
    webDir: "/tmp/custom-runtime/share/weft-codex/web",
    skillsDir: "/tmp/custom-runtime/share/weft-codex/skills",
    source: "override",
  })
})

test("a source checkout prefers the incrementally built debug daemon", () => {
  const files = new Set([
    "/workspace/target/debug/weftd",
    "/workspace/target/release/weftd",
  ])
  assert.deepEqual(resolveRuntimeLayout({
    executablePath: "/opt/homebrew/bin/node",
    moduleUrl: "file:///workspace/launcher/dist/processes.js",
    exists: (candidate) => files.has(candidate),
  }), {
    weftdPath: "/workspace/target/debug/weftd",
    webDir: "/workspace/crates/daemon/web",
    skillsDir: "/workspace/skills",
    source: "source",
  })
})
