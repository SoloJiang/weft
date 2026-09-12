import { execFile } from "node:child_process"
import { access } from "node:fs/promises"
import { promisify } from "node:util"

const execFileAsync = promisify(execFile)

export const DEFAULT_APP_PATH = "/Applications/ChatGPT.app"

export interface CodexInstall {
  appPath: string
  bundleId: string
  displayName: string
  version: string
  build: string
  executablePath: string
  resourcesPath: string
}

async function plistValue(plistPath: string, key: string): Promise<string> {
  const { stdout } = await execFileAsync("/usr/bin/plutil", ["-extract", key, "raw", "-o", "-", plistPath])
  return stdout.trim()
}

export async function inspectCodexInstall(appPath = DEFAULT_APP_PATH): Promise<CodexInstall> {
  const plistPath = `${appPath}/Contents/Info.plist`
  await access(plistPath)
  const [bundleId, bundleName, version, build, executable] = await Promise.all([
    plistValue(plistPath, "CFBundleIdentifier"),
    plistValue(plistPath, "CFBundleName"),
    plistValue(plistPath, "CFBundleShortVersionString"),
    plistValue(plistPath, "CFBundleVersion"),
    plistValue(plistPath, "CFBundleExecutable"),
  ])
  return {
    appPath,
    bundleId,
    displayName: bundleName,
    version,
    build,
    executablePath: `${appPath}/Contents/MacOS/${executable}`,
    resourcesPath: `${appPath}/Contents/Resources`,
  }
}
