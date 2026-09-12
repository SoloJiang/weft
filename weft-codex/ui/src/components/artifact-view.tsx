import * as React from "react"
import { ArrowLeft, Maximize2, Minimize2, PencilLine, RefreshCw, Save, Network } from "lucide-react"

import { ApiError, api } from "@/api"
import { Button } from "@/components/ui/button"
import { useI18n, type MessageKey } from "@/i18n"
import type { Artifact, ArtifactErrorBody } from "@/types"
import { MindMap, caseCount, type NodeQuestion } from "./mindmap"
import { AsyncButton, EmptyState } from "./shared"

/**
 * Distinguish a revision conflict from every other failure.
 *
 * The API answers with a typed `code`; matching on it is the whole point of
 * that contract, and it is what lets the conflict get its own recovery path
 * instead of a generic red banner.
 */
function conflictOf(error: unknown): ArtifactErrorBody | null {
  if (!(error instanceof ApiError)) return null
  const body = error.body as unknown as ArtifactErrorBody
  return body?.code === "revision_conflict" ? body : null
}

/** Dynamic keys would defeat the typed message table, so both are explicit. */
const KIND_LABELS: Record<string, MessageKey> = {
  test_cases: "artifact.kind.testCases",
  requirements: "artifact.kind.requirements",
  plan: "artifact.kind.plan",
  change_set_summary: "artifact.kind.changeSet",
}

const STATUS_LABELS: Record<string, MessageKey> = {
  draft: "artifact.status.draft",
  ready: "artifact.status.ready",
  stale: "artifact.status.stale",
  superseded: "artifact.status.superseded",
}

export function kindLabel(kind: string, t: ReturnType<typeof useI18n>["t"]): string {
  const key = KIND_LABELS[kind]
  return key ? t(key) : t("artifact.kind.other")
}

export function statusLabel(status: string, t: ReturnType<typeof useI18n>["t"]): string {
  const key = STATUS_LABELS[status]
  return key ? t(key) : status
}

/**
 * Send a note to this issue's lead thread.
 *
 * Goes through the normal lead-message endpoint so it inherits the bus delivery
 * semantics: the orchestrator steers a live turn instead of starting a second
 * one (which the server accepts and then silently never runs) and confirms the
 * turn actually started. Nothing about node questions justifies a second path.
 */
async function notifyLead(issueId: number, text: string): Promise<void> {
  await api(`/api/issues/${issueId}/message`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ text }),
  })
}

/** A question names the node by its full path, so the lead knows which case. */
function questionText(
  question: NodeQuestion,
  artifact: Artifact,
  t: ReturnType<typeof useI18n>["t"],
): string {
  const where = question.path.join(" › ")
  const lead = t(question.kind === "ask" ? "mindmap.askPrefix" : "mindmap.suggestPrefix", {
    where,
    revision: artifact.revision,
  })
  return `${lead}\n\n${question.text}`
}

export function ArtifactView({
  artifactId,
  onBack,
  onError,
}: {
  artifactId: number
  onBack: () => void
  onError: (error: unknown) => void
}) {
  const { t } = useI18n()
  const [artifact, setArtifact] = React.useState<Artifact | null>(null)
  const [draft, setDraft] = React.useState("")
  const [loading, setLoading] = React.useState(true)
  const [conflict, setConflict] = React.useState<ArtifactErrorBody | null>(null)
  const [mode, setMode] = React.useState<"map" | "source">("map")
  const [fullscreen, setFullscreen] = React.useState(false)

  const load = React.useCallback(async () => {
    const row = await api<Artifact>(`/api/artifacts/${artifactId}`)
    setArtifact(row)
    setDraft(row.content)
    setConflict(null)
    return row
  }, [artifactId])

  React.useEffect(() => {
    let active = true
    setLoading(true)
    load()
      .catch((error) => { if (active) onError(error) })
      .finally(() => { if (active) setLoading(false) })
    return () => { active = false }
  }, [load, onError])

  const dirty = Boolean(artifact) && draft !== artifact?.content
  const isTree = artifact?.format === "markdown_tree"
  const cases = isTree ? caseCount(draft) : 0

  const save = async () => {
    if (!artifact) return
    try {
      const saved = await api<Artifact>(`/api/artifacts/${artifact.id}`, {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({
          expected_revision: artifact.revision,
          content: draft,
          title: artifact.title,
          source: "user",
        }),
      })
      setArtifact(saved)
      setDraft(saved.content)
      setConflict(null)
      await notifyLead(
        saved.issue_id,
        t("mindmap.savedNotice", { revision: saved.revision, title: saved.title }),
      )
    } catch (error) {
      // Someone (the lead, or another window) wrote first. Keep the user's text
      // — discarding it here would lose work — and offer to reload instead.
      const detail = conflictOf(error)
      if (!detail) throw error
      setConflict(detail)
    }
  }

  if (loading) return <div className="app-loading" role="status">{t("app.loading")}</div>
  if (!artifact) return <EmptyState titleKey="artifact.missing" bodyKey="artifact.missingBody" />

  return (
    <section
      className="artifact-view"
      data-fullscreen={fullscreen ? "true" : "false"}
      aria-label={t("artifact.viewLabel", { title: artifact.title })}
    >
      <header className="artifact-header">
        <Button variant="ghost" onClick={onBack}>
          <ArrowLeft aria-hidden="true" />
          {t("artifact.back")}
        </Button>
        <div className="artifact-heading">
          <h2>{artifact.title || kindLabel(artifact.kind, t)}</h2>
          <p className="artifact-meta">
            {statusLabel(artifact.status, t)}
            {" · "}
            {t("artifact.revision", { revision: artifact.revision })}
            {isTree ? ` · ${t("mindmap.caseCount", { count: cases })}` : null}
          </p>
        </div>
        {isTree ? (
          <div className="artifact-header-actions">
            <Button
              variant="ghost"
              aria-pressed={mode === "map"}
              onClick={() => setMode(mode === "map" ? "source" : "map")}
            >
              {mode === "map" ? <PencilLine aria-hidden="true" /> : <Network aria-hidden="true" />}
              {t(mode === "map" ? "mindmap.editSource" : "mindmap.showMap")}
            </Button>
            <Button
              variant="ghost"
              size="icon-sm"
              aria-pressed={fullscreen}
              aria-label={t(fullscreen ? "mindmap.exitFullscreen" : "mindmap.fullscreen")}
              onClick={() => setFullscreen((current) => !current)}
            >
              {fullscreen ? <Minimize2 aria-hidden="true" /> : <Maximize2 aria-hidden="true" />}
            </Button>
          </div>
        ) : null}
      </header>

      {artifact.stale_reason ? (
        <p className="artifact-stale" role="status">{artifact.stale_reason}</p>
      ) : null}

      {conflict ? (
        <div className="artifact-conflict" role="alert">
          <p>
            {t("artifact.conflict", {
              expected: conflict.expectedRevision ?? artifact.revision,
              actual: conflict.actualRevision ?? artifact.revision,
            })}
          </p>
          <AsyncButton
            label={t("artifact.reload")}
            pendingLabel={t("artifact.reloading")}
            onAction={async () => { await load() }}
            onError={onError}
          >
            <RefreshCw aria-hidden="true" />
          </AsyncButton>
        </div>
      ) : null}

      {isTree && mode === "map" ? (
        <div className="artifact-map">
          <MindMap
            content={draft}
            onError={onError}
            onAsk={async (question: NodeQuestion) => {
              await notifyLead(artifact.issue_id, questionText(question, artifact, t))
            }}
          />
        </div>
      ) : (
        <>
          <label className="sr-only" htmlFor="artifact-content">{t("artifact.contentLabel")}</label>
          <textarea
            id="artifact-content"
            className="artifact-content"
            value={draft}
            spellCheck={false}
            onChange={(event) => setDraft(event.target.value)}
          />
        </>
      )}

      <footer className="artifact-actions">
        <AsyncButton
          label={t("artifact.save")}
          pendingLabel={t("artifact.saving")}
          disabled={!dirty}
          onAction={save}
          onError={onError}
        >
          <Save aria-hidden="true" />
        </AsyncButton>
      </footer>
    </section>
  )
}
