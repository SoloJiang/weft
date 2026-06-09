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

export interface OpenSession {
  info: SessionInfo;
  status: SessionStatus;
  /** identity of the (direction, repo) slot this session occupies */
  directionId: number;
  repoId: number;
  /** the thread this session belongs to (a lead's home, a worker's parent). */
  threadId: number;
  nativeId: string | null;
  /** worker = a task's session; lead = the thread's persistent conversation */
  kind: "worker" | "lead";
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
  nudgeDirection: (directionId: number) => Promise<void>;

  /** The active thread's persistent lead conversation, if one is running. */
  leadSession: OpenSession | null;
  /** Start the thread's lead (idempotent — reuses a live one). */
  startLead: () => Promise<void>;
  /** Send a composed message into the lead's PTY (bracketed paste + enter). */
  sendToLead: (text: string) => Promise<void>;
  /** Per-thread collapse state for the lead dock; default expanded. */
  leadCollapsed: boolean;
  toggleLeadCollapsed: () => void;
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
  /** Whether the board canvas is showing the proposal's scope-confirm. */
  reviewingProposal: boolean;
  setReviewingProposal: (v: boolean) => void;

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
  /** Which workspace-home tab is active (Overview · Repos). */
  homeTab: "board" | "overview" | "repos";
  setHomeTab: (t: "board" | "overview" | "repos") => void;
  /** Jump to the workspace home's Repos tab. */
  openRepoMap: () => void;
  refreshRepoMap: () => Promise<void>;
  reprofileRepo: (repoId: number) => Promise<void>;
  editRepoProfile: (repoId: number, summary: string, role: string) => Promise<void>;

  /** The active thread's plan proposal (Task → scope), if any. */
  proposal: ResolvedProposal | null;
  refreshProposal: (threadId: number) => Promise<void>;
  startDraftPlan: () => Promise<void>;
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

  viewing: { directionId: number; repoId: number } | null;
  viewDirection: (directionId: number, repoId: number) => void;
  driveDirection: (directionId: number, repoId: number, focus: boolean) => Promise<void>;
  reviveDirection: (directionId: number) => Promise<void>;
  closeObserve: () => void;
  /** Set a task's lifecycle status (human override). */
  setTaskStatus: (directionId: number, status: string) => Promise<void>;
  /** Quality loop: executable-check results + in-flight set, per direction. */
  checksByDirection: Record<number, RepoChecks[]>;
  checkingDirections: Record<number, boolean>;
  verifyDirection: (directionId: number) => Promise<void>;
  focusSession: (sessionId: number) => void;
  resumeSession: (sessionId: number) => Promise<void>;
  killSession: (sessionId: number) => Promise<void>;
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
  const lastOutputRef = useRef<Record<number, number>>({});
  const autoCheckedRef = useRef<Set<number>>(new Set());
  // Directions with an auto-(re)dispatch in flight, so the poll-driven effect
  // never spawns a duplicate worker before the first spawn lands in `sessions`.
  const dispatchingRef = useRef<Set<number>>(new Set());
  const sessionsRef = useRef(sessions);
  sessionsRef.current = sessions;
  const [activeSessionId, setActiveSessionId] = useState<number | null>(null);
  const [viewing, setViewing] = useState<{ directionId: number; repoId: number } | null>(null);
  const [messages, setMessages] = useState<BusMsg[]>([]);
  const [needs, setNeeds] = useState<NeedItem[]>([]);
  const [asks, setAsks] = useState<PermissionAsk[]>([]);
  const [writeTriggers, setWriteTriggers] = useState<WriteTrigger[]>([]);
  const [needsByWorkspace, setNeedsByWorkspace] = useState<Record<number, number>>({});
  const [showNeeds, setShowNeeds] = useState(false);
  const [repoProfiles, setRepoProfiles] = useState<RepoProfile[]>([]);
  const [repoEdges, setRepoEdges] = useState<RepoEdge[]>([]);
  const [homeTab, setHomeTab] = useState<"board" | "overview" | "repos">("board");
  const [proposal, setProposal] = useState<ResolvedProposal | null>(null);
  const [overview, setOverview] = useState<ThreadOverview[]>([]);
  // Lead dock: per-thread collapse memory; bus drawer; proposal-review state.
  const [leadCollapsedByThread, setLeadCollapsedByThread] = useState<
    Record<number, boolean>
  >({});
  const [showBus, setShowBus] = useState(false);
  const [reviewingProposal, setReviewingProposal] = useState(false);
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
      setShowNeeds(false);
      setHomeTab("board");
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
    setShowNeeds(false);
    setHomeTab("board");
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

  const setStatus = useCallback((sessionId: number, status: SessionStatus) => {
    setSessions((m) =>
      m[sessionId] ? { ...m, [sessionId]: { ...m[sessionId], status } } : m,
    );
  }, []);

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
      const info = await api.openSession(directionId, repoId, currentLang());
      lastOutputRef.current[info.session_id] = Date.now();
      autoCheckedRef.current.delete(directionId);
      setSessions((m) => ({
        ...m,
        [info.session_id]: {
          info,
          status: "starting",
          directionId,
          repoId,
          threadId: activeThreadId ?? -1,
          nativeId: null,
          kind: "worker",
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

  const viewDirection = useCallback((directionId: number, repoId: number) => {
    setViewing({ directionId, repoId });
    setActiveSessionId(null);
    setShowNeeds(false);
    setHomeTab("board");
  }, []);

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
      const info = await api.driveSession(directionId, repoId, currentLang());
      lastOutputRef.current[info.session_id] = Date.now();
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
            status: info.resumed ? "running" : "starting",
            directionId,
            repoId,
            threadId: activeThreadId ?? -1,
            nativeId: info.native_id,
            kind: "worker",
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

  // Start (or reuse) the thread's persistent lead conversation. Unlike a worker,
  // the lead is embedded in the dock, not opened full-screen, and it stays alive
  // across the thread's life so the human can keep talking to it.
  const startLead = useCallback(async () => {
    if (activeThreadId == null) return;
    const thread = activeThreadId;
    const live = Object.values(sessionsRef.current).find(
      (s) => s.kind === "lead" && s.threadId === thread && s.status !== "exited",
    );
    if (live) return; // already running — the dock shows it
    const lead = await api.planWithLead(thread, currentLang());
    // Hand-built SessionInfo (lead has no backend SessionInfo); keep fields in sync with the type.
    const info: SessionInfo = {
      session_id: lead.session_id,
      repo: "",
      worktree: lead.cwd,
      branch: "",
      tool: lead.tool,
      resumed: false,
      native_id: null,
    };
    lastOutputRef.current[lead.session_id] = Date.now();
    setSessions((m) => ({
      ...m,
      [lead.session_id]: {
        info,
        status: "running",
        directionId: -1,
        repoId: -1,
        threadId: thread,
        nativeId: null,
        kind: "lead",
      },
    }));
  }, [activeThreadId]);

  const sendToLead = useCallback(async (text: string) => {
    const body = text.trimEnd();
    if (!body) return;
    const thread = activeThreadId;
    const lead = Object.values(sessionsRef.current).find(
      (s) => s.kind === "lead" && s.threadId === thread && s.status !== "exited",
    );
    if (!lead) return;
    // Bracketed paste keeps multi-line prompts intact in the TUI, then Enter submits.
    const payload = `\x1b[200~${body}\x1b[201~\r`;
    await api.writePty(lead.info.session_id, payload);
  }, [activeThreadId]);

  const toggleLeadCollapsed = useCallback(() => {
    const thread = activeThreadId;
    if (thread == null) return;
    setLeadCollapsedByThread((m) => ({ ...m, [thread]: !m[thread] }));
  }, [activeThreadId]);

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

  const focusSession = useCallback((id: number) => setActiveSessionId(id), []);

  const resumeSession = useCallback(async (sessionId: number) => {
    const info = await api.resumeSession(sessionId);
    setSessions((m) => ({ ...m, [sessionId]: { ...m[sessionId], info, status: "starting" } }));
  }, []);

  const killSession = useCallback(async (sessionId: number) => {
    await api.killSession(sessionId);
    setSessions((m) => {
      const n = { ...m };
      delete n[sessionId];
      return n;
    });
    setActiveSessionId((cur) => (cur === sessionId ? null : cur));
  }, []);

  const postHuman = useCallback(
    async (to: string | null, text: string) => {
      if (activeThreadId == null || !text.trim()) return;
      await api.busPostHuman(activeThreadId, to, text.trim());
    },
    [activeThreadId],
  );

  const nudgeDirection = useCallback(
    async (directionId: number) => {
      const sess = Object.values(sessions).find(
        (s) => s.directionId === directionId && s.status === "running",
      );
      if (!sess) return;
      await api.writePty(
        sess.info.session_id,
        "You have new messages on the thread bus. Call the bus_inbox tool to read them.\r",
      );
    },
    [sessions],
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

  const startDraftPlan = useCallback(async () => {
    if (activeThreadId == null) return;
    // Seed an empty proposal so the human can draft scope by hand. The agentic
    // lead will pre-fill this instead, later.
    await api.saveProposal(activeThreadId, {
      rationale: "",
      directions: [{ name: "Direction 1", tool: "claude", writes: [] }],
    });
    await refreshProposal(activeThreadId);
  }, [activeThreadId, refreshProposal]);

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

  // bridge events: session id capture + exit drive UI status
  useEffect(() => {
    const unId = listen<{ sessionId: number; nativeId: string }>(
      "session://id",
      (e) => {
        const { sessionId, nativeId } = e.payload;
        setSessions((m) =>
          m[sessionId]
            ? { ...m, [sessionId]: { ...m[sessionId], nativeId, status: "running" } }
            : m,
        );
      },
    );
    const unExit = listen<{ sessionId: number }>("pty://exit", (e) => {
      setStatus(e.payload.sessionId, "exited");
    });
    return () => {
      void unId.then((f) => f());
      void unExit.then((f) => f());
    };
  }, [setStatus]);

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

  // The lead is persistent and lives in the dock, so while it's running we poll
  // for its proposal and surface it as a card (the lead keeps running — the human
  // can keep talking and the lead can re-propose). It does NOT take over the board.
  const leadForActive =
    activeThreadId != null
      ? Object.values(sessions).find(
          (s) =>
            s.kind === "lead" &&
            s.threadId === activeThreadId &&
            s.status !== "exited",
        )
      : undefined;
  const leadActiveId = leadForActive?.info.session_id ?? null;
  useEffect(() => {
    if (activeThreadId == null || leadActiveId == null) return;
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
  }, [activeThreadId, leadActiveId]);

  // Auto-verify loop (ARCHITECTURE §4.13): when a worker's PTY goes quiet for a
  // while it has likely finished its turn — run that direction's checks once per
  // idle episode so "done" means "checks ran", not self-report. Output resuming
  // re-arms it. Tool-general: keys off PTY silence, not any tool's event format.
  useEffect(() => {
    const IDLE_MS = 20000;
    const un = listen<{ session_id: number }>("pty://output", (e) => {
      lastOutputRef.current[e.payload.session_id] = Date.now();
    });
    const h = setInterval(() => {
      const now = Date.now();
      for (const s of Object.values(sessionsRef.current)) {
        if (s.kind !== "worker" || s.status !== "running") continue;
        const last = lastOutputRef.current[s.info.session_id] ?? 0;
        const idle = now - last > IDLE_MS;
        if (idle && !autoCheckedRef.current.has(s.directionId)) {
          autoCheckedRef.current.add(s.directionId);
          void verifyDirection(s.directionId);
        } else if (!idle) {
          autoCheckedRef.current.delete(s.directionId);
        }
      }
    }, 5000);
    return () => {
      clearInterval(h);
      void un.then((f) => f());
    };
  }, [verifyDirection]);

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
  // but has no live session — e.g. after an app restart, when in-memory PTYs are
  // gone — gets its worker (re)dispatched so it runs without a manual click.
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

  const leadSession = leadForActive ?? null;
  const leadCollapsed =
    activeThreadId != null ? !!leadCollapsedByThread[activeThreadId] : false;

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
    nudgeDirection,
    leadSession,
    startLead,
    sendToLead,
    leadCollapsed,
    toggleLeadCollapsed,
    showBus,
    setShowBus,
    navCollapsed,
    setNavCollapsed,
    reviewingProposal,
    setReviewingProposal,
    projectsDir,
    setProjectsDir,
    defaultTool,
    setDefaultTool,
    dangerousMode,
    setDangerousMode,
    dangerNudge,
    setDangerNudge,
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
    startDraftPlan,
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
    focusSession,
    resumeSession,
    killSession,
  };
  return <Ctx.Provider value={value}>{children}</Ctx.Provider>;
}
