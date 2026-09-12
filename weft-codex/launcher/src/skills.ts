import { constants as fsConstants } from "node:fs"
import {
  access,
  copyFile,
  mkdir,
  readdir,
  readFile,
  rm,
  writeFile,
} from "node:fs/promises"
import { homedir } from "node:os"
import path from "node:path"

const MANAGED_MARKER = ".weft-managed"
const SKILL_FILE = "SKILL.md"

export interface SkillSyncOptions {
  sourceDir: string
  targetDir?: string | undefined
  force?: boolean | undefined
  readFile?: ((filePath: string) => Promise<string>) | undefined
  writeFile?: ((filePath: string, contents: string) => Promise<void>) | undefined
  copyFile?: ((from: string, to: string) => Promise<void>) | undefined
  mkdir?: ((dirPath: string, options?: { recursive?: boolean }) => Promise<void>) | undefined
  readdir?: ((dirPath: string) => Promise<string[]>) | undefined
  access?: ((filePath: string) => Promise<void>) | undefined
  rm?: ((filePath: string, options?: { force?: boolean }) => Promise<void>) | undefined
}

export interface SkillSyncEntry {
  name: string
  action: "installed" | "updated" | "unchanged" | "skipped"
  version?: string
  previousVersion?: string
  reason?: string
}

export interface SkillSyncResult {
  sourceDir: string
  targetDir: string
  entries: SkillSyncEntry[]
  installed: string[]
  updated: string[]
  unchanged: string[]
  skipped: SkillSyncEntry[]
}

export function defaultCodexSkillsDir(codexHome = process.env.CODEX_HOME): string {
  return path.join(codexHome && codexHome.length > 0 ? codexHome : path.join(homedir(), ".codex"), "skills")
}

/**
 * Read the product skill version from YAML frontmatter.
 *
 * Version is intentional, not content-derived: we only refresh a managed skill
 * when this field changes. That keeps day-to-day file churn from thrashing the
 * user's Codex home.
 */
export function parseSkillVersion(contents: string): string | null {
  if (!contents.startsWith("---\n") && !contents.startsWith("---\r\n")) return null
  const end = contents.indexOf("\n---", 4)
  if (end < 0) return null
  const frontmatter = contents.slice(4, end)
  for (const rawLine of frontmatter.split(/\r?\n/)) {
    const line = rawLine.trim()
    if (!line || line.startsWith("#")) continue
    const match = /^version\s*:\s*(.+)$/.exec(line)
    if (!match) continue
    let value = match[1]?.trim() ?? ""
    if (
      (value.startsWith("\"") && value.endsWith("\""))
      || (value.startsWith("'") && value.endsWith("'"))
    ) {
      value = value.slice(1, -1).trim()
    }
    return value.length > 0 ? value : null
  }
  return null
}

export function formatManagedMarker(version: string): string {
  return `version=${version}\n`
}

export function parseManagedMarker(contents: string): { managed: true; version: string | null } {
  const first = contents.split(/\r?\n/).map((line) => line.trim()).find((line) => line.length > 0) ?? ""
  if (!first) return { managed: true, version: null }
  const prefixed = /^version\s*=\s*(.+)$/i.exec(first)
  if (prefixed) {
    const version = prefixed[1]?.trim() ?? ""
    return { managed: true, version: version.length > 0 ? version : null }
  }
  // Legacy hash markers from the first managed-skills cut still mean "we own this".
  if (/^[a-f0-9]{64}$/i.test(first)) return { managed: true, version: null }
  // Any other marker body is still treated as managed ownership.
  return { managed: true, version: first }
}

async function listSkillNames(
  sourceDir: string,
  readdirImpl: (dirPath: string) => Promise<string[]>,
  accessImpl: (filePath: string) => Promise<void>,
): Promise<string[]> {
  let names: string[]
  try {
    names = await readdirImpl(sourceDir)
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error)
    throw new Error(`skills source is unavailable at ${sourceDir}: ${message}`)
  }

  const skills: string[] = []
  for (const name of names) {
    if (name.startsWith(".")) continue
    try {
      await accessImpl(path.join(sourceDir, name, SKILL_FILE))
      skills.push(name)
    } catch {
      // Only directories with SKILL.md are product skills.
    }
  }
  return skills.sort()
}

async function installSkillFiles(options: {
  sourceFile: string
  targetSkillDir: string
  targetFile: string
  markerFile: string
  version: string
  copyFile: (from: string, to: string) => Promise<void>
  mkdir: (dirPath: string, options?: { recursive?: boolean }) => Promise<void>
  writeFile: (filePath: string, contents: string) => Promise<void>
  readdir: (dirPath: string) => Promise<string[]>
  rm: (filePath: string, options?: { force?: boolean }) => Promise<void>
  cleanupExtras: boolean
}): Promise<void> {
  await options.mkdir(options.targetSkillDir, { recursive: true })
  await options.copyFile(options.sourceFile, options.targetFile)
  await options.writeFile(options.markerFile, formatManagedMarker(options.version))
  if (!options.cleanupExtras) return
  try {
    const children = await options.readdir(options.targetSkillDir)
    for (const child of children) {
      if (child === SKILL_FILE || child === MANAGED_MARKER) continue
      await options.rm(path.join(options.targetSkillDir, child), { force: true })
    }
  } catch {
    // Best-effort cleanup; the skill body is already refreshed.
  }
}

/**
 * Install or refresh weft-managed Codex skills.
 *
 * Product skills ship inside the weft-codex runtime and are copied into the
 * user's Codex home so Desktop can load them. Refresh is version-gated: a
 * managed skill is rewritten only when the package skill's frontmatter
 * `version` changes (or when `--force` is used). Unmarked local forks are left
 * alone unless forced.
 */
export async function ensureManagedSkills(options: SkillSyncOptions): Promise<SkillSyncResult> {
  const sourceDir = path.resolve(options.sourceDir)
  const targetDir = path.resolve(options.targetDir ?? defaultCodexSkillsDir())
  const force = options.force === true
  const readFileImpl = options.readFile ?? ((filePath: string) => readFile(filePath, "utf8"))
  const writeFileImpl = options.writeFile ?? ((filePath: string, contents: string) => writeFile(filePath, contents, "utf8"))
  const copyFileImpl = options.copyFile ?? ((from: string, to: string) => copyFile(from, to))
  const mkdirImpl = options.mkdir ?? (async (dirPath: string) => {
    await mkdir(dirPath, { recursive: true })
  })
  const readdirImpl = options.readdir ?? (async (dirPath: string) => readdir(dirPath))
  const accessImpl = options.access ?? (async (filePath: string) => {
    await access(filePath, fsConstants.R_OK)
  })
  const rmImpl = options.rm ?? (async (filePath: string) => {
    await rm(filePath, { force: true })
  })

  const skillNames = await listSkillNames(sourceDir, readdirImpl, accessImpl)
  await mkdirImpl(targetDir, { recursive: true })

  const entries: SkillSyncEntry[] = []
  for (const name of skillNames) {
    const sourceFile = path.join(sourceDir, name, SKILL_FILE)
    const targetSkillDir = path.join(targetDir, name)
    const targetFile = path.join(targetSkillDir, SKILL_FILE)
    const markerFile = path.join(targetSkillDir, MANAGED_MARKER)
    const sourceContents = await readFileImpl(sourceFile)
    const sourceVersion = parseSkillVersion(sourceContents)
    if (!sourceVersion) {
      entries.push({
        name,
        action: "skipped",
        reason: "package skill is missing a frontmatter version field",
      })
      continue
    }

    let existingContents: string | null = null
    try {
      existingContents = await readFileImpl(targetFile)
    } catch {
      existingContents = null
    }

    let managed = false
    let installedVersion: string | null = null
    try {
      const marker = parseManagedMarker(await readFileImpl(markerFile))
      managed = marker.managed
      installedVersion = marker.version
    } catch {
      managed = false
      installedVersion = null
    }

    if (existingContents === null) {
      await installSkillFiles({
        sourceFile,
        targetSkillDir,
        targetFile,
        markerFile,
        version: sourceVersion,
        copyFile: copyFileImpl,
        mkdir: mkdirImpl,
        writeFile: writeFileImpl,
        readdir: readdirImpl,
        rm: rmImpl,
        cleanupExtras: false,
      })
      entries.push({ name, action: "installed", version: sourceVersion })
      continue
    }

    if (!managed && !force) {
      entries.push({
        name,
        action: "skipped",
        version: sourceVersion,
        reason: "local skill differs and is not weft-managed; re-run with --force to overwrite",
      })
      continue
    }

    const sameVersion = installedVersion !== null && installedVersion === sourceVersion
    if (sameVersion && !force) {
      // Version is the upgrade signal. Content drift at the same version is
      // left alone so we do not thrash Codex home on every package rebuild.
      const entry: SkillSyncEntry = {
        name,
        action: "unchanged",
        version: sourceVersion,
      }
      if (installedVersion) entry.previousVersion = installedVersion
      entries.push(entry)
      continue
    }

    await installSkillFiles({
      sourceFile,
      targetSkillDir,
      targetFile,
      markerFile,
      version: sourceVersion,
      copyFile: copyFileImpl,
      mkdir: mkdirImpl,
      writeFile: writeFileImpl,
      readdir: readdirImpl,
      rm: rmImpl,
      cleanupExtras: true,
    })
    const entry: SkillSyncEntry = {
      name,
      action: "updated",
      version: sourceVersion,
      reason: force && sameVersion
        ? "forced refresh at the same version"
        : installedVersion
          ? `version ${installedVersion} → ${sourceVersion}`
          : `installed version marker upgraded to ${sourceVersion}`,
    }
    if (installedVersion) entry.previousVersion = installedVersion
    entries.push(entry)
  }

  return {
    sourceDir,
    targetDir,
    entries,
    installed: entries.filter((entry) => entry.action === "installed").map((entry) => entry.name),
    updated: entries.filter((entry) => entry.action === "updated").map((entry) => entry.name),
    unchanged: entries.filter((entry) => entry.action === "unchanged").map((entry) => entry.name),
    skipped: entries.filter((entry) => entry.action === "skipped"),
  }
}

export function formatSkillSyncSummary(result: SkillSyncResult): string {
  const parts = [
    `${result.installed.length} installed`,
    `${result.updated.length} updated`,
    `${result.unchanged.length} unchanged`,
    `${result.skipped.length} skipped`,
  ]
  return `skills ${parts.join(", ")} → ${result.targetDir}`
}
