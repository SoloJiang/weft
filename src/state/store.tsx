import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useRef,
  useState,
  type ReactNode,
} from "react";
import { listen } from "@tauri-apps/api/event";
import { api } from "../lib/api";
import { currentLang } from "../i18n";
import { mergeSnapshot, metaFromInit, metaFromSnapshot, metaFromUsage } from "../session/sessionMeta";
import type {
  BusMsg,
  Direction,
  ImageAttachment,
  LeadChatPush,
  LeadMessage,
  LiveWorkerSlot,
  NeedItem,
  PermissionAsk,
  Proposal,
  RepoChecks,
  RepoEdge,
  RepoProfile,
  RepoRef,
  ResolvedProposal,
  ThreadOverview,
  ObserveRef,
  SessionInfo,
  SessionMeta,
  SessionMetaSnapshot,
  SessionStatus,
  SlashCmd,
  Thread,
  ToolStatus,
  Workspace,
  Worktree,
  WriteTrigger,
} from "../lib/types";

export type HomeTab = "board" | "repos" | "settings";
export type ThreadTab = "lead" | "board";

export interface OpenSession {
  info: SessionInfo;
  status: SessionStatus;
  /** identity of the (direction, repo) slot this session occupies */
  directionId: number;
  repoId: number;
  /** the thread this session belongs to (the worker's parent). */
  threadId: number;
  nativeId: string | null;
}

interface Store {
  workspaces: Workspace[];
  activeWorkspaceId: number | null;
  repos: RepoRef[];
  threads: Thread[];
  directionsByThread: Record<number, Direction[]>;
  worktreesByDirection: Record<number, Worktree[]>;

  activeThreadId: number | null;
  sessions: Record<number, OpenSession>;
  messages: BusMsg[];
  postHuman: (to: string | null, text: string) => Promise<void>;

  /** Lead chat: weft-owned timeline per thread (engine pushes, no polling). */
  leadMessages: Record<number, LeadMessage[]>;
  /** Lead engine turn state per thread: busy/idle/stopped + queue depth. */
  leadTurn: Record<number, { state: "busy" | "idle" | "stopped"; queued: number }>;
  /** Slash commands the lead's CLI reports as available (init event). */
  leadSlash: Record<number, SlashCmd[]>;
  /** Hydrate a thread's timeline from DB + make sure the engine runs. */
  loadLeadChat: (threadId: number) => Promise<void>;
  /** Pull a lead's slash commands on demand. */
  discoverLeadSlash: (threadId: number) => void;
  /** Send a human message to the lead (optimistic; engine queues when busy). */
  sendLeadChat: (
    threadId: number,
    text: string,
    images?: ImageAttachment[],
    files?: string[],
  ) => Promise<void>;
  /** Interrupt the lead's current turn. */
  interruptLead: (threadId: number) => Promise<void>;
  /** Chat-mode worker engine state, keyed by session id. */
  workerTurn: Record<number, { state: "busy" | "idle" | "stopped"; queued: number }>;
  workerSlash: Record<number, SlashCmd[]>;
  discoverWorkerSlash: (sessionId: number) => void;
  /** The tool call running right now (transient): lead by thread, worker by session. */
  leadActivity: Record<number, { name: string; summary: string } | null>;
  workerActivity: Record<number, { name: string; summary: string } | null>;
  /** 会话信息面板的每会话快照:lead 按 thread_id、worker 按 session_id。 */
  leadMeta: Record<number, SessionMeta>;
  workerMeta: Record<number, SessionMeta>;
  /** worker 重挂时由 session_for 回包回填 meta(首条消息前不空白)。 */
  hydrateWorkerMeta: (sessionId: number, snap: ObserveRef) => void;
  /** codex/opencode worker 的带外 meta(session_meta 命令)并入 workerMeta。 */
  mergeWorkerMeta: (sessionId: number, snap: SessionMetaSnapshot) => void;
  /** 非-claude lead 的带外 meta(lead_session_meta 命令)并入 leadMeta。 */
  mergeLeadMeta: (threadId: number, snap: SessionMetaSnapshot) => void;
  /** The thread-bus drawer (demoted from a permanent rail). */
  showBus: boolean;
  setShowBus: (open: boolean) => void;
  /** Left sidebar collapse (manual + auto on narrow windows). */
  navCollapsed: boolean;
  setNavCollapsed: (v: boolean) => void;
  /** App settings (persisted to localStorage). */
  projectsDir: string;
  setProjectsDir: (p: string) => void;
  defaultTool: string;
  setDefaultTool: (t: string) => void;
  /** The user's explicit Settings choice; null = auto-detected. */
  configuredTool: string | null;
  /** detect_tools result, loaded once at startup (for tool pickers). */
  installedTools: ToolStatus[];
  refreshInstalledTools: () => Promise<void>;
  refreshDefaultTool: () => Promise<void>;
  /** Dangerous mode: agents skip all permission prompts (global). */
  dangerousMode: boolean;
  setDangerousMode: (on: boolean) => void;
  /** The per-day "turn on Dangerous mode?" nudge toast state. */
  dangerNudge: "ask" | "enabled" | null;
  setDangerNudge: (v: "ask" | "enabled" | null) => void;
  /** Runaway guardrails: idle + wall-clock caps in minutes (0 disables). */
  idleCapMins: number;
  wallCapMins: number;
  setGuardrails: (idleMins: number, wallMins: number) => void;
  /** Whether the board canvas is showing the proposal's scope-confirm. */
  reviewingProposal: boolean;
  setReviewingProposal: (v: boolean) => void;
  /** Active issue-level tab: console first, board second. */
  threadTab: ThreadTab;
  setThreadTab: (tab: ThreadTab) => void;
  /** Mark skills as changed; idle sessions/leads lazily refresh their engines. */
  /** Bumped on any skills mutation; consumers re-fetch enabled skills off this. */
  skillsDirtyAt: number;
  markSkillsDirty: () => void;

  /** Open agent→human questions across the workspace; the Needs-you surface. */
  needs: NeedItem[];
  /** Pending tool permission requests (the Ask Bridge). */
  asks: PermissionAsk[];
  /** Lead-proposed write declarations awaiting human approve/deny. */
  writeTriggers: WriteTrigger[];
  approveWriteTrigger: (item: WriteTrigger, tool?: string) => Promise<void>;
  denyWriteTrigger: (item: WriteTrigger) => Promise<void>;
  /** Pending needs count per workspace id (for the workspace switcher). */
  needsByWorkspace: Record<number, number>;
  /** Whether the Needs-you view occupies the main region. */
  showNeeds: boolean;
  openNeeds: () => void;
  refreshNeeds: () => Promise<void>;
  answerAsk: (item: NeedItem, text: string) => Promise<void>;
  goToAsk: (item: NeedItem) => Promise<void>;
  answerPermission: (
    askId: number,
    answer: "allow" | "deny" | "always" | "full",
  ) => Promise<void>;

  /** The curator's repo map: profiles + dependency edges. */
  repoProfiles: RepoProfile[];
  repoEdges: RepoEdge[];
  /** Which workspace-home tab is active (Board · Repos). */
  homeTab: HomeTab;
  setHomeTab: (t: HomeTab) => void;
  /** Switch to Settings, snapshotting the current view so closeSettings can restore it. */
  openSettings: () => void;
  /** Leave Settings and restore the view that was active when openSettings ran. */
  closeSettings: () => void;
  /** Jump to the workspace home's Repos tab. */
  openRepoMap: () => void;
  refreshRepoMap: () => Promise<void>;
  refreshReposAndMap: (workspaceId?: number) => Promise<void>;
  reprofileRepo: (repoId: number) => Promise<void>;
  reanalyzeDeps: () => Promise<void>;
  deleteRepo: (repoId: number) => Promise<void>;
  /** The active workspace's hidden curator thread id (ensured lazily, no nav). */
  curatorThreadId: number | null;
  ensureCuratorThread: () => Promise<void>;
  /** Curator panel (in Repo Map) open/width, persisted per workspace. */
  curatorPanelOpen: boolean;
  setCuratorPanelOpen: (open: boolean) => void;
  curatorPanelWidth: number;
  setCuratorPanelWidth: (w: number) => void;
  /** Repos view right drawer: one of detail/curator at a time. selectedRepoId
   *  drives both node highlight and the detail tab. Width persists per workspace;
   *  open does NOT (drawer starts closed each visit). */
  repoDrawerOpen: boolean;
  repoDrawerTab: "detail" | "curator";
  selectedRepoId: number | null;
  repoDrawerWidth: number;
  openRepoDetail: (repoId: number) => void;
  openCurator: () => void;
  closeRepoDrawer: () => void;
  setRepoDrawerTab: (tab: "detail" | "curator") => void;
  setRepoDrawerWidth: (w: number) => void;
  /** Pin a repo's one-line summary (tier ownership untouched). */
  editRepoSummary: (repoId: number, summary: string) => Promise<void>;
  /** Pin a repo's tier (summary ownership untouched). */
  editRepoTier: (repoId: number, tier: string) => Promise<void>;

  /** The active thread's plan proposal (Task → scope), if any. */
  proposal: ResolvedProposal | null;
  refreshProposal: (threadId: number) => Promise<void>;
  saveProposal: (proposal: Proposal) => Promise<void>;
  confirmProposal: () => Promise<void>;
  setProposalDirectionBase: (index: number, name: string, repo: string, base: string, expectedOldBase: string) => Promise<void>;

  /** Workspace board: per-thread roll-ups for the portfolio view. */
  overview: ThreadOverview[];
  refreshOverview: () => Promise<void>;

  selectWorkspace: (id: number) => Promise<void>;
  refreshWorkspaces: () => Promise<void>;
  selectThread: (threadId: number) => Promise<void>;
  loadThreadChildren: (threadId: number) => Promise<void>;
  /** Leave the active thread for the workspace portfolio board. */
  backToWorkspace: () => void;

  createWorkspace: (name: string) => Promise<void>;
  renameWorkspace: (workspaceId: number, name: string) => Promise<void>;
  renameThread: (threadId: number, title: string) => Promise<void>;
  renameDirection: (directionId: number, name: string) => Promise<void>;
  addRepo: (name: string, path: string) => Promise<void>;
  /** Batch add of existing local repos, sequential + tolerant. Reports per-item
   *  progress; refreshes the repo list once at the end. Duplicates are deduped
   *  silently by the backend (same path / remote already in the workspace). */
  addRepos: (
    items: Array<{ name: string; path: string }>,
    onProgress: (index: number, status: "cloning" | "ok" | "error", error?: string) => void,
    signal?: AbortSignal,
  ) => Promise<void>;
  cloneRepo: (url: string, dest: string, name: string) => Promise<void>;
  /** Batch clone: each item to `<dest>/<name>`, sequential + tolerant. Reports
   *  per-item progress; refreshes the repo list once at the end. */
  importRepos: (
    items: Array<{ url: string; name: string }>,
    dest: string,
    onProgress: (index: number, status: "cloning" | "ok" | "error", error?: string) => void,
    signal?: AbortSignal,
  ) => Promise<void>;
  createRepo: (name: string, dest: string) => Promise<void>;
  createThread: (title: string, kind: string) => Promise<Thread>;
  createDirection: (
    threadId: number,
    name: string,
    tool: string,
    repoId: number,
    reason: string,
  ) => Promise<void>;
  deleteThread: (threadId: number) => Promise<void>;
  /** Delete a finished task's worktree (directory + record); keeps the branch. */
  deleteWorktree: (worktreeId: number, directionId: number) => Promise<void>;

  viewing: {
    directionId: number;
    repoId: number;
    /** Which side panel to open when entering the session view. */
    sidePanel?: "diff" | "files";
  } | null;
  viewDirection: (
    directionId: number,
    repoId: number,
    opts?: { sidePanel?: "diff" | "files" },
  ) => void;
  driveDirection: (
    directionId: number,
    repoId: number,
    focus: boolean,
  ) => Promise<number | null>;
  sendToWorker: (
    directionId: number,
    repoId: number,
    text: string,
    images?: ImageAttachment[],
    files?: string[],
  ) => Promise<void>;
  reviveDirection: (directionId: number) => Promise<void>;
  closeObserve: () => void;
  /** Set a task's lifecycle status (human override). */
  setTaskStatus: (directionId: number, status: string) => Promise<void>;
  /** Quality loop: executable-check results + in-flight set, per direction. */
  checksByDirection: Record<number, RepoChecks[]>;
  checkingDirections: Record<number, boolean>;
  verifyDirection: (directionId: number) => Promise<void>;
  /** Review-agent rung: on-demand pre-PR self-review verdict + in-flight set. */
  /**
   * Run the global review skill inside the direction's own session.
   * `focus` surfaces the worker conversation so the human watches the review
   * command land (manual trigger); auto-review leaves it headless.
   */
  requestSkillReview: (
    directionId: number,
    opts?: { focus?: boolean },
  ) => Promise<void>;
  /** The configured review skill ("" = auto-detect superpowers'). */
  reviewSkill: string;
  setReviewSkill: (s: string) => void;
  /** Auto-run the review skill when a task flows into the review column. */
  autoReview: boolean;
  setAutoReview: (on: boolean) => void;
  /** OS notifications for new Needs-you items / review-ready sub-tasks. */
  notifyEnabled: boolean;
  setNotifyEnabled: (on: boolean) => void;
  /** Prevent system idle sleep while any session is running. */
  keepAwake: boolean;
  setKeepAwake: (on: boolean) => void;
  /** App updater: available update metadata, or null if none. */
  updateAvailable: { version: string; body: string } | null;
  /** Download, install, and relaunch into the new version. */
  installUpdate: () => Promise<void>;
  /** Dismiss the update nudge for this session. */
  dismissUpdate: () => void;
  focusSession: (sessionId: number) => void;
}

const Ctx = createContext<Store | null>(null);
export const useStore = () => {
  const s = useContext(Ctx);
  if (!s) throw new Error("useStore outside provider");
  return s;
};

// Curator panel (in the Repo Map) UI prefs, persisted per workspace.
const CURATOR_WIDTH_DEFAULT = 360;
const CURATOR_WIDTH_MIN = 280;
const CURATOR_WIDTH_MAX = 560;
const curatorOpenKey = (ws: number) => `weft.curatorPanel.${ws}.open`;
const curatorWidthKey = (ws: number) => `weft.curatorPanel.${ws}.width`;

export function StoreProvider({ children }: { children: ReactNode }) {
  const [workspaces, setWorkspaces] = useState<Workspace[]>([]);
  const [activeWorkspaceId, setActiveWorkspaceId] = useState<number | null>(null);
  // Live mirror so async tasks (e.g. a slow batch clone) can check the CURRENT
  // workspace instead of the stale one captured when they started.
  const activeWorkspaceIdRef = useRef(activeWorkspaceId);
  activeWorkspaceIdRef.current = activeWorkspaceId;
  const [repos, setRepos] = useState<RepoRef[]>([]);
  const [threads, setThreads] = useState<Thread[]>([]);
  const [directionsByThread, setDirections] = useState<Record<number, Direction[]>>({});
  const [worktreesByDirection, setWorktrees] = useState<Record<number, Worktree[]>>({});
  const [activeThreadId, setActiveThreadId] = useState<number | null>(null);
  // Live mirror so async tasks can check the CURRENT active thread instead of
  // the stale one captured when they started (mirrors activeWorkspaceIdRef).
  const activeThreadIdRef = useRef(activeThreadId);
  activeThreadIdRef.current = activeThreadId;
  const [sessions, setSessions] = useState<Record<number, OpenSession>>({});
  const [checksByDirection, setChecksByDirection] = useState<Record<number, RepoChecks[]>>({});
  const [checkingDirections, setCheckingDirections] = useState<Record<number, boolean>>({});
  // Directions with an auto-(re)dispatch in flight, so the poll-driven effect
  // never spawns a duplicate worker before the first spawn lands in `sessions`.
  const dispatchingRef = useRef<Set<number>>(new Set());
  const sessionsRef = useRef(sessions);
  sessionsRef.current = sessions;
  const [viewing, setViewing] = useState<{
    directionId: number;
    repoId: number;
    sidePanel?: "diff" | "files";
  } | null>(null);
  const [messages, setMessages] = useState<BusMsg[]>([]);
  const [needs, setNeeds] = useState<NeedItem[]>([]);
  const [asks, setAsks] = useState<PermissionAsk[]>([]);
  const [writeTriggers, setWriteTriggers] = useState<WriteTrigger[]>([]);
  const [needsByWorkspace, setNeedsByWorkspace] = useState<Record<number, number>>({});
  const [showNeeds, setShowNeeds] = useState(false);
  const [repoProfiles, setRepoProfiles] = useState<RepoProfile[]>([]);
  const [repoEdges, setRepoEdges] = useState<RepoEdge[]>([]);
  const [homeTab, setHomeTab] = useState<HomeTab>("board");
  const [curatorThreadId, setCuratorThreadId] = useState<number | null>(null);
  const [curatorPanelOpen, setCuratorPanelOpenState] = useState(true);
  const [curatorPanelWidth, setCuratorPanelWidthState] = useState(CURATOR_WIDTH_DEFAULT);
  const [repoDrawerOpen, setRepoDrawerOpen] = useState(false);
  const [repoDrawerTab, setRepoDrawerTabState] = useState<"detail" | "curator">("detail");
  const [selectedRepoId, setSelectedRepoId] = useState<number | null>(null);
  const [repoDrawerWidth, setRepoDrawerWidthState] = useState(CURATOR_WIDTH_DEFAULT);
  // Coalesce curator-thread creation per workspace: StrictMode double-mounts and
  // the backend get-or-create is not atomic, so concurrent ensures for the SAME
  // workspace could create dupes. Keyed by ws so switching to another workspace
  // mid-flight still ensures that one (a single boolean would drop it).
  const ensuringCuratorRef = useRef<Set<number>>(new Set());
  // Snapshot of the view that was active when the user opened Settings, so
  // the back arrow restores it instead of dropping them on the board.
  const prevHomeRef = useRef<{
    homeTab: HomeTab;
    activeThreadId: number | null;
    viewing: { directionId: number; repoId: number; sidePanel?: "diff" | "files" } | null;
    showNeeds: boolean;
  } | null>(null);
  const [proposal, setProposal] = useState<ResolvedProposal | null>(null);
  const [overview, setOverview] = useState<ThreadOverview[]>([]);
  // Thread-bus drawer + proposal-review state.
  const [showBus, setShowBus] = useState(false);
  const [reviewingProposal, setReviewingProposal] = useState(false);
  const [threadTab, setThreadTab] = useState<ThreadTab>("lead");
  const [navCollapsed, setNavCollapsed] = useState(() => window.innerWidth < 820);

  // App settings, persisted to localStorage.
  const [projectsDir, setProjectsDirState] = useState(
    () => localStorage.getItem("weft-projects-dir") ?? "",
  );
  const setProjectsDir = useCallback((p: string) => {
    localStorage.setItem("weft-projects-dir", p);
    setProjectsDirState(p);
  }, []);
  const [defaultTool, setDefaultToolState] = useState("codex");
  const [configuredTool, setConfiguredTool] = useState<string | null>(null);
  const [installedTools, setInstalledTools] = useState<ToolStatus[]>([]);
  // Re-probe the CLIs on demand (the diagnostics panel's Refresh button).
  const refreshInstalledTools = useCallback(async () => {
    try {
      setInstalledTools(await api.detectTools());
    } catch {
      // Pure-vite dev without the Tauri backend.
    }
  }, []);
  // Re-resolve the effective default tool — saving an alias can change it (a
  // configured tool that was falling back becomes available, or vice versa), so
  // the picker and Needs-you approvals must not keep a stale value.
  const refreshDefaultTool = useCallback(async () => {
    try {
      const info = await api.getDefaultTool();
      setDefaultToolState(info.tool);
      setConfiguredTool(info.configured);
    } catch {
      // Pure-vite dev without the Tauri backend.
    }
  }, []);
  useEffect(() => {
    void (async () => {
      try {
        const [info, tools] = await Promise.all([api.getDefaultTool(), api.detectTools()]);
        setDefaultToolState(info.tool);
        setConfiguredTool(info.configured);
        setInstalledTools(tools);
      } catch {
        // Pure-vite dev without the Tauri backend: keep the static defaults.
      }
    })();
  }, []);
  const setDefaultTool = useCallback((tl: string) => {
    setDefaultToolState(tl);
    setConfiguredTool(tl);
    void api.setDefaultTool(tl);
  }, []);
  // The global review skill: "" = auto-detect from the agent's own slash list.
  const [reviewSkill, setReviewSkillState] = useState(
    () => localStorage.getItem("weft-review-skill") ?? "",
  );
  const setReviewSkill = useCallback((s: string) => {
    localStorage.setItem("weft-review-skill", s);
    setReviewSkillState(s);
  }, []);
  // Auto-review: entering the review column runs the review skill (with a
  // self-repair directive) in the sub-task's own session. Default ON.
  const [autoReview, setAutoReviewState] = useState(
    () => localStorage.getItem("weft-auto-review") !== "0",
  );
  const setAutoReview = useCallback((on: boolean) => {
    localStorage.setItem("weft-auto-review", on ? "1" : "0");
    setAutoReviewState(on);
  }, []);
  // System notifications: new Needs-you items / review-ready sub-tasks raise an
  // OS notification while the window is unfocused. Default ON.
  const [notifyEnabled, setNotifyEnabledState] = useState(
    () => localStorage.getItem("weft-notify") !== "0",
  );
  const setNotifyEnabled = useCallback((on: boolean) => {
    localStorage.setItem("weft-notify", on ? "1" : "0");
    setNotifyEnabledState(on);
  }, []);
  // Keep-awake: hold a "prevent idle sleep" OS assertion while any session is
  // busy (the display may still turn off). Default ON; synced to the backend
  // on launch — its state is in-memory (same pattern as dangerous mode).
  const [keepAwake, setKeepAwakeState] = useState(
    () => localStorage.getItem("weft-keep-awake") !== "0",
  );
  const setKeepAwake = useCallback((on: boolean) => {
    localStorage.setItem("weft-keep-awake", on ? "1" : "0");
    setKeepAwakeState(on);
    void api.setKeepAwake(on);
  }, []);
  useEffect(() => {
    void api.setKeepAwake(localStorage.getItem("weft-keep-awake") !== "0");
  }, []);
  // Auto-check for app updates on launch and hourly thereafter (silent; only
  // surface when found, and don't re-nag a version the user already dismissed).
  const [updateAvailable, setUpdateAvailable] = useState<{ version: string; body: string } | null>(null);
  const dismissedUpdateRef = useRef<string | null>(null);
  useEffect(() => {
    let cancelled = false;
    const run = async () => {
      try {
        const { checkUpdate } = await import("../lib/updater");
        const info = await checkUpdate();
        if (cancelled || !info) return;
        if (info.version === dismissedUpdateRef.current) return; // already dismissed this one
        setUpdateAvailable((prev) => (prev?.version === info.version ? prev : info));
      } catch {
        /* updater unavailable in dev or offline */
      }
    };
    void run();
    const UPDATE_POLL_MS = 60 * 60 * 1000; // re-check hourly for long-running sessions
    const id = setInterval(() => void run(), UPDATE_POLL_MS);
    return () => {
      cancelled = true;
      clearInterval(id);
    };
  }, []);
  const installUpdate = useCallback(async () => {
    const { installUpdate: doInstall } = await import("../lib/updater");
    await doInstall();
  }, []);
  const dismissUpdate = useCallback(() => {
    setUpdateAvailable((cur) => {
      if (cur) dismissedUpdateRef.current = cur.version; // suppress re-nag until a newer version
      return null;
    });
  }, []);
  const [dangerousMode, setDangerousModeState] = useState(
    () => localStorage.getItem("weft-dangerous") === "1",
  );
  const setDangerousMode = useCallback((on: boolean) => {
    localStorage.setItem("weft-dangerous", on ? "1" : "0");
    setDangerousModeState(on);
    void api.setDangerousMode(on);
    // Turning it on retro-approves the existing permission backlog (the backend
    // releases the blocked agents); clear them from the UI now. Human questions
    // (needs) are NOT auto-answered — they stay.
    if (on) setAsks([]);
  }, []);
  const [dangerNudge, setDangerNudge] = useState<"ask" | "enabled" | null>(null);
  // Sync the persisted Dangerous-mode flag to the backend on launch (the bus
  // registry starts fresh each run).
  useEffect(() => {
    void api.setDangerousMode(localStorage.getItem("weft-dangerous") === "1");
  }, []);

  // Runaway guardrails (§7): idle + wall-clock caps in MINUTES, persisted. The
  // backend seeds its defaults from the WEFT_* env, so we only push when the user
  // has an explicit saved value — an env override survives an untouched install.
  const [idleCapMins, setIdleCapMins] = useState(
    () => Number(localStorage.getItem("weft-idle-cap-mins") ?? "30"),
  );
  const [wallCapMins, setWallCapMins] = useState(
    () => Number(localStorage.getItem("weft-wall-cap-mins") ?? "120"),
  );
  const setGuardrails = useCallback((idleMins: number, wallMins: number) => {
    const idle = Math.max(0, Math.round(idleMins));
    const wall = Math.max(0, Math.round(wallMins));
    localStorage.setItem("weft-idle-cap-mins", String(idle));
    localStorage.setItem("weft-wall-cap-mins", String(wall));
    setIdleCapMins(idle);
    setWallCapMins(wall);
    void api.setGuardrails(idle * 60, wall * 60);
  }, []);
  useEffect(() => {
    const i = localStorage.getItem("weft-idle-cap-mins");
    const w = localStorage.getItem("weft-wall-cap-mins");
    if (i != null && w != null) void api.setGuardrails(Number(i) * 60, Number(w) * 60);
  }, []);

  // Auto-collapse the sidebar when the window gets narrow; auto-restore when it
  // widens again (only on threshold crossings, so manual toggles stick).
  useEffect(() => {
    const TH = 820;
    let prevNarrow = window.innerWidth < TH;
    const onResize = () => {
      const narrow = window.innerWidth < TH;
      if (narrow !== prevNarrow) {
        prevNarrow = narrow;
        setNavCollapsed(narrow);
      }
    };
    window.addEventListener("resize", onResize);
    return () => window.removeEventListener("resize", onResize);
  }, []);

  const refreshWorkspaces = useCallback(async () => {
    const ws = await api.listWorkspaces();
    setWorkspaces(ws);
    setActiveWorkspaceId((cur) => {
      // Keep the live selection as long as it still exists.
      if (cur != null && ws.some((w) => w.id === cur)) return cur;
      // Cold start / webview reload drops the in-memory selection back to null;
      // restore the last-used workspace instead of snapping to the first one.
      // Only fall back to the first when the saved id is gone (deleted) or there
      // is none yet.
      const saved = Number(localStorage.getItem("weft-active-workspace"));
      if (saved && ws.some((w) => w.id === saved)) return saved;
      return ws[0]?.id ?? null;
    });
  }, []);

  const selectWorkspace = useCallback(async (id: number) => {
    setActiveWorkspaceId(id);
    // Clear the old workspace's repo map first so the curator panel (gated on
    // repoProfiles.length >= 2) can't mount from stale, other-workspace profiles
    // during the switch and ensure a thread for the wrong workspace.
    setRepoProfiles([]);
    setRepoEdges([]);
    // Remember the choice so a relaunch/reload lands here, not on the first one.
    localStorage.setItem("weft-active-workspace", String(id));
    // Curator panel: drop the previous workspace's thread id and load this
    // workspace's remembered panel state (absent = first visit = open).
    setCuratorThreadId(null);
    const openRaw = localStorage.getItem(curatorOpenKey(id));
    setCuratorPanelOpenState(openRaw == null ? true : openRaw === "1");
    const wRaw = Number(localStorage.getItem(curatorWidthKey(id)));
    setCuratorPanelWidthState(
      Number.isFinite(wRaw) && wRaw > 0
        ? Math.min(CURATOR_WIDTH_MAX, Math.max(CURATOR_WIDTH_MIN, wRaw))
        : CURATOR_WIDTH_DEFAULT,
    );
    // Repos drawer: width persists (shared key with the old curator panel), but
    // open state does not — each workspace visit starts with the canvas full-width.
    setRepoDrawerOpen(false);
    setRepoDrawerTabState("detail");
    setSelectedRepoId(null);
    const dwRaw = Number(localStorage.getItem(curatorWidthKey(id)));
    setRepoDrawerWidthState(
      Number.isFinite(dwRaw) && dwRaw > 0
        ? Math.min(CURATOR_WIDTH_MAX, Math.max(CURATOR_WIDTH_MIN, dwRaw))
        : CURATOR_WIDTH_DEFAULT,
    );
    const [r, t] = await Promise.all([api.listRepos(id), api.listThreads(id)]);
    setRepos(r);
    setThreads(t);
    setDirections({});
    setWorktrees({});
    setActiveThreadId(null);
    setViewing(null);
    setShowNeeds(false);
    setHomeTab("board");
    setProposal(null);
    setOverview([]);
  }, []);

  const loadThreadChildren = useCallback(async (threadId: number) => {
    const dirs = await api.listDirections(threadId);
    setDirections((m) => ({ ...m, [threadId]: dirs }));
    const entries = await Promise.all(
      dirs.map(async (d) => [d.id, await api.listWorktrees(d.id)] as const),
    );
    setWorktrees((m) => {
      const next = { ...m };
      for (const [id, wts] of entries) next[id] = wts;
      return next;
    });
  }, []);

  const selectThread = useCallback(
    async (threadId: number) => {
      setActiveThreadId(threadId);
      setViewing(null);
      setShowNeeds(false);
      setHomeTab("board");
      setThreadTab("lead");
      setShowBus(false);
      setReviewingProposal(false);
      try {
        setProposal(await api.getProposal(threadId));
      } catch {
        setProposal(null);
      }
      await loadThreadChildren(threadId);
    },
    [loadThreadChildren],
  );

  const refreshOverview = useCallback(async () => {
    if (activeWorkspaceId == null) {
      setOverview([]);
      return;
    }
    try {
      setOverview(await api.workspaceOverview(activeWorkspaceId));
    } catch {
      /* ignore */
    }
  }, [activeWorkspaceId]);

  const backToWorkspace = useCallback(() => {
    setActiveThreadId(null);
    setViewing(null);
    setShowNeeds(false);
    setHomeTab("board");
    setThreadTab("lead");
  }, []);

  const openSettings = useCallback(() => {
    // Snapshot first — once we flip homeTab + clear thread/viewing the
    // info is gone and the back arrow can't restore it.
    prevHomeRef.current = {
      homeTab,
      activeThreadId,
      viewing,
      showNeeds,
    };
    setActiveThreadId(null);
    setViewing(null);
    setShowNeeds(false);
    setHomeTab("settings");
  }, [homeTab, activeThreadId, viewing, showNeeds]);

  const closeSettings = useCallback(() => {
    const prev = prevHomeRef.current;
    prevHomeRef.current = null;
    if (!prev) {
      // First-launch / direct deep link into Settings — nothing to restore.
      setHomeTab("board");
      return;
    }
    setShowNeeds(prev.showNeeds);
    setViewing(prev.viewing);
    setActiveThreadId(prev.activeThreadId);
    setHomeTab(prev.homeTab === "settings" ? "board" : prev.homeTab);
  }, []);

  const createWorkspace = useCallback(
    async (name: string) => {
      const ws = await api.createWorkspace(name);
      await refreshWorkspaces();
      await selectWorkspace(ws.id);
    },
    [refreshWorkspaces, selectWorkspace],
  );

  const renameWorkspace = useCallback(async (workspaceId: number, name: string) => {
    const ws = await api.renameWorkspace(workspaceId, name);
    setWorkspaces((cur) => cur.map((w) => (w.id === ws.id ? ws : w)));
  }, []);

  const renameThread = useCallback(
    async (threadId: number, title: string) => {
      const t = await api.renameThread(threadId, title);
      setThreads((cur) => cur.map((x) => (x.id === t.id ? t : x)));
      // needs/asks/write-triggers carry a snapshot of thread_title; patch in place
      setNeeds((cur) =>
        cur.map((n) => (n.thread_id === t.id ? { ...n, thread_title: t.title } : n)),
      );
      setAsks((cur) =>
        cur.map((a) => (a.thread === t.id ? { ...a, thread_title: t.title } : a)),
      );
      setWriteTriggers((cur) =>
        cur.map((w) => (w.thread_id === t.id ? { ...w, thread_title: t.title } : w)),
      );
      void refreshOverview();
    },
    [refreshOverview],
  );

  const renameDirection = useCallback(async (directionId: number, name: string) => {
    const d = await api.renameDirection(directionId, name);
    setDirections((m) => ({
      ...m,
      [d.thread_id]: (m[d.thread_id] ?? []).map((x) => (x.id === d.id ? d : x)),
    }));
    // needs.direction_name and asks.dir_name carry the direction's display name;
    // patch them in place so cards/notifications reflect the rename without
    // waiting for the next refreshNeeds poll. (WriteTrigger.name is a planned
    // direction not yet created, so it is unrelated to this rename.)
    setNeeds((cur) =>
      cur.map((n) => (n.direction_id === d.id ? { ...n, direction_name: d.name } : n)),
    );
    setAsks((cur) =>
      cur.map((a) =>
        a.thread === d.thread_id && a.dir === d.slug ? { ...a, dir_name: d.name } : a,
      ),
    );
  }, []);

  const refreshReposAndMap = useCallback(async (workspaceId?: number) => {
    // Compare against the LIVE active workspace (ref), so a refresh for a
    // workspace the user has since left is dropped instead of overwriting the
    // current workspace's repo list/map (e.g. after a cancelled batch import).
    const ws = workspaceId ?? activeWorkspaceIdRef.current;
    if (ws == null || ws !== activeWorkspaceIdRef.current) return;
    const list = await api.listRepos(ws);
    if (ws !== activeWorkspaceIdRef.current) return; // user switched during the fetch
    setRepos(list);
    // a freshly added repo is eagerly profiled server-side; pull the new map
    try {
      const g = await api.repoGraph(ws);
      if (ws !== activeWorkspaceIdRef.current) return; // user switched during the fetch
      setRepoProfiles(g.nodes);
      setRepoEdges(g.edges);
    } catch {
      /* ignore */
    }
  }, []);

  const addRepo = useCallback(
    async (name: string, path: string) => {
      if (activeWorkspaceId == null) return;
      await api.addRepoRef(activeWorkspaceId, name, path);
      await refreshReposAndMap(activeWorkspaceId);
    },
    [activeWorkspaceId, refreshReposAndMap],
  );

  const addRepos = useCallback(
    async (
      items: Array<{ name: string; path: string }>,
      onProgress: (index: number, status: "cloning" | "ok" | "error", error?: string) => void,
      signal?: AbortSignal,
    ) => {
      if (activeWorkspaceId == null) return;
      // Sequential + tolerant, mirroring importRepos: a non-git path (or any
      // failure) reports per-row and doesn't abort the rest. Backend dedups
      // already-present repos silently, so re-adds are harmless no-ops.
      for (let i = 0; i < items.length; i++) {
        if (signal?.aborted) break;
        onProgress(i, "cloning");
        try {
          await api.addRepoRef(activeWorkspaceId, items[i].name, items[i].path);
          onProgress(i, "ok");
        } catch (e) {
          onProgress(i, "error", String(e));
        }
      }
      await refreshReposAndMap(activeWorkspaceId);
    },
    [activeWorkspaceId, refreshReposAndMap],
  );

  const cloneRepo = useCallback(
    async (url: string, dest: string, name: string) => {
      if (activeWorkspaceId == null) return;
      await api.cloneRepo(activeWorkspaceId, url, dest, name);
      await refreshReposAndMap(activeWorkspaceId);
    },
    [activeWorkspaceId, refreshReposAndMap],
  );

  const createRepo = useCallback(
    async (name: string, dest: string) => {
      if (activeWorkspaceId == null) return;
      await api.createRepo(activeWorkspaceId, name, dest);
      await refreshReposAndMap(activeWorkspaceId);
    },
    [activeWorkspaceId, refreshReposAndMap],
  );

  const importRepos = useCallback(
    async (
      items: Array<{ url: string; name: string }>,
      dest: string,
      onProgress: (index: number, status: "cloning" | "ok" | "error", error?: string) => void,
      signal?: AbortSignal,
    ) => {
      if (activeWorkspaceId == null) return;
      // Sequential + tolerant: one failed clone doesn't abort the rest. `signal`
      // (set when the dialog is closed/cancelled mid-batch) stops queuing the
      // next clone — the in-flight one still finishes, but no more are started.
      for (let i = 0; i < items.length; i++) {
        if (signal?.aborted) break;
        onProgress(i, "cloning");
        try {
          await api.cloneRepo(activeWorkspaceId, items[i].url, dest, items[i].name);
          onProgress(i, "ok");
        } catch (e) {
          onProgress(i, "error", String(e));
        }
      }
      // Refresh even on abort — the clones that already finished are real repos.
      await refreshReposAndMap(activeWorkspaceId);
    },
    [activeWorkspaceId, refreshReposAndMap],
  );

  const createThread = useCallback(
    async (title: string, kind: string) => {
      if (activeWorkspaceId == null) throw new Error("no workspace");
      const t = await api.createThread(activeWorkspaceId, title, kind);
      setThreads(await api.listThreads(activeWorkspaceId));
      void refreshOverview();
      return t;
    },
    [activeWorkspaceId],
  );

  const deleteThread = useCallback(
    async (threadId: number) => {
      await api.deleteThread(threadId);
      if (activeWorkspaceId != null)
        setThreads(await api.listThreads(activeWorkspaceId));
      setDirections((m) => {
        const n = { ...m };
        delete n[threadId];
        return n;
      });
      setActiveThreadId((cur) => (cur === threadId ? null : cur));
    },
    [activeWorkspaceId],
  );

  // Reclaim one finished task's worktree directory; the branch, row, and card
  // stay. The backend keeps the row (its record that Weft made this branch) and
  // just removes the directory, so mirror that by flipping the row's `exists` to
  // false — the card hides the Delete item and disables the now-defunct worktree's
  // actions, without losing the provenance.
  const deleteWorktree = useCallback(
    async (worktreeId: number, directionId: number) => {
      await api.deleteWorktree(worktreeId);
      setWorktrees((m) => {
        const cur = m[directionId];
        if (!cur) return m;
        return {
          ...m,
          [directionId]: cur.map((w) =>
            w.id === worktreeId ? { ...w, exists: false } : w,
          ),
        };
      });
    },
    [],
  );

  // ALL workers run on the chat engine — one product-native conversation UI
  // per vendor dialect (claude stream-json, codex exec --json, opencode run
  // --format json). Escape hatches per tool: codex app deep link, terminal
  // takeover command for all three.

  // Single entry to a worker's conversation surface. All "open/focus a worker"
  // paths route here → `viewing` → WorkerConversation (no separate activeSessionId).
  const openWorker = useCallback((directionId: number, repoId: number) => {
    setViewing({ directionId, repoId });
    setShowNeeds(false);
    setHomeTab("board");
  }, []);

  // Spawn (or focus) a worker for a (direction, repo) slot. focus=true opens it
  // full-screen (a click); focus=false dispatches it in the background.
  const spawnWorker = useCallback(
    async (directionId: number, repoId: number, focus: boolean) => {
      const existing = Object.values(sessionsRef.current).find(
        (s) => s.directionId === directionId && s.repoId === repoId,
      );
      if (existing) {
        if (focus) openWorker(directionId, repoId);
        return;
      }
      const info = await api.chatOpenWorker(directionId, repoId, currentLang());
      setSessions((m) => ({
        ...m,
        [info.session_id]: {
          info,
          status: "running",
          directionId,
          repoId,
          threadId: activeThreadId ?? -1,
          nativeId: info.native_id,
        },
      }));
      if (focus) openWorker(directionId, repoId);
    },
    [activeThreadId, openWorker],
  );

  const viewDirection = useCallback(
    (directionId: number, repoId: number, opts?: { sidePanel?: "diff" | "files" }) => {
      setViewing({ directionId, repoId, sidePanel: opts?.sidePanel });
      setShowNeeds(false);
      setHomeTab("board");
    },
    [],
  );

  const closeObserve = useCallback(() => setViewing(null), []);

  // Explicit "continue/attach": attach to a live session if one exists, else ask
  // the backend to resume the same native conversation (or fresh-dispatch only
  // when no native id was ever captured). Never re-seeds a live/finished task.
  const driveDirection = useCallback(
    async (
      directionId: number,
      repoId: number,
      focus: boolean,
    ): Promise<number | null> => {
      const existing = Object.values(sessionsRef.current).find(
        (s) =>
          s.directionId === directionId &&
          s.repoId === repoId &&
          s.status !== "exited",
      );
      if (existing) {
        if (focus) openWorker(directionId, repoId);
        return existing.info.session_id;
      }
      const info = await api.chatOpenWorker(directionId, repoId, currentLang());
      setSessions((m) => {
        const pruned = Object.fromEntries(
          Object.entries(m).filter(
            ([, s]) => !(s.directionId === directionId && s.repoId === repoId && s.status === "exited"),
          ),
        );
        return {
          ...pruned,
          [info.session_id]: {
            info,
            status: "running",
            directionId,
            repoId,
            threadId: activeThreadId ?? -1,
            nativeId: info.native_id,
          },
        };
      });
      if (focus) openWorker(directionId, repoId);
      return info.session_id;
    },
    [activeThreadId, openWorker],
  );

  // Adopt a backend-initiated worker (boot revive, or one still alive after a
  // frontend reload/HMR) into the session map so it gets a status dot. Idempotent
  // and keyed on the session id. Unlike driveDirection it NEVER calls chatOpenWorker
  // — the engine is already live, so there is nothing to start; calling it would
  // respawn a stopped worker. Uses the slot's OWN thread id (a revived worker can
  // belong to any thread, not activeThreadId). Auto-verify is handled separately and
  // authoritatively by the backend (see the idle-turn handler), so the busy seed
  // below is UI-only (typing indicator / Stop button / nav running count) and arms
  // no verify latch.
  const adoptWorker = useCallback((slot: LiveWorkerSlot) => {
    const sid = slot.info.session_id;
    if (slot.busy) {
      // Seed the worker's busy turn state so the chat surface shows the typing
      // indicator + Stop button and WorkspaceNav counts it as running — a revived
      // worker emits no turn push until its turn completes. Done BEFORE the
      // already-mapped early return so a session that driveDirection (the
      // active-thread redispatch) inserted without a workerTurn entry still gets
      // seeded. Guard on absence so a raced idle/stopped value the listener already
      // recorded wins. (Verify is backend-driven, so this seeds UI state only.)
      setWorkerTurn((t) =>
        t[sid] ? t : { ...t, [sid]: { state: "busy", queued: slot.queued } },
      );
    }
    if (sessionsRef.current[sid]) return;
    // Reconcile status with any turn state the lead-chat listener already recorded:
    // if the worker's idle push raced in before this adoption, the live slot still
    // says busy, but the dot/live-counts must show idle (not stuck "running").
    const ts = workerTurnRef.current[sid]?.state;
    const status: SessionStatus =
      ts === "idle" ? "idle" : ts === "stopped" ? "exited" : slot.busy ? "running" : "idle";
    setSessions((m) =>
      m[sid]
        ? m
        : {
            ...m,
            [sid]: {
              info: slot.info,
              status,
              directionId: slot.direction_id,
              repoId: slot.repo_id,
              threadId: slot.thread_id,
              nativeId: slot.info.native_id,
            },
          },
    );
  }, []);

  // Pull the backend's live worker engines and adopt any the frontend doesn't
  // know about. Called on mount (backstop for workers live before the listener
  // registered) and on the `worker-revived` event (boot revives that land after
  // mount). The in-flight guard collapses concurrent triggers; a request that
  // arrives mid-pull sets `pending` so the latest state is re-fetched afterward
  // (e.g. the revive event firing while the mount pull is still in flight).
  const hydratingRef = useRef(false);
  const hydratePendingRef = useRef(false);
  const hydrateLiveWorkers = useCallback(async () => {
    if (hydratingRef.current) {
      hydratePendingRef.current = true;
      return;
    }
    hydratingRef.current = true;
    try {
      do {
        hydratePendingRef.current = false;
        const slots = await api.listLiveWorkerSlots();
        // Load each adopted worker's thread directions so WorkspaceNav can match the
        // session to its direction and count it as running — a revived worker can
        // live in a thread the user never opened this session, whose
        // directionsByThread entry would otherwise be empty. (Best-effort; verify
        // does not depend on this — the backend reads the phase itself.)
        const threadIds = [...new Set(slots.map((s) => s.thread_id))];
        await Promise.all(
          threadIds.map(async (tid) => {
            try {
              const dirs = await api.listDirections(tid);
              setDirections((m) => ({ ...m, [tid]: dirs }));
            } catch {
              /* best-effort: a thread whose directions fail to load just won't
                 show its running count until opened */
            }
          }),
        );
        for (const slot of slots) adoptWorker(slot);
      } while (hydratePendingRef.current);
    } catch {
      /* best-effort hydration */
    } finally {
      hydratingRef.current = false;
    }
  }, [adoptWorker]);

  // Adopt backend-headless workers the frontend never drove (boot revive, or
  // alive after a reload/HMR). Register the `worker-revived` listener BEFORE the
  // mount pull: `listen` is async, so doing the pull first would leave a gap where
  // a boot sweep that emits the event between the pull's snapshot and the
  // subscription is lost with no later trigger. Awaiting `listen` first closes
  // that gap — the mount pull then runs with the listener live (anything revived
  // during it re-pulls via the pending guard), and later revives (whose
  // nudge-driven turns emit no busy push to react to) are caught by the event.
  useEffect(() => {
    let un: (() => void) | undefined;
    let cancelled = false;
    void (async () => {
      un = await listen("worker-revived", () => void hydrateLiveWorkers());
      if (cancelled) {
        un();
        un = undefined;
        return;
      }
      void hydrateLiveWorkers();
    })();
    return () => {
      cancelled = true;
      un?.();
    };
  }, [hydrateLiveWorkers]);

  // Lazy attach + send: the worker surface is always input-able. Sending into a
  // worker with no live engine transparently resumes/dispatches it (focus=false,
  // so we stay on the same surface — no navigation), then delivers the message.
  // resume reuses the prior session row, so session_id is stable (no flicker).
  const sendToWorker = useCallback(
    async (
      directionId: number,
      repoId: number,
      text: string,
      images?: ImageAttachment[],
      files?: string[],
    ) => {
      const live = Object.values(sessionsRef.current).find(
        (s) => s.directionId === directionId && s.repoId === repoId && s.status !== "exited",
      );
      // driveDirection returns the (possibly freshly-resumed) session id directly.
      // sessionsRef won't reflect a new session until React re-renders, so re-scanning
      // it here would drop the first message to an idle/recovered worker (the send
      // would no-op after ChatComposer already cleared the input).
      const sessionId =
        live?.info.session_id ?? (await driveDirection(directionId, repoId, false));
      if (sessionId == null) return;
      await api.chatSend(sessionId, text, images, files);
    },
    [driveDirection],
  );

  // Automation-first (§4 principle 7): once a task is materialized, dispatch its
  // worker(s) right away — every write worktree gets an agent, no human click.
  const dispatchDirection = useCallback(
    async (directionId: number) => {
      let wts;
      try {
        wts = await api.listWorktrees(directionId);
      } catch {
        return;
      }
      // Skip reclaimed worktrees (exists=false): the directory is gone, so
      // spawning a worker in it would fail.
      for (const w of wts.filter((w) => w.exists)) {
        await spawnWorker(directionId, w.repo_id, false);
      }
    },
    [spawnWorker],
  );

  // Restart continuity (§4 principle 7): bring a working task's worker back by
  // RESUME (not a fresh re-run) once per repo. Reuses driveDirection's
  // resume-or-fresh + dedupe-by-live logic.
  const reviveDirection = useCallback(
    async (directionId: number) => {
      let wts;
      try {
        wts = await api.listWorktrees(directionId);
      } catch {
        return;
      }
      // Skip reclaimed worktrees (exists=false): a resume would drive a worker
      // into a missing cwd.
      for (const w of wts.filter((w) => w.exists)) {
        await driveDirection(directionId, w.repo_id, false);
      }
    },
    [driveDirection],
  );

  const createDirection = useCallback(
    async (
      threadId: number,
      name: string,
      tool: string,
      repoId: number,
      reason: string,
    ) => {
      const dir = await api.createDirection(threadId, name, tool, repoId, reason);
      await loadThreadChildren(threadId);
      void dispatchDirection(dir.id);
    },
    [loadThreadChildren, dispatchDirection],
  );

  // ── Lead chat (weft-owned conversation; engine pushes via `lead-chat`) ──
  const [leadMessages, setLeadMessages] = useState<Record<number, LeadMessage[]>>({});
  const [leadTurn, setLeadTurn] = useState<
    Record<number, { state: "busy" | "idle" | "stopped"; queued: number }>
  >({});
  const [leadSlash, setLeadSlash] = useState<Record<number, SlashCmd[]>>({});
  const [workerTurn, setWorkerTurn] = useState<
    Record<number, { state: "busy" | "idle" | "stopped"; queued: number }>
  >({});
  const workerTurnRef = useRef(workerTurn);
  workerTurnRef.current = workerTurn;
  const [workerSlash, setWorkerSlash] = useState<Record<number, SlashCmd[]>>({});
  const [leadActivity, setLeadActivity] = useState<
    Record<number, { name: string; summary: string } | null>
  >({});
  const [workerActivity, setWorkerActivity] = useState<
    Record<number, { name: string; summary: string } | null>
  >({});
  const [leadMeta, setLeadMeta] = useState<Record<number, SessionMeta>>({});
  const [workerMeta, setWorkerMeta] = useState<Record<number, SessionMeta>>({});
  const hydrateWorkerMeta = useCallback((sessionId: number, snap: ObserveRef) => {
    // First-paint only: the snapshot carries no per-server tools (claude's tool
    // catalog arrives via the `init` event), so never overwrite richer live meta
    // — the 2s session_for poll would otherwise wipe the MCP tool lists.
    setWorkerMeta((m) => (m[sessionId] ? m : { ...m, [sessionId]: metaFromSnapshot(snap) }));
  }, []);
  const mergeWorkerMeta = useCallback((sessionId: number, snap: SessionMetaSnapshot) => {
    setWorkerMeta((m) => ({ ...m, [sessionId]: mergeSnapshot(m[sessionId], snap) }));
  }, []);
  const mergeLeadMeta = useCallback((threadId: number, snap: SessionMetaSnapshot) => {
    setLeadMeta((m) => ({ ...m, [threadId]: mergeSnapshot(m[threadId], snap) }));
  }, []);
  // Skills dirty latch: bump on any skills mutation; idle sessions/leads compare
  // against their last-refreshed stamp to flag one engine refresh per episode.
  const [skillsDirtyAt, setSkillsDirtyAt] = useState(0);
  const markSkillsDirty = useCallback(() => setSkillsDirtyAt(Date.now()), []);
  const skillsRefreshedRef = useRef<Record<number, number>>({});
  // Last worker turn state seen by the lead-chat listener, kept synchronously so
  // auto-verify fires once per real turn end (see the turn handler).
  const lastWorkerTurnRef = useRef<Record<number, string>>({});

  useEffect(() => {
    const un = listen<LeadChatPush>("lead-chat", (e) => {
      const p = e.payload;
      if (p.type === "message") {
        setLeadMessages((m) => {
          const list = m[p.thread_id] ?? [];
          if (list.some((x) => x.id === p.message.id)) return m;
          return { ...m, [p.thread_id]: [...list, p.message] };
        });
      } else if (p.type === "delta") {
        setLeadMessages((m) => ({
          ...m,
          [p.thread_id]: (m[p.thread_id] ?? []).map((x) => {
            if (x.id !== p.message_id) return x;
            let text = "";
            try {
              text = (JSON.parse(x.content).text as string) ?? "";
            } catch {
              /* fresh row */
            }
            return { ...x, content: JSON.stringify({ text: text + p.text }) };
          }),
        }));
      } else if (p.type === "finalize") {
        setLeadMessages((m) => ({
          ...m,
          [p.thread_id]: (m[p.thread_id] ?? []).map((x) =>
            x.id === p.message_id
              ? {
                  ...x,
                  status: p.status as LeadMessage["status"],
                  // Replace the streamed body when the engine sends cleaned content
                  // (sentinels stripped post-stream) so the raw tags vanish live.
                  ...(p.content != null
                    ? { content: JSON.stringify({ text: p.content }) }
                    : {}),
                }
              : x,
          ),
        }));
      } else if (p.type === "tool_result") {
        // A running tool row got its output: replace content + status in place.
        setLeadMessages((m) => ({
          ...m,
          [p.thread_id]: (m[p.thread_id] ?? []).map((x) =>
            x.id === p.message_id
              ? { ...x, content: p.content, status: p.status as LeadMessage["status"] }
              : x,
          ),
        }));
      } else if (p.type === "activity") {
        const act = { name: p.name, summary: p.summary };
        if (p.session_id != null) {
          const sid = p.session_id;
          setWorkerActivity((a) => ({ ...a, [sid]: act }));
        } else {
          setLeadActivity((a) => ({ ...a, [p.thread_id]: act }));
        }
      } else if (p.type === "turn") {
        if (p.session_id != null) {
          const sid = p.session_id;
          // Prior turn state (synchronous) so auto-verify fires on a real turn end
          // (busy/undefined→idle), not on every idle push: per-turn dialects
          // (codex/opencode) emit a terminal idle then an EOF idle for ONE turn, and
          // a revived worker's first observed state IS the idle push (no busy push).
          const prevTurn = lastWorkerTurnRef.current[sid];
          lastWorkerTurnRef.current[sid] = p.state;
          setWorkerActivity((a) => ({ ...a, [sid]: null }));
          setWorkerTurn((t) => ({ ...t, [sid]: { state: p.state, queued: p.queued } }));
          setSessions((m) =>
            m[sid]
              ? {
                  ...m,
                  [sid]: {
                    ...m[sid],
                    status:
                      p.state === "busy" ? "running" : p.state === "idle" ? "idle" : "exited",
                  },
                }
              : m,
          );
          // Backend-authoritative auto-verify: when a worker turn ends, ask the
          // backend (fresh DB phase read) whether this direction should be checked,
          // and run it if so. Replaces the old frontend busy→idle/phase-cache effect
          // — works for any worker (known, revived, or headless) and reads the phase
          // at completion time, so a planning→working transition mid-turn is honored.
          if (p.state === "idle" && prevTurn !== "idle") {
            void (async () => {
              try {
                const dirId = await api.autoVerifyCheck(sid);
                if (dirId != null) verifyDirectionRef.current(dirId);
              } catch {
                /* best-effort auto-verify */
              }
            })();
          }

        } else {
          setLeadActivity((a) => ({ ...a, [p.thread_id]: null }));
          setLeadTurn((t) => ({
            ...t,
            [p.thread_id]: { state: p.state, queued: p.queued },
          }));
        }
      } else if (p.type === "init") {
        if (p.session_id != null) {
          const sid = p.session_id;
          setWorkerSlash((s) => ({ ...s, [sid]: p.slash_commands }));
          setWorkerMeta((m) => ({ ...m, [sid]: metaFromInit(m[sid], p) }));
          // The early initialize-derived push has no native id yet — keep the old one.
          if (p.native_id) {
            setSessions((m) =>
              m[sid] ? { ...m, [sid]: { ...m[sid], nativeId: p.native_id } } : m,
            );
          }
        } else {
          setLeadSlash((s) => ({ ...s, [p.thread_id]: p.slash_commands }));
          setLeadMeta((m) => ({ ...m, [p.thread_id]: metaFromInit(m[p.thread_id], p) }));
        }
        // An init implies a live engine: a stale "stopped" flips to idle (a
        // turn event will overwrite the moment anything actually runs).
        if (p.session_id != null) {
          const sid = p.session_id;
          setWorkerTurn((t) =>
            (t[sid]?.state ?? "stopped") === "stopped"
              ? { ...t, [sid]: { state: "idle", queued: 0 } }
              : t,
          );
        } else {
          setLeadTurn((t) =>
            (t[p.thread_id]?.state ?? "stopped") === "stopped"
              ? { ...t, [p.thread_id]: { state: "idle", queued: 0 } }
              : t,
          );
        }
      } else if (p.type === "usage") {
        if (p.session_id != null) {
          const sid = p.session_id;
          setWorkerMeta((m) => ({ ...m, [sid]: metaFromUsage(m[sid], p) }));
        } else {
          setLeadMeta((m) => ({ ...m, [p.thread_id]: metaFromUsage(m[p.thread_id], p) }));
        }
      }
    });
    return () => {
      void un.then((f) => f());
    };
  }, []);

  const discoverLeadSlash = useCallback((threadId: number) => {
    void (async () => {
      try {
        await api.leadEnsure(threadId, currentLang());
      } catch {
        /* discovery can still try the non-resident fallback below */
      }
      try {
        const cmds = await api.discoverSlash(threadId, null);
        if (cmds.length > 0) {
          setLeadSlash((s) => ({ ...s, [threadId]: cmds }));
        }
      } catch {
        /* slash discovery is best-effort */
      }
    })();
  }, []);

  const loadLeadChat = useCallback(async (threadId: number) => {
    const msgs = await api.listLeadMessages(threadId);
    setLeadMessages((m) => ({
      ...m,
      [threadId]: msgs.filter((x) => x.kind !== "meta"),
    }));
    // Fire the engine up and refresh slash commands, including workspace skills.
    discoverLeadSlash(threadId);
    try {
      const st = await api.leadState(threadId);
      setLeadTurn((t) => ({
        ...t,
        [threadId]: { state: st.state, queued: st.queued },
      }));
      if (st.slash_commands.length > 0) {
        setLeadSlash((s) => ({ ...s, [threadId]: st.slash_commands }));
      }
      // First-paint only (same reason as hydrateWorkerMeta): don't let a
      // tool-less snapshot clobber the init event's MCP tool catalog on remount.
      setLeadMeta((m) => (m[threadId] ? m : { ...m, [threadId]: metaFromSnapshot(st) }));
    } catch {
      /* engine state is cosmetic at load time */
    }
  }, [discoverLeadSlash]);

  // Pull a worker's slash commands on demand: opencode runs live GET /command,
  // claude returns its initialize list, codex mirrors TUI built-ins plus skills.
  // Best-effort — an empty result leaves the existing palette untouched.
  const discoverWorkerSlash = useCallback((sessionId: number) => {
    void api
      .discoverSlash(null, sessionId)
      .then((cmds) => {
        if (cmds.length > 0) setWorkerSlash((s) => ({ ...s, [sessionId]: cmds }));
      })
      .catch(() => {});
  }, []);

  const sendLeadChat = useCallback(
    async (threadId: number, text: string, images?: ImageAttachment[], files?: string[]) => {
      await api.leadSend(threadId, text, currentLang(), images, files);
    },
    [],
  );

  const interruptLead = useCallback(async (threadId: number) => {
    await api.leadInterrupt(threadId);
  }, []);

  const setTaskStatus = useCallback(async (directionId: number, status: string) => {
    // optimistic: flip the card now, then persist
    setDirections((m) => {
      const next: Record<number, Direction[]> = {};
      for (const [tid, list] of Object.entries(m)) {
        next[Number(tid)] = list.map((d) =>
          d.id === directionId ? { ...d, status } : d,
        );
      }
      return next;
    });
    try {
      await api.setTaskStatus(directionId, status);
    } catch {
      /* reverts on next poll */
    }
  }, []);

  const verifyingRef = useRef<Set<number>>(new Set());
  const verifyAgainRef = useRef<Set<number>>(new Set());
  const verifyDirection = useCallback(async (directionId: number) => {
    if (verifyingRef.current.has(directionId)) {
      // A run is in flight; request one more pass after it (coalesced) so a
      // re-verify (e.g. review-then-repair) isn't dropped and results don't lag
      // the latest code.
      verifyAgainRef.current.add(directionId);
      return;
    }
    verifyingRef.current.add(directionId);
    setCheckingDirections((m) => ({ ...m, [directionId]: true }));
    try {
      do {
        verifyAgainRef.current.delete(directionId);
        const res = await api.verifyDirection(directionId);
        setChecksByDirection((m) => ({ ...m, [directionId]: res }));
      } while (verifyAgainRef.current.has(directionId));
    } catch {
      /* leave prior results */
    } finally {
      verifyAgainRef.current.delete(directionId);
      verifyingRef.current.delete(directionId);
      setCheckingDirections((m) => ({ ...m, [directionId]: false }));
    }
  }, []);
  const verifyDirectionRef = useRef(verifyDirection);
  verifyDirectionRef.current = verifyDirection;

  // Review = the global review skill running INSIDE the worker's own
  // conversation (no built-in review engine; the repo's PR harness stays the
  // authority). Auto-detect prefers superpowers' requesting-code-review when
  // the agent reports it; the setting overrides.
  const resolveReviewSkill = useCallback(() => {
    const configured = reviewSkill.trim().replace(/^\//, "");
    if (configured) return configured;
    const all = [...Object.values(leadSlash), ...Object.values(workerSlash)].flat();
    return (
      all.find((c) => /(^|:)requesting-code-review$/.test(c.name))?.name ??
      "superpowers:requesting-code-review"
    );
  }, [reviewSkill, leadSlash, workerSlash]);

  const requestSkillReview = useCallback(
    async (directionId: number, opts?: { focus?: boolean }) => {
      const writes = await api.listWorktrees(directionId).catch(() => []);
      // Only a worktree still on disk can be reviewed; a reclaimed one
      // (exists=false) has no working copy to open.
      const first = writes.find((w) => w.exists);
      if (!first) return;
      const live = Object.values(sessionsRef.current).find(
        (s) => s.directionId === directionId && s.status !== "exited",
      );
      // Manual trigger: open the worker conversation up front so the human lands
      // in the session and watches the review command get inserted. Auto-review
      // (opts undefined) stays headless — it only surfaces the post-fix state.
      if (opts?.focus) openWorker(directionId, live?.repoId ?? first.repo_id);
      // driveDirection returns the (possibly freshly-resumed) session id directly;
      // sessionsRef won't reflect a new session until React re-renders, so reuse
      // that id rather than re-scanning the ref (which could miss the just-created
      // session and drop the review command, stranding the user in an idle view).
      const sessionId =
        live?.info.session_id ??
        (await driveDirection(directionId, first.repo_id, false));
      if (sessionId == null) return;
      // Review-then-repair: the skill reviews, the same agent fixes what it
      // found and re-verifies — the human only sees the post-fix state.
      const directive =
        currentLang() === "zh"
          ? "review 结束后，直接修复发现的问题并重新跑检查自验，然后简要汇报。"
          : "After the review, fix the findings directly, re-run the checks to verify, then report briefly.";
      const cmd = `/${resolveReviewSkill()} ${directive}`;
      await api.chatSend(sessionId, cmd);
    },
    [driveDirection, openWorker, resolveReviewSkill],
  );

  // Automation-first: a task flowing into "review" triggers the review skill
  // by itself (once per entry; the setting turns this off).
  const autoReviewedRef = useRef<Set<number>>(new Set());
  useEffect(() => {
    const all = Object.values(directionsByThread).flat();
    for (const d of all) {
      if (d.status !== "review") {
        autoReviewedRef.current.delete(d.id);
        continue;
      }
      if (!autoReview || autoReviewedRef.current.has(d.id)) continue;
      autoReviewedRef.current.add(d.id);
      void requestSkillReview(d.id);
    }
  }, [directionsByThread, autoReview, requestSkillReview]);

  const focusSession = useCallback((id: number) => {
    const s = sessionsRef.current[id];
    if (s) openWorker(s.directionId, s.repoId);
  }, [openWorker]);

  const postHuman = useCallback(
    async (to: string | null, text: string) => {
      if (activeThreadId == null || !text.trim()) return;
      await api.busPostHuman(activeThreadId, to, text.trim());
    },
    [activeThreadId],
  );

  const refreshNeeds = useCallback(async () => {
    // Permission Asks are global (not workspace-scoped); always refresh them.
    try {
      setAsks(await api.pendingAsks());
    } catch {
      /* server may not be ready */
    }
    if (activeWorkspaceId == null) {
      setNeeds([]);
      setWriteTriggers([]);
      return;
    }
    try {
      setNeeds(await api.needsYou(activeWorkspaceId));
      setWriteTriggers(await api.writeTriggers(activeWorkspaceId));
    } catch {
      /* bus may not be ready */
    }
    // per-workspace counts so the switcher can flag OTHER workspaces.
    try {
      setNeedsByWorkspace(Object.fromEntries(await api.workspaceNeedsCounts()));
    } catch {
      /* ignore */
    }
  }, [activeWorkspaceId]);

  const openNeeds = useCallback(() => {
    setViewing(null);
    setHomeTab("board");
    setShowNeeds(true);
  }, []);

  const refreshRepoMap = useCallback(async () => {
    const ws = activeWorkspaceId;
    if (ws == null) {
      setRepoProfiles([]);
      setRepoEdges([]);
      return;
    }
    try {
      const g = await api.repoGraph(ws);
      // Guard against a workspace switch during the fetch (e.g. a late
      // repo-graph-updated event for the old workspace): don't write ws's graph
      // into a now-different active view.
      if (activeWorkspaceIdRef.current !== ws) return;
      setRepoProfiles(g.nodes);
      setRepoEdges(g.edges);
    } catch {
      /* workspace may be empty */
    }
  }, [activeWorkspaceId]);

  const openRepoMap = useCallback(() => {
    setActiveThreadId(null);
    setShowNeeds(false);
    setViewing(null); // else Main renders WorkerConversation over the repo tab
    setHomeTab("repos");
    void refreshRepoMap();
  }, [refreshRepoMap]);

  const reprofileRepo = useCallback(
    async (repoId: number) => {
      await api.reprofileRepo(repoId);
      await refreshRepoMap();
    },
    [refreshRepoMap],
  );

  // Manually re-run the agent dependency curator, then refresh the map so the
  // newly inferred runtime/infra edges appear. Slow (spawns a read-only agent).
  const reanalyzeDeps = useCallback(async () => {
    const ws = activeWorkspaceId;
    if (ws == null) return;
    await api.analyzeWorkspaceDeps(ws);
    // The analysis can run up to the curator timeout; if the user switched
    // workspaces meanwhile, don't overwrite the now-current map with ws's graph.
    if (activeWorkspaceIdRef.current === ws) await refreshRepoMap();
  }, [activeWorkspaceId, refreshRepoMap]);

  const editRepoSummary = useCallback(
    async (repoId: number, summary: string) => {
      await api.updateRepoProfile(repoId, summary, null);
      await refreshRepoMap();
    },
    [refreshRepoMap],
  );
  const editRepoTier = useCallback(
    async (repoId: number, tier: string) => {
      await api.updateRepoProfile(repoId, null, tier);
      await refreshRepoMap();
    },
    [refreshRepoMap],
  );

  // Remove a repo from the workspace (ref + profile + bound tasks + worktrees);
  // the user's repo on disk is untouched. Refreshes the repo list + map after.
  const deleteRepo = useCallback(
    async (repoId: number) => {
      await api.deleteRepo(repoId);
      if (activeWorkspaceId != null) await refreshReposAndMap(activeWorkspaceId);
      // delete_repo cascades directions/sessions/worktrees bound to the repo
      // across threads — refresh the board overview and the open thread's children
      // so stale task cards (now pointing at deleted rows) don't linger and open
      // blank worker views or failed diff/session fetches.
      await refreshOverview();
      if (activeThreadId != null) await loadThreadChildren(activeThreadId);
    },
    [activeWorkspaceId, refreshReposAndMap, refreshOverview, activeThreadId, loadThreadChildren],
  );

  // Ensure the workspace's hidden curator thread exists and remember its id —
  // WITHOUT navigating. The curator chat renders embedded in the Repo Map panel
  // (CuratorPanel), so unlike a normal lead chat we never selectThread.
  const ensureCuratorThread = useCallback(async () => {
    const ws = activeWorkspaceId;
    if (ws == null || ensuringCuratorRef.current.has(ws)) return;
    ensuringCuratorRef.current.add(ws);
    try {
      const id = await api.openCuratorChat(ws); // get-or-create; returns the id
      const list = await api.listThreads(ws);
      // Bail if the user switched workspaces while these requests were in flight.
      if (activeWorkspaceIdRef.current !== ws) return;
      // The curator thread may have just been created — sync `threads` so the
      // embedded LeadTab can resolve its lead_tool.
      setThreads(list);
      setCuratorThreadId(id);
    } finally {
      ensuringCuratorRef.current.delete(ws);
    }
  }, [activeWorkspaceId]);

  const setCuratorPanelOpen = useCallback((open: boolean) => {
    setCuratorPanelOpenState(open);
    const ws = activeWorkspaceIdRef.current;
    if (ws != null) localStorage.setItem(curatorOpenKey(ws), open ? "1" : "0");
  }, []);

  const setCuratorPanelWidth = useCallback((w: number) => {
    const clamped = Math.min(CURATOR_WIDTH_MAX, Math.max(CURATOR_WIDTH_MIN, Math.round(w)));
    setCuratorPanelWidthState(clamped);
    const ws = activeWorkspaceIdRef.current;
    if (ws != null) localStorage.setItem(curatorWidthKey(ws), String(clamped));
  }, []);

  const openRepoDetail = useCallback((repoId: number) => {
    setSelectedRepoId(repoId);
    setRepoDrawerTabState("detail");
    setRepoDrawerOpen(true);
  }, []);
  const openCurator = useCallback(() => {
    setRepoDrawerTabState("curator");
    setRepoDrawerOpen(true);
  }, []);
  const closeRepoDrawer = useCallback(() => setRepoDrawerOpen(false), []);
  const setRepoDrawerTab = useCallback((tab: "detail" | "curator") => setRepoDrawerTabState(tab), []);
  const setRepoDrawerWidth = useCallback((w: number) => {
    const clamped = Math.min(CURATOR_WIDTH_MAX, Math.max(CURATOR_WIDTH_MIN, Math.round(w)));
    setRepoDrawerWidthState(clamped);
    const ws = activeWorkspaceIdRef.current;
    if (ws != null) localStorage.setItem(curatorWidthKey(ws), String(clamped));
  }, []);

  const refreshProposal = useCallback(async (threadId: number) => {
    try {
      setProposal(await api.getProposal(threadId));
    } catch {
      setProposal(null);
    }
  }, []);

  const saveProposal = useCallback(
    async (next: Proposal) => {
      if (activeThreadId == null) return;
      await api.saveProposal(activeThreadId, next);
      await refreshProposal(activeThreadId);
    },
    [activeThreadId, refreshProposal],
  );

  // In-flight base-branch save, keyed PER THREAD — confirm/approve flush their OWN
  // thread's pending save before acting (the field saves on blur fire-and-forget), and
  // a cross-thread reset must never corrupt another thread's serialization chain.
  const pendingBaseSave = useRef<Map<number, Promise<unknown>>>(new Map());
  // Base saves that rejected, keyed thread → set of failed LANE identities
  // (`name\0repo`). The chain's `.catch` swallows predecessors for serialization, so
  // confirm/approve consult this latch (not just the final promise) before acting.
  // Per-LANE within a thread: a successful retry of one lane clears only THAT lane's
  // failure, so a different lane's success can't mask an earlier lane's stale base;
  // confirm/approve abort while the thread still has ANY unrecovered lane failure.
  const baseSaveFailed = useRef<Map<number, Set<string>>>(new Map());

  const confirmProposal = useCallback(async () => {
    if (activeThreadId == null) return;
    // Flush any in-flight base-branch save before materializing. If it REJECTED
    // (e.g. a re-propose moved the lane, or a DB error), the backend still holds the
    // old base — refresh the proposal to the real state and abort rather than
    // materializing from a stale base. Consume the promise so a retry isn't blocked
    // by the settled rejection.
    const pending = pendingBaseSave.current.get(activeThreadId) ?? Promise.resolve();
    pendingBaseSave.current.delete(activeThreadId);
    try {
      await pending;
    } catch {
      // handled by the latch below
    }
    // Also abort if any EARLIER link in the chain failed — the chain's
    // `.catch(() => {})` swallows predecessors for serialization, so the final
    // promise may resolve even when a prior save rejected.
    if ((baseSaveFailed.current.get(activeThreadId)?.size ?? 0) > 0) {
      baseSaveFailed.current.delete(activeThreadId);
      await refreshProposal(activeThreadId);
      return;
    }
    const ids = await api.confirmProposal(activeThreadId);
    setProposal(null);
    setReviewingProposal(false);
    await loadThreadChildren(activeThreadId);
    // Automation-first: dispatch every new task's worker immediately.
    for (const id of ids) void dispatchDirection(id);
  }, [activeThreadId, loadThreadChildren, dispatchDirection, refreshProposal]);

  const setProposalDirectionBase = useCallback(
    (index: number, name: string, repo: string, base: string, expectedOldBase: string): Promise<void> => {
      if (activeThreadId == null) return Promise.resolve();
      const tid = activeThreadId;
      // Include the lane INDEX: a proposal can hold two pending writes with the same
      // name+repo, and a name+repo-only key would be shared — one lane's successful save
      // would then clear the other lane's still-pending failure, so confirm/approve would
      // stop aborting even though the first lane's base edit never landed.
      const laneKey = `${index}\0${name}\0${repo}`;
      // Serialize onto any in-flight base save (chain, don't replace) and use the
      // targeted server-side setter — no whole-proposal rebuild from stale state,
      // no status downgrade. confirm/approve await pendingBaseSave before acting.
      // `expectedOldBase` is the persisted base the field was editing FROM: the backend
      // rejects the save if a same-identity re-propose changed the lane's base meanwhile.
      const p = (pendingBaseSave.current.get(tid) ?? Promise.resolve())
        .catch(() => {})
        .then(() => api.setProposalDirectionBase(tid, index, name, repo, expectedOldBase, base.trim()))
        .then(() => {
          // This LANE's save LANDED — clear ONLY this lane's failure (not the whole
          // thread), so a successful retry isn't treated as failed by the next
          // Create/Approve, while a DIFFERENT lane's earlier failure stays latched.
          baseSaveFailed.current.get(tid)?.delete(laneKey);
          // Don't let a save that completes after a thread switch overwrite the
          // global proposal with the old thread's data.
          if (activeThreadIdRef.current === tid) {
            return refreshProposal(tid);
          }
        })
        .catch((err) => {
          // Latch so confirm/approve know ANY link in the chain rejected, not just
          // the final one. The predecessor's `.catch(() => {})` already swallows
          // earlier rejections for serialization purposes; this terminal catch sets
          // the latch and re-throws so THIS link's own promise also rejects (the
          // NEXT edit's `.catch(() => {})` will swallow it in turn).
          const set = baseSaveFailed.current.get(tid) ?? new Set<string>();
          set.add(laneKey);
          baseSaveFailed.current.set(tid, set);
          throw err;
        });
      pendingBaseSave.current.set(tid, p);
      return p;
    },
    [activeThreadId, refreshProposal],
  );

  const answerAsk = useCallback(
    async (item: NeedItem, text: string) => {
      if (!text.trim()) return;
      // optimistic: drop the answered ask immediately, then reconcile
      setNeeds((cur) => cur.filter((n) => n.ask_id !== item.ask_id));
      await api.answerAsk(item.thread_id, item.ask_id, text.trim());
      await refreshNeeds();
    },
    [refreshNeeds],
  );

  const approveWriteTrigger = useCallback(
    async (item: WriteTrigger, tool?: string) => {
      // Flush any in-flight base-branch save first. If it REJECTED (re-propose moved
      // the lane, or a DB error), the backend still holds the old base — refresh the
      // active proposal and abort rather than approving from a stale base. Mirrors
      // confirmProposal. Consume the promise so a retry isn't blocked.
      const pending = pendingBaseSave.current.get(item.thread_id) ?? Promise.resolve();
      pendingBaseSave.current.delete(item.thread_id);
      try {
        await pending;
      } catch {
        // handled by the latch below
      }
      // Also abort if any EARLIER link in the chain failed — the chain's
      // `.catch(() => {})` swallows predecessors for serialization, so the final
      // promise may resolve even when a prior save rejected.
      if ((baseSaveFailed.current.get(item.thread_id)?.size ?? 0) > 0) {
        baseSaveFailed.current.delete(item.thread_id);
        // A rejected base save can mean a re-proposal changed the lane indices, so this
        // card's item.index is now stale. Refresh the Needs cards (dropping the stale
        // one) as well as the proposal before aborting — otherwise a second click on the
        // stale card would approve/dispatch the WRONG lane once the latch is cleared.
        await refreshNeeds();
        if (activeThreadId != null) await refreshProposal(activeThreadId);
        return;
      }
      setWriteTriggers((cur) =>
        cur.filter((w) => !(w.thread_id === item.thread_id && w.index === item.index)),
      );
      try {
        const dirId = await api.approveWriteTrigger(item.thread_id, item.index, tool ?? defaultTool);
        void dispatchDirection(dirId);
      } finally {
        await refreshNeeds();
      }
    },
    [dispatchDirection, refreshNeeds, defaultTool, activeThreadId, refreshProposal],
  );

  const denyWriteTrigger = useCallback(
    async (item: WriteTrigger) => {
      // Flush any in-flight base-branch save for this thread FIRST: a blur-save still
      // writing the proposal can otherwise land AFTER deny_direction and restore the
      // lane's decision:"" — making the denied write reappear as pending. Mirrors the
      // flush in confirmProposal/approveWriteTrigger.
      const pending = pendingBaseSave.current.get(item.thread_id) ?? Promise.resolve();
      pendingBaseSave.current.delete(item.thread_id);
      try {
        await pending;
      } catch {
        // handled by the latch check below
      }
      // If that save REJECTED, a re-proposal may have moved/replaced the lanes (the
      // server-side base setter rejects when item.index's lane was replaced), so
      // item.index is no longer trustworthy — denying by it could deny the WRONG lane.
      // Abort and refresh to the real state instead of denying a stale index.
      if ((baseSaveFailed.current.get(item.thread_id)?.size ?? 0) > 0) {
        baseSaveFailed.current.delete(item.thread_id);
        await refreshNeeds();
        return;
      }
      setWriteTriggers((cur) =>
        cur.filter((w) => !(w.thread_id === item.thread_id && w.index === item.index)),
      );
      try {
        await api.denyWriteTrigger(item.thread_id, item.index);
      } finally {
        await refreshNeeds();
      }
    },
    [refreshNeeds],
  );

  const answerPermission = useCallback(
    async (askId: number, answer: "allow" | "deny" | "always" | "full") => {
      // optimistic: drop the card immediately, then unblock the tool
      setAsks((cur) => cur.filter((a) => a.id !== askId));
      // Per-day nudge: granting broad access (always / full) without Dangerous
      // mode → once a day, suggest turning it on.
      if ((answer === "always" || answer === "full") && !dangerousMode) {
        const today = new Date().toISOString().slice(0, 10);
        if (localStorage.getItem("weft-danger-nudge") !== today) {
          localStorage.setItem("weft-danger-nudge", today);
          setDangerNudge("ask");
        }
      }
      try {
        await api.answerPermission(askId, answer);
      } catch {
        /* already resolved/expired */
      }
    },
    [dangerousMode],
  );

  const goToAsk = useCallback(
    async (item: NeedItem) => {
      setShowNeeds(false);
      setViewing(null);
      const live = Object.values(sessions).find(
        (s) => s.directionId === item.direction_id,
      );
      if (live) {
        setActiveThreadId(item.thread_id);
        openWorker(live.directionId, live.repoId);
        return;
      }
      await selectThread(item.thread_id);
    },
    [sessions, selectThread, openWorker],
  );

  useEffect(() => {
    void refreshWorkspaces();
  }, [refreshWorkspaces]);
  useEffect(() => {
    if (activeWorkspaceId != null) void selectWorkspace(activeWorkspaceId);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [activeWorkspaceId]);

  // Reset a thread's sub-view (lead tab, in-flight proposal review) only when the
  // active thread actually CHANGES. This lives in the store — not in ThreadBoard —
  // so it survives the board unmounting/remounting as the worker overlay opens and
  // closes; otherwise every "back" out of a worker snapped to the lead chat. Paths
  // that set the thread directly (e.g. opening a Needs-you ask) are covered too.
  const prevThreadRef = useRef<number | null>(null);
  useEffect(() => {
    if (activeThreadId === prevThreadRef.current) return;
    prevThreadRef.current = activeThreadId;
    if (activeThreadId != null) {
      setThreadTab("lead");
      setReviewingProposal(false);
    }
  }, [activeThreadId]);

  // Needs-you: poll workspace-wide, plus a push refresh when the coordinator
  // signals a new ask (needs-you://changed). Poll is the safety net; the event
  // makes new questions appear near-instantly.
  useEffect(() => {
    if (activeWorkspaceId == null) {
      setNeeds([]);
      return;
    }
    let alive = true;
    const tick = () => {
      if (alive) void refreshNeeds();
    };
    tick();
    const h = setInterval(tick, 4000);
    const unChanged = listen("needs-you://changed", tick);
    return () => {
      alive = false;
      clearInterval(h);
      void unChanged.then((f) => f());
    };
  }, [activeWorkspaceId, refreshNeeds]);

  // Live-refresh the repo map when the curator calibrates an edge (or the auto
  // pass finishes) for the active workspace.
  useEffect(() => {
    const un = listen<number>("repo-graph-updated", (e) => {
      if (e.payload === activeWorkspaceId) void refreshRepoMap();
    });
    return () => {
      void un.then((f) => f());
    };
  }, [activeWorkspaceId, refreshRepoMap]);

  // While an issue is open, keep its proposal fresh (the lead re-proposes over
  // the chat engine; the timeline card is the anchor, this state feeds the
  // scope-confirm canvas). Cheap local read, so a simple poll is fine.
  useEffect(() => {
    if (activeThreadId == null) return;
    const thread = activeThreadId;
    let alive = true;
    const tick = async () => {
      try {
        const p = await api.getProposal(thread);
        if (alive && p) setProposal(p);
      } catch {
        /* planner not ready */
      }
    };
    void tick();
    const h = setInterval(tick, 2500);
    return () => {
      alive = false;
      clearInterval(h);
    };
  }, [activeThreadId]);

  // Idle skill-refresh: when skills changed (dirty timestamp) and a session goes
  // busy→idle, flag its engine once so the next send picks up new skills.
  const prevWorkerTurnRef = useRef<Record<number, string>>({});
  useEffect(() => {
    for (const [sidStr, turn] of Object.entries(workerTurn)) {
      const sid = Number(sidStr);
      const prev = prevWorkerTurnRef.current[sid];
      prevWorkerTurnRef.current[sid] = turn.state;
      if (prev === "busy" && turn.state === "idle" &&
          skillsDirtyAt > (skillsRefreshedRef.current[sid] ?? 0)) {
        skillsRefreshedRef.current[sid] = Date.now();
        void api.flagSessionSkillRefresh(sid).catch(() => {});
      }
    }
  }, [workerTurn, skillsDirtyAt]);

  const prevLeadTurnRef = useRef<Record<number, string>>({});
  useEffect(() => {
    for (const [tidStr, turn] of Object.entries(leadTurn)) {
      const tid = Number(tidStr);
      const prev = prevLeadTurnRef.current[tid];
      prevLeadTurnRef.current[tid] = turn.state;
      // lead engines refreshed in the same per-id ref space, negative-keyed to
      // avoid colliding with worker session ids.
      const key = -tid;
      if (prev === "busy" && turn.state === "idle" &&
          skillsDirtyAt > (skillsRefreshedRef.current[key] ?? 0)) {
        skillsRefreshedRef.current[key] = Date.now();
        void api.flagLeadSkillRefresh(tid).catch(() => {});
      }
    }
  }, [leadTurn, skillsDirtyAt]);

  useEffect(() => {
    if (activeThreadId == null) {
      setMessages([]);
      return;
    }
    let alive = true;
    const tick = async () => {
      try {
        const m = await api.threadMessages(activeThreadId);
        if (alive) setMessages(m);
      } catch {
        /* bus may not be ready */
      }
      // reflect agent-driven status changes (set via the bus MCP tool)
      try {
        const dirs = await api.listDirections(activeThreadId);
        if (alive) setDirections((m) => ({ ...m, [activeThreadId]: dirs }));
      } catch {
        /* ignore */
      }
    };
    void tick();
    const h = setInterval(tick, 1500);
    return () => {
      alive = false;
      clearInterval(h);
    };
  }, [activeThreadId]);

  // Automation-first across restarts (§4 principle 7): a task that's "working"
  // but has no live session — e.g. after an app restart, when in-memory engines
  // are gone — gets its worker (re)dispatched so it runs without a manual click.
  // Spawning reuses the existing worktree, so the agent continues the task.
  useEffect(() => {
    if (activeThreadId == null) return;
    const dirs = directionsByThread[activeThreadId] ?? [];
    for (const d of dirs) {
      if (d.status !== "working") continue;
      const hasLive = Object.values(sessionsRef.current).some(
        (s) => s.directionId === d.id && s.status !== "exited",
      );
      if (hasLive || dispatchingRef.current.has(d.id)) continue;
      dispatchingRef.current.add(d.id);
      void reviveDirection(d.id).finally(() => dispatchingRef.current.delete(d.id));
    }
  }, [activeThreadId, directionsByThread, reviveDirection]);

  const value: Store = {
    workspaces,
    activeWorkspaceId,
    repos,
    threads,
    directionsByThread,
    worktreesByDirection,
    activeThreadId,
    sessions,
    messages,
    postHuman,
    leadMessages,
    leadTurn,
    leadSlash,
    loadLeadChat,
    discoverLeadSlash,
    sendLeadChat,
    interruptLead,
    workerTurn,
    workerSlash,
    discoverWorkerSlash,
    leadActivity,
    workerActivity,
    leadMeta,
    workerMeta,
    hydrateWorkerMeta,
    mergeWorkerMeta,
    mergeLeadMeta,
    showBus,
    setShowBus,
    navCollapsed,
    setNavCollapsed,
    reviewingProposal,
    setReviewingProposal,
    threadTab,
    setThreadTab,
    skillsDirtyAt,
    markSkillsDirty,
    projectsDir,
    setProjectsDir,
    defaultTool,
    setDefaultTool,
    configuredTool,
    installedTools,
    refreshInstalledTools,
    refreshDefaultTool,
    dangerousMode,
    setDangerousMode,
    dangerNudge,
    setDangerNudge,
    idleCapMins,
    wallCapMins,
    setGuardrails,
    needs,
    asks,
    writeTriggers,
    approveWriteTrigger,
    denyWriteTrigger,
    needsByWorkspace,
    showNeeds,
    openNeeds,
    refreshNeeds,
    answerAsk,
    goToAsk,
    answerPermission,
    repoProfiles,
    repoEdges,
    homeTab,
    setHomeTab,
    openSettings,
    closeSettings,
    openRepoMap,
    refreshRepoMap,
    refreshReposAndMap,
    reprofileRepo,
    reanalyzeDeps,
    deleteRepo,
    curatorThreadId,
    ensureCuratorThread,
    curatorPanelOpen,
    setCuratorPanelOpen,
    curatorPanelWidth,
    setCuratorPanelWidth,
    repoDrawerOpen,
    repoDrawerTab,
    selectedRepoId,
    repoDrawerWidth,
    openRepoDetail,
    openCurator,
    closeRepoDrawer,
    setRepoDrawerTab,
    setRepoDrawerWidth,
    editRepoSummary,
    editRepoTier,
    proposal,
    refreshProposal,
    saveProposal,
    confirmProposal,
    setProposalDirectionBase,
    overview,
    refreshOverview,
    selectWorkspace,
    refreshWorkspaces,
    selectThread,
    loadThreadChildren,
    backToWorkspace,
    createWorkspace,
    renameWorkspace,
    renameThread,
    renameDirection,
    addRepo,
    addRepos,
    cloneRepo,
    importRepos,
    createRepo,
    createThread,
    createDirection,
    deleteThread,
    deleteWorktree,
    viewing,
    viewDirection,
    driveDirection,
    sendToWorker,
    reviveDirection,
    closeObserve,
    setTaskStatus,
    checksByDirection,
    checkingDirections,
    verifyDirection,
    requestSkillReview,
    reviewSkill,
    setReviewSkill,
    autoReview,
    setAutoReview,
    notifyEnabled,
    setNotifyEnabled,
    keepAwake,
    setKeepAwake,
    focusSession,
    updateAvailable,
    installUpdate,
    dismissUpdate,
  };
  return <Ctx.Provider value={value}>{children}</Ctx.Provider>;
}
