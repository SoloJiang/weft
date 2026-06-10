// Mirrors the SeaORM models (serde serializes Rust field names as-is: snake_case).

export type Tool = "claude" | "codex" | "opencode" | "none";
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

/** One effective skill/rule for a repo, tagged with the layer it comes from
 *  (personal / repo) and whether a higher layer shadows it (§ M6 有效配置). */
export interface ConfigItem {
  name: string;
  kind: "skill" | "rule";
  layer: "personal" | "repo" | "team";
  path: string;
  overridden: boolean;
}

export interface Thread {
  id: number;
  workspace_id: number;
  title: string;
  slug: string;
  kind: string;
  created_at: string;
}

export interface FileDiff {
  path: string;
  added: number;
  removed: number;
}

export interface WorktreeDiff {
  files: FileDiff[];
  patch: string;
}

/** Normalized observe-mode transcript event (from the tool's own sidecar). */
export type NormEvent =
  | { kind: "message"; role: "user" | "assistant"; text: string; ts: string }
  | { kind: "tool"; name: string; summary: string; ts: string };

export interface Direction {
  id: number;
  thread_id: number;
  name: string;
  slug: string;
  tool: string;
  branch: string;
  /** agent/human-driven lifecycle: queued | planning | working | review | done. */
  status: string;
  /** worker mandate: "plan+impl" (plans its direction first) | "impl-only". */
  mandate: string;
  created_at: string;
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
  native_id: string | null;
}

/** Read-only snapshot backing the observe surface (mirrors Rust ObserveRef). */
export interface ObserveRef {
  worktree: string;
  branch: string;
  tool: string;
  session_id: number | null;
  native_id: string | null;
  status: string | null;
}

/** One executable verification rung's result (ARCHITECTURE §4.13). */
export interface CheckResult {
  name: string;
  status: string; // pass | fail
  code: number;
  output_tail: string;
}
export interface RepoChecks {
  repo: string;
  worktree: string;
  checks: CheckResult[];
}

/** One row in a chat timeline (lead console / chat-mode workers). */
export interface LeadMessage {
  id: number;
  thread_id: number;
  session_id: number | null;
  turn_id: number;
  role: "user" | "assistant" | "system";
  kind: "text" | "tool" | "command" | "proposal" | "approval" | "worker_event" | "meta";
  /** kind-shaped JSON string, e.g. {"text": "..."} for kind=text */
  content: string;
  status: "streaming" | "complete" | "interrupted" | "error" | "queued";
  created_at: string;
}

/** Incremental pushes on the `lead-chat` Tauri event (engine → UI). */
export type LeadChatPush =
  | { type: "message"; thread_id: number; message: LeadMessage }
  | { type: "delta"; thread_id: number; message_id: number; text: string }
  | { type: "finalize"; thread_id: number; message_id: number; status: string }
  | {
      type: "turn";
      thread_id: number;
      session_id: number | null;
      state: "busy" | "idle" | "stopped";
      queued: number;
    }
  | {
      type: "init";
      thread_id: number;
      session_id: number | null;
      native_id: string;
      slash_commands: string[];
    }
  | {
      /** The tool call currently executing — transient, cleared by `turn`. */
      type: "activity";
      thread_id: number;
      session_id: number | null;
      name: string;
      summary: string;
    };

/** One composer attachment heading to the engine (pasted or picked image). */
export interface ImageAttachment {
  media_type: string;
  /** base64 payload, no data-URI prefix. */
  data: string;
}

/** Snapshot of the lead engine, for mount-time hydration. */
export interface LeadStateInfo {
  state: "busy" | "idle" | "stopped";
  queued: number;
  native_id: string | null;
  slash_commands: string[];
  cwd: string;
}

/** UI-side runtime status for a live session panel. */
export type SessionStatus = "running" | "idle" | "exited";

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

/** The lead's proposed split of a Task into directions: ONE write repo each
 *  (by NAME) plus the required reason — reads are unmanaged (scope rework). */
export interface ProposedDirection {
  name: string;
  tool: string;
  repo: string;
  reason: string;
  mandate?: string;
  decision?: string;
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
  repo: ScopeEntry;
  reason: string;
  /** "plan+impl" | "impl-only" */
  mandate: string;
  decision: string;
}
export interface ResolvedProposal {
  thread_id: number;
  rationale: string;
  status: string; // proposed | confirmed
  directions: ResolvedDirection[];
}

/** A thread's roll-up for the workspace board (cards = threads). */
/** A locally-installed coding-agent CLI, for Settings' default-tool picker. */
export interface ToolStatus {
  tool: string;
  installed: boolean;
  version: string | null;
  path: string | null;
  meets_min: boolean;
}

export interface ThreadOverview {
  thread_id: number;
  title: string;
  kind: string;
  direction_ids: number[];
  /** stored lifecycle status per direction (same order as direction_ids). */
  statuses: string[];
  write_repos: { id: number; name: string }[];
}

/** A tool's permission request, blocked on the human (the Ask Bridge §4.3). */
export interface PermissionAsk {
  id: number;
  thread: number;
  dir: string;
  tool: string;
  summary: string;
  detail: string;
  ts: number;
  /** owning thread title + asking task name, for context on the card. */
  thread_title: string;
  dir_name: string;
}

/** A lead-proposed write declaration awaiting human approve/deny (Needs you). */
export interface WriteTrigger {
  thread_id: number;
  thread_title: string;
  index: number;
  name: string;
  repo_name: string;
  reason: string;
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
