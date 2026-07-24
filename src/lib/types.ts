// Mirrors the SeaORM models (serde serializes Rust field names as-is: snake_case).

export type Tool = "claude" | "codex" | "opencode" | "none";
export type ThreadKind = "feature" | "bugfix" | "refactor" | "spike";

/** Process-pressure state reported by the backend governor. The frontend renders
 *  this discriminant as-is; threshold and hysteresis decisions stay in Rust. */
export type ProcessQuotaLevel = "normal" | "warning" | "degraded";

/** App-wide process quota snapshot. Mirrors the governor's camelCase DTO and is
 *  also the payload of `process-quota://changed`. */
export interface ProcessQuotaStatus {
  status: ProcessQuotaLevel;
  processCount: number;
  processLimit: number | null;
  usagePercent: number | null;
  warningPercent: number;
  degradedPercent: number;
  recoveryPercent: number;
  /** Monotonic state-transition sequence used to reject stale fetch/event races. */
  transitionSeq: number;
}

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
  /** Captured `origin` remote URL ("" for a local repo with no origin). Lets the
   *  add dialog pre-flag pasted URLs already in the workspace; the backend is the
   *  authority for dedup. */
  remote_url: string;
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

/** "vs target" diff: like WorktreeDiff, plus the ref actually compared against
 *  and the per-task target editor's current/default values (one round-trip). */
export interface TargetDiff {
  files: FileDiff[];
  patch: string;
  /** Ref compared against, e.g. "origin/main". */
  resolved: string;
  /** Stored per-task target branch ("" = using the default). */
  target: string;
  /** Effective default (the repo's base branch), shown as the placeholder. */
  default_branch: string;
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
  /** the ref the work branch was created off; "" = the repo's default branch. */
  base_branch: string;
  created_at: string;
}

export interface Worktree {
  id: number;
  repo_id: number;
  direction_id: number;
  branch: string;
  path: string;
  created_at: string;
  /** Whether the worktree directory is still present on disk (backend-checked). */
  exists: boolean;
}

export interface SessionInfo {
  session_id: number;
  repo: string;
  worktree: string;
  branch: string;
  tool: string;
  /** Effective binary for the resume command (configured alias, else `tool`). */
  command: string;
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
  queue: QueuedItem[];
}

/** Session ref backing the worker conversation surface (mirrors Rust ObserveRef). */
export interface ObserveRef {
  worktree: string;
  branch: string;
  tool: string;
  /** Effective binary for the resume command (configured alias, else `tool`). */
  command: string;
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
  /** true 当 mcpServers 来自权威来源(claude init / 带外探测的非 null 结果)——
   *  权威的空列表是"确实没有 MCP",引擎/持久化快照的补洞不得复活旧行。 */
  mcpAuthoritative?: boolean;
  /** 引擎自有的 skill,带外 `session_meta` 填:codex 走 `skills/list`,claude 扫会话 cwd
   *  的 skill 目录(`.claude`/`.agents`);opencode 无对等,留空。 */
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
  /** 引擎 skill(codex `skills/list` / claude 扫 cwd skill 目录);`null` = 没探到
   *  (保留旧行),非 null = 权威列表(空数组即清空)。 */
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
    | "plan_card"
    | "test_cases"
    | "settled"
    /** Marker row the backend inserts where a conversation rewind truncated the
     *  timeline; content is {"from_message_id": number, "deleted": number}. */
    | "rewind";
  /** kind-shaped JSON string, e.g. {"text": "..."} for kind=text */
  content: string;
  status: "streaming" | "complete" | "interrupted" | "error" | "queued";
  /** Delivery-order key assigned when a queued row is actually handed to the
   *  agent. Null/absent rows retain their insertion id as the order key. */
  seq?: number | null;
  /** Engine-side rewind anchor (claude assistant uuid / codex turn id), recorded
   *  on the user row that opened a turn. Backend-owned; the UI never reads it. */
  native_anchor?: string | null;
  created_at: string;
}

/** An issue's test-case document (0..1 per thread): markdown tree derived by
 *  the lead in phase 1.5 and editable in the TestPlanPanel. */
export interface TestPlan {
  id: number;
  thread_id: number;
  /** Markdown tree — `#` title + nested unordered lists; leaves are cases. */
  content: string;
  /** Last writer: "lead" | "user". */
  source: string;
  updated_at: string;
}

/** Incremental pushes on the `lead-chat` Tauri event (engine → UI). */
export type LeadChatPush =
  | { type: "message"; thread_id: number; message: LeadMessage }
  | { type: "delta"; thread_id: number; message_id: number; text: string }
  | {
      type: "finalize";
      thread_id: number;
      message_id: number;
      status: string;
      /** Cleaned final text, present only when sentinels were stripped after they
       *  streamed raw — the row content is replaced so the tags vanish live. */
      content?: string;
      /** Present when a queued row was delivered and must move from enqueue order
       *  to its authoritative transcript position immediately. */
      seq?: number;
    }
  | {
      type: "turn";
      thread_id: number;
      session_id: number | null;
      state: TurnState;
      /** True only for the watchdog's stall→busy recovery push — the UI keeps the
       *  running-tool label on recovery but still clears it on a real/promoted turn. */
      recovered?: boolean;
      queue: QueuedItem[];
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
    }
  | {
      /** A conversation rewind truncated this thread's rows for one session
       *  (null = lead console): reload the thread's messages. Carries the
       *  session's NEW native id (null = fresh native session on next send) so
       *  live session state (Open App / Take Over) can't point at the
       *  abandoned pre-rewind conversation. */
      type: "rewound";
      thread_id: number;
      session_id: number | null;
      native_id: string | null;
    };

/** Rewind scope for `chat_rewind`: the conversation rows, the worktree code
 *  (files restored to before the message, uncommitted changes overwritten with
 *  a safety snapshot kept), or both. The lead console only ever uses
 *  "conversation" (via `lead_rewind`, which takes no mode). */
export type RewindMode = "conversation" | "code" | "both";

/** Result of `chat_rewind` / `lead_rewind`: the selected message's text (goes
 *  back into the composer for edit/resend — only meaningful when the
 *  conversation was rewound), how many rows were deleted, the session's new
 *  native id (null = a fresh native session starts on next send), and whether
 *  worktree files were restored. */
export interface RewindOutcome {
  rewound_text: string;
  deleted: number;
  native_id: string | null;
  code_restored: boolean;
}

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
  state: TurnState;
  queue: QueuedItem[];
  native_id: string | null;
  slash_commands: SlashCmd[];
  cwd: string;
  /** Effective binary for the lead's resume command (alias, else identity). */
  command: string;
  // —— 会话信息面板回填 ——
  context_tokens: number | null;
  window: number | null;
  model: string | null;
  mcp_servers: { name: string; status: string }[];
  /** claude 引擎缓存的扁平 tools(`mcp__server__tool`);codex/opencode 为空。 */
  tools: string[];
}

/** UI-side runtime status for a live session panel. `stalled` = a busy turn gone
 * silent past the stall-hint threshold (still in-flight, just not progressing). */
export type SessionStatus = "running" | "stalled" | "idle" | "exited";

/** Engine turn state as pushed by the backend / held per session. `stalled` =
 * busy but silent past the stall hint; `stopped` = no live engine (UI default). */
export type TurnState = "busy" | "stalled" | "idle" | "stopped";

export interface FileDiff {
  path: string;
  added: number;
  removed: number;
}
export interface DiffSummary {
  files: FileDiff[];
}
export interface FileNode {
  path: string;
  name: string;
  kind: "file" | "directory";
  children?: FileNode[];
}
export interface FileTree {
  nodes: FileNode[];
  truncated: boolean;
  total: number;
}


export interface BusMsg {
  from: string;
  to: string;
  text: string;
  ts: number;
  kind: string;
}

/** One monorepo sub-component the agent surfaced, for the map's expanded view. */
export interface RepoComponent {
  name: string;
  path: string;
  /** frontend | backend | "" (unclassified). */
  tier: string;
  summary: string;
  /** Names of sibling components (same repo) this one depends on. */
  deps: string[];
  /** Feature domains owned by this component (agent-assigned). */
  domains?: string[];
}

/** The curator's profile of one repo, as the UI sees it (ARCHITECTURE §4.9).
 *  The curator is agent-only: `tier` comes from the deep per-repo pass and is ""
 *  (with `analyzed=false`) until the agent classifies the repo. */
export interface RepoProfile {
  repo_id: number;
  repo_name: string;
  /** frontend | backend | "" (unclassified / analyzing). */
  tier: string;
  stack: string[];
  summary: string;
  source: string; // agent | user | "" (placeholder)
  profiled_commit: string;
  /** false = the agent hasn't classified this repo yet (placeholder node). */
  analyzed: boolean;
  /** Monorepo sub-components (empty for a single-purpose repo). */
  components: RepoComponent[];
  /** Live analysis lifecycle (run-state registry): "idle" | "running" | "failed".
   *  Distinct from `analyzed`: an unanalyzed repo may be idle, running, or failed —
   *  the detail panel renders each differently instead of one eternal spinner. */
  analysis_state: "idle" | "running" | "failed";
  /** Error from the last failed analysis (set only when analysis_state === "failed"). */
  analysis_error?: string | null;
  /** Role category within the tier (free-text, agent-assigned). "" until classified. */
  category: string;
  /** Feature domains owned by this repo (agent-assigned). */
  domains: string[];
  /** Architectural layer label, assigned by the cross-repo curator pass (it sees the
   *  whole workspace, so labels are consistent). The repo map's band header text.
   *  "" until the cross-repo analysis has run. */
  layer: string;
  /** Vertical rank of `layer` (higher = closer to the user; 0 = foundation). The map
   *  stacks bands by this; same-layer repos share a rank. 0 until classified. */
  layer_rank: number;
}

/** A directed dependency edge: `from` consumes `to`, evidenced by `via`. */
export interface RepoEdge {
  from: number;
  to: number;
  via: string;
  /** Relationship kind: "lib" (declared package dep) | "http" | "grpc" | "queue"
   *  | "infra". Optional for backward compat with pre-curator payloads. */
  kind?: string;
  /** "agent" (inferred) | "user" (human-pinned). */
  source?: string;
  /** Confidence 0–100. */
  confidence?: number;
  /** Free-text rationale explaining why this dependency exists (agent-supplied). */
  rationale?: string;
}

export interface RepoGraph {
  nodes: RepoProfile[];
  edges: RepoEdge[];
}

/** One item waiting in the engine's send queue (mirrors Rust QueuedItem). */
export interface QueuedItem {
  id: number;
  text: string;
  images: number;
  files: number;
  /** True when the original send carried files or images; disables inline edit. */
  has_attachments: boolean;
}

/** The lead's proposed split of a Task into directions: ONE write repo each
 *  (by NAME) plus the required reason — reads are unmanaged (scope rework). */
export interface ProposedDirection {
  name: string;
  tool: string;
  repo: string;
  reason: string;
  mandate?: string;
  base_branch?: string;
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
  /** the chosen base branch; "" = the repo's default branch. */
  base_branch: string;
  decision: string;
}
export interface ResolvedProposal {
  thread_id: number;
  rationale: string;
  status: string; // proposed | confirmed
  /**
   * Proposal version ("last proposed at"): bumped on every re-proposal (R50-2). Used to reset a
   * dirty base-branch edit on ANY re-proposal, even one with the same name/repo/base.
   */
  created_at: string;
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

/** A persisted "full access" grant: every ask from this (thread, dir) auto-allows. */
export interface FullGrant {
  thread: number;
  dir: string;
}

/** A persisted "always allow" grant: this exact `summary` from (thread, dir). */
export interface AlwaysGrant {
  thread: number;
  dir: string;
  summary: string;
}

/** Standing authorization grants that persist across restarts (Ask Bridge). The
 *  board marks issues whose access was inherited and offers a one-click revoke. */
export interface GrantSnapshot {
  full: FullGrant[];
  always: AlwaysGrant[];
}

/** A lead-proposed write declaration awaiting human approve/deny (Needs you). */
export interface WriteTrigger {
  thread_id: number;
  thread_title: string;
  index: number;
  name: string;
  repo_name: string;
  reason: string;
  /** the lead's chosen base branch for this write; "" = the repo's default branch. */
  base_branch: string;
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
  /** `false` for a display-only NOTICE (the self-clearing stall hint) — rendered
   * without an answer box; answering is refused backend-side. */
  answerable: boolean;
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
