// Mirrors the SeaORM models (serde serializes Rust field names as-is: snake_case).

export type Tool = "claude" | "codex" | "opencode" | "none";
export type Role = "write" | "read";
export type ThreadKind = "feature" | "bugfix" | "refactor" | "spike";

export interface Workspace {
  id: number;
  name: string;
  slug: string;
  created_at: string;
}

export interface RepoRef {
  id: number;
  workspace_id: number;
  name: string;
  slug: string;
  local_git_path: string;
  base_ref: string;
  default_tool: string;
}

export interface Thread {
  id: number;
  workspace_id: number;
  title: string;
  slug: string;
  kind: string;
  status: string;
  created_at: string;
}

export interface Direction {
  id: number;
  thread_id: number;
  name: string;
  slug: string;
  tool: string;
  branch: string;
  created_at: string;
}

export interface DirectionRepo {
  id: number;
  direction_id: number;
  repo_id: number;
  role: string;
}

export interface Worktree {
  id: number;
  repo_id: number;
  direction_id: number;
  branch: string;
  path: string;
  created_at: string;
}

export interface SessionInfo {
  session_id: number;
  repo: string;
  worktree: string;
  branch: string;
  tool: string;
  resumed: boolean;
}

/** UI-side runtime status for a live session panel. */
export type SessionStatus =
  | "starting"
  | "running"
  | "waiting-approval"
  | "idle"
  | "exited";

export interface FileDiff {
  path: string;
  added: number;
  removed: number;
}
export interface DiffSummary {
  files: FileDiff[];
}

export interface BusMsg {
  from: string;
  to: string;
  text: string;
  ts: number;
  kind: string;
}

/** The curator's profile of one repo, as the UI sees it (ARCHITECTURE §4.9). */
export interface RepoProfile {
  repo_id: number;
  repo_name: string;
  role: string; // service | app | library | infra | docs | unknown
  stack: string[];
  summary: string;
  published: string[];
  deps: string[];
  source: string; // inferred | user
  profiled_commit: string;
  stale: boolean;
}

/** A directed dependency edge: `from` consumes `to`, evidenced by `via`. */
export interface RepoEdge {
  from: number;
  to: number;
  via: string;
}

export interface RepoGraph {
  nodes: RepoProfile[];
  edges: RepoEdge[];
}

/** The lead's proposed split of a Task into directions (by repo NAME).
 *  Only the WRITE set is scoped — reads are unmanaged (agents read freely). */
export interface ProposedDirection {
  name: string;
  tool: string;
  writes: string[];
}
export interface Proposal {
  rationale: string;
  directions: ProposedDirection[];
}

/** A write repo resolved against the workspace repos, for review/edit. */
export interface ScopeEntry {
  repo_id: number;
  repo_name: string;
  known: boolean;
}
export interface ResolvedDirection {
  name: string;
  tool: string;
  writes: ScopeEntry[];
}
export interface ResolvedProposal {
  thread_id: number;
  rationale: string;
  status: string; // proposed | confirmed
  directions: ResolvedDirection[];
}

/** An open agent→human question, aggregated workspace-wide for "Needs you". */
export interface NeedItem {
  ask_id: number;
  thread_id: number;
  thread_title: string;
  direction_id: number;
  direction_name: string;
  text: string;
  ts: number;
}
