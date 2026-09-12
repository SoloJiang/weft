import * as React from "react"
import {
  AlertTriangle,
  CircleCheck,
  FolderGit2,
  Inbox,
  KanbanSquare,
  Plus,
  Search,
  SquarePen,
  X,
} from "lucide-react"

import { api, jsonRequest } from "@/api"
import { primaryBranch } from "@/components/issue-conversations"
import { Button } from "@/components/ui/button"
import { Input } from "@/components/ui/input"
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select"
import { useI18n } from "@/i18n"
import { useWeftSession } from "@/session"
import { useWeftWorkspace } from "@/workspace-store"
import { inboxAttentionKey } from "@/lib/attention-reason"
import {
  buildInbox,
  inboxFollow,
  inboxIssueIds,
  inboxRowMeta,
  searchFollow,
  type InboxItem,
  type SearchHit,
  searchBoard,
} from "@/lib/sidebar-entries"
import {
  sidebarFooter,
  THREAD_RESOLVE_DELAYS_MS,
  threadLinkStatus,
  workspaceFollowForThread,
  type SidebarFooter,
} from "@/lib/thread-resolve"
import { isTypingTarget } from "@/lib/utils"
import type { SurfaceRoute } from "@/route"
import type {
  BoardEntry,
  ThreadBinding,
  ThreadLocationResponse,
} from "@/types"

/** Only one of the two header entries can be open; neither is the default. */
type SidebarPanel = "none" | "search" | "inbox"

interface ResolvedThread {
  binding: ThreadBinding
  workspaceId: number
}

type SidebarLocation =
  | { kind: "workspace"; route: SurfaceRoute }
  | { kind: "bound-thread"; threadId: string; binding: ThreadBinding }
  | { kind: "unbound-thread"; threadId: string }

function buildThreadMap(
  board: BoardEntry[],
  resolved: ReadonlyMap<string, ResolvedThread>,
): Map<string, ThreadBinding> {
  const bindings = new Map<string, ThreadBinding>()
  for (const entry of board) {
    for (const binding of entry.threads) bindings.set(binding.thread_id, binding)
  }
  for (const [threadId, location] of resolved) bindings.set(threadId, location.binding)
  return bindings
}

function workspaceForThread(
  threadId: string,
  board: BoardEntry[],
  resolved: ReadonlyMap<string, ResolvedThread>,
): number | null {
  const remembered = resolved.get(threadId)
  if (remembered) return remembered.workspaceId
  const entry = board.find((candidate) => (
    candidate.threads.some((binding) => binding.thread_id === threadId)
    || candidate.issue.lead_codex_thread_id === threadId
  ))
  return entry?.issue.workspace_id ?? null
}

function deriveLocation(
  hostView: "workspace" | "thread",
  threadId: string | undefined,
  route: SurfaceRoute,
  bindings: ReadonlyMap<string, ThreadBinding>,
): SidebarLocation {
  if (!threadId || hostView !== "thread") {
    return { kind: "workspace", route }
  }
  const binding = bindings.get(threadId)
  if (binding) return { kind: "bound-thread", threadId, binding }
  return { kind: "unbound-thread", threadId }
}

function ScrollingIssueTitle({ title }: { title: string }) {
  const viewportRef = React.useRef<HTMLSpanElement>(null)

  React.useLayoutEffect(() => {
    const viewport = viewportRef.current
    if (!viewport) return

    const updateOverflow = () => {
      viewport.dataset.overflow = viewport.scrollWidth > viewport.clientWidth + 1 ? "true" : "false"
    }
    updateOverflow()

    const observer = new ResizeObserver(updateOverflow)
    observer.observe(viewport)
    return () => observer.disconnect()
  }, [title])

  const scrollTo = (position: "start" | "end") => {
    const viewport = viewportRef.current
    if (!viewport || viewport.dataset.overflow !== "true") return
    const left = position === "end" ? viewport.scrollWidth - viewport.clientWidth : 0
    const behavior = window.matchMedia("(prefers-reduced-motion: reduce)").matches
      ? "auto"
      : "smooth"
    viewport.scrollTo({ left, behavior })
  }

  return (
    <span
      ref={viewportRef}
      className="sidebar-issue-title-viewport"
      onMouseEnter={() => scrollTo("end")}
      onMouseLeave={() => scrollTo("start")}
    >
      <span className="sidebar-row-title">{title}</span>
    </span>
  )
}

function SidebarStatusFooter({
  footer,
  onRetry,
}: {
  footer: SidebarFooter
  onRetry: (threadId: string) => void
}) {
  const { t } = useI18n()
  if (footer.kind === "none") return null
  if (footer.kind === "retry") {
    return (
      <footer className="sidebar-footer">
        <span className="sidebar-error" role="alert" title={t("err.prefix") + t("err.threadOpen")}>
          {t("err.prefix") + t("err.threadOpen")}
        </span>
        <Button
          variant="ghost"
          size="xs"
          className="sidebar-footer-retry"
          onClick={() => onRetry(footer.threadId)}
        >
          {t("action.retryOpenThread")}
        </Button>
      </footer>
    )
  }
  if (footer.kind === "linking") {
    return (
      <footer className="sidebar-footer">
        <span className="sidebar-status" role="status">{t("sidebar.linkingThread")}</span>
      </footer>
    )
  }
  return (
    <footer className="sidebar-footer">
      <span className="sidebar-status" role="status">{t("sidebar.unboundThread")}</span>
    </footer>
  )
}

/**
 * The same two entries the host renders in its sidebar header, drawn here when
 * it cannot. The capability must not depend on a host structure we do not own:
 * the browser path has no host at all, and a Codex release that reshapes the
 * mode row would otherwise take search and the inbox down with it.
 */
function HeaderEntries({
  inboxCount,
  panel,
  onOpen,
}: {
  inboxCount: number
  panel: SidebarPanel
  onOpen: (panel: SidebarPanel) => void
}) {
  const { t } = useI18n()
  return (
    <div className="sidebar-header-entries">
      <Button
        variant="ghost"
        size="icon-sm"
        aria-label={t("entries.search")}
        title={t("entries.search")}
        data-active={panel === "search" ? "true" : "false"}
        onClick={() => onOpen("search")}
      >
        <Search aria-hidden="true" />
      </Button>
      <Button
        variant="ghost"
        size="icon-sm"
        className="sidebar-inbox-entry"
        aria-label={inboxCount ? t("entries.inboxCount", { count: inboxCount }) : t("entries.inbox")}
        title={t("entries.inbox")}
        data-active={panel === "inbox" ? "true" : "false"}
        onClick={() => onOpen("inbox")}
      >
        <Inbox aria-hidden="true" />
        {inboxCount ? (
          <span className="sidebar-inbox-badge" aria-hidden="true">
            {inboxCount > 99 ? "99+" : inboxCount}
          </span>
        ) : null}
      </Button>
    </div>
  )
}

function SearchPanel({
  hits,
  query,
  onQuery,
  onOpenHit,
}: {
  hits: SearchHit[]
  query: string
  onQuery: (value: string) => void
  onOpenHit: (hit: SearchHit) => void
}) {
  const { t } = useI18n()
  const inputRef = React.useRef<HTMLInputElement>(null)

  React.useEffect(() => { inputRef.current?.focus() }, [])

  const kindLabels: Record<SearchHit["kind"], string> = {
    issue: t("entries.kind.issue"),
    direction: t("entries.kind.direction"),
    artifact: t("entries.kind.artifact"),
    thread: t("entries.kind.thread"),
  }

  let results: React.ReactNode = null
  if (query.trim() && !hits.length) {
    results = <p className="sidebar-empty" role="status">{t("entries.noMatches")}</p>
  } else if (hits.length) {
    results = hits.map((hit) => (
      <button
        key={hit.key}
        type="button"
        className="sidebar-result-row"
        onClick={() => onOpenHit(hit)}
      >
        <span className="sidebar-result-kind">{kindLabels[hit.kind]}</span>
        <span>
          <span className="sidebar-row-title">{hit.title}</span>
          <span className="sidebar-row-meta">{hit.meta}</span>
        </span>
      </button>
    ))
  }

  return (
    <>
      <div className="sidebar-search-field">
        <Search aria-hidden="true" />
        <Input
          ref={inputRef}
          type="search"
          value={query}
          aria-label={t("entries.searchLabel")}
          placeholder={t("entries.searchPlaceholder")}
          onChange={(event) => onQuery(event.target.value)}
        />
      </div>
      <div className="sidebar-panel-list">{results}</div>
    </>
  )
}

function InboxPanel({
  items,
  onOpenItem,
}: {
  items: InboxItem[]
  onOpenItem: (item: InboxItem) => void
}) {
  const { t } = useI18n()
  if (!items.length) {
    return <p className="sidebar-empty" role="status">{t("entries.inboxEmpty")}</p>
  }
  return (
    <div className="sidebar-panel-list">
      {items.map((item) => (
        <button
          key={item.key}
          type="button"
          className="sidebar-attention-row"
          data-kind={item.kind}
          onClick={() => onOpenItem(item)}
        >
          {item.kind === "review" ? <CircleCheck aria-hidden="true" /> : <AlertTriangle aria-hidden="true" />}
          <span>
            <span className="sidebar-row-title">{item.title}</span>
            <span
              className="sidebar-row-meta"
              data-attention-reason={item.reason || undefined}
            >
              {inboxRowMeta(item.issueTitle, t(inboxAttentionKey(item.kind, item.reason)))}
            </span>
          </span>
        </button>
      ))}
    </div>
  )
}

export default function SidebarApp() {
  const { t } = useI18n()
  const session = useWeftSession()
  const store = useWeftWorkspace()
  const workspaceId = session.workspaceId
  const route = session.route
  const { workspaces, board, repos, loading, deliveryFailures } = store
  const [panel, setPanel] = React.useState<SidebarPanel>("none")
  const [query, setQuery] = React.useState("")
  const [resolvedThreads, setResolvedThreads] = React.useState<Map<string, ResolvedThread>>(
    () => new Map(),
  )
  const resolveSequence = React.useRef(0)
  const [resolveExhausted, setResolveExhausted] = React.useState(false)
  const stateRef = React.useRef({ workspaceId, route })
  const threadMap = React.useMemo(
    () => buildThreadMap(board, resolvedThreads),
    [board, resolvedThreads],
  )
  const location = React.useMemo(
    () => deriveLocation(session.hostView, session.threadId, route, threadMap),
    [session.hostView, session.threadId, route, threadMap],
  )
  stateRef.current = { workspaceId, route }

  React.useEffect(() => {
    if (location.kind !== "unbound-thread") {
      setResolveExhausted(false)
      return
    }
    const sequence = resolveSequence.current + 1
    resolveSequence.current = sequence
    setResolveExhausted(false)
    let cancelled = false
    const resolve = async () => {
      for (const delay of THREAD_RESOLVE_DELAYS_MS) {
        if (delay) await new Promise((done) => window.setTimeout(done, delay))
        if (cancelled || resolveSequence.current !== sequence) return
        let response: ThreadLocationResponse
        try {
          response = await api<ThreadLocationResponse>(
            "/api/threads/resolve",
            jsonRequest("POST", { thread_id: location.threadId }),
          )
        } catch {
          continue
        }
        if (!response.binding || !response.workspaceId) continue
        setResolvedThreads((current) => {
          const next = new Map(current)
          next.set(location.threadId, {
            binding: response.binding as ThreadBinding,
            workspaceId: response.workspaceId as number,
          })
          return next
        })
        const follow = workspaceFollowForThread({
          hostView: session.hostView,
          currentWorkspaceId: stateRef.current.workspaceId,
          threadWorkspaceId: response.workspaceId,
        })
        if (follow.action === "adopt") session.adoptWorkspace(follow.workspaceId)
        return
      }
      if (!cancelled && resolveSequence.current === sequence) {
        setResolveExhausted(true)
      }
    }
    void resolve()
    return () => { cancelled = true }
  }, [location, session])

  const footer = sidebarFooter(
    session.failedThreadId,
    threadLinkStatus(location.kind, resolveExhausted),
  )

  let activeIssueId: number | null = null
  if (location.kind === "bound-thread") {
    activeIssueId = location.binding.issue_id
  } else if (location.kind === "workspace" && location.route.view === "issue") {
    activeIssueId = location.route.issueId
  }

  React.useEffect(() => {
    if (location.kind !== "bound-thread") return
    const follow = workspaceFollowForThread({
      hostView: session.hostView,
      currentWorkspaceId: workspaceId,
      threadWorkspaceId: workspaceForThread(location.threadId, board, resolvedThreads),
    })
    if (follow.action === "adopt") session.adoptWorkspace(follow.workspaceId)
  }, [board, location, resolvedThreads, session, workspaceId])

  const navigate = React.useCallback((next: SurfaceRoute) => {
    session.navigate(next)
  }, [session])

  const selectWorkspace = React.useCallback((id: number) => {
    session.selectWorkspace(id)
  }, [session])

  const openThread = React.useCallback((threadId: string) => {
    void session.openThread(threadId).catch(() => {
      // Session keeps failedThreadId so the footer can offer a retry.
    })
  }, [session])

  const openIssue = React.useCallback((entry: BoardEntry) => {
    const primary = primaryBranch(entry, null)
    const threadId = primary?.thread_id || entry.issue.lead_codex_thread_id
    if (threadId) {
      openThread(threadId)
      return
    }
    navigate({ view: "issue", issueId: entry.issue.id })
  }, [navigate, openThread])

  const deferredQuery = React.useDeferredValue(query)
  const searchHits = React.useMemo(
    () => searchBoard(board, repos, deferredQuery),
    [board, repos, deferredQuery],
  )
  const inboxItems = React.useMemo(
    () => buildInbox(board, deliveryFailures),
    [board, deliveryFailures],
  )
  const needingYou = React.useMemo(() => inboxIssueIds(inboxItems), [inboxItems])

  React.useEffect(() => {
    return session.host.onCommand((command) => {
      setPanel(command === "search.open" ? "search" : "inbox")
    })
  }, [session.host])

  // The one owner of slash in Weft. ⌘K is not ours to take — the host binds it
  // to its command menu — and the kanban used to bind slash as well, which was
  // harmless only while the surfaces were separate iframes with separate
  // windows; the same-document shell made one keystroke open two search
  // surfaces at once.
  //
  // Deliberately unadvertised: Codex parks focus in its ProseMirror composer,
  // which `isTypingTarget` correctly refuses to steal from, so slash does
  // nothing at all in the app's default state. A badge here would promise what
  // the host will not give — the same mistake the ⌘K hint made
  // (docs/compat/codex-builds.md §5.10).
  React.useEffect(() => {
    const openSearch = (event: KeyboardEvent) => {
      if (event.key !== "/" || event.metaKey || event.ctrlKey || event.altKey) return
      if (isTypingTarget(event.target)) return
      event.preventDefault()
      setPanel("search")
    }
    window.addEventListener("keydown", openSearch)
    return () => window.removeEventListener("keydown", openSearch)
  }, [])

  // The host paints the badge but never counts: the board lives here.
  React.useEffect(() => { session.host.setInboxCount(inboxItems.length) }, [inboxItems.length, session.host])

  const closePanel = React.useCallback(() => {
    setPanel("none")
    setQuery("")
  }, [])

  // Escape has to work wherever focus went — clicking a result list, then the
  // panel background, used to leave the only exit as the close button.
  React.useEffect(() => {
    if (panel === "none") return
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key !== "Escape") return
      event.preventDefault()
      closePanel()
    }
    window.addEventListener("keydown", onKeyDown)
    return () => window.removeEventListener("keydown", onKeyDown)
  }, [panel, closePanel])

  const openHit = React.useCallback((hit: SearchHit) => {
    closePanel()
    const follow = searchFollow(hit)
    if (follow.action === "open-thread") {
      session.setRoute({ view: "issue", issueId: follow.issueId })
      openThread(follow.threadId)
      return
    }
    if (follow.action === "show-artifact") {
      navigate({ view: "artifact", issueId: follow.issueId, artifactId: follow.artifactId })
      return
    }
    if (follow.action === "show-issue") {
      navigate({ view: "issue", issueId: follow.issueId })
      return
    }
    const entry = board.find((candidate) => candidate.issue.id === follow.issueId)
    if (entry) {
      openIssue(entry)
      return
    }
    navigate({ view: "issue", issueId: follow.issueId })
  }, [board, closePanel, navigate, openIssue, openThread, session])

  const openInboxItem = React.useCallback((item: InboxItem) => {
    closePanel()
    if (item.failureKey) {
      // Acting on it is the only acknowledgement there is; the bus event will
      // not fire again for a failure that already happened.
      store.dismissDeliveryFailure(item.failureKey)
    }
    const follow = inboxFollow(item)
    if (follow.action === "open-thread") {
      session.setRoute({ view: "issue", issueId: follow.issueId })
      openThread(follow.threadId)
      return
    }
    navigate({ view: "issue", issueId: follow.issueId })
  }, [closePanel, navigate, openThread, session, store])

  const headerActionsAreNative = session.headerActions === "native"

  let issueList: React.ReactNode
  if (loading && !board.length) {
    issueList = <p className="sidebar-empty" role="status">{t("app.loading")}</p>
  } else if (!board.length) {
    issueList = <p className="sidebar-empty">{t("sidebar.noIssues")}</p>
  } else {
    issueList = board.map((entry) => {
      const selected = activeIssueId === entry.issue.id
      const needsYou = needingYou.has(entry.issue.id)
      const leadThreadId = primaryBranch(entry, null)?.thread_id || entry.issue.lead_codex_thread_id
      const opening = Boolean(leadThreadId) && session.openingThreadId === leadThreadId
      const openLabel = needsYou
        ? t("sidebar.openIssueLeadNeedsYou", { title: entry.issue.title })
        : t("sidebar.openIssueLead", { title: entry.issue.title })
      return (
        <div
          key={entry.issue.id}
          className="sidebar-issue-row"
          data-active={selected ? "true" : "false"}
        >
          <button
            type="button"
            className="sidebar-issue-main"
            data-opening={opening ? "true" : "false"}
            aria-busy={opening}
            disabled={opening}
            aria-label={opening ? t("loading.openingThread") : openLabel}
            onClick={() => openIssue(entry)}
          >
            <ScrollingIssueTitle title={entry.issue.title} />
            {needsYou ? (
              <span className="sidebar-needs-you-chip">{t("kanban.issueNeedsYou")}</span>
            ) : null}
          </button>
        </div>
      )
    })
  }

  return (
    <aside className="sidebar-surface" aria-label={t("sidebar.title")}>
      <div className="sidebar-workspace-row">
        <Select
          value={workspaceId ? String(workspaceId) : undefined}
          disabled={!workspaces.length}
          onValueChange={(value) => selectWorkspace(Number(value))}
        >
          <SelectTrigger
            className="sidebar-workspace-select"
            id="sidebar-workspace-select"
            aria-label={t("workspace.label")}
          >
            <SelectValue placeholder={t("workspace.none")} />
          </SelectTrigger>
          <SelectContent>
            {workspaces.map((workspace) => (
              <SelectItem key={workspace.id} value={String(workspace.id)}>{workspace.name}</SelectItem>
            ))}
          </SelectContent>
        </Select>
        <Button
          variant="ghost"
          size="icon-sm"
          aria-label={t("ws.add")}
          title={t("ws.add")}
          onClick={() => session.openDialog({ type: "workspace" })}
        >
          <Plus aria-hidden="true" />
        </Button>
        {headerActionsAreNative ? null : (
          <HeaderEntries inboxCount={inboxItems.length} panel={panel} onOpen={setPanel} />
        )}
      </div>

      {workspaceId ? (
        <div className="sidebar-primary-actions">
          <Button
            variant="ghost"
            className="sidebar-create-button"
            onClick={() => session.openDialog({ type: "issue" })}
          >
            <SquarePen aria-hidden="true" />
            {t("issue.create")}
          </Button>
        </div>
      ) : null}

      <nav className="sidebar-nav" aria-label={t("nav.primary")}>
        <Button
          variant="ghost"
          className="sidebar-nav-button"
          data-active={location.kind === "workspace" && route.view === "kanban" ? "true" : "false"}
          aria-current={location.kind === "workspace" && route.view === "kanban" ? "page" : undefined}
          onClick={() => navigate({ view: "kanban", issueId: null })}
        >
          <KanbanSquare aria-hidden="true" />
          {t("nav.kanban")}
        </Button>
        <Button
          variant="ghost"
          className="sidebar-nav-button"
          data-active={location.kind === "workspace" && route.view === "repos" ? "true" : "false"}
          aria-current={location.kind === "workspace" && route.view === "repos" ? "page" : undefined}
          onClick={() => navigate({ view: "repos", issueId: null })}
        >
          <FolderGit2 aria-hidden="true" />
          {t("nav.repos")}
        </Button>
      </nav>

      <div className="sidebar-scroll">
        <section className="sidebar-section" aria-labelledby="sidebar-issues-heading">
          <div className="sidebar-section-heading">
            <h2 id="sidebar-issues-heading">{t("sidebar.issues")}</h2>
          </div>
          <div className="sidebar-list">{issueList}</div>
        </section>

      </div>

      {panel === "none" ? null : (
        <section
          className="sidebar-panel"
          aria-label={t(panel === "search" ? "entries.search" : "entries.inbox")}
        >
          <header className="sidebar-panel-header">
            <h2>{t(panel === "search" ? "entries.search" : "entries.inbox")}</h2>
            <Button
              variant="ghost"
              size="icon-sm"
              aria-label={t("entries.close")}
              title={t("entries.close")}
              onClick={closePanel}
            >
              <X aria-hidden="true" />
            </Button>
          </header>
          {panel === "search" ? (
            <SearchPanel hits={searchHits} query={query} onQuery={setQuery} onOpenHit={openHit} />
          ) : (
            <InboxPanel items={inboxItems} onOpenItem={openInboxItem} />
          )}
        </section>
      )}

      <SidebarStatusFooter footer={footer} onRetry={openThread} />
    </aside>
  )
}
