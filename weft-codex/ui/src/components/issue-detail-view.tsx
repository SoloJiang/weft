import * as React from "react"
import { X } from "lucide-react"

import { api } from "@/api"
import { Button } from "@/components/ui/button"
import { useI18n } from "@/i18n"
import { directionAttentionKey, leadAttentionKey } from "@/lib/attention-reason"
import type { ArtifactSummary, BoardEntry, BusMessage, Direction, Repo } from "@/types"
import { EmptyState } from "./shared"
import { IssueArtifacts, LeadConversations } from "./issue-conversations"
import { DirectionActions, directionMeta, IssueActions, type WorkActions } from "./kanban-view"

/**
 * What the detail needs in order to own the issue's conversations.
 *
 * Kept separate from `WorkActions` because these are navigation, not work:
 * they move the human between surfaces rather than changing orchestration
 * state.
 */
export interface IssueConversationActions {
  activeThreadId: string | null
  openingThreadId: string | null
  onOpenThread: (threadId: string) => void
  onPromoteLead: (issueId: number, threadId: string) => Promise<void>
  onOpenArtifact: (artifact: ArtifactSummary) => void
}

function partyLabel(party: string, directions: Direction[], t: ReturnType<typeof useI18n>["t"]): string {
  if (party === "human") return t("party.you")
  if (party === "lead") return t("party.lead")
  const task = directions.find((direction) => direction.id === Number(party))
  return task?.name ?? t("party.task")
}

function formatTimestamp(timestamp: string, lang: "en" | "zh"): string {
  const milliseconds = Number(timestamp) * 1000
  if (!Number.isFinite(milliseconds) || milliseconds <= 0) return timestamp || ""
  const locale = lang === "zh" ? "zh-CN" : "en-US"
  return new Intl.DateTimeFormat(locale, { dateStyle: "medium", timeStyle: "short" }).format(new Date(milliseconds))
}

function TaskDetail({ direction, repos, actions }: { direction: Direction; repos: Repo[]; actions: WorkActions }) {
  const { t } = useI18n()
  return (
    <article className="task-detail">
      <header className="task-detail-head">
        <div className="task-detail-identity">
          <h3>{direction.name}</h3>
          <p className="meta">{directionMeta(direction, repos)}</p>
        </div>
        <span className={`status-chip status-${direction.status}`}>{t(`status.${direction.status}`)}</span>
        <DirectionActions direction={direction} actions={actions} />
      </header>
      {direction.attention ? (
        <p
          className="task-attention"
          role="status"
          data-attention-reason={direction.attention_reason || undefined}
        >
          {t(directionAttentionKey(direction.attention_reason))}
        </p>
      ) : null}
      {direction.reason ? <p className="task-reason">{direction.reason}</p> : null}
      {direction.spec ? (
        <section className="task-brief">
          <h4>{t("detail.taskBrief")}</h4>
          <pre className="spec">{direction.spec}</pre>
        </section>
      ) : null}
    </article>
  )
}

function Timeline({ rows, directions }: { rows: BusMessage[]; directions: Direction[] }) {
  const { t, lang } = useI18n()
  if (!rows.length) return <div className="timeline"><p className="meta">{t("detail.emptyBus")}</p></div>
  return (
    <div className="timeline">
      {rows.map((row) => {
        const from = partyLabel(row.from_party, directions, t)
        const to = partyLabel(row.to_party, directions, t)
        return (
          <article key={row.id} className={row.from_party === "human" ? "msg human" : "msg"}>
            <div className="msg-meta">{from} → {to} · {formatTimestamp(row.ts, lang)}</div>
            <div className="msg-text">{row.text}</div>
          </article>
        )
      })}
    </div>
  )
}

export function IssueDetailView({
  entry,
  repos,
  revision,
  actions,
  conversations,
  onClose,
}: {
  entry?: BoardEntry
  repos: Repo[]
  revision: number
  actions: WorkActions
  conversations: IssueConversationActions
  onClose: () => void
}) {
  const { t } = useI18n()
  const [rows, setRows] = React.useState<BusMessage[]>([])

  React.useEffect(() => {
    if (!entry) {
      setRows([])
      return
    }
    let active = true
    api<BusMessage[]>(`/api/issues/${entry.issue.id}/bus`)
      .then((messages) => { if (active) setRows(messages) })
      .catch(actions.onError)
    return () => { active = false }
  }, [entry, revision, actions.onError])

  return (
    <section className="view active" aria-labelledby="issue-detail-heading">
      <h1 id="issue-detail-heading" className="sr-only">{entry?.issue.title ?? t("detail.title")}</h1>
      {/* Close, not back: the board never went away. */}
      <div className="actions detail-nav">
        <Button variant="ghost" aria-label={t("detail.close")} onClick={onClose}>
          <X aria-hidden="true" />
        </Button>
      </div>
      {!entry ? (
        <EmptyState titleKey="detail.notFoundTitle" bodyKey="detail.notFoundBody" actionKey="detail.close" onAction={onClose} />
      ) : (
        <div id="issue-detail">
          <header className="issue-head detail-head">
            <div className="issue-identity">
              <div className="detail-title">{entry.issue.title}</div>
              <div className="issue-context">
                <span className="issue-kind">{t(`kind.${entry.issue.kind}`)}</span>
                <span className="meta">#{entry.issue.id}</span>
                {entry.issue.lead_attention ? (
                  <span
                    className="issue-lead-attention"
                    role="status"
                    data-attention-reason={entry.issue.lead_attention_reason || undefined}
                  >
                    {t(leadAttentionKey(entry.issue.lead_attention_reason))}
                  </span>
                ) : null}
              </div>
            </div>
            <IssueActions issue={entry.issue} actions={actions} />
          </header>
          <h2 className="section-title">{t("detail.conversations")}</h2>
          <LeadConversations
            entry={entry}
            activeThreadId={conversations.activeThreadId}
            openingThreadId={conversations.openingThreadId}
            onOpenThread={conversations.onOpenThread}
            onPromoteLead={(threadId) => conversations.onPromoteLead(entry.issue.id, threadId)}
            onError={actions.onError}
          />
          {entry.artifacts?.length ? (
            <>
              <h2 className="section-title">{t("sidebar.artifacts")}</h2>
              <IssueArtifacts artifacts={entry.artifacts} onOpen={conversations.onOpenArtifact} />
            </>
          ) : null}
          <h2 className="section-title">{t("detail.directions")}</h2>
          <div className="task-list">
            {!entry.directions.length ? <p className="meta">{t("detail.noTasks")}</p> : null}
            {entry.directions.map((direction) => <TaskDetail key={direction.id} direction={direction} repos={repos} actions={actions} />)}
          </div>
          <h2 className="section-title">{t("detail.busTimeline")}</h2>
          <Timeline rows={rows} directions={entry.directions} />
        </div>
      )}
    </section>
  )
}
