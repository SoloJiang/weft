import * as React from "react"

import type { WeftHost, HeaderActions, HostView } from "@/host"
import {
  beginThreadOpen,
  clearThreadOpenFailure,
  finishThreadOpen,
  idleThreadOpen,
  openCodexThread,
  type ThreadOpenState,
} from "@/lib/thread-open"
import { readInitialRoute, readInitialWorkspaceId, type SurfaceRoute } from "@/route"
import type { ActiveDialogState, DialogState } from "@/types"

export interface WeftSession {
  workspaceId: number | null
  setWorkspaceId: (id: number | null) => void
  selectWorkspace: (id: number) => void
  /** Switch workspace without leaving the native thread. */
  adoptWorkspace: (id: number) => void
  route: SurfaceRoute
  navigate: (route: SurfaceRoute) => void
  /** Update the Weft route without covering the native thread. */
  setRoute: (route: SurfaceRoute) => void
  dialog: DialogState
  openDialog: (dialog: ActiveDialogState) => void
  closeDialog: () => void
  host: WeftHost
  hostView: HostView
  threadId?: string
  headerActions: HeaderActions
  openingThreadId: string | null
  failedThreadId: string | null
  openThread: (threadId: string) => Promise<void>
}

const SessionContext = React.createContext<WeftSession | null>(null)

export function WeftSessionProvider({
  host,
  children,
}: {
  host: WeftHost
  children: React.ReactNode
}) {
  const [workspaceId, setWorkspaceId] = React.useState<number | null>(readInitialWorkspaceId)
  const [route, setRoute] = React.useState<SurfaceRoute>(readInitialRoute)
  const [dialog, setDialog] = React.useState<DialogState>(null)
  const [hostView, setHostView] = React.useState<HostView>(host.view)
  const [threadId, setThreadId] = React.useState<string | undefined>(host.threadId)
  const [headerActions, setHeaderActions] = React.useState<HeaderActions>(host.headerActions)
  const [threadOpen, setThreadOpen] = React.useState<ThreadOpenState>(idleThreadOpen)
  const inFlightRef = React.useRef<{ threadId: string; promise: Promise<void> } | null>(null)
  const generationRef = React.useRef(0)

  React.useEffect(() => {
    return host.onView((view, nextThreadId) => {
      setHostView(view)
      setThreadId(nextThreadId)
    })
  }, [host])

  React.useEffect(() => {
    const sync = () => setHeaderActions(host.headerActions)
    sync()
    const observer = new MutationObserver(sync)
    observer.observe(document.documentElement, {
      attributes: true,
      attributeFilter: ["data-weft-codex-header-actions"],
    })
    return () => observer.disconnect()
  }, [host])

  const selectWorkspace = React.useCallback((id: number) => {
    host.showWorkspace()
    generationRef.current += 1
    inFlightRef.current = null
    setThreadOpen(idleThreadOpen)
    setWorkspaceId(id)
    setRoute({ view: "kanban", issueId: null })
  }, [host])

  const adoptWorkspace = React.useCallback((id: number) => {
    setWorkspaceId(id)
    setRoute({ view: "kanban", issueId: null })
  }, [])

  const navigate = React.useCallback((next: SurfaceRoute) => {
    host.showWorkspace()
    setThreadOpen(clearThreadOpenFailure)
    setRoute(next)
  }, [host])

  const rememberRoute = React.useCallback((next: SurfaceRoute) => {
    setRoute(next)
  }, [])

  const openThread = React.useCallback(async (threadId: string) => {
    const inflight = inFlightRef.current
    if (inflight && inflight.threadId === threadId) return inflight.promise

    const generation = generationRef.current + 1
    generationRef.current = generation
    setThreadOpen(beginThreadOpen(threadId))

    const run = (async () => {
      try {
        await openCodexThread(threadId)
        if (generationRef.current !== generation) return
        setThreadOpen((current) => finishThreadOpen(current, threadId, true))
      } catch (error) {
        if (generationRef.current !== generation) return
        setThreadOpen((current) => finishThreadOpen(current, threadId, false))
        throw error
      } finally {
        if (inFlightRef.current?.threadId === threadId) inFlightRef.current = null
      }
    })()
    inFlightRef.current = { threadId, promise: run }
    return run
  }, [])

  const openDialog = React.useCallback((next: ActiveDialogState) => {
    setDialog(next)
  }, [])

  const closeDialog = React.useCallback(() => {
    setDialog(null)
  }, [])

  const value = React.useMemo<WeftSession>(() => ({
    workspaceId,
    setWorkspaceId,
    selectWorkspace,
    adoptWorkspace,
    route,
    navigate,
    setRoute: rememberRoute,
    dialog,
    openDialog,
    closeDialog,
    host,
    hostView,
    threadId,
    headerActions,
    openingThreadId: threadOpen.openingThreadId,
    failedThreadId: threadOpen.failedThreadId,
    openThread,
  }), [
    workspaceId,
    selectWorkspace,
    adoptWorkspace,
    route,
    navigate,
    rememberRoute,
    dialog,
    openDialog,
    closeDialog,
    host,
    hostView,
    threadId,
    headerActions,
    threadOpen,
    openThread,
  ])

  return <SessionContext.Provider value={value}>{children}</SessionContext.Provider>
}

export function useWeftSession(): WeftSession {
  const value = React.useContext(SessionContext)
  if (!value) throw new Error("useWeftSession must be used inside WeftSessionProvider")
  return value
}
