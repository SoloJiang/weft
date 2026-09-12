import * as React from "react"

import { api, apiUrl, jsonRequest, slugify } from "@/api"
import { useI18n } from "@/i18n"
import {
  deliveryFailureKey,
  parseDeliveryFailure,
  rememberDeliveryFailure,
  type DeliveryFailure,
} from "@/lib/sidebar-entries"
import { createIssueFollow } from "@/lib/create-issue"
import { normalizeDirectionStatus } from "@/lib/issue-board"
import { pickWorkspaceId } from "@/lib/workspace-id"
import { useWeftSession } from "@/session"
import type {
  BoardEntry,
  IssueKind,
  MessageIntent,
  Repo,
  RepoImportResponse,
  RepoMap,
  ToastKind,
  UiState,
  Workspace,
} from "@/types"

export interface ToastMessage {
  id: number
  message: string
  kind: ToastKind
}

export interface WeftWorkspace {
  workspaces: Workspace[]
  repos: Repo[]
  board: BoardEntry[]
  repoMap: RepoMap | null
  revision: number
  loading: boolean
  deliveryFailures: DeliveryFailure[]
  toasts: ToastMessage[]
  refreshWorkspace: (id: number) => Promise<void>
  refreshCurrent: () => Promise<void>
  loadWorkspaces: (preferredId?: number) => Promise<Workspace[]>
  createWorkspace: (name: string) => Promise<void>
  createIssue: (title: string, kind: IssueKind) => Promise<void>
  importRepositories: (paths: string[]) => Promise<RepoImportResponse>
  sendMessage: (
    target: "lead" | "task",
    id: number,
    text: string,
    intent: MessageIntent,
  ) => Promise<void>
  notify: (message: string, kind?: ToastKind) => void
  notifyError: (error: unknown) => void
  dismissDeliveryFailure: (key: string) => void
}

const EVENT_NAMES = [
  "direction.updated",
  "issue.updated",
  "workspace.updated",
  "repo.added",
  "repo.profile",
  "repo.relations",
  "bus.message",
  "bus.parked",
  "bus.undelivered",
  "thread.human-active",
  "lead.attention",
  "thread.binding.updated",
]

const WorkspaceContext = React.createContext<WeftWorkspace | null>(null)

function errorText(error: unknown, network: string, unknown: string): string {
  if (error instanceof TypeError) return network
  if (error instanceof Error && error.message) return error.message
  return unknown
}

function normalizeBoard(entries: BoardEntry[]): BoardEntry[] {
  return entries.map((entry) => ({
    ...entry,
    threads: Array.isArray(entry.threads) ? entry.threads : [],
    directions: (Array.isArray(entry.directions) ? entry.directions : []).map((direction) => ({
      ...direction,
      status: normalizeDirectionStatus(direction.status),
    })),
  }))
}

export function WeftWorkspaceProvider({ children }: { children: React.ReactNode }) {
  const { t } = useI18n()
  const session = useWeftSession()
  const setWorkspaceId = session.setWorkspaceId
  const workspaceId = session.workspaceId
  const [workspaces, setWorkspaces] = React.useState<Workspace[]>([])
  const [repos, setRepos] = React.useState<Repo[]>([])
  const [board, setBoard] = React.useState<BoardEntry[]>([])
  const [repoMap, setRepoMap] = React.useState<RepoMap | null>(null)
  const [revision, setRevision] = React.useState(0)
  const [loading, setLoading] = React.useState(true)
  const [deliveryFailures, setDeliveryFailures] = React.useState<DeliveryFailure[]>([])
  const [toasts, setToasts] = React.useState<ToastMessage[]>([])
  const loadSequence = React.useRef(0)
  const toastSequence = React.useRef(0)
  const workspaceIdRef = React.useRef(workspaceId)
  workspaceIdRef.current = workspaceId

  const notify = React.useCallback((message: string, kind: ToastKind = "info") => {
    toastSequence.current += 1
    const id = toastSequence.current
    setToasts((current) => [...current, { id, message, kind }])
    window.setTimeout(() => {
      setToasts((current) => current.filter((toast) => toast.id !== id))
    }, 6000)
  }, [])

  const notifyError = React.useCallback((error: unknown) => {
    notify(t("err.prefix") + errorText(error, t("err.network"), t("err.unknown")), "error")
  }, [notify, t])

  const refreshWorkspace = React.useCallback(async (id: number) => {
    const sequence = loadSequence.current + 1
    loadSequence.current = sequence
    const [nextRepos, nextBoard, nextRepoMap] = await Promise.all([
      api<Repo[]>(`/api/workspaces/${id}/repos`),
      api<BoardEntry[]>(`/api/issues?workspace_id=${id}`),
      api<RepoMap>(`/api/workspaces/${id}/repo-map`),
    ])
    if (loadSequence.current !== sequence) return
    setRepos(nextRepos)
    setBoard(normalizeBoard(nextBoard))
    setRepoMap(nextRepoMap)
    setRevision((current) => current + 1)
  }, [])

  const loadWorkspaces = React.useCallback(async (preferredId?: number) => {
    const [rows, uiState] = await Promise.all([
      api<Workspace[]>("/api/workspaces"),
      api<UiState>("/api/ui-state").catch(() => ({ lastWorkspaceId: null, detailPaneWidth: null })),
    ])
    setWorkspaces(rows)
    const nextId = pickWorkspaceId(
      rows.map((workspace) => workspace.id),
      {
        preferredId,
        currentId: workspaceIdRef.current,
        persistedId: uiState.lastWorkspaceId,
      },
    )
    setWorkspaceId(nextId)
    return rows
  }, [setWorkspaceId])

  React.useEffect(() => {
    let active = true
    loadWorkspaces()
      .catch(notifyError)
      .finally(() => { if (active) setLoading(false) })
    return () => { active = false }
  }, [loadWorkspaces, notifyError])

  React.useEffect(() => {
    if (!workspaceId) {
      loadSequence.current += 1
      setRepos([])
      setBoard([])
      setRepoMap(null)
      return
    }
    setLoading(true)
    refreshWorkspace(workspaceId)
      .catch(notifyError)
      .finally(() => setLoading(false))
  }, [workspaceId, refreshWorkspace, notifyError])

  const refreshWorkspaceRef = React.useRef(refreshWorkspace)
  const loadWorkspacesRef = React.useRef(loadWorkspaces)
  const notifyErrorRef = React.useRef(notifyError)
  refreshWorkspaceRef.current = refreshWorkspace
  loadWorkspacesRef.current = loadWorkspaces
  notifyErrorRef.current = notifyError

  // One connection for the whole tree. Callbacks live in refs so a workspace
  // switch does not tear the socket down and open another.
  React.useEffect(() => {
    const source = new EventSource(apiUrl("/api/events"))
    let timer: number | undefined
    let refreshWorkspaceList = false

    const scheduleRefresh = (event: Event) => {
      if (event.type === "workspace.updated") refreshWorkspaceList = true
      window.clearTimeout(timer)
      timer = window.setTimeout(() => {
        const jobs: Promise<unknown>[] = []
        const id = workspaceIdRef.current
        if (id) jobs.push(refreshWorkspaceRef.current(id))
        if (refreshWorkspaceList) {
          refreshWorkspaceList = false
          jobs.push(loadWorkspacesRef.current())
        }
        Promise.all(jobs).catch(notifyErrorRef.current)
      }, 400)
    }

    const onEvent = (event: Event) => {
      scheduleRefresh(event)
      if (event.type !== "bus.undelivered") return
      if (!(event instanceof MessageEvent) || typeof event.data !== "string") return
      const failure = parseDeliveryFailure(event.data)
      if (!failure) return
      setDeliveryFailures((current) => rememberDeliveryFailure(current, failure))
    }

    for (const name of EVENT_NAMES) source.addEventListener(name, onEvent)
    return () => {
      window.clearTimeout(timer)
      source.close()
    }
  }, [])

  const refreshCurrent = React.useCallback(async () => {
    const id = workspaceIdRef.current
    if (!id) return
    await refreshWorkspace(id)
  }, [refreshWorkspace])

  React.useEffect(() => {
    if (!workspaceId) return
    void api("/api/ui-state", jsonRequest("POST", { lastWorkspaceId: workspaceId })).catch(() => {
      // Remembering the last workspace is best-effort; the board still loads.
    })
  }, [workspaceId])

  const createWorkspace = React.useCallback(async (name: string) => {
    const created = await api<{ id: number }>("/api/workspaces", jsonRequest("POST", { name, slug: slugify(name) }))
    await loadWorkspaces(created.id)
    session.navigate({ view: "kanban", issueId: null })
    notify(t("success.workspaceCreated"), "success")
  }, [loadWorkspaces, notify, session, t])

  const createIssue = React.useCallback(async (title: string, kind: IssueKind) => {
    const id = workspaceIdRef.current
    if (!id) throw new Error(t("err.unknown"))
    const created = await api<{ id: number; codexThreadId?: string | null }>("/api/issues", jsonRequest("POST", {
      workspace_id: id,
      title,
      slug: slugify(title),
      kind,
    }))
    await refreshCurrent()
    notify(t("success.issueCreated"), "success")
    const follow = createIssueFollow(created)
    if (follow.action === "open-thread") {
      session.setRoute(follow.route)
      void session.openThread(follow.threadId).catch(() => {
        notifyError(new Error(t("err.threadOpen")))
      })
      return
    }
    session.navigate(follow.route)
  }, [notify, notifyError, refreshCurrent, session, t])

  const importRepositories = React.useCallback(async (paths: string[]) => {
    const id = workspaceIdRef.current
    if (!id) throw new Error(t("err.unknown"))
    const response = await api<RepoImportResponse>(
      `/api/workspaces/${id}/repos/import`,
      jsonRequest("POST", { paths }),
    )
    await refreshCurrent()
    if (response.added) notify(t("success.reposAdded", { count: response.added }), "success")
    else if (!response.failed) notify(t("success.reposExisting"), "info")
    return response
  }, [notify, refreshCurrent, t])

  const sendMessage = React.useCallback(async (
    target: "lead" | "task",
    id: number,
    text: string,
    intent: MessageIntent,
  ) => {
    const path = target === "lead" ? `/api/issues/${id}/message` : `/api/directions/${id}/message`
    await api(path, jsonRequest("POST", { text }))
    await refreshCurrent()
    notify(t(intent === "continue" ? "success.continueSent" : "success.messageSent"), "success")
  }, [notify, refreshCurrent, t])

  const dismissDeliveryFailure = React.useCallback((key: string) => {
    setDeliveryFailures((current) => (
      current.filter((failure) => deliveryFailureKey(failure) !== key)
    ))
  }, [])

  const value = React.useMemo<WeftWorkspace>(() => ({
    workspaces,
    repos,
    board,
    repoMap,
    revision,
    loading,
    deliveryFailures,
    toasts,
    refreshWorkspace,
    refreshCurrent,
    loadWorkspaces,
    createWorkspace,
    createIssue,
    importRepositories,
    sendMessage,
    notify,
    notifyError,
    dismissDeliveryFailure,
  }), [
    workspaces,
    repos,
    board,
    repoMap,
    revision,
    loading,
    deliveryFailures,
    toasts,
    refreshWorkspace,
    refreshCurrent,
    loadWorkspaces,
    createWorkspace,
    createIssue,
    importRepositories,
    sendMessage,
    notify,
    notifyError,
    dismissDeliveryFailure,
  ])

  return <WorkspaceContext.Provider value={value}>{children}</WorkspaceContext.Provider>
}

export function useWeftWorkspace(): WeftWorkspace {
  const value = React.useContext(WorkspaceContext)
  if (!value) throw new Error("useWeftWorkspace must be used inside WeftWorkspaceProvider")
  return value
}

export function ToastStack() {
  const { toasts } = useWeftWorkspace()
  return (
    <div className="notifications weft-overlay-toasts" aria-live="polite" aria-atomic="false">
      {toasts.map((toast) => (
        <div
          key={toast.id}
          className={`toast toast-${toast.kind}`}
          role={toast.kind === "error" ? "alert" : "status"}
        >
          {toast.message}
        </div>
      ))}
    </div>
  )
}
