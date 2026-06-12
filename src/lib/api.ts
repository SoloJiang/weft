import { invoke } from "@tauri-apps/api/core";
import { open as openDialog } from "@tauri-apps/plugin-dialog";
import type {
  BackupStatusDto,
  BusMsg,
  ConfigItem,
  DefaultToolInfo,
  Direction,
  EnabledSkill,
  ImageAttachment,
  ImRoute,
  LeadMessage,
  LeadStateInfo,
  NeedItem,
  NormEvent,
  ObserveRef,
  ParsedSkill,
  PermissionAsk,
  Proposal,
  RepoChecks,
  RepoGraph,
  RepoRef,
  ResolvedProposal,
  SessionInfo,
  SkillSource,
  SlashCmd,
  Thread,
  ThreadOverview,
  ToolStatus,
  Workspace,
  Worktree,
  WorktreeDiff,
  WriteTrigger,
} from "./types";

// Tauri converts camelCase command args to snake_case Rust params.

export const api = {
  listWorkspaces: () => invoke<Workspace[]>("list_workspaces"),
  createWorkspace: (name: string) =>
    invoke<Workspace>("create_workspace", { name }),
  renameWorkspace: (workspaceId: number, name: string) =>
    invoke<Workspace>("rename_workspace", { workspaceId, name }),
  ensureDefaultWorkspace: () =>
    invoke<number>("ensure_default_workspace"),

  listRepos: (workspaceId: number) =>
    invoke<RepoRef[]>("list_repos", { workspaceId }),
  addRepoRef: (workspaceId: number, name: string, localGitPath: string) =>
    invoke<RepoRef>("add_repo_ref", { workspaceId, name, localGitPath }),
  cloneRepo: (workspaceId: number, url: string, dest: string, name: string) =>
    invoke<RepoRef>("clone_repo", { workspaceId, url, dest, name }),
  createRepo: (workspaceId: number, name: string, dest: string) =>
    invoke<RepoRef>("create_repo", { workspaceId, name, dest }),
  postLeadToolResult: (threadId: number, payload: unknown) =>
    invoke<void>("post_lead_tool_result", { threadId, payload }),

  // Repo map (curator): profiles + cross-repo dependency graph.
  repoGraph: (workspaceId: number) =>
    invoke<RepoGraph>("repo_graph", { workspaceId }),
  reprofileRepo: (repoId: number) =>
    invoke<void>("reprofile_repo", { repoId }),
  updateRepoProfile: (repoId: number, summary: string, role: string) =>
    invoke<void>("update_repo_profile", { repoId, summary, role }),

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
  setTaskStatus: (directionId: number, status: string) =>
    invoke<void>("set_task_status", { directionId, status }),
  renameDirection: (directionId: number, name: string) =>
    invoke<Direction>("rename_direction", { directionId, name }),

  // Planner: the lead's proposed Task → scope decomposition (§4.10, §5.1).
  getProposal: (threadId: number) =>
    invoke<ResolvedProposal | null>("get_proposal", { threadId }),
  saveProposal: (threadId: number, proposal: Proposal) =>
    invoke<void>("save_proposal", { threadId, proposal }),
  confirmProposal: (threadId: number) =>
    invoke<number[]>("confirm_proposal", { threadId }),
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
  listLeadMessages: (threadId: number) =>
    invoke<LeadMessage[]>("list_lead_messages", { threadId }),
  /** Live slash-command discovery for a worker (sessionId) or the lead
   *  (threadId) — claude's initialize list, opencode's GET /command, codex none. */
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
  chatInterrupt: (sessionId: number) =>
    invoke<void>("chat_interrupt", { sessionId }),
  chatStop: (sessionId: number) => invoke<void>("chat_stop", { sessionId }),
  sessionFor: (directionId: number, repoId: number) =>
    invoke<ObserveRef | null>("session_for", { directionId, repoId }),
  readTranscript: (cwd: string, tool: string) =>
    invoke<NormEvent[]>("read_transcript", { cwd, tool }),
  worktreeDiff: (cwd: string) =>
    invoke<WorktreeDiff>("worktree_diff", { cwd }),

  // Quality loop: run inferred checks across a direction's write worktrees.
  verifyDirection: (directionId: number) =>
    invoke<RepoChecks[]>("verify_direction", { directionId }),

  threadMessages: (threadId: number) =>
    invoke<BusMsg[]>("thread_messages", { threadId }),
  busPostHuman: (threadId: number, to: string | null, text: string) =>
    invoke<void>("bus_post_human", { threadId, to, text }),

  // Ask Bridge: pending tool permission requests + the answer.
  pendingAsks: () => invoke<PermissionAsk[]>("pending_asks"),
  workspaceNeedsCounts: () =>
    invoke<[number, number][]>("workspace_needs_counts"),
  answerPermission: (askId: number, answer: "allow" | "deny" | "always" | "full") =>
    invoke<void>("answer_permission", { askId, answer }),

  // Needs-you: open agent→human questions, aggregated across the workspace.
  needsYou: (workspaceId: number) =>
    invoke<NeedItem[]>("needs_you", { workspaceId }),
  answerAsk: (threadId: number, askId: number, text: string) =>
    invoke<void>("answer_ask", { threadId, askId, text }),

  // Write triggers: lead-proposed repo writes awaiting human approve/deny.
  writeTriggers: (workspaceId: number) =>
    invoke<WriteTrigger[]>("write_triggers", { workspaceId }),
  approveWriteTrigger: (threadId: number, index: number, tool: string) =>
    invoke<number>("approve_write_trigger", { threadId, index, tool }),
  denyWriteTrigger: (threadId: number, index: number) =>
    invoke<void>("deny_write_trigger", { threadId, index }),

  // Inspect escape hatches (§4.7): real ways into the hidden plumbing.
  openTerminal: (path: string) => invoke<void>("open_terminal", { path }),
  revealPath: (path: string) => invoke<void>("reveal_path", { path }),
  openUrl: (url: string) => invoke<void>("open_url", { url }),

  // Which coding-agent CLIs are installed locally (for Settings).
  detectTools: () => invoke<ToolStatus[]>("detect_tools"),
  getDefaultTool: () => invoke<DefaultToolInfo>("get_default_tool"),
  setDefaultTool: (tool: string) => invoke<void>("set_default_tool", { tool }),
  // Dangerous mode (global): every agent's tool asks auto-allow.
  setDangerousMode: (on: boolean) => invoke<void>("set_dangerous_mode", { on }),
  // Keep-awake: prevent system idle sleep while any session is running.
  setKeepAwake: (on: boolean) => invoke<void>("set_keep_awake", { on }),
  // Runaway guardrails: idle + wall-clock caps (seconds; 0 disables) for
  // force-stopping a stuck/runaway agent (enforcement pending on the engine).
  setGuardrails: (idleSecs: number, wallSecs: number) =>
    invoke<void>("set_guardrails", { idleSecs, wallSecs }),
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
    invoke<{ app_id: string; has_secret: boolean; bound: boolean; enabled: boolean }>(
      "im_get_settings",
    ),
  imSetSettings: (appId: string, appSecret: string) =>
    invoke<void>("im_set_settings", { appId, appSecret }),
  imSetEnabled: (enabled: boolean) =>
    invoke<void>("im_set_enabled", { enabled }),
  imStatus: () => invoke<string>("im_status"),
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
  // Native folder picker; returns the chosen absolute path, or null if cancelled.
  pickFolder: async (title?: string) => {
    const sel = await openDialog({ directory: true, multiple: false, title });
    return typeof sel === "string" ? sel : null;
  },
  // Native multi-file picker; [] when cancelled.
  pickFiles: async (title?: string) => {
    const sel = await openDialog({ directory: false, multiple: true, title });
    return Array.isArray(sel) ? sel : typeof sel === "string" ? [sel] : [];
  },
};
