import { Boxes, GitBranch, Plus, RefreshCw } from "lucide-react"

import { Button } from "@/components/ui/button"
import { useI18n } from "@/i18n"
import type { RepoMap, RepoMapEntry } from "@/types"
import { AsyncButton, EmptyState, parseComponents, parseStack } from "./shared"

function RepositoryCard({
  entry,
  onAnalyze,
  onError,
}: {
  entry: RepoMapEntry
  onAnalyze: (id: number) => Promise<void>
  onError: (error: unknown) => void
}) {
  const { t } = useI18n()
  const { repo, profile } = entry
  const stateKey = profile ? `repo.state.${profile.run_state}` as const : "repo.state.notAnalyzed" as const
  const components = parseComponents(profile?.components ?? "")
  let analyzeLabel = t("repo.analyze")
  if (profile?.run_state === "failed") analyzeLabel = t("repo.retryAnalysis")
  else if (profile?.run_state === "done") analyzeLabel = t("repo.reanalyze")

  return (
    <article className="repo">
      <header className="repo-head">
        <div className="repo-identity">
          <h2>{repo.name}</h2>
          <code className="path">{repo.path}</code>
          <span className="repo-base"><GitBranch aria-hidden="true" />{repo.base_ref}</span>
        </div>
        <div className="repo-actions">
          <span className={`runstate runstate-${profile?.run_state ?? "idle"}`}>{t(stateKey)}</span>
          <AsyncButton
            variant="ghost"
            disabled={profile?.run_state === "running"}
            label={analyzeLabel}
            pendingLabel={t("loading.analyzingRepo")}
            onAction={() => onAnalyze(repo.id)}
            onError={onError}
          >
            <RefreshCw aria-hidden="true" />
          </AsyncButton>
        </div>
      </header>
      {profile?.run_error ? <p className="repo-error">{profile.run_error}</p> : null}
      {profile?.summary ? <p className="summary">{profile.summary}</p> : null}
      {profile ? (
        <div className="tags">
          {profile.tier ? <span className="tag">{profile.tier}</span> : null}
          {profile.layer ? <span className="tag layer">{profile.layer} #{profile.layer_rank}</span> : null}
          {parseStack(profile.stack).map((item) => <span key={item} className="tag">{item}</span>)}
        </div>
      ) : null}
      {components.length ? (
        <details className="repo-components">
          <summary><Boxes aria-hidden="true" />{t("repo.components", { count: components.length })}</summary>
          <div className="repo-component-list">
            {components.map((component) => (
              <section key={`${component.path}:${component.name}`} className="repo-component">
                <div>
                  <strong>{component.name}</strong>
                  <code>{component.path}</code>
                </div>
                {component.summary ? <p>{component.summary}</p> : null}
              </section>
            ))}
          </div>
        </details>
      ) : null}
    </article>
  )
}

export function RepositoriesView({
  workspaceId,
  repoMap,
  onCreateWorkspace,
  onOpenImport,
  onAnalyzeRepository,
  onError,
}: {
  workspaceId: number | null
  repoMap: RepoMap | null
  onCreateWorkspace: () => void
  onOpenImport: () => void
  onAnalyzeRepository: (id: number) => Promise<void>
  onError: (error: unknown) => void
}) {
  const { t } = useI18n()
  const entries = repoMap?.repos ?? []
  const hasRepos = entries.length > 0

  return (
    <section className="view active" aria-labelledby="repos-heading">
      {workspaceId ? (
        <header className="view-heading repo-view-heading">
          <div>
            <h1 id="repos-heading">{t("nav.repos")}</h1>
            <p>{t("repo.pageDescription")}</p>
          </div>
          <Button onClick={onOpenImport}>
            <Plus aria-hidden="true" />
            {t("repo.add")}
          </Button>
        </header>
      ) : <h1 id="repos-heading" className="sr-only">{t("nav.repos")}</h1>}

      <div id="repo-list">
        {!workspaceId ? (
          <EmptyState titleKey="empty.workspaceTitle" bodyKey="empty.workspaceBody" actionKey="empty.workspaceAction" onAction={onCreateWorkspace} />
        ) : null}
        {workspaceId && !hasRepos ? (
          <EmptyState
            titleKey="empty.reposTitle"
            bodyKey="empty.reposBody"
            actionKey="empty.reposAction"
            onAction={onOpenImport}
          />
        ) : null}
        {entries.map((entry) => <RepositoryCard key={entry.repo.id} entry={entry} onAnalyze={onAnalyzeRepository} onError={onError} />)}
        {repoMap?.relations.length ? (
          <section className="relations">
            <h2 className="section-title">{t("repo.relations")}</h2>
            {repoMap.relations.map((relation) => (
              <div key={relation.id} className="rel">
                <strong>{relation.from_repo} → {relation.to_repo}</strong>
                <span>{relation.kind} · {relation.confidence}%</span>
                <p>{relation.rationale}</p>
              </div>
            ))}
          </section>
        ) : null}
      </div>

      {workspaceId && hasRepos ? (
        <section className="repo-map-section" aria-labelledby="repo-map-heading">
          <h2 id="repo-map-heading">{t("repo.mapDoc")}</h2>
          {repoMap?.repoMap ? <pre id="repo-map-doc">{repoMap.repoMap}</pre> : <p className="meta">{t("repo.mapPending")}</p>}
        </section>
      ) : null}
    </section>
  )
}
