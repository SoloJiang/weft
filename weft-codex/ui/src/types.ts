export const STATUSES = ["queued", "working", "review", "done"] as const
export const ISSUE_KINDS = ["feature", "bugfix", "refactor", "spike"] as const

export type DirectionStatus = (typeof STATUSES)[number]
export type IssueKind = (typeof ISSUE_KINDS)[number]
export type AppView = "kanban" | "repos" | "issue" | "artifact"
export type MessageIntent = "message" | "continue"

export interface Workspace {
  id: number
  name: string
  slug: string
  created_at: string
}

export interface Repo {
  id: number
  workspace_id: number
  name: string
  path: string
  base_ref: string
  remote_url: string
  base_ref_is_default: number
  created_at: string
}

export interface RepoImportItem {
  requested_path: string
  status: "added" | "existing" | "error"
  repo?: Repo
  error?: string
}

export interface RepoImportResponse {
  results: RepoImportItem[]
  added: number
  existing: number
  failed: number
  analysisQueued: boolean
}

export interface RepoComponent {
  name: string
  path: string
  summary: string
}

export interface Issue {
  id: number
  workspace_id: number
  title: string
  slug: string
  kind: IssueKind
  lead_codex_thread_id: string
  lead_attention: number
  lead_attention_reason: string
  created_at: string
}

export interface Direction {
  id: number
  issue_id: number
  name: string
  slug: string
  branch: string
  status: DirectionStatus
  repo_id: number
  reason: string
  mandate: string
  target_branch: string
  base_branch: string
  spec: string
  codex_thread_id: string
  attention: number
  attention_reason: string
  created_at: string
}

export interface BoardEntry {
  issue: Issue
  directions: Direction[]
  threads: ThreadBinding[]
  artifacts: ArtifactSummary[]
}

export type ArtifactStatus = "draft" | "ready" | "stale" | "superseded"

/** What the board carries: enough to show an artifact exists, never its body. */
export interface ArtifactSummary {
  id: number
  issue_id: number
  kind: string
  title: string
  format: string
  revision: number
  status: ArtifactStatus
  stale_reason: string
  updated_at: string
}

export interface Artifact extends ArtifactSummary {
  content: string
  source: string
  source_thread_id: string
  created_at: string
}

/**
 * The structured half of an artifact API failure. The `code` is the contract —
 * never branch on `error`, which is prose meant for humans.
 */
export interface ArtifactErrorBody {
  code: string
  error: string
  expectedRevision?: number
  actualRevision?: number
  limit?: number
  actual?: number
}

export interface ThreadBinding {
  thread_id: string
  issue_id: number
  direction_id: number | null
  parent_thread_id: string
  root_thread_id: string
  title: string
  is_primary: number
  created_at: string
  updated_at: string
}

export interface ThreadLocationResponse {
  binding: ThreadBinding | null
  workspaceId: number | null
}

export interface RepoProfile {
  repo_id: number
  run_state: "idle" | "running" | "failed" | "done"
  run_error: string
  tier: string
  stack: string
  summary: string
  components: string
  layer: string
  layer_rank: number
  codex_thread_id: string
  updated_at: string
}

export interface RepoMapEntry {
  repo: Repo
  profile: RepoProfile | null
}

export interface RepoRelation {
  id: number
  workspace_id: number
  from_repo: string
  to_repo: string
  kind: string
  via: string
  confidence: number
  rationale: string
}

export interface RepoMap {
  repos: RepoMapEntry[]
  relations: RepoRelation[]
  repoMap: string
}

export interface BusMessage {
  id: number
  from_party: string
  to_party: string
  text: string
  kind: string
  ts: string
}

export type DialogState =
  | { type: "workspace" }
  | { type: "issue" }
  | { type: "repositories" }
  | { type: "message"; target: "lead" | "task"; id: number; intent: MessageIntent }
  | null

export type ActiveDialogState = Exclude<DialogState, null>

export type DialogSubmission =
  | { type: "workspace"; name: string }
  | { type: "issue"; title: string; kind: IssueKind }
  | { type: "repositories"; paths: string[] }
  | {
      type: "message"
      target: "lead" | "task"
      id: number
      text: string
      intent: MessageIntent
    }

export type ToastKind = "info" | "success" | "error"

export interface UiState {
  lastWorkspaceId: number | null
  detailPaneWidth: number | null
}
