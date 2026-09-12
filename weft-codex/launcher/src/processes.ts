import { spawn, type ChildProcess } from "node:child_process"
import { existsSync } from "node:fs"
import { access, mkdir } from "node:fs/promises"
import { createServer } from "node:net"
import { homedir } from "node:os"
import path from "node:path"
import { fileURLToPath } from "node:url"

import type { CodexInstall } from "./codex-install.js"

const DEFAULT_WEFTD_URL = "http://127.0.0.1:47810"

export interface SpawnedProcess {
  child: ChildProcess
  owned: boolean
  label: string
}

export interface WeftdOptions {
  url?: string | undefined
  executablePath?: string | undefined
  webDir?: string | undefined
  homeDir?: string | undefined
}

export interface RuntimeLayout {
  weftdPath: string
  webDir: string
  skillsDir: string
  source: "override" | "bundle" | "source"
}

interface RuntimeLayoutOptions {
  executablePath?: string | undefined
  moduleUrl?: string | undefined
  runtimeRoot?: string | undefined
  exists?: ((candidate: string) => boolean) | undefined
}

export function defaultProfileDir(): string {
  return path.join(homedir(), ".weft-codex", "desktop-profile")
}

function layoutAt(root: string, source: RuntimeLayout["source"]): RuntimeLayout {
  return {
    weftdPath: path.join(root, "bin", "weftd"),
    webDir: path.join(root, "share", "weft-codex", "web"),
    skillsDir: path.join(root, "share", "weft-codex", "skills"),
    source,
  }
}

/** Resolve colocated runtime files before falling back to the source checkout. */
export function resolveRuntimeLayout(options: RuntimeLayoutOptions = {}): RuntimeLayout {
  const exists = options.exists ?? existsSync
  const executablePath = options.executablePath ?? process.execPath
  const moduleUrl = options.moduleUrl ?? import.meta.url

  if (options.runtimeRoot) {
    return layoutAt(path.resolve(options.runtimeRoot), "override")
  }

  const archiveRoot = path.dirname(path.dirname(executablePath))
  const archiveLayout = layoutAt(archiveRoot, "bundle")
  if (exists(archiveLayout.weftdPath) && exists(path.join(archiveLayout.webDir, "index.html"))) {
    return archiveLayout
  }

  const modulePath = fileURLToPath(moduleUrl)
  const sourceRoot = path.resolve(path.dirname(modulePath), "../..")
  const releaseDaemon = path.join(sourceRoot, "target", "release", "weftd")
  const debugDaemon = path.join(sourceRoot, "target", "debug", "weftd")
  return {
    weftdPath: exists(debugDaemon) ? debugDaemon : releaseDaemon,
    webDir: path.join(sourceRoot, "crates", "daemon", "web"),
    skillsDir: path.join(sourceRoot, "skills"),
    source: "source",
  }
}

export function defaultWeftdPath(): string {
  return resolveRuntimeLayout({ runtimeRoot: process.env.WEFT_CODEX_RUNTIME_ROOT }).weftdPath
}

export function defaultWebDir(): string {
  return resolveRuntimeLayout({ runtimeRoot: process.env.WEFT_CODEX_RUNTIME_ROOT }).webDir
}

export function defaultSkillsDir(): string {
  return resolveRuntimeLayout({ runtimeRoot: process.env.WEFT_CODEX_RUNTIME_ROOT }).skillsDir
}

export function codexLaunchArgs(profileDir: string, debugPort: number): string[] {
  if (!Number.isInteger(debugPort) || debugPort < 1 || debugPort > 65535) {
    throw new Error("CDP port is invalid")
  }
  return [
    `--user-data-dir=${profileDir}`,
    "--remote-debugging-address=127.0.0.1",
    `--remote-debugging-port=${debugPort}`,
  ]
}

export async function reserveLoopbackPort(): Promise<number> {
  return new Promise<number>((resolve, reject) => {
    const server = createServer()
    server.unref()
    server.once("error", reject)
    server.listen(0, "127.0.0.1", () => {
      const address = server.address()
      if (!address || typeof address === "string") {
        server.close()
        reject(new Error("Could not allocate a loopback port"))
        return
      }
      const port = address.port
      server.close((error) => {
        if (error) reject(error)
        else resolve(port)
      })
    })
  })
}

function loopbackHttpUrl(value: string): URL {
  const url = new URL(value)
  const loopback = url.hostname === "127.0.0.1" || url.hostname === "localhost" || url.hostname === "[::1]"
  if (url.protocol !== "http:" || !loopback || !url.port) {
    throw new Error("weftd URL must be loopback HTTP with an explicit port")
  }
  return url
}

export async function waitForHttp(url: string, timeoutMs = 20_000): Promise<void> {
  const deadline = Date.now() + timeoutMs
  let lastError = "endpoint unavailable"
  while (Date.now() < deadline) {
    try {
      const response = await fetch(url, { signal: AbortSignal.timeout(1000) })
      if (response.ok) return
      lastError = `HTTP ${response.status}`
    } catch (error) {
      lastError = error instanceof Error ? error.message : String(error)
    }
    await new Promise((resolve) => setTimeout(resolve, 150))
  }
  throw new Error(`Timed out waiting for ${url}: ${lastError}`)
}

export async function spawnCodex(
  install: CodexInstall,
  profileDir: string,
  debugPort: number,
): Promise<SpawnedProcess> {
  await mkdir(profileDir, { recursive: true, mode: 0o700 })
  const child = spawn(install.executablePath, codexLaunchArgs(profileDir, debugPort), {
    stdio: "ignore",
    windowsHide: true,
  })
  await waitForSpawn(child, "Codex Desktop")
  return { child, owned: true, label: "Codex Desktop" }
}

export async function ensureWeftd(options: WeftdOptions = {}): Promise<SpawnedProcess | null> {
  const url = loopbackHttpUrl(options.url ?? DEFAULT_WEFTD_URL)
  try {
    await waitForHttp(new URL("/healthz", url).href, 800)
    return null
  } catch {
    // Start an owned daemon below.
  }

  const executablePath = options.executablePath ?? defaultWeftdPath()
  const webDir = options.webDir ?? defaultWebDir()
  await Promise.all([access(executablePath), access(path.join(webDir, "index.html"))])
  const env: NodeJS.ProcessEnv = {
    ...process.env,
    WEFTD_ADDR: `${url.hostname}:${url.port}`,
    WEFT_WEB_DIR: webDir,
  }
  if (options.homeDir) env.WEFT_CODEX_HOME = options.homeDir
  const child = spawn(executablePath, [], {
    env,
    stdio: ["ignore", "ignore", "pipe"],
    windowsHide: true,
  })
  child.stderr?.on("data", (chunk: Buffer) => {
    const text = chunk.toString("utf8").trimEnd()
    if (text) process.stderr.write(`${text}\n`)
  })
  await waitForSpawn(child, "weftd")
  try {
    await waitForHttp(new URL("/healthz", url).href)
  } catch (error) {
    await stopProcess({ child, owned: true, label: "weftd" })
    throw error
  }
  return { child, owned: true, label: "weftd" }
}

export async function stopProcess(processInfo: SpawnedProcess | null): Promise<void> {
  if (!processInfo?.owned || processInfo.child.exitCode !== null) return
  const child = processInfo.child
  child.kill("SIGTERM")
  const exited = await waitForExit(child, 5000)
  if (exited || child.exitCode !== null) return
  child.kill("SIGKILL")
  await waitForExit(child, 2000)
}

function waitForSpawn(child: ChildProcess, label: string): Promise<void> {
  if (child.pid) return Promise.resolve()
  return new Promise<void>((resolve, reject) => {
    const onSpawn = () => {
      cleanup()
      resolve()
    }
    const onError = (error: Error) => {
      cleanup()
      reject(new Error(`${label} failed to start: ${error.message}`))
    }
    const cleanup = () => {
      child.off("spawn", onSpawn)
      child.off("error", onError)
    }
    child.once("spawn", onSpawn)
    child.once("error", onError)
  })
}

function waitForExit(child: ChildProcess, timeoutMs: number): Promise<boolean> {
  if (child.exitCode !== null) return Promise.resolve(true)
  return new Promise<boolean>((resolve) => {
    const timeout = setTimeout(() => {
      child.off("exit", onExit)
      resolve(false)
    }, timeoutMs)
    const onExit = () => {
      clearTimeout(timeout)
      resolve(true)
    }
    child.once("exit", onExit)
  })
}
