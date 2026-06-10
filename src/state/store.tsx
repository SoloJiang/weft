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
import type {
  BusMsg,
  Direction,
  ImageAttachment,
  LeadChatPush,
  LeadMessage,
  NeedItem,
  PermissionAsk,
  Proposal,
  RepoChecks,
  RepoEdge,
  RepoProfile,
  RepoRef,
  ResolvedProposal,
  ThreadOverview,
  SessionInfo,
  SessionStatus,
  Thread,
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
  activeSessionId: number | null;
  messages: BusMsg[];
  postHuman: (to: string | null, text: string) => Promise<void>;

  /** Lead chat: weft-owned timeline per thread (engine pushes, no polling). */
  leadMessages: Record<number, LeadMessage[]>;
  /** Lead engine turn state per thread: busy/idle/stopped + queue depth. */
  leadTurn: Record<number, { state: "busy" | "idle" | "stopped"; queued: number }>;
  /** Slash commands the lead's CLI reports as available (init event). */
  leadSlash: Record<number, string[]>;
  /** Hydrate a thread's timeline from DB + make sure the engine runs. */
  loadLeadChat: (threadId: number) => Promise<void>;
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
  workerSlash: Record<number, string[]>;
  /** The tool call running right now (transient): lead by thread, worker by session. */
  leadActivity: Record<number, { name: string; summary: string } | null>;
  workerActivity: Record<number, { name: string; summary: string } | null>;
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

  /** Open agent→human questions across the workspace; the Needs-you surface. */
  needs: NeedItem[];
  /** Pending tool permission requests (the Ask Bridge). */
  asks: PermissionAsk[];
  /** Lead-proposed write declarations awaiting human approve/deny. */
  writeTriggers: WriteTrigger[];
  approveWriteTrigger: (item: WriteTrigger) => Promise<void>;
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
  /** Jump to the workspace home's Repos tab. */
  openRepoMap: () => void;
  refreshRepoMap: () => Promise<void>;
  reprofileRepo: (repoId: number) => Promise<void>;
  editRepoProfile: (repoId: number, summary: string, role: string) => Promise<void>;

  /** The active thread's plan proposal (Task → scope), if any. */
  proposal: ResolvedProposal | null;
  refreshProposal: (threadId: number) => Promise<void>;
  saveProposal: (proposal: Proposal) => Promise<void>;
  confirmProposal: () => Promise<void>;

  /** Workspace board: per-thread roll-ups for the portfolio view. */
  overview: ThreadOverview[];
  refreshOverview: () => Promise<void>;

  selectWorkspace: (id: number) => Promise<void>;
  refreshWorkspaces: () => Promise<void>;
  selectThread: (threadId: number) => Promise<void>;
  loadThreadChildren: (threadId: number) => Promise<void>;
  backToBoard: () => void;
  /** Leave the active thread for the workspace portfolio board. */
  backToWorkspace: () => void;

  createWorkspace: (name: string) => Promise<void>;
  addRepo: (name: string, path: string) => Promise<void>;
  cloneRepo: (url: string, dest: string, name: string) => Promise<void>;
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

  viewing: { directionId: number; repoId: number; diff?: boolean } | null;
  viewDirection: (directionId: number, repoId: number, opts?: { diff?: boolean }) => void;
  driveDirection: (directionId: number, repoId: number, focus: boolean) => Promise<void>;
  reviveDirection: (directionId: number) => Promise<void>;
  closeObserve: () => void;
  /** Set a task's lifecycle status (human override). */
  setTaskStatus: (directionId: number, status: string) => Promise<void>;
  /** Quality loop: executable-check results + in-flight set, per direction. */
  checksByDirection: Record<number, RepoChecks[]>;
  checkingDirections: Record<number, boolean>;
  verifyDirection: (directionId: number) => Promise<void>;
  /** Review-agent rung: on-demand pre-PR self-review verdict + in-flight set. */
  /** Run the global review skill inside the direction's own session. */
  requestSkillReview: (directionId: number) => Promise<void>;
  /** Deliver a message to a (direction, repo)'s worker, waking it if needed. */
  sendToDirection: (directionId: number, repoId: number, text: string) => Promise<void>;
  /** The configured review skill ("" = auto-detect superpowers'). */
  reviewSkill: string;
  setReviewSkill: (s: string) => void;
  /** Auto-run the review skill when a task flows into the review column. */
  autoReview: boolean;
  setAutoReview: (on: boolean) => void;
  focusSession: (sessionId: number) => void;
}

const Ctx = createContext<Store | null>(null);
export const useStore = () => {
  const s = useContext(Ctx);
  if (!s) throw new Error("useStore outside provider");
  return s;
};

export function StoreProvider({ children }: { children: ReactNode }) {
  const [workspaces, setWorkspaces] = useState<Workspace[]>([]);
  const [activeWorkspaceId, setActiveWorkspaceId] = useState<number | null>(null);
  const [repos, setRepos] = useState<RepoRef[]>([]);
  const [threads, setThreads] = useState<Thread[]>([]);
  const [directionsByThread, setDirections] = useState<Record<number, Direction[]>>({});
  const [worktreesByDirection, setWorktrees] = useState<Record<number, Worktree[]>>({});
  const [activeThreadId, setActiveThreadId] = useState<number | null>(null);
  const [sessions, setSessions] = useState<Record<number, OpenSession>>({});
  const [checksByDirection, setChecksByDirection] = useState<Record<number, RepoChecks[]>>({});
  const [checkingDirections, setCheckingDirections] = useState<Record<number, boolean>>({});
  // Idle tracking for the auto-verify loop: last PTY-output time per session,
  // and which directions we've already auto-checked this idle episode.
  const autoCheckedRef = useRef<Set<number>>(new Set());
  // Directions with an auto-(re)dispatch in flight, so the poll-driven effect
  // never spawns a duplicate worker before the first spawn lands in `sessions`.
  const dispatchingRef = useRef<Set<number>>(new Set());
  const sessionsRef = useRef(sessions);
  sessionsRef.current = sessions;
  const [activeSessionId, setActiveSessionId] = useState<number | null>(null);
  const [viewing, setViewing] = useState<{
    directionId: number;
    repoId: number;
    diff?: boolean;
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
  const [defaultTool, setDefaultToolState] = useState(
    () => localStorage.getItem("weft-default-tool") ?? "claude",
  );
  const setDefaultTool = useCallback((tl: string) => {
    localStorage.setItem("weft-default-tool", tl);
    setDefaultToolState(tl);
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
    setActiveWorkspaceId((cur) => cur ?? ws[0]?.id ?? null);
  }, []);

  const selectWorkspace = useCallback(async (id: number) => {
    setActiveWorkspaceId(id);
    const [r, t] = await Promise.all([api.listRepos(id), api.listThreads(id)]);
    setRepos(r);
    setThreads(t);
    setDirections({});
    setWorktrees({});
    setActiveThreadId(null);
    setActiveSessionId(null);
    setViewing(null);
    setShowNeeds(false);
    setHomeTab("board");
    setRepoProfiles([]);
    setRepoEdges([]);
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
      setActiveSessionId(null);
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

  const backToBoard = useCallback(() => setActiveSessionId(null), []);

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
    setActiveSessionId(null);
    setViewing(null);
    setShowNeeds(false);
    setHomeTab("board");
    setThreadTab("lead");
  }, []);

  const createWorkspace = useCallback(
    async (name: string) => {
      const ws = await api.createWorkspace(name);
      await refreshWorkspaces();
      await selectWorkspace(ws.id);
    },
    [refreshWorkspaces, selectWorkspace],
  );

  const refreshAfterRepo = useCallback(async (ws: number) => {
    setRepos(await api.listRepos(ws));
    // a freshly added repo is eagerly profiled server-side; pull the new map
    try {
      const g = await api.repoGraph(ws);
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
      await refreshAfterRepo(activeWorkspaceId);
    },
    [activeWorkspaceId, refreshAfterRepo],
  );

  const cloneRepo = useCallback(
    async (url: string, dest: string, name: string) => {
      if (activeWorkspaceId == null) return;
      await api.cloneRepo(activeWorkspaceId, url, dest, name);
      await refreshAfterRepo(activeWorkspaceId);
    },
    [activeWorkspaceId, refreshAfterRepo],
  );

  const createRepo = useCallback(
    async (name: string, dest: string) => {
      if (activeWorkspaceId == null) return;
      await api.createRepo(activeWorkspaceId, name, dest);
      await refreshAfterRepo(activeWorkspaceId);
    },
    [activeWorkspaceId, refreshAfterRepo],
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

  // ALL workers run on the chat engine — one product-native conversation UI
  // per vendor dialect (claude stream-json, codex exec --json, opencode run
  // --format json). Escape hatches per tool: codex app deep link, terminal
  // takeover command for all three.

  // Spawn (or focus) a worker for a (direction, repo) slot. focus=true opens it
  // full-screen (a click); focus=false dispatches it in the background.
  const spawnWorker = useCallback(
    async (directionId: number, repoId: number, focus: boolean) => {
      const existing = Object.values(sessionsRef.current).find(
        (s) => s.directionId === directionId && s.repoId === repoId,
      );
      if (existing) {
        if (focus) {
          setActiveSessionId(existing.info.session_id);
          setShowNeeds(false);
          setHomeTab("board");
        }
        return;
      }
      const info = await api.chatOpenWorker(directionId, repoId, currentLang());
      autoCheckedRef.current.delete(directionId);
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
      if (focus) {
        setActiveSessionId(info.session_id);
        setShowNeeds(false);
        setHomeTab("board");
      }
    },
    [activeThreadId],
  );

  const viewDirection = useCallback(
    (directionId: number, repoId: number, opts?: { diff?: boolean }) => {
      setViewing({ directionId, repoId, diff: opts?.diff });
      setActiveSessionId(null);
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
    async (directionId: number, repoId: number, focus: boolean) => {
      const existing = Object.values(sessionsRef.current).find(
        (s) =>
          s.directionId === directionId &&
          s.repoId === repoId &&
          s.status !== "exited",
      );
      if (existing) {
        if (focus) {
          setActiveSessionId(existing.info.session_id);
          setShowNeeds(false);
          setHomeTab("board");
        }
        return;
      }
      const info = await api.chatOpenWorker(directionId, repoId, currentLang());
      autoCheckedRef.current.delete(directionId);
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
      if (focus) {
        setActiveSessionId(info.session_id);
        setShowNeeds(false);
        setHomeTab("board");
      }
    },
    [activeThreadId],
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
      for (const w of wts) {
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
      for (const w of wts) {
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
  const [leadSlash, setLeadSlash] = useState<Record<number, string[]>>({});
  const [workerTurn, setWorkerTurn] = useState<
    Record<number, { state: "busy" | "idle" | "stopped"; queued: number }>
  >({});
  const [workerSlash, setWorkerSlash] = useState<Record<number, string[]>>({});
  const [leadActivity, setLeadActivity] = useState<
    Record<number, { name: string; summary: string } | null>
  >({});
  const [workerActivity, setWorkerActivity] = useState<
    Record<number, { name: string; summary: string } | null>
  >({});

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
              ? { ...x, status: p.status as LeadMessage["status"] }
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
          // The early initialize-derived push has no native id yet — keep the old one.
          if (p.native_id) {
            setSessions((m) =>
              m[sid] ? { ...m, [sid]: { ...m[sid], nativeId: p.native_id } } : m,
            );
          }
        } else {
          setLeadSlash((s) => ({ ...s, [p.thread_id]: p.slash_commands }));
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
      }
    });
    return () => {
      void un.then((f) => f());
    };
  }, []);

  const loadLeadChat = useCallback(async (threadId: number) => {
    const msgs = await api.listLeadMessages(threadId);
    setLeadMessages((m) => ({
      ...m,
      [threadId]: msgs.filter((x) => x.kind !== "meta"),
    }));
    // Fire the engine up so init delivers slash commands + the console is live.
    void api.leadEnsure(threadId, currentLang()).catch(() => {});
    try {
      const st = await api.leadState(threadId);
      setLeadTurn((t) => ({
        ...t,
        [threadId]: { state: st.state, queued: st.queued },
      }));
      if (st.slash_commands.length > 0) {
        setLeadSlash((s) => ({ ...s, [threadId]: st.slash_commands }));
      }
    } catch {
      /* engine state is cosmetic at load time */
    }
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

  const verifyDirection = useCallback(async (directionId: number) => {
    setCheckingDirections((m) => ({ ...m, [directionId]: true }));
    try {
      const res = await api.verifyDirection(directionId);
      setChecksByDirection((m) => ({ ...m, [directionId]: res }));
    } catch {
      /* leave prior results */
    } finally {
      setCheckingDirections((m) => ({ ...m, [directionId]: false }));
    }
  }, []);

  // Review = the global review skill running INSIDE the worker's own
  // conversation (no built-in review engine; the repo's PR harness stays the
  // authority). Auto-detect prefers superpowers' requesting-code-review when
  // the agent reports it; the setting overrides.
  const resolveReviewSkill = useCallback(() => {
    const configured = reviewSkill.trim().replace(/^\//, "");
    if (configured) return configured;
    const all = [...Object.values(leadSlash), ...Object.values(workerSlash)].flat();
    return (
      all.find((c) => /(^|:)requesting-code-review$/.test(c)) ??
      "superpowers:requesting-code-review"
    );
  }, [reviewSkill, leadSlash, workerSlash]);

  // Deliver a composed message to a (direction, repo)'s worker — waking it
  // first when nothing is live (the engine resumes). The delivery half of diff
  // annotations and skill reviews.
  const sendToDirection = useCallback(
    async (directionId: number, repoId: number, text: string) => {
      let sess = Object.values(sessionsRef.current).find(
        (s) => s.directionId === directionId && s.repoId === repoId && s.status !== "exited",
      );
      if (!sess) {
        await driveDirection(directionId, repoId, false);
        sess = Object.values(sessionsRef.current).find(
          (s) => s.directionId === directionId && s.repoId === repoId && s.status !== "exited",
        );
      }
      if (!sess) return;
      await api.chatSend(sess.info.session_id, text);
    },
    [driveDirection],
  );

  const requestSkillReview = useCallback(
    async (directionId: number) => {
      const writes = await api.listWorktrees(directionId).catch(() => []);
      const first = writes[0];
      if (!first) return;
      let sess = Object.values(sessionsRef.current).find(
        (s) => s.directionId === directionId && s.status !== "exited",
      );
      if (!sess) {
        await driveDirection(directionId, first.repo_id, false);
        sess = Object.values(sessionsRef.current).find(
          (s) => s.directionId === directionId && s.status !== "exited",
        );
      }
      if (!sess) return;
      // Review-then-repair: the skill reviews, the same agent fixes what it
      // found and re-verifies — the human only sees the post-fix state.
      const directive =
        currentLang() === "zh"
          ? "review 结束后，直接修复发现的问题并重新跑检查自验，然后简要汇报。"
          : "After the review, fix the findings directly, re-run the checks to verify, then report briefly.";
      const cmd = `/${resolveReviewSkill()} ${directive}`;
      await api.chatSend(sess.info.session_id, cmd);
    },
    [driveDirection, resolveReviewSkill],
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

  const focusSession = useCallback((id: number) => setActiveSessionId(id), []);

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
    setActiveSessionId(null);
    setViewing(null);
    setHomeTab("board");
    setShowNeeds(true);
  }, []);

  const refreshRepoMap = useCallback(async () => {
    if (activeWorkspaceId == null) {
      setRepoProfiles([]);
      setRepoEdges([]);
      return;
    }
    try {
      const g = await api.repoGraph(activeWorkspaceId);
      setRepoProfiles(g.nodes);
      setRepoEdges(g.edges);
    } catch {
      /* workspace may be empty */
    }
  }, [activeWorkspaceId]);

  const openRepoMap = useCallback(() => {
    setActiveThreadId(null);
    setActiveSessionId(null);
    setShowNeeds(false);
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

  const editRepoProfile = useCallback(
    async (repoId: number, summary: string, role: string) => {
      await api.updateRepoProfile(repoId, summary, role);
      await refreshRepoMap();
    },
    [refreshRepoMap],
  );

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

  const confirmProposal = useCallback(async () => {
    if (activeThreadId == null) return;
    const ids = await api.confirmProposal(activeThreadId);
    setProposal(null);
    setReviewingProposal(false);
    await loadThreadChildren(activeThreadId);
    // Automation-first: dispatch every new task's worker immediately.
    for (const id of ids) void dispatchDirection(id);
  }, [activeThreadId, loadThreadChildren, dispatchDirection]);

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
    async (item: WriteTrigger) => {
      setWriteTriggers((cur) =>
        cur.filter((w) => !(w.thread_id === item.thread_id && w.index === item.index)),
      );
      try {
        const dirId = await api.approveWriteTrigger(item.thread_id, item.index);
        void dispatchDirection(dirId);
      } finally {
        await refreshNeeds();
      }
    },
    [dispatchDirection, refreshNeeds],
  );

  const denyWriteTrigger = useCallback(
    async (item: WriteTrigger) => {
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
        setActiveSessionId(live.info.session_id);
        return;
      }
      await selectThread(item.thread_id);
    },
    [sessions, selectThread],
  );

  useEffect(() => {
    void refreshWorkspaces();
  }, [refreshWorkspaces]);
  useEffect(() => {
    if (activeWorkspaceId != null) void selectWorkspace(activeWorkspaceId);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [activeWorkspaceId]);

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

  // Auto-verify (ARCHITECTURE §4.13): a worker turning busy→idle means its
  // queue drained and the turn finished — run that direction's checks once per
  // idle episode so "done" means "checks ran", not self-report. Going busy
  // again re-arms the latch (so the NEXT turn end verifies again); a fresh
  // dispatch clears it too (spawnWorker/driveDirection).
  const prevTurnRef = useRef<Record<number, string>>({});
  useEffect(() => {
    for (const [sidStr, turn] of Object.entries(workerTurn)) {
      const sid = Number(sidStr);
      const prev = prevTurnRef.current[sid];
      if (prev === turn.state) continue;
      prevTurnRef.current[sid] = turn.state;
      const sess = sessionsRef.current[sid];
      if (!sess) continue;
      if (turn.state === "busy") {
        autoCheckedRef.current.delete(sess.directionId);
      } else if (
        prev === "busy" &&
        turn.state === "idle" &&
        !autoCheckedRef.current.has(sess.directionId)
      ) {
        autoCheckedRef.current.add(sess.directionId);
        void verifyDirection(sess.directionId);
      }
    }
  }, [workerTurn, verifyDirection]);

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
    activeSessionId,
    messages,
    postHuman,
    leadMessages,
    leadTurn,
    leadSlash,
    loadLeadChat,
    sendLeadChat,
    interruptLead,
    workerTurn,
    workerSlash,
    leadActivity,
    workerActivity,
    showBus,
    setShowBus,
    navCollapsed,
    setNavCollapsed,
    reviewingProposal,
    setReviewingProposal,
    threadTab,
    setThreadTab,
    projectsDir,
    setProjectsDir,
    defaultTool,
    setDefaultTool,
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
    openRepoMap,
    refreshRepoMap,
    reprofileRepo,
    editRepoProfile,
    proposal,
    refreshProposal,
    saveProposal,
    confirmProposal,
    overview,
    refreshOverview,
    selectWorkspace,
    refreshWorkspaces,
    selectThread,
    loadThreadChildren,
    backToBoard,
    backToWorkspace,
    createWorkspace,
    addRepo,
    cloneRepo,
    createRepo,
    createThread,
    createDirection,
    deleteThread,
    viewing,
    viewDirection,
    driveDirection,
    reviveDirection,
    closeObserve,
    setTaskStatus,
    checksByDirection,
    checkingDirections,
    verifyDirection,
    requestSkillReview,
    sendToDirection,
    reviewSkill,
    setReviewSkill,
    autoReview,
    setAutoReview,
    focusSession,
  };
  return <Ctx.Provider value={value}>{children}</Ctx.Provider>;
}
