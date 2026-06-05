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
