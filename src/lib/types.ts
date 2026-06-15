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
  /** The CLI the lead engine runs (claude | codex | opencode). */
  lead_tool: string;
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

/** A live worker engine the backend is running, mirrored into the frontend
 *  session map so boot-revived / reload-survived workers get a status dot +
 *  auto-verify. Mirrors Rust `LiveWorkerSlot`. */
export interface LiveWorkerSlot {
  info: SessionInfo;
  direction_id: number;
  repo_id: number;
  thread_id: number;
  busy: boolean;
  queued: number;
}

/** Session ref backing the worker conversation surface (mirrors Rust ObserveRef). */
export interface ObserveRef {
  worktree: string;
  branch: string;
  tool: string;
  session_id: number | null;
  native_id: string | null;
  status: string | null;
  // —— 会话信息面板回填 ——
  context_tokens: number | null;
  window: number | null;
  model: string | null;
  mcp_servers: { name: string; status: string }[];
  /** claude 引擎缓存的扁平 tools(`mcp__server__tool`);codex/opencode 为空。 */
  tools: string[];
}

/** One MCP server row in the session info panel. */
export interface McpServerInfo {
  name: string;
  status: string;
  /** claude 才有:由 init.tools 按 mcp__<server>__<tool> 分组;codex/opencode 为空。 */
  tools: string[];
}

/** A skill the running engine actually has (codex), shown in the session panel. */
export interface EngineSkill {
  name: string;
  description: string;
}

/** Per-session snapshot for the session info panel
 *  (lead 按 thread_id、worker 按 session_id 存)。 */
export interface SessionMeta {
  contextTokens?: number;
  window?: number;
  model?: string;
  mcpServers: McpServerInfo[];
  /** codex 的真实 skill,带外 `session_meta` 填;claude 不填。 */
  engineSkills?: EngineSkill[];
  /** codex 的思考强度(low/medium/high/…)。 */
  reasoningEffort?: string;
}

/** Band-outside meta for codex/opencode workers (M2), from the `session_meta`
 *  command. 只列 server(无 tools 目录)。`mcp_servers` 是 Option:`null` = 探测失败
 *  (前端保留旧行);非 null = 权威(即使空数组也替换/清空)。 */
export interface SessionMetaSnapshot {
  context_tokens: number | null;
  window: number | null;
  model: string | null;
  mcp_servers: { name: string; status: string }[] | null;
  /** codex 真实 skill;`null` = 没探到(保留旧行),非 null = 权威列表。 */
  skills: { name: string; description: string }[] | null;
  /** codex 思考强度;`null` = 未配置。 */
  reasoning_effort: string | null;
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
  kind:
    | "text"
    | "tool"
    | "command"
    | "proposal"
    | "approval"
    | "worker_event"
    | "meta"
    | "action_card"
    | "settled";
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
      slash_commands: SlashCmd[];
      mcp_servers: { name: string; status: string }[];
      tools: string[];
      model: string | null;
      window: number | null;
    }
  | {
      /** 每个 turn 结束推一次当前上下文占用。 */
      type: "usage";
      thread_id: number;
      session_id: number | null;
      context_tokens: number;
      window: number | null;
      model: string | null;
    }
  | {
      /** The tool call currently executing — transient, cleared by `turn`.
       *  Used for codex pills, which have no input/output to expand. */
      type: "activity";
      thread_id: number;
      session_id: number | null;
      name: string;
      summary: string;
    }
  | {
      /** A persisted `kind:"tool"` row received its result: replace the row's
       *  content (now carrying output) and its status. */
      type: "tool_result";
      thread_id: number;
      message_id: number;
      content: string;
      status: string;
    };

/** One slash command for the composer palette: the token plus whatever metadata
 *  the CLI reported (claude adds description + arg hint; opencode adds a
 *  description). `name` is the match + dispatch key. */
export interface SlashCmd {
  name: string;
  description?: string;
  arg_hint?: string;
}

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
  slash_commands: SlashCmd[];
  cwd: string;
  // —— 会话信息面板回填 ——
  context_tokens: number | null;
  window: number | null;
  model: string | null;
  mcp_servers: { name: string; status: string }[];
  /** claude 引擎缓存的扁平 tools(`mcp__server__tool`);codex/opencode 为空。 */
  tools: string[];
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
/** Why a CLI is missing / unusable / outdated, for the diagnostics panel. */
export interface ToolDiagnostic {
  kind:
    | "MissingTarget"
    | "NotExecutable"
    | "SpawnFailed"
    | "VersionProbeFailed"
    | "BelowMinimum";
  message: string;
}

/** A locally-installed coding-agent CLI, for Settings' default-tool picker. */
export interface ToolStatus {
  tool: string;
  installed: boolean;
  version: string | null;
  path: string | null;
  meets_min: boolean;
  diagnostics: ToolDiagnostic[];
}

export interface SkillSource {
  id: number;
  git_url: string;
  git_ref: string;
  last_synced: string;
  last_status: string; // "never" | "ok" | "error:<msg>"
}
export interface ParsedSkill {
  name: string;
  description: string;
  dir: string;
}
export interface EnabledSkill {
  source_id: number;
  name: string;
  description: string;
  dir: string;
  overridden: boolean;
  global: boolean;
}

/** The resolved default coding tool plus the user's explicit choice (if any). */
export interface DefaultToolInfo {
  tool: string;
  configured: string | null;
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

/** IM 话题绑定行：issue ↔ 飞书话题 1:1 映射（M2-5）。 */
export interface ImRoute {
  thread_id: number;
  channel: string;
  chat_id: string;
  im_thread_ref: string;
  created_at: string;
}

/** Backup config + last-run telemetry surfaced to the Settings panel. Mirrors
 *  `commands_backup::BackupStatusDto` (Rust uses serde rename_all = camelCase). */
export interface BackupStatusDto {
  enabled: boolean;
  remoteUrl: string;
  autoBackupEnabled: boolean;
  backupOnExit: boolean;
  intervalSeconds: number;
  lastBackupAt: string | null;
  lastBackupCommitSha: string | null;
  lastBackupBytes: number | null;
  lastError: string | null;
}
