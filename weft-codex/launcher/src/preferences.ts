import { mkdir, readFile, rename, writeFile } from "node:fs/promises"
import { homedir } from "node:os"
import path from "node:path"

import type { HostMode } from "./renderer-agent.js"

export interface HostPreferences {
  version: 1
  mode: HostMode
}
export function defaultPreferencesPath(): string {
  return path.join(homedir(), ".weft-codex", "desktop-host.json")
}

function mode(value: unknown): HostMode | null {
  if (value === "work" || value === "codex" || value === "weft") return value
  return null
}

export async function readHostPreferences(filePath = defaultPreferencesPath()): Promise<HostPreferences> {
  try {
    const parsed: unknown = JSON.parse(await readFile(filePath, "utf8"))
    if (parsed && typeof parsed === "object" && !Array.isArray(parsed)) {
      const selected = mode((parsed as Record<string, unknown>).mode)
      if (selected) return { version: 1, mode: selected }
    }
  } catch {
    // First run or a malformed old file uses the product default below.
  }
  return { version: 1, mode: "weft" }
}

export async function writeHostPreferences(
  preferences: HostPreferences,
  filePath = defaultPreferencesPath(),
): Promise<void> {
  const directory = path.dirname(filePath)
  await mkdir(directory, { recursive: true, mode: 0o700 })
  const temporary = `${filePath}.${process.pid}.tmp`
  await writeFile(temporary, `${JSON.stringify(preferences, null, 2)}\n`, { mode: 0o600 })
  await rename(temporary, filePath)
}
