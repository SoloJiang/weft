import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useState,
  type ReactNode,
} from "react";
import { listen } from "@tauri-apps/api/event";
import { api } from "../lib/api";
import type {
  BusMsg,
  Direction,
  DirectionRepo,
  NeedItem,
  Proposal,
  RepoEdge,
  RepoProfile,
  RepoRef,
  ResolvedProposal,
  SessionInfo,
  SessionStatus,
  Thread,
  Workspace,
  Worktree,
} from "../lib/types";

export interface OpenSession {
  info: SessionInfo;
  status: SessionStatus;
  /** identity of the (direction, repo) slot this session occupies */
  directionId: number;
  repoId: number;
  nativeId: string | null;
}

interface Store {
  workspaces: Workspace[];
  activeWorkspaceId: number | null;
  repos: RepoRef[];
  threads: Thread[];
  directionsByThread: Record<number, Direction[]>;
  worktreesByDirection: Record<number, Worktree[]>;
  directionReposByDirection: Record<number, DirectionRepo[]>;

  activeThreadId: number | null;
  sessions: Record<number, OpenSession>;
  activeSessionId: number | null;
  messages: BusMsg[];
  postHuman: (to: string | null, text: string) => Promise<void>;
  nudgeDirection: (directionId: number) => Promise<void>;

  /** Open agent→human questions across the workspace; the Needs-you surface. */
  needs: NeedItem[];
  /** Whether the Needs-you view occupies the main region. */
  showNeeds: boolean;
  openNeeds: () => void;
  refreshNeeds: () => Promise<void>;
  answerAsk: (item: NeedItem, text: string) => Promise<void>;
  goToAsk: (item: NeedItem) => Promise<void>;

  /** The curator's repo map: profiles + dependency edges. */
  repoProfiles: RepoProfile[];
  repoEdges: RepoEdge[];
  showRepoMap: boolean;
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

  selectWorkspace: (id: number) => Promise<void>;
  refreshWorkspaces: () => Promise<void>;
  selectThread: (threadId: number) => Promise<void>;
  loadThreadChildren: (threadId: number) => Promise<void>;
  backToBoard: () => void;

  createWorkspace: (name: string) => Promise<void>;
  addRepo: (name: string, path: string) => Promise<void>;
  createThread: (title: string, kind: string) => Promise<Thread>;
  createDirection: (
    threadId: number,
    name: string,
    tool: string,
    scope: { repo_id: number; role: "write" | "read" }[],
  ) => Promise<void>;
  deleteThread: (threadId: number) => Promise<void>;

  openSession: (directionId: number, repoId: number) => Promise<void>;
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
  const [directionReposByDirection, setDirectionRepos] = useState<
    Record<number, DirectionRepo[]>
  >({});
  const [activeThreadId, setActiveThreadId] = useState<number | null>(null);
  const [sessions, setSessions] = useState<Record<number, OpenSession>>({});
  const [activeSessionId, setActiveSessionId] = useState<number | null>(null);
  const [messages, setMessages] = useState<BusMsg[]>([]);
  const [needs, setNeeds] = useState<NeedItem[]>([]);
  const [showNeeds, setShowNeeds] = useState(false);
  const [repoProfiles, setRepoProfiles] = useState<RepoProfile[]>([]);
  const [repoEdges, setRepoEdges] = useState<RepoEdge[]>([]);
  const [showRepoMap, setShowRepoMap] = useState(false);
  const [proposal, setProposal] = useState<ResolvedProposal | null>(null);

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
    setShowRepoMap(false);
    setRepoProfiles([]);
    setRepoEdges([]);
    setProposal(null);
  }, []);

  const loadThreadChildren = useCallback(async (threadId: number) => {
    const dirs = await api.listDirections(threadId);
    setDirections((m) => ({ ...m, [threadId]: dirs }));
    const entries = await Promise.all(
      dirs.map(
        async (d) =>
          [
            d.id,
            await api.listWorktrees(d.id),
            await api.listDirectionRepos(d.id),
          ] as const,
      ),
    );
    setWorktrees((m) => {
      const next = { ...m };
      for (const [id, wts] of entries) next[id] = wts;
      return next;
    });
    setDirectionRepos((m) => {
      const next = { ...m };
      for (const [id, , drs] of entries) next[id] = drs;
      return next;
    });
  }, []);

  const selectThread = useCallback(
    async (threadId: number) => {
      setActiveThreadId(threadId);
      setActiveSessionId(null);
      setShowNeeds(false);
      setShowRepoMap(false);
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

  const createWorkspace = useCallback(
    async (name: string) => {
      const ws = await api.createWorkspace(name);
      await refreshWorkspaces();
      await selectWorkspace(ws.id);
    },
    [refreshWorkspaces, selectWorkspace],
  );

  const addRepo = useCallback(
    async (name: string, path: string) => {
      if (activeWorkspaceId == null) return;
      await api.addRepoRef(activeWorkspaceId, name, path);
      setRepos(await api.listRepos(activeWorkspaceId));
      // a freshly added repo is eagerly profiled server-side; pull the new map
      try {
        const g = await api.repoGraph(activeWorkspaceId);
        setRepoProfiles(g.nodes);
        setRepoEdges(g.edges);
      } catch {
        /* ignore */
      }
    },
    [activeWorkspaceId],
  );

  const createThread = useCallback(
    async (title: string, kind: string) => {
      if (activeWorkspaceId == null) throw new Error("no workspace");
      const t = await api.createThread(activeWorkspaceId, title, kind);
      setThreads(await api.listThreads(activeWorkspaceId));
      return t;
    },
    [activeWorkspaceId],
  );

  const createDirection = useCallback(
    async (
      threadId: number,
      name: string,
      tool: string,
      scope: { repo_id: number; role: "write" | "read" }[],
    ) => {
      await api.createDirection(threadId, name, tool, scope);
      await loadThreadChildren(threadId);
    },
    [loadThreadChildren],
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

  const openSession = useCallback(
    async (directionId: number, repoId: number) => {
      // focus an existing live session for this slot if present
      const existing = Object.values(sessions).find(
        (s) => s.directionId === directionId && s.repoId === repoId,
      );
      if (existing) {
        setActiveSessionId(existing.info.session_id);
        setShowNeeds(false);
        setShowRepoMap(false);
        return;
      }
      const info = await api.openSession(directionId, repoId);
      setSessions((m) => ({
        ...m,
        [info.session_id]: { info, status: "starting", directionId, repoId, nativeId: null },
      }));
      setActiveSessionId(info.session_id);
      setShowNeeds(false);
      setShowRepoMap(false);
    },
    [sessions],
  );

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
    if (activeWorkspaceId == null) {
      setNeeds([]);
      return;
    }
    try {
      setNeeds(await api.needsYou(activeWorkspaceId));
    } catch {
      /* bus may not be ready */
    }
  }, [activeWorkspaceId]);

  const openNeeds = useCallback(() => {
    setActiveSessionId(null);
    setShowRepoMap(false);
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
    setActiveSessionId(null);
    setShowNeeds(false);
    setShowRepoMap(true);
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
    await api.confirmProposal(activeThreadId);
    setProposal(null);
    await loadThreadChildren(activeThreadId);
  }, [activeThreadId, loadThreadChildren]);

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
    };
    void tick();
    const h = setInterval(tick, 1500);
    return () => {
      alive = false;
      clearInterval(h);
    };
  }, [activeThreadId]);

  const value: Store = {
    workspaces,
    activeWorkspaceId,
    repos,
    threads,
    directionsByThread,
    worktreesByDirection,
    directionReposByDirection,
    activeThreadId,
    sessions,
    activeSessionId,
    messages,
    postHuman,
    nudgeDirection,
    needs,
    showNeeds,
    openNeeds,
    refreshNeeds,
    answerAsk,
    goToAsk,
    repoProfiles,
    repoEdges,
    showRepoMap,
    openRepoMap,
    refreshRepoMap,
    reprofileRepo,
    editRepoProfile,
    proposal,
    refreshProposal,
    startDraftPlan,
    saveProposal,
    confirmProposal,
    selectWorkspace,
    refreshWorkspaces,
    selectThread,
    loadThreadChildren,
    backToBoard,
    createWorkspace,
    addRepo,
    createThread,
    createDirection,
    deleteThread,
    openSession,
    focusSession,
    resumeSession,
    killSession,
  };
  return <Ctx.Provider value={value}>{children}</Ctx.Provider>;
}
