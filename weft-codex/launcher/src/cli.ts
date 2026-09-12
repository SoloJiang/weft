#!/usr/bin/env node

import type { ChildProcess } from "node:child_process"
import { constants as fsConstants } from "node:fs"
import { access } from "node:fs/promises"
import path from "node:path"

import { hostCommand } from "./arguments.js"
import { inspectCodexInstall } from "./codex-install.js"
import { CdpSession, listCdpTargets, selectRendererTarget } from "./cdp.js"
import { pickRepositoryDirectories } from "./native-dialogs.js"
import { readHostPreferences, writeHostPreferences } from "./preferences.js"
import {
  defaultProfileDir,
  defaultSkillsDir,
  defaultWebDir,
  defaultWeftdPath,
  ensureWeftd,
  reserveLoopbackPort,
  resolveRuntimeLayout,
  spawnCodex,
  stopProcess,
  waitForHttp,
  type SpawnedProcess,
} from "./processes.js"
import { probeRenderer } from "./probes.js"
import { RendererSupervisor, type RendererHostEvent, type RendererReadySnapshot } from "./renderer-host.js"
import type { HostMode } from "./renderer-agent.js"
import {
  defaultCodexSkillsDir,
  ensureManagedSkills,
  formatSkillSyncSummary,
  type SkillSyncResult,
} from "./skills.js"

const HOST_VERSION = "0.1.1"

function option(name: string): string | undefined {
  const prefix = `--${name}=`
  return process.argv.find((argument) => argument.startsWith(prefix))?.slice(prefix.length)
}

function flag(name: string): boolean {
  return process.argv.includes(`--${name}`)
}

function positivePort(value: string | undefined): number | null {
  if (!value) return null
  const parsed = Number(value)
  if (!Number.isInteger(parsed) || parsed < 1 || parsed > 65535) {
    throw new Error(`Invalid port: ${value}`)
  }
  return parsed
}

function selectedMode(value: string | undefined): HostMode | null {
  if (!value) return null
  if (value === "work" || value === "codex" || value === "weft") return value
  throw new Error(`Invalid mode: ${value}`)
}

async function inspectInstall(): Promise<void> {
  const install = await inspectCodexInstall(option("app-path"))
  process.stdout.write(`${JSON.stringify(install, null, 2)}\n`)
}

interface DoctorCheck {
  id: string
  ok: boolean
  detail: string
}

async function pathCheck(id: string, candidate: string, mode: number): Promise<DoctorCheck> {
  try {
    await access(candidate, mode)
    return { id, ok: true, detail: candidate }
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error)
    return { id, ok: false, detail: `${candidate}: ${message}` }
  }
}

function runtimeCheck(): DoctorCheck {
  const bunVersion = (process.versions as NodeJS.ProcessVersions & { bun?: string }).bun
  if (bunVersion) return { id: "runtime", ok: true, detail: `Bun ${bunVersion} compiled host` }
  const major = Number(process.versions.node.split(".")[0])
  return {
    id: "runtime",
    ok: Number.isInteger(major) && major >= 22,
    detail: `Node ${process.versions.node}; Node 22 or newer is required`,
  }
}

async function syncSkills(force = false): Promise<SkillSyncResult> {
  const skillsDir = option("skills-dir") ?? defaultSkillsDir()
  const targetDir = option("codex-skills-dir") ?? defaultCodexSkillsDir(option("codex-home"))
  return ensureManagedSkills({
    sourceDir: skillsDir,
    targetDir,
    force,
  })
}

async function installSkillsCommand(): Promise<void> {
  const result = await syncSkills(flag("force"))
  process.stdout.write(`${JSON.stringify(result, null, 2)}\n`)
  if (result.skipped.length > 0) process.exitCode = 3
}

async function doctor(): Promise<void> {
  const layout = resolveRuntimeLayout({ runtimeRoot: process.env.WEFT_CODEX_RUNTIME_ROOT })
  const weftdPath = option("weftd-path") ?? defaultWeftdPath()
  const webDir = option("web-dir") ?? defaultWebDir()
  const skillsDir = option("skills-dir") ?? defaultSkillsDir()
  const checks: DoctorCheck[] = [
    runtimeCheck(),
    await pathCheck("weftd", weftdPath, fsConstants.X_OK),
    await pathCheck("web", path.join(webDir, "index.html"), fsConstants.R_OK),
    await pathCheck("skills", path.join(skillsDir, "weft-derive-test-cases", "SKILL.md"), fsConstants.R_OK),
  ]
  let skills: SkillSyncResult | null = null
  try {
    skills = await syncSkills(false)
    const skipped = skills.skipped.map((entry) => entry.name).join(", ")
    checks.push({
      id: "codex-skills",
      ok: skills.skipped.length === 0,
      detail: skipped
        ? `${formatSkillSyncSummary(skills)}; local overrides: ${skipped}`
        : formatSkillSyncSummary(skills),
    })
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error)
    checks.push({ id: "codex-skills", ok: false, detail: message })
  }
  try {
    const install = await inspectCodexInstall(option("app-path"))
    checks.push({
      id: "codex",
      ok: install.bundleId === "com.openai.codex",
      detail: `${install.appPath} ${install.version} (${install.build}) · ${install.bundleId}`,
    })
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error)
    checks.push({ id: "codex", ok: false, detail: message })
  }
  const ok = checks.every((check) => check.ok)
  process.stdout.write(`${JSON.stringify({
    ok,
    version: HOST_VERSION,
    layout,
    skills,
    checks,
  }, null, 2)}\n`)
  if (!ok) process.exitCode = 1
}

function help(): void {
  process.stdout.write(`weft-codex ${HOST_VERSION}\n\n`)
  process.stdout.write("Usage: weft-codex [start|doctor|install-skills|inspect-install|probe|attach] [options]\n")
  process.stdout.write("Running without a command starts a managed Codex instance in Weft mode.\n")
  process.stdout.write("Use --safe-mode to launch the isolated official Codex without weftd or UI injection.\n")
  process.stdout.write("Product skills sync into $CODEX_HOME/skills on doctor/start/attach and refresh on upgrades.\n")
}

async function probe(): Promise<void> {
  const endpoint = option("endpoint") ?? "http://127.0.0.1:9222"
  const install = await inspectCodexInstall(option("app-path"))
  const targets = await listCdpTargets(endpoint)
  const target = selectRendererTarget(targets, option("target-url"))
  if (!target.webSocketDebuggerUrl) throw new Error("Selected renderer has no debugger URL")
  const session = await CdpSession.connect(target.webSocketDebuggerUrl)
  try {
    const report = await probeRenderer(session)
    process.stdout.write(`${JSON.stringify({ install, target: publicTarget(target), ...report }, null, 2)}\n`)
  } finally {
    session.close()
  }
}

function publicTarget(target: { id: string; title: string; url: string }) {
  return { id: target.id, title: target.title, url: target.url }
}

function eventLogger(event: RendererHostEvent): void {
  if (event.type === "mode.changed" || event.type === "thread.open.missing") {
    process.stderr.write(`[weft-codex] ${event.type} ${JSON.stringify(event)}\n`)
  }
}

function readyLogger(snapshot: RendererReadySnapshot): void {
  if (snapshot.safeMode) {
    process.stderr.write(`[weft-codex] safe mode: ${snapshot.reason ?? "unknown compatibility failure"}\n`)
    return
  }
  process.stderr.write(
    `[weft-codex] renderer ready: tier=${snapshot.probe.tier} mode=${snapshot.status?.mode ?? "unknown"} cspBypass=${snapshot.status?.cspBypass ?? false}\n`,
  )
}

async function runHost(endpoint: string, ownedCodex: SpawnedProcess | null): Promise<void> {
  const weftdUrl = option("weftd-url") ?? "http://127.0.0.1:47810"
  const webBaseUrl = option("web-url") ?? `${weftdUrl.replace(/\/$/, "")}/`
  const preferencesPath = option("preferences")
  const preferences = await readHostPreferences(preferencesPath)
  const initialMode = selectedMode(option("mode")) ?? preferences.mode
  let preferenceWrite = Promise.resolve()

  const supervisor = new RendererSupervisor({
    endpoint,
    targetUrl: option("target-url") ?? "app://-/index.html",
    webBaseUrl,
    initialMode,
    onEvent(event) {
      eventLogger(event)
      if (event.type !== "mode.changed" || !event.mode) return
      preferenceWrite = preferenceWrite
        .then(() => writeHostPreferences({ version: 1, mode: event.mode as HostMode }, preferencesPath))
        .catch((error: unknown) => {
          const message = error instanceof Error ? error.message : String(error)
          process.stderr.write(`[weft-codex] preference write failed: ${message}\n`)
        })
    },
    onReady: readyLogger,
    onWarning(message) {
      process.stderr.write(`[weft-codex] ${message}\n`)
    },
    pickRepositories: pickRepositoryDirectories,
  })

  const snapshot = await supervisor.start()
  process.stdout.write(`${JSON.stringify({ endpoint, webBaseUrl, ...snapshot }, null, 2)}\n`)
  if (flag("once")) {
    await supervisor.stop()
    return
  }

  try {
    await waitForShutdown(ownedCodex?.child)
  } finally {
    await supervisor.stop()
    await preferenceWrite
  }
}

async function ensureSkillsBestEffort(): Promise<void> {
  try {
    const result = await syncSkills(false)
    process.stderr.write(`[weft-codex] ${formatSkillSyncSummary(result)}\n`)
    for (const entry of result.skipped) {
      process.stderr.write(`[weft-codex] skill skipped: ${entry.name}${entry.reason ? ` (${entry.reason})` : ""}\n`)
    }
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error)
    process.stderr.write(`[weft-codex] skill sync failed: ${message}\n`)
  }
}

async function attach(): Promise<void> {
  const endpoint = option("endpoint") ?? "http://127.0.0.1:9222"
  await ensureSkillsBestEffort()
  const daemon = await ensureWeftd({
    url: option("weftd-url"),
    executablePath: option("weftd-path"),
    webDir: option("web-dir"),
    homeDir: option("weft-home"),
  })
  try {
    await waitForHttp(new URL("/json/version", endpoint).href)
    await runHost(endpoint, null)
  } finally {
    await stopProcess(daemon)
  }
}

async function start(): Promise<void> {
  const install = await inspectCodexInstall(option("app-path"))
  const safeMode = flag("safe-mode")
  if (!safeMode) await ensureSkillsBestEffort()
  const daemon = safeMode
    ? null
    : await ensureWeftd({
      url: option("weftd-url"),
      executablePath: option("weftd-path"),
      webDir: option("web-dir"),
      homeDir: option("weft-home"),
    })
  let codex: SpawnedProcess | null = null
  try {
    const debugPort = positivePort(option("debug-port")) ?? await reserveLoopbackPort()
    codex = await spawnCodex(install, option("profile-dir") ?? defaultProfileDir(), debugPort)
    const endpoint = `http://127.0.0.1:${debugPort}`
    await waitForHttp(new URL("/json/version", endpoint).href, 30_000)
    if (safeMode) {
      process.stdout.write(`${JSON.stringify({ endpoint, safeMode: true }, null, 2)}\n`)
      await waitForShutdown(codex.child)
      return
    }
    await runHost(endpoint, codex)
  } finally {
    await stopProcess(codex)
    await stopProcess(daemon)
  }
}

function waitForShutdown(child?: ChildProcess): Promise<void> {
  return new Promise<void>((resolve) => {
    let finished = false
    const done = () => {
      if (finished) return
      finished = true
      process.off("SIGINT", done)
      process.off("SIGTERM", done)
      child?.off("exit", done)
      resolve()
    }
    process.once("SIGINT", done)
    process.once("SIGTERM", done)
    child?.once("exit", done)
  })
}

async function main(): Promise<void> {
  const command = hostCommand(process.argv)
  if (command === "--version" || command === "version") {
    process.stdout.write(`${HOST_VERSION}\n`)
    return
  }
  if (command === "--help" || command === "help") {
    help()
    return
  }
  if (command === "doctor") {
    await doctor()
    return
  }
  if (command === "install-skills") {
    await installSkillsCommand()
    return
  }
  if (command === "inspect-install") {
    await inspectInstall()
    return
  }
  if (command === "probe") {
    await probe()
    return
  }
  if (command === "attach") {
    await attach()
    return
  }
  if (command === "start") {
    await start()
    return
  }
  throw new Error(`Unknown command: ${command}`)
}

main().catch((error: unknown) => {
  const message = error instanceof Error ? error.message : String(error)
  process.stderr.write(`weft-codex: ${message}\n`)
  process.exitCode = 1
})
