import { CdpSession, listCdpTargets, selectRendererTarget, type CdpTarget } from "./cdp.js"
import { probeRenderer, type CompatibilityTier, type ProbeReport } from "./probes.js"
import {
  buildRendererAgentSource,
  type HostMode,
  type RendererAgentConfig,
} from "./renderer-agent.js"

interface AddScriptResult {
  identifier: string
}

interface RuntimeEvaluateResult {
  result?: { value?: unknown; description?: string }
  exceptionDetails?: unknown
}

interface BindingCalledEvent {
  name?: unknown
  payload?: unknown
}

export interface RendererAgentStatus {
  version: 1
  mode: HostMode
  view: "workspace" | "thread"
  tier: "weft-mode"
  cspBypass: boolean
  uiMounted: boolean
  uiReady: boolean
  nativeModeSwitcher: boolean
}

export interface RendererHostEvent {
  version: 1
  type:
    | "agent.ready"
    | "ui.ready"
    | "ui.error"
    | "mode.changed"
    | "thread.open.missing"
    | "repositories.pick"
  mode?: HostMode
  surface?: string
  threadId?: string
  tier?: string
  requestId?: string
}

export interface RendererHostOptions {
  endpoint: string
  targetUrl: string
  webBaseUrl: string
  initialMode: HostMode
  onEvent?(event: RendererHostEvent): void
  onReady?(snapshot: RendererReadySnapshot): void
  onWarning?(message: string): void
  pickRepositories?(): Promise<string[]>
}

export interface RendererReadySnapshot {
  target: Pick<CdpTarget, "id" | "title" | "url">
  probe: ProbeReport
  status: RendererAgentStatus | null
  safeMode: boolean
  reason?: string
}

function isHostMode(value: unknown): value is HostMode {
  return value === "work" || value === "codex" || value === "weft"
}

function parseHostEvent(value: unknown): RendererHostEvent | null {
  if (!value || typeof value !== "object" || Array.isArray(value)) return null
  const candidate = value as Record<string, unknown>
  if (candidate.version !== 1 || typeof candidate.type !== "string") return null
  const allowed = new Set([
    "agent.ready",
    "ui.ready",
    "ui.error",
    "mode.changed",
    "thread.open.missing",
    "repositories.pick",
  ])
  if (!allowed.has(candidate.type)) return null
  if (candidate.mode !== undefined && !isHostMode(candidate.mode)) return null
  if (candidate.surface !== undefined && typeof candidate.surface !== "string") return null
  if (candidate.threadId !== undefined && typeof candidate.threadId !== "string") return null
  if (candidate.tier !== undefined && typeof candidate.tier !== "string") return null
  if (candidate.requestId !== undefined && typeof candidate.requestId !== "string") return null
  return candidate as unknown as RendererHostEvent
}

function parseAgentStatus(value: unknown): RendererAgentStatus | null {
  if (!value || typeof value !== "object" || Array.isArray(value)) return null
  const candidate = value as Partial<RendererAgentStatus>
  if (candidate.version !== 1 || !isHostMode(candidate.mode)) return null
  if (candidate.view !== "workspace" && candidate.view !== "thread") return null
  if (candidate.tier !== "weft-mode") return null
  const booleans = [
    candidate.cspBypass,
    candidate.uiMounted,
    candidate.uiReady,
    candidate.nativeModeSwitcher,
  ]
  if (booleans.some((value) => typeof value !== "boolean")) return null
  return candidate as RendererAgentStatus
}

function canInject(tier: CompatibilityTier): boolean {
  return tier === "weft-mode"
}

async function evaluateValue(session: CdpSession, expression: string): Promise<unknown> {
  const response = await session.send<RuntimeEvaluateResult>("Runtime.evaluate", {
    expression,
    returnByValue: true,
    awaitPromise: true,
  })
  if (response.exceptionDetails) {
    throw new Error(response.result?.description || "Renderer evaluation failed")
  }
  return response.result?.value
}

async function agentStatus(session: CdpSession): Promise<RendererAgentStatus | null> {
  const value = await evaluateValue(
    session,
    `window.__weftCodexAgentV1 && window.__weftCodexAgentV1.status
      ? window.__weftCodexAgentV1.status()
      : null`,
  )
  return parseAgentStatus(value)
}

function surfacesMounted(status: RendererAgentStatus | null): boolean {
  return Boolean(status?.uiMounted)
}

function surfacesReady(status: RendererAgentStatus | null): boolean {
  return Boolean(status?.uiReady)
}

/**
 * Readiness is mode-dependent.
 *
 * Weft has to handshake the shell. Work and Codex must not — they never
 * mount it — and treating a missing shell as failure used to dispose the
 * agent, which took the mode menu's third item with it (08-16 spec §4).
 */
export function installReady(wantsShell: boolean, status: RendererAgentStatus | null): boolean {
  if (wantsShell) return surfacesReady(status)
  return status !== null
}

/**
 * Name the phase that actually failed.
 *
 * A generic "handshake" used to be reported even when the roots never
 * attached, which points debugging at the bundle when the host shell is what
 * never arrived.
 */
export function attachFailureReason(wantsShell: boolean, status: RendererAgentStatus | null): string {
  if (!wantsShell) return "The Weft agent did not attach to the host document"
  if (surfacesMounted(status)) return "Weft UI did not finish mounting into the host document"
  return "Codex did not render its shell in time for the Weft surfaces to mount"
}

/**
 * Wait for the agent itself, without waiting for a Weft shell.
 *
 * In Work or Codex the agent never mounts the shell — `ensureWeftMounted`
 * returns immediately unless the mode is Weft — so the only thing to wait for
 * is the agent answering at all. That is what keeps the third item in the
 * native mode menu alive, which is the whole point of running in those modes
 * (08-16 spec §4).
 */
export async function waitForAgent(
  readStatus: () => Promise<RendererAgentStatus | null>,
  budgetMs: number,
): Promise<RendererAgentStatus | null> {
  const deadline = Date.now() + budgetMs
  let latest: RendererAgentStatus | null = null
  while (Date.now() < deadline) {
    try {
      latest = await readStatus()
    } catch {
      // A renderer reload briefly has no execution context.
    }
    if (latest) return latest
    await new Promise((resolve) => setTimeout(resolve, 120))
  }
  return latest
}

/**
 * Wait for the injected surfaces, in two phases with separate budgets.
 *
 * Mounting and handshaking fail for unrelated reasons and on unrelated clocks.
 * The roots can only attach once Codex has rendered its own shell — the sidebar
 * anchor does not exist before then — and that is host hydration, which we do
 * not control and which is slowest exactly when the machine is busiest. The
 * second phase, by contrast, is our own Weft bundle loading over loopback:
 * once the roots are attached it either completes quickly or something is
 * genuinely wrong.
 *
 * Spending one budget on both meant a slow host shell consumed the whole window
 * and the run was reported as a bundle-load failure — the wrong diagnosis, and
 * it sent debugging at the assets instead of at the wait.
 */
export async function waitForSurfaces(
  readStatus: () => Promise<RendererAgentStatus | null>,
  budget: { mountMs: number; handshakeMs: number },
): Promise<RendererAgentStatus | null> {
  let latest: RendererAgentStatus | null = null
  const poll = async () => {
    try {
      latest = await readStatus()
    } catch {
      // A renderer reload briefly has no execution context. Keep the last
      // status and wait for the document-start agent in the new document.
    }
  }

  const mountDeadline = Date.now() + budget.mountMs
  while (Date.now() < mountDeadline) {
    await poll()
    // A run that handshakes before we notice it mounted is still a success.
    if (surfacesReady(latest)) return latest
    if (surfacesMounted(latest)) break
    await new Promise((resolve) => setTimeout(resolve, 120))
  }

  // The handshake budget starts when the roots attach, so host hydration never
  // eats into it.
  const handshakeDeadline = Date.now() + budget.handshakeMs
  while (Date.now() < handshakeDeadline) {
    if (surfacesReady(latest)) return latest
    await new Promise((resolve) => setTimeout(resolve, 120))
    await poll()
  }
  return latest
}

interface ClickPoint {
  x: number
  y: number
  current?: string
}

function clickPoint(value: unknown): ClickPoint | null {
  if (!value || typeof value !== "object" || Array.isArray(value)) return null
  const candidate = value as Partial<ClickPoint>
  if (typeof candidate.x !== "number" || typeof candidate.y !== "number") return null
  if (candidate.current !== undefined && typeof candidate.current !== "string") return null
  return candidate as ClickPoint
}

async function dispatchClick(session: CdpSession, point: ClickPoint): Promise<void> {
  await session.send("Input.dispatchMouseEvent", {
    type: "mouseMoved",
    x: point.x,
    y: point.y,
    button: "none",
  })
  await session.send("Input.dispatchMouseEvent", {
    type: "mousePressed",
    x: point.x,
    y: point.y,
    button: "left",
    clickCount: 1,
  })
  await session.send("Input.dispatchMouseEvent", {
    type: "mouseReleased",
    x: point.x,
    y: point.y,
    button: "left",
    clickCount: 1,
  })
}

async function ensureNativeCodexMode(session: CdpSession): Promise<boolean> {
  const trigger = clickPoint(await evaluateValue(session, `(() => {
    const sidebar = document.querySelector("[data-app-action-sidebar-scroll]");
    const navigation = sidebar && sidebar.closest("nav");
    if (!sidebar || !navigation) return null;
    const buttons = [...navigation.querySelectorAll('button[aria-haspopup="menu"][aria-expanded][data-state]')]
      .filter((button) => !sidebar.contains(button));
    if (buttons.length !== 1) return null;
    const button = buttons[0];
    const rect = button.getBoundingClientRect();
    return { x: rect.x + rect.width / 2, y: rect.y + rect.height / 2, current: button.textContent || "" };
  })()`))
  if (!trigger) return false
  if (/codex/i.test(trigger.current ?? "")) return true
  await dispatchClick(session, trigger)
  await new Promise((resolve) => setTimeout(resolve, 180))

  const codexItem = clickPoint(await evaluateValue(session, `(() => {
    const sidebar = document.querySelector("[data-app-action-sidebar-scroll]");
    const navigation = sidebar && sidebar.closest("nav");
    const button = navigation
      ? [...navigation.querySelectorAll('button[aria-haspopup="menu"][aria-expanded][data-state]')]
          .filter((candidate) => !sidebar.contains(candidate))[0]
      : null;
    if (!button || !button.id) return null;
    const menu = [...document.querySelectorAll('[role="menu"]')]
      .find((candidate) => candidate.getClientRects().length && candidate.getAttribute("aria-labelledby") === button.id);
    const item = menu
      ? [...menu.querySelectorAll('[role="menuitem"]')]
          .find((candidate) => candidate.getClientRects().length && /^Codex/i.test(candidate.textContent || ""))
      : null;
    if (!item) return null;
    const rect = item.getBoundingClientRect();
    return { x: rect.x + rect.width / 2, y: rect.y + rect.height / 2 };
  })()`))
  if (!codexItem) return false
  await dispatchClick(session, codexItem)

  const deadline = Date.now() + 3500
  while (Date.now() < deadline) {
    try {
      const current = await evaluateValue(session, `(() => {
        const sidebar = document.querySelector("[data-app-action-sidebar-scroll]");
        const navigation = sidebar && sidebar.closest("nav");
        const button = navigation
          ? [...navigation.querySelectorAll('button[aria-haspopup="menu"][aria-expanded][data-state]')]
              .filter((candidate) => !sidebar.contains(candidate))[0]
          : null;
        return button ? button.textContent || "" : "";
      })()`)
      if (typeof current === "string" && /codex/i.test(current)) return true
    } catch {
      // The native mode switch may replace the renderer execution context.
    }
    await new Promise((resolve) => setTimeout(resolve, 120))
  }
  return false
}

class AttachedRenderer {
  readonly target: CdpTarget
  readonly probe: ProbeReport
  readonly session: CdpSession
  private scriptIdentifier: string | null = null
  private removeBindingListener: (() => void) | null = null
  private cspBypass = false

  private constructor(target: CdpTarget, probe: ProbeReport, session: CdpSession) {
    this.target = target
    this.probe = probe
    this.session = session
  }

  static async connect(target: CdpTarget): Promise<AttachedRenderer> {
    if (!target.webSocketDebuggerUrl) throw new Error("Selected renderer has no debugger URL")
    const session = await CdpSession.connect(target.webSocketDebuggerUrl)
    let probe = await probeRenderer(session)
    const hydrationDeadline = Date.now() + 20_000
    // Keep retrying below weft-mode, not just at safe-mode. Codex paints the
    // sidebar shell before it fills the conversation list, and after the CSP
    // reload it does so again — a probe landing in that window used to give up
    // with no retry, because the loop only looked for safe-mode. A genuinely
    // missing anchor still settles at safe-mode once the deadline passes.
    while (probe.tier !== "weft-mode" && Date.now() < hydrationDeadline) {
      await new Promise((resolve) => setTimeout(resolve, 150))
      if (session.isClosed()) break
      try {
        probe = await probeRenderer(session)
      } catch {
        // A native startup route may replace the execution context once.
      }
    }
    return new AttachedRenderer(target, probe, session)
  }

  async install(
    options: RendererHostOptions,
    bindingName: string,
  ): Promise<RendererReadySnapshot> {
    if (!canInject(this.probe.tier)) {
      return {
        target: this.publicTarget(),
        probe: this.probe,
        status: null,
        safeMode: true,
        reason: "Required renderer capabilities are unavailable",
      }
    }

    await Promise.all([
      this.session.send("Page.enable"),
      this.session.send("Runtime.enable"),
    ])
    if (options.initialMode === "weft") {
      const nativeCodexReady = await ensureNativeCodexMode(this.session)
      if (!nativeCodexReady) {
        return {
          target: this.publicTarget(),
          probe: this.probe,
          status: null,
          safeMode: true,
          reason: "Could not establish Codex as the native base for Weft mode",
        }
      }
    }
    await this.session.send("Runtime.addBinding", { name: bindingName })
    this.removeBindingListener = this.session.on("Runtime.bindingCalled", (params) => {
      const called = params as BindingCalledEvent
      if (called.name !== bindingName || typeof called.payload !== "string") return
      let parsed: unknown
      try {
        parsed = JSON.parse(called.payload)
      } catch {
        return
      }
      const event = parseHostEvent(parsed)
      if (!event) return
      options.onEvent?.(event)
      if (event.type === "repositories.pick" && event.requestId) {
        void this.handleRepositoryPicker(event.requestId, options)
      }
    })

    await this.installAgentScript({
      webBaseUrl: options.webBaseUrl,
      bindingName,
      initialMode: options.initialMode,
      cspBypass: false,
    })

    // Work and Codex do not put Weft on stage, so there is no shell to wait
    // for. Waiting anyway spent both budgets on something that cannot happen
    // and then tore the agent down — taking the mode menu's third item with
    // it, which left no way into Weft at all.
    const wantsShell = options.initialMode === "weft"

    let status = wantsShell
      ? await waitForSurfaces(() => agentStatus(this.session), { mountMs: 4500, handshakeMs: 4500 })
      : null

    // Weft waits for the shell; if it never arrives, CSP is the usual cause
    // on 6662. Work and Codex skip that wait — there is no shell — but they
    // still take this path so the next document already has the bypass. The
    // user can then pick Weft without a reload that would drop their thread.
    const needsCspFallback = wantsShell ? !surfacesReady(status) : true
    if (needsCspFallback) {
      options.onWarning?.(wantsShell
        ? "Weft bundle could not load; enabling dedicated-instance CSP compatibility mode"
        : "Enabling dedicated-instance CSP compatibility mode so Weft can mount when chosen")
      this.cspBypass = true
      await this.session.send("Page.setBypassCSP", { enabled: true })
      await this.installAgentScript({
        webBaseUrl: options.webBaseUrl,
        bindingName,
        initialMode: status?.mode ?? options.initialMode,
        cspBypass: true,
      })
      // Chromium applies Page.setBypassCSP to subsequent document loads. The
      // current app:// document has already committed its CSP, so a dedicated
      // renderer reload is required before the Weft bundle can load.
      // A reload restarts host hydration from nothing, so the mount phase needs
      // the longer budget here — the bundle load itself is no slower than before.
      await this.session.send("Page.reload", { ignoreCache: true })
      status = wantsShell
        ? await waitForSurfaces(() => agentStatus(this.session), { mountMs: 15000, handshakeMs: 8000 })
        : await waitForAgent(() => agentStatus(this.session), 15000)
    }

    if (!installReady(wantsShell, status)) {
      await this.disposeAgent()
      return {
        target: this.publicTarget(),
        probe: this.probe,
        status,
        safeMode: true,
        reason: attachFailureReason(wantsShell, status),
      }
    }

    return {
      target: this.publicTarget(),
      probe: this.probe,
      status,
      safeMode: false,
    }
  }

  isClosed(): boolean {
    return this.session.isClosed()
  }

  async dispose(): Promise<void> {
    this.removeBindingListener?.()
    this.removeBindingListener = null
    await this.disposeAgent().catch(() => undefined)
    this.session.close()
  }

  private async installAgentScript(config: RendererAgentConfig): Promise<void> {
    if (this.scriptIdentifier) {
      await this.session.send("Page.removeScriptToEvaluateOnNewDocument", {
        identifier: this.scriptIdentifier,
      }).catch(() => undefined)
    }
    const source = buildRendererAgentSource(config)
    const added = await this.session.send<AddScriptResult>("Page.addScriptToEvaluateOnNewDocument", { source })
    this.scriptIdentifier = added.identifier
    await evaluateValue(this.session, source)
  }

  private async handleRepositoryPicker(requestId: string, options: RendererHostOptions): Promise<void> {
    try {
      if (!options.pickRepositories) throw new Error("Native repository picker is unavailable")
      const paths = await options.pickRepositories()
      await this.deliverActionResult(requestId, { ok: true, result: { paths } })
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error)
      await this.deliverActionResult(requestId, { ok: false, error: message }).catch(() => undefined)
    }
  }

  private async deliverActionResult(requestId: string, response: Record<string, unknown>): Promise<void> {
    const serializedRequestId = JSON.stringify(requestId)
    const serializedResponse = JSON.stringify(response).replaceAll("<", "\\u003c")
    await evaluateValue(
      this.session,
      `window.__weftCodexAgentV1 && window.__weftCodexAgentV1.deliverActionResult
        ? window.__weftCodexAgentV1.deliverActionResult(${serializedRequestId}, ${serializedResponse})
        : false`,
    )
  }

  private async disposeAgent(): Promise<void> {
    if (!this.session.isClosed()) {
      await evaluateValue(
        this.session,
        `window.__weftCodexAgentV1 && window.__weftCodexAgentV1.dispose
          ? window.__weftCodexAgentV1.dispose()
          : null`,
      ).catch(() => undefined)
      if (this.scriptIdentifier) {
        await this.session.send("Page.removeScriptToEvaluateOnNewDocument", {
          identifier: this.scriptIdentifier,
        }).catch(() => undefined)
      }
      if (this.cspBypass) {
        await this.session.send("Page.setBypassCSP", { enabled: false }).catch(() => undefined)
        // CSP enforcement is fixed when a document commits. Reload after
        // disabling bypass so an attached dedicated instance is not left in a
        // privileged document after the host exits.
        await this.session.send("Page.reload", { ignoreCache: true }).catch(() => undefined)
      }
    }
    this.scriptIdentifier = null
  }

  private publicTarget(): Pick<CdpTarget, "id" | "title" | "url"> {
    return { id: this.target.id, title: this.target.title, url: this.target.url }
  }
}

export class RendererSupervisor {
  private attached: AttachedRenderer | null = null
  private timer: ReturnType<typeof setInterval> | null = null
  private reconciling = false
  private stopped = false
  private readonly bindingName = "weftCodexHost"

  constructor(private readonly options: RendererHostOptions) {}

  async start(): Promise<RendererReadySnapshot> {
    const deadline = Date.now() + 30_000
    let snapshot: RendererReadySnapshot | null = null
    let lastError: unknown = new Error("Renderer target is not ready")
    while (!snapshot && Date.now() < deadline) {
      try {
        snapshot = await this.reconcile(true)
      } catch (error) {
        lastError = error
      }
      if (!snapshot) await new Promise((resolve) => setTimeout(resolve, 150))
    }
    if (!snapshot) {
      const detail = lastError instanceof Error ? lastError.message : String(lastError)
      throw new Error(`Timed out waiting for the Codex renderer: ${detail}`)
    }
    this.timer = setInterval(() => {
      void this.reconcile(false).catch((error: unknown) => {
        const message = error instanceof Error ? error.message : String(error)
        this.options.onWarning?.(`Renderer reconnect failed: ${message}`)
      })
    }, 1000)
    return snapshot
  }

  async stop(): Promise<void> {
    this.stopped = true
    if (this.timer) clearInterval(this.timer)
    this.timer = null
    const attached = this.attached
    this.attached = null
    if (attached) await attached.dispose()
  }

  private async reconcile(required: boolean): Promise<RendererReadySnapshot | null> {
    if (this.stopped || this.reconciling) return null
    this.reconciling = true
    try {
      const targets = await listCdpTargets(this.options.endpoint)
      const target = selectRendererTarget(targets, this.options.targetUrl)
      const current = this.attached
      if (current && !current.isClosed() && current.target.id === target.id) return null
      if (current) await current.dispose()
      const next = await AttachedRenderer.connect(target)
      this.attached = next
      const snapshot = await next.install(this.options, this.bindingName)
      this.options.onReady?.(snapshot)
      return snapshot
    } catch (error) {
      if (required) throw error
      return null
    } finally {
      this.reconciling = false
    }
  }
}
