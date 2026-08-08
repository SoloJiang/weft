import { invoke } from "@tauri-apps/api/core";
import { open as openDialog } from "@tauri-apps/plugin-dialog";
import type {
  BackupStatusDto,
  AttentionSnapshot,
  AuthorityPolicyRevision,
  AuthorityPolicyRules,
  BusMsg,
  ConfigItem,
  DefaultToolInfo,
  Direction,
  EnabledSkill,
  EvidenceRow,
  FileTree,
  GrantSnapshot,
  ImageAttachment,
  ImRoute,
  IssueReadinessDto,
  LaneGate,
  LeadMessage,
  LeadStateInfo,
  LiveWorkerSlot,
  ObserveRef,
  ParsedSkill,
  PlanRevision,
  ProcessQuotaStatus,
  Proposal,
  ReadOnlyGrants,
  RepoChecks,
  RepoGraph,
  RepoRef,
  ResolvedProposal,
  ResourceDashboardSnapshot,
  RewindMode,
  RewindOutcome,
  SessionInfo,
  SessionMetaSnapshot,
  SkillSource,
  SlashCmd,
  SwitchOutcome,
  TestPlan,
  Thread,
  ThreadOverview,
  ToolStatus,
  Workspace,
  Worktree,
  WorktreeDiff,
  TargetDiff,
} from "./types";

export interface RepoActionCommandResult {
  execution_outcome: "freshly_completed" | "replayed" | "in_progress";
  repo: RepoRef | null;
}

// Tauri converts camelCase command args to snake_case Rust params.

export const api = {
  listWorkspaces: () => invoke<Workspace[]>("list_workspaces"),
  createWorkspace: (name: string) =>
    invoke<Workspace>("create_workspace", { name }),
  renameWorkspace: (workspaceId: number, name: string) =>
    invoke<Workspace>("rename_workspace", { workspaceId, name }),
  deleteWorkspace: (workspaceId: number) =>
    invoke<void>("delete_workspace", { workspaceId }),
  ensureDefaultWorkspace: () =>
    invoke<number>("ensure_default_workspace"),

  listRepos: (workspaceId: number) =>
    invoke<RepoRef[]>("list_repos", { workspaceId }),
  addRepoRef: (
    workspaceId: number,
    name: string,
    localGitPath: string,
    guard?: { threadId: number; messageId: number; actionId: string; actionKind: string },
  ) => invoke<RepoActionCommandResult>("add_repo_ref", { workspaceId, name, localGitPath, ...guard }),
  checkGitRepo: (path: string) =>
    invoke<boolean>("check_git_repo", { path }),
  cloneRepo: (
    workspaceId: number,
    url: string,
    dest: string,
    name: string,
    guard?: { threadId: number; messageId: number; actionId: string; actionKind: string },
  ) => invoke<RepoActionCommandResult>("clone_repo", { workspaceId, url, dest, name, ...guard }),
  createRepo: (
    workspaceId: number,
    name: string,
    dest: string,
    guard?: { threadId: number; messageId: number; actionId: string; actionKind: string },
  ) => invoke<RepoActionCommandResult>("create_repo", { workspaceId, name, dest, ...guard }),
  /** Best-effort hidden delivery to the lead; resolves false when the lead
   *  ignored it (stopped/dead engine) so callers can keep follow-up UI honest. */
  postLeadToolResult: (threadId: number, payload: unknown, lang: string) =>
    invoke<boolean>("post_lead_tool_result", { threadId, payload, lang }),
  approvePlanCard: (
    threadId: number,
    messageId: number,
    lang: string,
    allowProposedScope = false,
  ) => invoke<boolean>("approve_plan_card", { threadId, messageId, lang, allowProposedScope }),
  /** The issue's test-case document (null = never derived). */
  getTestPlan: (threadId: number) =>
    invoke<TestPlan | null>("get_test_plan", { threadId }),
  /** Save a panel edit (source="user"); the caller separately posts
   *  `test_cases_updated` feedback so the lead learns the new content. */
  saveTestPlan: (threadId: number, content: string) =>
    invoke<void>("save_test_plan", { threadId, content }),
  resolveActionCard: (messageId: number, name: string) =>
    invoke<void>("resolve_action_card", { messageId, name }),

  // Repo map (curator): profiles + cross-repo dependency graph.
  repoGraph: (workspaceId: number) =>
    invoke<RepoGraph>("repo_graph", { workspaceId }),
  // "Analyze deps": run a deterministic FORCED analysis pass (cancellable). Unlike the
  // auto pass, a forced pass retries repos stuck in `failed` (a first analysis that hit
  // a transient error). The promise resolves when the pass completes (or is cancelled)
  // with a report the caller surfaces (all-checkouts-missing, or repos left unanalyzed).
  reanalyzeWorkspaceDeps: (workspaceId: number) =>
    invoke<{ all_missing: boolean; cancelled: boolean; unanalyzed: string[] }>(
      "reanalyze_workspace_deps",
      { workspaceId },
    ),
  // Stop an in-flight "Analyze deps" forced pass (bails at the next safe point).
  cancelReanalyzeWorkspaceDeps: (workspaceId: number) =>
    invoke<void>("cancel_reanalyze_workspace_deps", { workspaceId }),
  // Remove a repo from its workspace (ref + profile + bound tasks + worktrees).
  // The user's actual repository on disk is left untouched.
  deleteRepo: (repoId: number) =>
    invoke<void>("delete_repo", { repoId }),
  // Get-or-create the workspace's hidden curator-chat thread; returns its id.
  openCuratorChat: (workspaceId: number) =>
    invoke<number>("open_curator_chat", { workspaceId }),
  // Fetch the latest markdown repo-map doc for a workspace; null before first analysis.
  getRepoMapDoc: (workspaceId: number) =>
    invoke<string | null>("get_repo_map_doc", { workspaceId }),
  // Calibrate a repo's profile. Pass only the field the user changed; the other
  // stays `null` so editing the summary doesn't pin the tier and vice versa.
  updateRepoProfile: (repoId: number, summary: string | null, tier: string | null) =>
    invoke<void>("update_repo_profile", { repoId, summary, tier }),

  listThreads: (workspaceId: number) =>
    invoke<Thread[]>("list_threads", { workspaceId }),
  workspaceOverview: (workspaceId: number) =>
    invoke<ThreadOverview[]>("workspace_overview", { workspaceId }),
  createThread: (workspaceId: number, title: string, kind: string) =>
    invoke<Thread>("create_thread", { workspaceId, title, kind }),
  renameThread: (threadId: number, title: string) =>
    invoke<Thread>("rename_thread", { threadId, title }),
  deleteThread: (threadId: number) =>
    invoke<void>("delete_thread", { threadId }),

  listDirections: (threadId: number) =>
    invoke<Direction[]>("list_directions", { threadId }),
  issueReadiness: (threadId: number) =>
    invoke<IssueReadinessDto>("issue_readiness", { threadId }),
  setTaskStatus: (directionId: number, status: string) =>
    invoke<void>("set_task_status", { directionId, status }),
  renameDirection: (directionId: number, name: string) =>
    invoke<Direction>("rename_direction", { directionId, name }),

  // Planner: the lead's proposed Task → scope decomposition (§4.10, §5.1).
  getProposal: (threadId: number) =>
    invoke<ResolvedProposal | null>("get_proposal", { threadId }),
  saveProposal: (threadId: number, proposal: Proposal) =>
    invoke<void>("save_proposal", { threadId, proposal }),
  confirmProposal: (threadId: number, manualTool?: string) =>
    invoke<number[]>("confirm_proposal", { threadId, manualTool: manualTool ?? null }),
  setProposalDirectionBase: (threadId: number, index: number, name: string, repo: string, expectedBase: string, expectedVersion: string, base: string) =>
    invoke<void>("set_proposal_direction_base", { threadId, index, name, repo, expectedBase, expectedVersion, base }),
  createDirection: (
    threadId: number,
    name: string,
    tool: string,
    repoId: number,
    reason: string,
  ) =>
    invoke<Direction>("create_direction", { threadId, name, tool, repoId, reason }),

  listWorktrees: (directionId: number) =>
    invoke<Worktree[]>("list_worktrees", { directionId }),
  listWorktreeFiles: (cwd: string) =>
    invoke<FileTree>("list_worktree_files", { cwd }),
  // Delete one finished task's worktree (directory + record); keeps the branch.
  deleteWorktree: (worktreeId: number) =>
    invoke<void>("delete_worktree", { worktreeId }),

  // Lead chat engine: weft-owned conversation (headless stream-json claude).
  leadSend: (
    threadId: number,
    text: string,
    lang: string,
    images?: ImageAttachment[],
    files?: string[],
  ) => invoke<void>("lead_send", { threadId, text, lang, images, files }),
  leadInterrupt: (threadId: number) =>
    invoke<void>("lead_interrupt", { threadId }),
  leadEnsure: (threadId: number, lang: string) =>
    invoke<void>("lead_ensure", { threadId, lang }),
  leadStop: (threadId: number) => invoke<void>("lead_stop", { threadId }),
  leadState: (threadId: number) =>
    invoke<LeadStateInfo>("lead_state", { threadId }),
  /** Band-outside meta for a non-claude lead (null for claude — event-fed). */
  leadSessionMeta: (threadId: number) =>
    invoke<SessionMetaSnapshot | null>("lead_session_meta", { threadId }),
  listLeadMessages: (threadId: number) =>
    invoke<LeadMessage[]>("list_lead_messages", { threadId }),
  /** Live (actually-running) worker engines the backend wants the frontend to
   *  adopt into its session map. Read-only — never starts/attaches an engine. */
  listLiveWorkerSlots: () =>
    invoke<LiveWorkerSlot[]>("list_live_worker_slots"),
  /** Backend-authoritative auto-verify gate: returns the direction id to verify if
   *  the worker's direction is in working/review (fresh DB read), else null. */
  autoVerifyCheck: (sessionId: number) =>
    invoke<number | null>("auto_verify_check", { sessionId }),
  /** Live slash-command discovery for a worker (sessionId) or the lead
   *  (threadId) — claude's initialize list, opencode's GET /command,
   *  codex's mirrored TUI built-ins plus dynamic skills. */
  discoverSlash: (threadId: number | null, sessionId: number | null) =>
    invoke<SlashCmd[]>("discover_slash", { threadId, sessionId }),

  // Chat-mode workers (claude): same engine, keyed by session id.
  chatOpenWorker: (directionId: number, repoId: number, lang: string) =>
    invoke<SessionInfo>("chat_open_worker", { directionId, repoId, lang }),
  chatSend: (
    sessionId: number,
    text: string,
    images?: ImageAttachment[],
    files?: string[],
  ) => invoke<void>("chat_send", { sessionId, text, images, files }),
  /** Rewind to just before `messageId` (a completed user text row), scoped by
   *  `mode`: "conversation" truncates the rows and forks the native session at
   *  that point, "code" restores the worktree files, "both" does both. The
   *  message's text comes back for composer prefill (conversation modes). */
  chatRewind: (sessionId: number, messageId: number, mode: RewindMode) =>
    invoke<RewindOutcome>("chat_rewind", { sessionId, messageId, mode }),
  /** Lead-console rewind — conversation-only (code rewind never applies to the
   *  lead), so it takes no mode. */
  leadRewind: (threadId: number, messageId: number, lang?: string) =>
    invoke<RewindOutcome>("lead_rewind", { threadId, messageId, lang }),
  /** Switch the LEAD's engine identity and/or model override (issue #96/#98,
   *  layer 1 of 3 — independent of any worker's tool and of the global
   *  default). Clears the native session id and stages a history digest for
   *  the new engine's first turn; `model=null` clears any override. Same
   *  tool + same model is a valid "reload" (force the engine to restart, e.g.
   *  to pick up an externally-edited CLI config). */
  switchLeadTool: (threadId: number, tool: string, model: string | null, lang?: string) =>
    invoke<SwitchOutcome>("switch_lead_tool", { threadId, tool, model, lang }),
  /** Switch a WORKER's engine identity and/or model override (issue #96/#98,
   *  layer 2 of 3 — independent of the thread's lead and of the global
   *  default). Same semantics as switchLeadTool, keyed by session id; also
   *  updates the owning direction's `tool` so a later reopen doesn't revert it. */
  switchWorkerTool: (sessionId: number, tool: string, model: string | null) =>
    invoke<SwitchOutcome>("switch_worker_tool", { sessionId, tool, model }),
  chatInterrupt: (sessionId: number) =>
    invoke<void>("chat_interrupt", { sessionId }),
  chatStop: (sessionId: number) => invoke<void>("chat_stop", { sessionId }),
  chatDequeue: (sessionId: number, messageId: number) =>
    invoke<void>("chat_dequeue", { sessionId, messageId }),
  chatEditQueued: (sessionId: number, messageId: number, text: string) =>
    invoke<void>("chat_edit_queued", { sessionId, messageId, text }),
  chatReorderQueue: (sessionId: number, order: number[]) =>
    invoke<void>("chat_reorder_queue", { sessionId, order }),
  leadDequeue: (threadId: number, messageId: number) =>
    invoke<void>("lead_dequeue", { threadId, messageId }),
  leadEditQueued: (threadId: number, messageId: number, text: string) =>
    invoke<void>("lead_edit_queued", { threadId, messageId, text }),
  leadReorderQueue: (threadId: number, order: number[]) =>
    invoke<void>("lead_reorder_queue", { threadId, order }),
  sessionFor: (directionId: number, repoId: number) =>
    invoke<ObserveRef | null>("session_for", { directionId, repoId }),
  sessionMeta: (directionId: number, repoId: number) =>
    invoke<SessionMetaSnapshot>("session_meta", { directionId, repoId }),
  worktreeDiff: (cwd: string) =>
    invoke<WorktreeDiff>("worktree_diff", { cwd }),
  /** PR-style diff against the task's target branch. `fetch` refreshes
   *  origin/<target> first (mode-enter / manual refresh / after a target edit). */
  worktreeDiffTarget: (cwd: string, directionId: number, fetch: boolean) =>
    invoke<TargetDiff>("worktree_diff_target", { cwd, directionId, fetch }),
  setDirectionTargetBranch: (directionId: number, target: string) =>
    invoke<void>("set_direction_target_branch", { directionId, target }),

  // Quality loop: run inferred checks across a direction's write worktrees.
  verifyDirection: (directionId: number) =>
    invoke<RepoChecks[]>("verify_direction", { directionId }),
  // Evidence ledger (issue #174): newest-first bounded page for one thread,
  // optionally scoped to a Lane. directionId=0 is issue-level evidence.
  listEvidence: (threadId: number, directionId?: number | null, limit?: number) =>
    invoke<EvidenceRow[]>("list_evidence", {
      threadId,
      directionId: directionId ?? null,
      limit: limit ?? null,
    }),

  // AuthorityPolicy (issue #172): the workspace's active policy (or null =
  // the hard-coded conservative default is in effect), its full revision
  // history, and the commands that tighten/loosen/revoke it.
  getAuthorityPolicy: (workspaceId: number) =>
    invoke<AuthorityPolicyRevision | null>("get_authority_policy", { workspaceId }),
  listAuthorityPolicyRevisions: (workspaceId: number) =>
    invoke<AuthorityPolicyRevision[]>("list_authority_policy_revisions", { workspaceId }),
  setAuthorityPolicy: (workspaceId: number, rules: AuthorityPolicyRules) =>
    invoke<AuthorityPolicyRevision>("set_authority_policy", { workspaceId, rules }),
  revokeAuthorityPolicy: (workspaceId: number) =>
    invoke<void>("revoke_authority_policy", { workspaceId }),
  // Every Lane in a thread currently blocked on a Gate, and the command that
  // resolves one (records the decision, then re-runs materialize so an
  // approval takes effect immediately).
  listLaneGates: (threadId: number) => invoke<LaneGate[]>("list_lane_gates", { threadId }),
  resolveLaneGate: (
    directionId: number,
    policyRevision: string,
    decision: "approved" | "denied",
    reason?: string,
  ) =>
    invoke<Worktree[]>("resolve_lane_gate", {
      directionId,
      policyRevision,
      decision,
      reason: reason ?? null,
    }),
  // Versioned dynamic scope (issue #172): newest-first history for a thread.
  listPlanRevisions: (threadId: number, limit?: number) =>
    invoke<PlanRevision[]>("list_plan_revisions", { threadId, limit: limit ?? null }),

  threadMessages: (threadId: number) =>
    invoke<BusMsg[]>("thread_messages", { threadId }),
  busPostHuman: (threadId: number, to: string | null, text: string) =>
    invoke<void>("bus_post_human", { threadId, to, text }),

  // Ask Bridge: answer a permission projected by AttentionSnapshot.
  answerPermission: (askId: number, answer: "allow" | "deny" | "always" | "full") =>
    invoke<void>("answer_permission", { askId, answer }),
  // Standing authorization grants (full / always) that persist across restarts —
  // the board's "inherited access" markers.
  listAuthGrants: () => invoke<GrantSnapshot>("list_auth_grants"),
  // Revoke a standing grant. dir=null clears the whole issue's grants (one-click
  // "revoke all"); dir set + actionKey=null clears one task; both set drops one
  // rule (actionKey is the canonical action identity, not the display summary).
  revokeAuthGrant: (thread: number, dir: string | null, actionKey: string | null) =>
    invoke<void>("revoke_auth_grant", { thread, dir, actionKey }),

  // Read-only auto-allow grants — in-memory only, NEVER persisted
  // (contrast the Full/Always grants above): a live snapshot, not something
  // restored at boot.
  readOnlyGrants: () => invoke<ReadOnlyGrants>("read_only_grants"),
  // "Release all read-only for this session": resolves the open ReadOnly-tier
  // backlog in (thread, dir) to Allow and installs a forward-looking rule for
  // the rest of the session. Returns how many open asks were just released.
  releaseSessionReadOnly: (thread: number, dir: string) =>
    invoke<number>("release_session_read_only", { thread, dir }),
  // Revoke a read-only grant. dir=null revokes the whole issue's propagation;
  // dir set revokes just that one session's batch grant.
  revokeReadOnlyGrant: (thread: number, dir: string | null) =>
    invoke<void>("revoke_read_only_grant", { thread, dir }),

  attentionItems: (workspaceId: number) =>
    invoke<AttentionSnapshot>("attention_items", { workspaceId }),
  attentionSnapshots: () => invoke<AttentionSnapshot[]>("attention_snapshots"),
  answerHumanRequest: (
    workspaceId: number,
    requestId: number,
    revision: number,
    text: string,
  ) => invoke<void>("answer_human_request", { workspaceId, requestId, revision, text }),
  retryPrTracking: (workspaceId: number, prId: number, failureEpisode: string) =>
    invoke<void>("retry_pr_tracking", { workspaceId, prId, failureEpisode }),
  // Inspect escape hatches (§4.7): real ways into the hidden plumbing.
  /** Open a real filesystem path verbatim (no chat-token / `:line` stripping). */
  openFile: (path: string) => invoke<void>("open_file", { path }),
  openTerminal: (path: string) => invoke<void>("open_terminal", { path }),
  // Reveal a real filesystem path (the Inspect working copy) — taken verbatim,
  // no chat-URI normalization.
  revealPath: (path: string) => invoke<void>("reveal_path", { path }),
  openUrl: (url: string) => invoke<void>("open_url", { url }),
  // Open / reveal a file the agent referenced in chat. `cwd` resolves relative
  // paths against the session's working copy; `isUrl` marks a link href (URI
  // syntax) vs a literal inline/prose path. Reject with "not_found" if missing.
  openPath: (path: string, cwd?: string, isUrl = false) =>
    invoke<void>("open_path", { path, cwd, isUrl }),
  revealPathIn: (path: string, cwd?: string, isUrl = false) =>
    invoke<void>("reveal_path_in", { path, cwd, isUrl }),

  // Which coding-agent CLIs are installed locally (for Settings).
  detectTools: () => invoke<ToolStatus[]>("detect_tools"),
  getDefaultTool: () => invoke<DefaultToolInfo>("get_default_tool"),
  setDefaultTool: (tool: string) => invoke<void>("set_default_tool", { tool }),
  getAutomaticEngineRoutingEnabled: () =>
    invoke<boolean>("get_automatic_engine_routing_enabled"),
  setAutomaticEngineRoutingEnabled: (enabled: boolean) =>
    invoke<void>("set_automatic_engine_routing_enabled", { enabled }),
  // OS-level computer use (window enumeration + screenshot of a
  // named app window) for visual verification. Opt-in — off by default.
  getComputerUseEnabled: () => invoke<boolean>("get_computer_use_enabled"),
  setComputerUseEnabled: (enabled: boolean) =>
    invoke<void>("set_computer_use_enabled", { enabled }),
  // who (if anyone) currently holds the computer-use control
  // mutex, for the global control banner + kill switch.
  getComputerControlState: () =>
    invoke<{ thread: number; dir: string; wt: number | null; expires_at_ms: number } | null>(
      "get_computer_control_state",
    ),
  computerEmergencyStop: () => invoke<void>("computer_emergency_stop"),
  // whether the most recent emergency stop
  // (button OR the OS-level global Escape shortcut) failed to persist the
  // disabled setting to disk — the kill switch itself still took effect
  // in-memory either way; see `ComputerControlBanner`'s own doc.
  getComputerStopPersistFailed: () => invoke<boolean>("get_computer_stop_persist_failed"),
  // auto fail-over to the fallback engine on a quota-exceeded turn.
  // Opt-in — off by default, since switching engines mid-task ships that
  // engine's own history digest to a DIFFERENT provider.
  getQuotaFailoverEnabled: () => invoke<boolean>("get_quota_failover_enabled"),
  setQuotaFailoverEnabled: (enabled: boolean) =>
    invoke<void>("set_quota_failover_enabled", { enabled }),
  // issue #110 T3: squash-merge a tracked PR/MR automatically once it
  // reaches this repo's truly-mergeable bar. Opt-in — off by default, since
  // this performs an irreversible action (merging code) with no human
  // confirming the specific merge.
  getPrAutoMergeEnabled: () => invoke<boolean>("get_pr_auto_merge_enabled"),
  setPrAutoMergeEnabled: (enabled: boolean) =>
    invoke<void>("set_pr_auto_merge_enabled", { enabled }),
  // Per-tool command overrides ("aliases", e.g. claude → cc-claude): identity →
  // command. Empty map when none configured.
  getToolCommands: () => invoke<Record<string, string>>("get_tool_commands"),
  // applyToExisting=false pins existing sessions of `tool` to their prior command
  // so only new sessions adopt the alias; true lets them adopt it on next run.
  setToolCommand: (tool: string, command: string, applyToExisting: boolean) =>
    invoke<void>("set_tool_command", { tool, command, applyToExisting }),
  // Dangerous mode (global): every agent's tool asks auto-allow.
  setDangerousMode: (on: boolean) => invoke<void>("set_dangerous_mode", { on }),
  // Keep-awake: prevent system idle sleep while any session is running.
  setKeepAwake: (on: boolean) => invoke<void>("set_keep_awake", { on }),
  processQuotaStatus: () => invoke<ProcessQuotaStatus>("process_quota_status"),
  // Desktop OS notifications via user-notify (click deep-link capable).
  osNotifyPermission: () => invoke<string>("os_notify_permission"),
  osNotifyRequestPermission: () => invoke<string>("os_notify_request_permission"),
  osNotifyAckOpen: (payload: {
    kind: string;
    threadId?: number | null;
    directionId?: number | null;
    repoId?: number | null;
    sessionId?: number | null;
    askId?: number | null;
    attentionId?: string | null;
    workspaceId?: number | null;
    openNeeds?: boolean | null;
    openCurator?: boolean | null;
  }) => invoke<void>("os_notify_ack_open", { payload }),
  osNotifyTakePendingOpen: () =>
    invoke<{
      kind: string;
      threadId?: number | null;
      directionId?: number | null;
      repoId?: number | null;
      sessionId?: number | null;
      askId?: number | null;
      attentionId?: string | null;
      workspaceId?: number | null;
      openNeeds?: boolean | null;
      openCurator?: boolean | null;
    } | null>("os_notify_take_pending_open"),
  osNotifyRestorePendingOpen: (payload: {
    kind: string;
    threadId?: number | null;
    directionId?: number | null;
    repoId?: number | null;
    sessionId?: number | null;
    askId?: number | null;
    attentionId?: string | null;
    workspaceId?: number | null;
    openNeeds?: boolean | null;
    openCurator?: boolean | null;
  }) => invoke<void>("os_notify_restore_pending_open", { payload }),
  osNotifySend: (req: {
    title: string;
    body: string;
    kind: string;
    threadId?: number | null;
    directionId?: number | null;
    repoId?: number | null;
    sessionId?: number | null;
    askId?: number | null;
    attentionId?: string | null;
    workspaceId?: number | null;
    openNeeds?: boolean | null;
    openCurator?: boolean | null;
  }) => invoke<void>("os_notify_send", { req }),
  // Local-runtime resource dashboard: read-only aggregate of
  // process_quota / proc_registry / session_gate. Polled while the Settings →
  // Resources page is open; no new sampling happens on the backend for this.
  resourceDashboardSnapshot: () =>
    invoke<ResourceDashboardSnapshot>("resource_dashboard_snapshot"),
  // Effective config (skills + rules) for a repo, tagged by layer + override.
  effectiveConfig: (repoPath: string, wsId?: number) =>
    invoke<ConfigItem[]>("effective_config", { repoPath, wsId }),
  listSkillSources: () => invoke<SkillSource[]>("list_skill_sources"),
  addSkillSource: (gitUrl: string, gitRef?: string) =>
    invoke<SkillSource>("add_skill_source", { gitUrl, gitRef }),
  removeSkillSource: (id: number) => invoke<void>("remove_skill_source", { id }),
  syncSkillSource: (id: number) => invoke<SkillSource>("sync_skill_source", { id }),
  syncAllSkillSources: () => invoke<SkillSource[]>("sync_all_skill_sources"),
  listParsedSkills: (id: number) => invoke<ParsedSkill[]>("list_parsed_skills", { id }),
  setSkillEnabled: (sourceId: number, name: string, scope: string, on: boolean) =>
    invoke<void>("set_skill_enabled", { sourceId, name, scope, on }),
  workspaceSkills: (wsId: number) => invoke<EnabledSkill[]>("workspace_skills", { wsId }),
  flagSessionSkillRefresh: (sessionId: number) =>
    invoke<void>("flag_session_skill_refresh", { sessionId }),
  flagLeadSkillRefresh: (threadId: number) =>
    invoke<void>("flag_lead_skill_refresh", { threadId }),
  imGetSettings: () =>
    invoke<{
      provider: "feishu" | "dingtalk";
      app_id: string;
      has_secret: boolean;
      bound: boolean;
      enabled: boolean;
      remote_standby: boolean;
    }>("im_get_settings"),
  imSetProvider: (provider: "feishu" | "dingtalk") =>
    invoke<void>("im_set_provider", { provider }),
  imSetSettings: (provider: "feishu" | "dingtalk", appId: string, appSecret: string) =>
    invoke<void>("im_set_settings", { provider, appId, appSecret }),
  imSetEnabled: (provider: "feishu" | "dingtalk", enabled: boolean) =>
    invoke<void>("im_set_enabled", { provider, enabled }),
  imResetOwner: (provider: "feishu" | "dingtalk") =>
    invoke<void>("im_reset_owner", { provider }),
  imSetRemoteStandby: (enabled: boolean) =>
    invoke<void>("im_set_remote_standby", { enabled }),
  imStatus: () => invoke<string>("im_status"),
  imSetDingTalkCopy: (copy: DingTalkCopy) =>
    invoke<void>("im_set_dingtalk_copy", { copy }),
  feishuScanBegin: () =>
    invoke<{ qr_data_uri: string; expire_secs: number; poll_interval_ms: number }>(
      "feishu_scan_begin",
    ),
  feishuScanStatus: () =>
    invoke<{ status: string; error_reason: string | null }>("feishu_scan_status"),
  feishuScanCancel: () => invoke<void>("feishu_scan_cancel"),
  imBindThread: (threadId: number, chatId: string, imThreadRef: string, channel = "feishu") =>
    invoke<ImRoute>("im_bind_thread", { threadId, channel, chatId, imThreadRef }),
  imUnbindThread: (threadId: number) =>
    invoke<void>("im_unbind_thread", { threadId }),
  imRouteForThread: (threadId: number) =>
    invoke<ImRoute | null>("im_route_for_thread", { threadId }),
  imListRoutes: () => invoke<ImRoute[]>("im_list_routes"),
  backupGetStatus: () => invoke<BackupStatusDto>("backup_get_status"),
  backupSavePrefs: (
    enabled: boolean,
    remoteUrl: string,
    autoBackupEnabled: boolean,
    backupOnExit: boolean,
  ) =>
    invoke<void>("backup_save_prefs", {
      enabled,
      remoteUrl,
      autoBackupEnabled,
      backupOnExit,
    }),
  backupTestRemote: (remoteUrl: string) =>
    invoke<void>("backup_test_remote", { remoteUrl }),
  backupRunNow: () => invoke<BackupStatusDto>("backup_run_now"),
  backupExportRecoveryKey: (targetPath: string) =>
    invoke<void>("backup_export_recovery_key", { targetPath }),
  backupRestore: (remoteUrl: string, recoveryKeyPath: string) =>
    invoke<void>("backup_restore", { remoteUrl, recoveryKeyPath }),
  // Database encryption
  dbEncryptionStatus: () => invoke<{ encrypted: boolean }>("db_encryption_status"),
  dbEnableEncryption: (password: string) =>
    invoke<{ restart_required: boolean }>("db_enable_encryption", { password }),
  dbDisableEncryption: (password: string) =>
    invoke<{ restart_required: boolean }>("db_disable_encryption", { password }),
  dbChangePassword: (oldPassword: string, newPassword: string) =>
    invoke<{ restart_required: boolean }>("db_change_password", { oldPassword, newPassword }),
  // Native folder picker; returns the chosen absolute path, or null if cancelled.
  pickFolder: async (title?: string) => {
    const sel = await openDialog({ directory: true, multiple: false, title });
    return typeof sel === "string" ? sel : null;
  },
  // Native multi-folder picker; [] when cancelled. Used to add several local
  // repos at once (the backend dedupes any already in the workspace).
  pickFolders: async (title?: string) => {
    const sel = await openDialog({ directory: true, multiple: true, title });
    if (Array.isArray(sel)) return sel;
    if (typeof sel === "string") return [sel];
    return [];
  },
  // Native multi-file picker; [] when cancelled.
  pickFiles: async (title?: string) => {
    const sel = await openDialog({ directory: false, multiple: true, title });
    if (Array.isArray(sel)) return sel;
    if (typeof sel === "string") return [sel];
    return [];
  },
};

export interface DingTalkCopy {
  locale: "en" | "zh";
  truncatedMarker: string;
  permissionTitle: string;
  permissionReplyCommand: string;
  verdictAllowed: string;
  verdictAlwaysAllowed: string;
  verdictFullAccess: string;
  verdictDenied: string;
  verdictExpired: string;
  verdictResolved: string;
  humanQuestionTitle: string;
  humanAnswerInstruction: string;
  humanAnswerPlaceholder: string;
  humanAnswered: string;
  answerPrefix: string;
  humanCancelled: string;
  issueNotFound: string;
  bindThreadPrefix: string;
  permissionAlreadyHandled: string;
  humanAlreadyAnswered: string;
  permissionCommandUsage: string;
  humanAnswerUsage: string;
  threadRequired: string;
  freeTextUnavailable: string;
  unboundThread: string;
  conciergeDmPrefix: string;
  conciergeGroupPrefix: string;
  leadPrefix: string;
  resyncOne: string;
  resyncMany: string;
  resyncMore: string;
  resyncHint: string;
}
