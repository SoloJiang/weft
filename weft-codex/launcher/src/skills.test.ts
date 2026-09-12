import assert from "node:assert/strict"
import test from "node:test"

import {
  ensureManagedSkills,
  formatManagedMarker,
  parseManagedMarker,
  parseSkillVersion,
} from "./skills.js"

function skillBody(version: string, body = "body"): string {
  return `---\nname: demo\nversion: ${version}\ndescription: demo\n---\n\n${body}\n`
}

function memoryFs(seed: Record<string, string> = {}) {
  const files = new Map(Object.entries(seed))
  const dirs = new Set<string>()
  for (const filePath of files.keys()) {
    let current = filePath
    while (current.includes("/")) {
      current = current.slice(0, current.lastIndexOf("/"))
      if (!current) break
      dirs.add(current)
    }
  }

  return {
    files,
    async readFile(filePath: string) {
      const value = files.get(filePath)
      if (value === undefined) throw new Error(`ENOENT: ${filePath}`)
      return value
    },
    async writeFile(filePath: string, contents: string) {
      files.set(filePath, contents)
      let current = filePath
      while (current.includes("/")) {
        current = current.slice(0, current.lastIndexOf("/"))
        if (!current) break
        dirs.add(current)
      }
    },
    async copyFile(from: string, to: string) {
      const value = files.get(from)
      if (value === undefined) throw new Error(`ENOENT: ${from}`)
      files.set(to, value)
      let current = to
      while (current.includes("/")) {
        current = current.slice(0, current.lastIndexOf("/"))
        if (!current) break
        dirs.add(current)
      }
    },
    async mkdir(dirPath: string) {
      dirs.add(dirPath)
    },
    async readdir(dirPath: string) {
      const prefix = dirPath.endsWith("/") ? dirPath : `${dirPath}/`
      const names = new Set<string>()
      for (const filePath of files.keys()) {
        if (!filePath.startsWith(prefix)) continue
        const rest = filePath.slice(prefix.length)
        names.add(rest.split("/")[0] ?? rest)
      }
      for (const candidate of dirs) {
        if (!candidate.startsWith(prefix)) continue
        const rest = candidate.slice(prefix.length)
        if (!rest || rest.includes("/")) continue
        names.add(rest)
      }
      return [...names].sort()
    },
    async access(filePath: string) {
      if (files.has(filePath) || dirs.has(filePath)) return
      throw new Error(`ENOENT: ${filePath}`)
    },
    async rm(filePath: string) {
      files.delete(filePath)
    },
  }
}

test("parses skill frontmatter versions and managed markers", () => {
  assert.equal(parseSkillVersion(skillBody("1.2.0")), "1.2.0")
  assert.equal(parseSkillVersion("# no frontmatter\n"), null)
  assert.deepEqual(parseManagedMarker(formatManagedMarker("1.2.0")), {
    managed: true,
    version: "1.2.0",
  })
  assert.deepEqual(parseManagedMarker("deadbeef".repeat(8) + "\n"), {
    managed: true,
    version: null,
  })
})

test("installs missing managed skills with a version marker", async () => {
  const fs = memoryFs({
    "/runtime/skills/weft-derive-test-cases/SKILL.md": skillBody("1"),
  })
  const result = await ensureManagedSkills({
    sourceDir: "/runtime/skills",
    targetDir: "/home/.codex/skills",
    ...fs,
  })
  assert.deepEqual(result.installed, ["weft-derive-test-cases"])
  assert.equal(
    fs.files.get("/home/.codex/skills/weft-derive-test-cases/SKILL.md"),
    skillBody("1"),
  )
  assert.equal(
    fs.files.get("/home/.codex/skills/weft-derive-test-cases/.weft-managed"),
    "version=1\n",
  )
})

test("updates a managed skill only when the package version changes", async () => {
  const fs = memoryFs({
    "/runtime/skills/weft-derive-test-cases/SKILL.md": skillBody("2", "new"),
    "/home/.codex/skills/weft-derive-test-cases/SKILL.md": skillBody("1", "old"),
    "/home/.codex/skills/weft-derive-test-cases/.weft-managed": "version=1\n",
    "/home/.codex/skills/weft-derive-test-cases/stale-notes.md": "keep me? no\n",
  })
  const result = await ensureManagedSkills({
    sourceDir: "/runtime/skills",
    targetDir: "/home/.codex/skills",
    ...fs,
  })
  assert.deepEqual(result.updated, ["weft-derive-test-cases"])
  assert.equal(
    fs.files.get("/home/.codex/skills/weft-derive-test-cases/SKILL.md"),
    skillBody("2", "new"),
  )
  assert.equal(fs.files.has("/home/.codex/skills/weft-derive-test-cases/stale-notes.md"), false)
  assert.equal(
    fs.files.get("/home/.codex/skills/weft-derive-test-cases/.weft-managed"),
    "version=2\n",
  )
})

test("same version does not thrash content drift", async () => {
  const fs = memoryFs({
    "/runtime/skills/weft-derive-test-cases/SKILL.md": skillBody("1", "package body"),
    "/home/.codex/skills/weft-derive-test-cases/SKILL.md": skillBody("1", "local tweak"),
    "/home/.codex/skills/weft-derive-test-cases/.weft-managed": "version=1\n",
  })
  const result = await ensureManagedSkills({
    sourceDir: "/runtime/skills",
    targetDir: "/home/.codex/skills",
    ...fs,
  })
  assert.deepEqual(result.unchanged, ["weft-derive-test-cases"])
  assert.equal(
    fs.files.get("/home/.codex/skills/weft-derive-test-cases/SKILL.md"),
    skillBody("1", "local tweak"),
  )
})

test("leaves an unmarked local skill alone unless forced", async () => {
  const fs = memoryFs({
    "/runtime/skills/weft-derive-test-cases/SKILL.md": skillBody("2"),
    "/home/.codex/skills/weft-derive-test-cases/SKILL.md": "# local fork\n",
  })
  const skipped = await ensureManagedSkills({
    sourceDir: "/runtime/skills",
    targetDir: "/home/.codex/skills",
    ...fs,
  })
  assert.equal(skipped.skipped.length, 1)
  assert.equal(fs.files.get("/home/.codex/skills/weft-derive-test-cases/SKILL.md"), "# local fork\n")

  const forced = await ensureManagedSkills({
    sourceDir: "/runtime/skills",
    targetDir: "/home/.codex/skills",
    force: true,
    ...fs,
  })
  assert.deepEqual(forced.updated, ["weft-derive-test-cases"])
  assert.equal(
    fs.files.get("/home/.codex/skills/weft-derive-test-cases/SKILL.md"),
    skillBody("2"),
  )
})

test("legacy hash markers are treated as managed and upgraded once", async () => {
  const fs = memoryFs({
    "/runtime/skills/weft-derive-test-cases/SKILL.md": skillBody("1", "fresh"),
    "/home/.codex/skills/weft-derive-test-cases/SKILL.md": skillBody("1", "stale"),
    "/home/.codex/skills/weft-derive-test-cases/.weft-managed": `${"ab".repeat(32)}\n`,
  })
  const result = await ensureManagedSkills({
    sourceDir: "/runtime/skills",
    targetDir: "/home/.codex/skills",
    ...fs,
  })
  assert.deepEqual(result.updated, ["weft-derive-test-cases"])
  assert.equal(
    fs.files.get("/home/.codex/skills/weft-derive-test-cases/.weft-managed"),
    "version=1\n",
  )
})

test("package skills without a version are skipped", async () => {
  const fs = memoryFs({
    "/runtime/skills/weft-derive-test-cases/SKILL.md": "---\nname: demo\n---\n\nbody\n",
  })
  const result = await ensureManagedSkills({
    sourceDir: "/runtime/skills",
    targetDir: "/home/.codex/skills",
    ...fs,
  })
  assert.equal(result.skipped.length, 1)
  assert.match(result.skipped[0]?.reason ?? "", /missing a frontmatter version/)
})
