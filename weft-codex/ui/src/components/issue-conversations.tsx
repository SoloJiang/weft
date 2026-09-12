import { CornerDownRight, FileText, MessageCircle, Star } from "lucide-react"

import { useI18n } from "@/i18n"
import type { ArtifactSummary, BoardEntry, ThreadBinding } from "@/types"
import { kindLabel, statusLabel } from "./artifact-view"
import { AsyncButton } from "./shared"

export function branchesFor(entry: BoardEntry, directionId: number | null): ThreadBinding[] {
  return entry.threads.filter((binding) => binding.direction_id === directionId)
}

export function primaryBranch(entry: BoardEntry, directionId: number | null): ThreadBinding | undefined {
  return branchesFor(entry, directionId).find((binding) => binding.is_primary === 1)
}

export function branchTitle(binding: ThreadBinding, forkIndex: number, fallback: string): string {
  if (binding.is_primary === 1) return fallback
  const title = binding.title.trim()
  if (title) return title
  return `${fallback} ${forkIndex}`
}

export interface ThreadRowProps {
  label: string
  threadId: string
  active: boolean
  opening?: boolean
  primary?: boolean
  nested?: boolean
  onOpen: (threadId: string) => void
  /** Present only on lead forks that can become the primary chat. */
  onPromote?: (threadId: string) => Promise<void>
  onError?: (error: unknown) => void
}

export function ThreadRow({
  label,
  threadId,
  active,
  opening = false,
  primary = false,
  nested = false,
  onOpen,
  onPromote,
  onError,
}: ThreadRowProps) {
  const { t } = useI18n()
  const openingLabel = t("loading.openingThread")
  const row = (
    <button
      type="button"
      className="thread-row"
      data-active={active ? "true" : "false"}
      data-nested={nested ? "true" : "false"}
      data-opening={opening ? "true" : "false"}
      aria-busy={opening}
      aria-current={active ? "page" : undefined}
      disabled={opening}
      onClick={() => onOpen(threadId)}
    >
      {nested ? <CornerDownRight aria-hidden="true" /> : <MessageCircle aria-hidden="true" />}
      <span className="thread-row-title" title={opening ? openingLabel : label}>
        {opening ? openingLabel : label}
      </span>
      {primary ? <span className="sidebar-primary-chip">{t("sidebar.primary")}</span> : null}
    </button>
  )
  if (!onPromote) return row
  // The promote control is a sibling, not a child: a button inside a button is
  // invalid and screen readers flatten it unpredictably.
  return (
    <div className="thread-row-wrap">
      {row}
      <AsyncButton
        variant="ghost"
        size="icon-sm"
        className="thread-row-promote"
        label={t("sidebar.makePrimary", { label })}
        pendingLabel={t("sidebar.makingPrimary")}
        onAction={() => onPromote(threadId)}
        onError={onError ?? (() => {})}
        iconOnly
      >
        <Star aria-hidden="true" />
      </AsyncButton>
    </div>
  )
}

/**
 * The issue's lead conversations: the primary chat and any forks off it.
 *
 * Only the lead half of what used to be the sidebar's conversation tree. The
 * task half moved nowhere because it was already here — the detail lists every
 * task with its own chat link, and drawing a second copy beside it would be
 * two controls for one thing.
 *
 * Opening a row still lands on Codex's own thread surface via `openThread`.
 * Weft supplies only the layer the host has no way to know about: which chats
 * belong to this issue, which one is primary, which are forks off it.
 */
export function LeadConversations({
  entry,
  activeThreadId,
  openingThreadId,
  onOpenThread,
  onPromoteLead,
  onError,
}: {
  entry: BoardEntry
  activeThreadId: string | null
  openingThreadId: string | null
  onOpenThread: (threadId: string) => void
  onPromoteLead: (threadId: string) => Promise<void>
  onError: (error: unknown) => void
}) {
  const { t } = useI18n()
  const leadBranches = branchesFor(entry, null)
  const leadPrimary = leadBranches.find((binding) => binding.is_primary === 1)
  const leadForks = leadBranches.filter((binding) => binding.is_primary !== 1)

  return (
    <div className="issue-conversations">
      {leadPrimary ? (
        <ThreadRow
          label={t("sidebar.mainChat")}
          threadId={leadPrimary.thread_id}
          active={activeThreadId === leadPrimary.thread_id}
          opening={openingThreadId === leadPrimary.thread_id}
          primary
          onOpen={onOpenThread}
        />
      ) : (
        <span className="sidebar-chat-pending">{t("sidebar.leadStarting")}</span>
      )}
      {leadForks.map((binding, index) => (
        <ThreadRow
          key={binding.thread_id}
          label={branchTitle(binding, index + 1, t("sidebar.forkChat"))}
          threadId={binding.thread_id}
          active={activeThreadId === binding.thread_id}
          opening={openingThreadId === binding.thread_id}
          nested
          onOpen={onOpenThread}
          onPromote={onPromoteLead}
          onError={onError}
        />
      ))}
    </div>
  )
}

export function IssueArtifacts({
  artifacts,
  onOpen,
}: {
  artifacts: ArtifactSummary[]
  onOpen: (artifact: ArtifactSummary) => void
}) {
  const { t } = useI18n()
  if (!artifacts.length) return null
  return (
    <div className="sidebar-artifacts">
      {artifacts.map((artifact) => (
        <button
          key={artifact.id}
          type="button"
          className="sidebar-artifact-row"
          data-status={artifact.status}
          onClick={() => onOpen(artifact)}
        >
          <FileText aria-hidden="true" />
          <span className="sidebar-artifact-name">
            {artifact.title || kindLabel(artifact.kind, t)}
          </span>
          <span className="sidebar-artifact-meta">
            {statusLabel(artifact.status, t)} · {t("artifact.revision", { revision: artifact.revision })}
          </span>
        </button>
      ))}
    </div>
  )
}
