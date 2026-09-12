import { execFile } from "node:child_process"
import { promisify } from "node:util"

const execFileAsync = promisify(execFile)

const PICK_REPOSITORIES_SCRIPT = `
set selectedFolders to choose folder with prompt "Choose one or more Git repositories" with multiple selections allowed
set output to ""
repeat with selectedFolder in selectedFolders
  set output to output & POSIX path of selectedFolder & linefeed
end repeat
return output
`

export function parseSelectedDirectories(output: string): string[] {
  const seen = new Set<string>()
  const paths: string[] = []
  for (const line of output.split(/\r?\n/)) {
    const selected = line.trim().replace(/\/$/, "")
    if (!selected.startsWith("/") || seen.has(selected)) continue
    seen.add(selected)
    paths.push(selected)
    if (paths.length === 64) break
  }
  return paths
}
export async function pickRepositoryDirectories(): Promise<string[]> {
  if (process.platform !== "darwin") {
    throw new Error("Native repository selection is currently available on macOS only")
  }
  try {
    const { stdout } = await execFileAsync("/usr/bin/osascript", ["-e", PICK_REPOSITORIES_SCRIPT], {
      timeout: 10 * 60 * 1000,
      maxBuffer: 1024 * 1024,
    })
    return parseSelectedDirectories(stdout)
  } catch (error) {
    const detail = error && typeof error === "object"
      ? `${String((error as { message?: unknown }).message ?? "")} ${String((error as { stderr?: unknown }).stderr ?? "")}`
      : String(error)
    if (/user canceled|\(-128\)/i.test(detail)) return []
    throw new Error(`Repository picker failed: ${detail.trim() || "unknown error"}`)
  }
}
