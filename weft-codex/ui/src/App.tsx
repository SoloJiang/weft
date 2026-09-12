import * as React from "react"
import { api, jsonRequest } from "@/api"
import { IssueDetailView } from "@/components/issue-detail-view"
import { ArtifactView } from "@/components/artifact-view"
import { KanbanView, type WorkActions } from "@/components/kanban-view"
import { RepositoriesView } from "@/components/repositories-view"
import { StageSplit } from "@/components/stage-split"
import { useI18n } from "@/i18n"
import { useWeftSession } from "@/session"
import { useWeftWorkspace } from "@/workspace-store"
import type { Direction } from "@/types"

export default function App() {
  const { t } = useI18n()
  const session = useWeftSession()
  const store = useWeftWorkspace()
  const workspaceId = session.workspaceId
  const view = session.route.view
  const detailIssueId = session.route.issueId
  const artifactId = session.route.artifactId ?? null
  const { workspaces, repos, board, repoMap, revision, loading, notify, notifyError } = store
  const openDialog = session.openDialog

  const launchLead = React.useCallback(async (issueId: number) => {
    const started = await api<{ codexThreadId: string }>(`/api/issues/${issueId}/spawn-lead`, jsonRequest("POST"))
    await store.refreshCurrent()
    window.setTimeout(() => {
      void session.openThread(started.codexThreadId).catch(() => {
        notifyError(new Error(t("err.threadOpen")))
      })
    }, 0)
  }, [notifyError, session, store, t])

  const completeTask = React.useCallback(async (direction: Direction) => {
    await api(`/api/directions/${direction.id}/complete`, jsonRequest("POST"))
    await store.refreshCurrent()
    notify(t("success.taskCompleted"), "success")
  }, [notify, store, t])

  const workActions = React.useMemo<WorkActions>(() => ({
    onError: notifyError,
    onOpenChat: async (issueId: number) => launchLead(issueId),
    // Dispatch then open: the caller's concept is "go to this task's chat",
    // and starting the worker is what that means when none is running.
    onOpenTaskChat: async (directionId: number) => {
      let started: { codexThreadId?: string }
      try {
        started = await api<{ codexThreadId?: string }>(
          `/api/directions/${directionId}/spawn`,
          jsonRequest("POST"),
        )
      } catch {
        throw new Error(t("err.taskStart"))
      }
      await store.refreshCurrent()
      if (started.codexThreadId) {
        window.setTimeout(() => {
          void session.openThread(started.codexThreadId as string).catch(() => {
            notifyError(new Error(t("err.threadOpen")))
          })
        }, 0)
      }
    },
    onCompleteTask: completeTask,
    onClearAttention: async (direction: Direction) => {
      await api(`/api/directions/${direction.id}/attention/clear`, jsonRequest("POST"))
      await store.refreshCurrent()
    },
    onContinueTask: (direction: Direction) => openDialog({ type: "message", target: "task", id: direction.id, intent: "continue" }),
  }), [notifyError, session, store, completeTask, launchLead, openDialog, t])

  // Conversations moved out of the sidebar and into the detail, so the
  // handlers that used to live beside the sidebar tree live here now.
  const conversationActions = React.useMemo(() => ({
    activeThreadId: session.threadId ?? null,
    openingThreadId: session.openingThreadId,
    onOpenThread: (threadId: string) => {
      void session.openThread(threadId).catch(() => {
        // Session keeps the failure so the sidebar footer can offer a retry.
      })
    },
    onPromoteLead: async (issueId: number, threadId: string) => {
      await api(`/api/issues/${issueId}/lead-thread`, jsonRequest("POST", { thread_id: threadId }))
      await store.refreshCurrent()
    },
    // Reading a document must not switch the native conversation; only the
    // workspace surface moves.
    onOpenArtifact: (artifact: { id: number; issue_id: number }) => {
      session.navigate({ view: "artifact", issueId: artifact.issue_id, artifactId: artifact.id })
    },
  }), [session, store])

  const analyzeRepository = async (id: number) => {
    await api(`/api/repos/${id}/analyze`, jsonRequest("POST"))
    notify(t("success.analysisStarted"), "success")
    await store.refreshCurrent()
  }

  const switchView = (next: "kanban" | "repos") => {
    session.navigate({ view: next, issueId: null })
  }

  const detailEntry = board.find((entry) => entry.issue.id === detailIssueId)
  let mainContent: React.ReactNode
  if (loading && !workspaces.length) {
    mainContent = <div className="app-loading" role="status">{t("app.loading")}</div>
  } else if (view === "repos") {
    mainContent = (
      <RepositoriesView
        workspaceId={workspaceId}
        repoMap={repoMap}
        onCreateWorkspace={() => openDialog({ type: "workspace" })}
        onOpenImport={() => openDialog({ type: "repositories" })}
        onAnalyzeRepository={analyzeRepository}
        onError={notifyError}
      />
    )
  } else if (view === "artifact" && artifactId) {
    mainContent = (
      <ArtifactView
        artifactId={artifactId}
        onBack={() => switchView("kanban")}
        onError={notifyError}
      />
    )
  } else {
    // Kanban and issue detail are the same stage, not two routes that replace
    // each other: opening a card must not take the board away.
    mainContent = (
      <StageSplit
        board={
          <KanbanView
            workspaceId={workspaceId}
            repos={repos}
            board={board}
            actions={workActions}
            onOpenCreateIssue={() => openDialog({ type: "issue" })}
            onCreateWorkspace={() => openDialog({ type: "workspace" })}
            onOpenIssue={(id) => session.navigate({ view: "issue", issueId: id })}
          />
        }
        detail={view === "issue" ? (
          <IssueDetailView
            entry={detailEntry}
            repos={repos}
            revision={revision}
            actions={workActions}
            conversations={conversationActions}
            onClose={() => switchView("kanban")}
          />
        ) : null}
      />
    )
  }

  return <main className="embedded-main">{mainContent}</main>
}
