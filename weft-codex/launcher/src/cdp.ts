export interface CdpTarget {
  id: string
  type: string
  title: string
  url: string
  webSocketDebuggerUrl?: string
}

interface CdpResponse {
  id?: number
  result?: unknown
  error?: { code: number; message: string }
  method?: string
  params?: unknown
}

interface PendingRequest {
  resolve(value: unknown): void
  reject(error: Error): void
  timeout: ReturnType<typeof setTimeout>
}

export type CdpEventListener = (params: unknown) => void

function endpointUrl(endpoint: string, path: string): URL {
  const base = endpoint.endsWith("/") ? endpoint : `${endpoint}/`
  return new URL(path.replace(/^\//, ""), base)
}

export async function listCdpTargets(endpoint: string): Promise<CdpTarget[]> {
  const response = await fetch(endpointUrl(endpoint, "/json/list"), {
    signal: AbortSignal.timeout(3000),
  })
  if (!response.ok) throw new Error(`CDP target list failed with HTTP ${response.status}`)
  const value: unknown = await response.json()
  if (!Array.isArray(value)) throw new Error("CDP target list returned an invalid payload")
  return value.filter((entry): entry is CdpTarget => {
    if (!entry || typeof entry !== "object") return false
    const candidate = entry as Partial<CdpTarget>
    return (
      typeof candidate.id === "string" &&
      typeof candidate.type === "string" &&
      typeof candidate.title === "string" &&
      typeof candidate.url === "string" &&
      (candidate.webSocketDebuggerUrl === undefined || typeof candidate.webSocketDebuggerUrl === "string")
    )
  })
}

export function selectRendererTarget(targets: CdpTarget[], urlHint?: string): CdpTarget {
  const pages = targets.filter((target) => target.type === "page" && target.webSocketDebuggerUrl)
  let selected: CdpTarget | undefined
  if (urlHint) {
    selected = pages.find((target) => target.url === urlHint)
    if (!selected) selected = pages.find((target) => target.url.includes(urlHint))
  }
  if (!selected && pages.length === 1) selected = pages[0]
  if (!selected) {
    const candidates = pages.map((target) => `${target.title} (${target.url})`).join(", ")
    throw new Error(`Could not select one renderer target. Candidates: ${candidates || "none"}`)
  }
  return selected
}

export class CdpSession {
  private nextId = 0
  private readonly pending = new Map<number, PendingRequest>()
  private readonly listeners = new Map<string, Set<CdpEventListener>>()
  private closed = false

  private constructor(private readonly socket: WebSocket) {
    socket.addEventListener("message", (event) => this.onMessage(String(event.data)))
    socket.addEventListener("close", () => {
      this.closed = true
      this.rejectPending(new Error("CDP connection closed"))
      this.emit("__closed", undefined)
    })
    socket.addEventListener("error", () => this.rejectPending(new Error("CDP connection failed")))
  }

  static async connect(webSocketUrl: string): Promise<CdpSession> {
    const socket = new WebSocket(webSocketUrl)
    await new Promise<void>((resolve, reject) => {
      const timeout = setTimeout(() => {
        socket.close()
        reject(new Error("CDP connection timed out"))
      }, 5000)
      const onOpen = () => {
        clearTimeout(timeout)
        socket.removeEventListener("error", onError)
        resolve()
      }
      const onError = () => {
        clearTimeout(timeout)
        socket.removeEventListener("open", onOpen)
        reject(new Error("CDP connection failed"))
      }
      socket.addEventListener("open", onOpen, { once: true })
      socket.addEventListener("error", onError, { once: true })
    })
    return new CdpSession(socket)
  }

  send<T>(method: string, params: Record<string, unknown> = {}): Promise<T> {
    if (this.closed || this.socket.readyState !== WebSocket.OPEN) {
      return Promise.reject(new Error("CDP connection is not open"))
    }
    this.nextId += 1
    const id = this.nextId
    return new Promise<T>((resolve, reject) => {
      const timeout = setTimeout(() => {
        if (!this.pending.delete(id)) return
        reject(new Error(`CDP ${method} timed out`))
      }, 5000)
      this.pending.set(id, {
        resolve: (value) => resolve(value as T),
        reject,
        timeout,
      })
      try {
        this.socket.send(JSON.stringify({ id, method, params }))
      } catch (error) {
        clearTimeout(timeout)
        this.pending.delete(id)
        reject(error instanceof Error ? error : new Error(String(error)))
      }
    })
  }

  on(method: string, listener: CdpEventListener): () => void {
    const listeners = this.listeners.get(method) ?? new Set<CdpEventListener>()
    listeners.add(listener)
    this.listeners.set(method, listeners)
    return () => {
      const current = this.listeners.get(method)
      if (!current) return
      current.delete(listener)
      if (!current.size) this.listeners.delete(method)
    }
  }

  isClosed(): boolean {
    return this.closed
  }

  close(): void {
    if (this.closed) return
    this.closed = true
    this.socket.close()
    this.rejectPending(new Error("CDP connection closed"))
  }

  private onMessage(payload: string): void {
    let message: CdpResponse
    try {
      message = JSON.parse(payload) as CdpResponse
    } catch {
      return
    }
    if (typeof message.id !== "number") {
      if (typeof message.method === "string") this.emit(message.method, message.params)
      return
    }
    const pending = this.pending.get(message.id)
    if (!pending) return
    this.pending.delete(message.id)
    clearTimeout(pending.timeout)
    if (message.error) {
      pending.reject(new Error(`CDP ${message.error.code}: ${message.error.message}`))
      return
    }
    pending.resolve(message.result)
  }

  private rejectPending(error: Error): void {
    for (const pending of this.pending.values()) {
      clearTimeout(pending.timeout)
      pending.reject(error)
    }
    this.pending.clear()
  }

  private emit(method: string, params: unknown): void {
    const listeners = this.listeners.get(method)
    if (!listeners) return
    for (const listener of [...listeners]) {
      try {
        listener(params)
      } catch {
        // A consumer callback must not break CDP response/event routing.
      }
    }
  }
}
