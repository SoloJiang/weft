import * as React from "react"

export type HostView = "workspace" | "thread"
export type HostCommand = "search.open" | "inbox.open"
export type HeaderActions = "native" | "fallback"

export interface WeftHost {
  readonly locale: string
  readonly view: HostView
  readonly threadId?: string
  readonly weftdOrigin: string
  readonly headerActions: HeaderActions
  openThread(threadId: string): Promise<void>
  showWorkspace(): void
  pickRepositories(): Promise<string[]>
  setInboxCount(count: number): void
  onCommand(handler: (command: HostCommand) => void): () => void
  onView(handler: (view: HostView, threadId?: string) => void): () => void
}

const HostContext = React.createContext<WeftHost | null>(null)

let installed: WeftHost | null = null

export function installHost(host: WeftHost | null): void {
  installed = host
}

export function currentHost(): WeftHost | null {
  return installed
}

export function HostProvider({
  host,
  children,
}: {
  host: WeftHost
  children: React.ReactNode
}) {
  React.useEffect(() => {
    installHost(host)
    return () => {
      if (installed === host) installHost(null)
    }
  }, [host])
  return React.createElement(HostContext.Provider, { value: host }, children)
}

export function useHost(): WeftHost {
  const host = React.useContext(HostContext)
  if (!host) throw new Error("useHost must be used inside HostProvider")
  return host
}

export function createPreviewHost(): WeftHost {
  return {
    locale: document.documentElement.lang || navigator.language || "en",
    view: "workspace",
    weftdOrigin: "",
    headerActions: "fallback",
    async openThread(threadId) {
      window.location.assign(`codex://threads/${encodeURIComponent(threadId)}`)
    },
    showWorkspace() {},
    async pickRepositories() {
      return []
    },
    setInboxCount() {},
    onCommand() {
      return () => {}
    },
    onView() {
      return () => {}
    },
  }
}

export function canPickFolders(host: WeftHost): boolean {
  return host.weftdOrigin.length > 0
}
