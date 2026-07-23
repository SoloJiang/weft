import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { FolderTree, GitCompare, Info } from "lucide-react";
import { isInFlight, useStore } from "../state/store";
import { api } from "../lib/api";
import type { EnabledSkill, ObserveRef, RewindMode } from "../lib/types";
import { ChatTimeline } from "./ChatTimeline";
import { LeadEmptyState } from "./LeadEmptyState";
import { ChatComposer } from "./ChatComposer";
import { DiffPanel } from "./DiffPanel";
import { FileTreePanel } from "./FileTreePanel";
import { SessionInfoPanel } from "./SessionInfoPanel";
import { Inspect } from "../components/Inspect";
import { ToolIcon, toolFullName } from "../components/ToolIcon";
import { ALL_REWIND_MODES, RewindDialog, RewindPickerDialog } from "./RewindDialog";
import { appLink, resumeCommand } from "../lib/resume";
import { useImeComposition } from "../lib/useImeComposition";
import {
  failChatHistoryLoad,
  workerChatHistoryStatus,
} from "../state/chatHistory";

type WorkerSessionLookup =
  | { slotKey: string | null; status: "loading"; ref: null; error: null }
  | { slotKey: string | null; status: "ready"; ref: ObserveRef | null; error: null }
  | { slotKey: string | null; status: "error"; ref: null; error: string };

/**
 * The worker conversation — same model as the lead console (LeadTab): a single,
 * always-input-able surface backed by weft's own store. No read-only mode, no
 * continue/terminate. Viewing reads persisted history without starting an
 * engine; the first send transparently resumes/dispatches (sendToWorker).
 */
export function WorkerConversation() {
  const {
    viewing,
    sessions,
    leadMessages,
    leadHistoryStatus,
    workerTurn,
    workerSlash,
    workerActivity,
    workerMeta,
    hydrateWorkerMeta,
    mergeWorkerMeta,
    discoverWorkerSlash,
    loadLeadChat,
    needs,
    answerAsk,
    sendToWorker,
    activeThreadId,
    activeWorkspaceId,
    skillsDirtyAt,
    markSkillsDirty,
    asks,
    setActiveSidePanel,
  } = useStore();
  const { t } = useTranslation();
  const directionId = viewing?.directionId ?? null;
  const repoId = viewing?.repoId ?? null;
  const slotKey = directionId == null || repoId == null ? null : `${directionId}:${repoId}`;
  const [sessionLookup, setSessionLookup] = useState<WorkerSessionLookup>({
    slotKey: null,
    status: "loading",
    ref: null,
    error: null,
  });
  const [sessionLookupRetry, setSessionLookupRetry] = useState(0);
  const [rail, setRail] = useState<"info" | "diff" | "files" | "none">("info");
  const [skills, setSkills] = useState<EnabledSkill[]>([]);
  // Conversation rewind: the message id awaiting confirm (null = dialog closed),
  // the Esc-Esc picker's open flag, and the composer prefill (seq bumps to
  // remount-inject the rewound text).
  const [rewindId, setRewindId] = useState<number | null>(null);
  const [pickerOpen, setPickerOpen] = useState(false);
  const [prefill, setPrefill] = useState<{ text: string; seq: number }>({ text: "", seq: 0 });

  // Effects clear a previous slot after commit. Keying the lookup lets render
  // reject stale data synchronously during the slot-switch frame itself.
  const activeSessionLookup: WorkerSessionLookup =
    sessionLookup.slotKey === slotKey
      ? sessionLookup
      : { slotKey, status: "loading", ref: null, error: null };
  const ref = activeSessionLookup.ref;
  const loadError = activeSessionLookup.error;

  // Mirror the live diff/files panel to the store so the nav rail can yield room
  // to it on narrow windows; clear it when this session unmounts.
  useEffect(() => {
    setActiveSidePanel(rail === "diff" || rail === "files" ? rail : null);
  }, [rail, setActiveSidePanel]);
  useEffect(() => () => setActiveSidePanel(null), [setActiveSidePanel]);

  // Live engine for this slot (if any). Computed before hooks so threadId is
  // available to the hydrate effect (rules-of-hooks: no early return above).
  const live = Object.values(sessions).find(
    (s) =>
      directionId != null &&
      repoId != null &&
      s.directionId === directionId &&
      s.repoId === repoId &&
      s.status !== "exited",
  );
  // The thread the worker's rows live under: the live session's own thread, else
  // the open thread (board path). Covers cross-thread entry from "Needs you".
  const threadId = live?.threadId ?? activeThreadId;

  // This worker's pending permission asks. A worker ask's `dir` is the direction
  // id as a string (backend uses direction_id.to_string(); board code matches
  // String(direction.id) / Number(a.dir)). Answered in the same in-session card
  // the lead uses, so an ask is actionable in one place — the dock only routes.
  const workerAsks =
    directionId == null
      ? []
      : asks.filter((a) => a.dir === String(directionId));

  // History lives in the thread timeline; hydrate it (worker rows ride along).
  useEffect(() => {
    if (threadId != null) void loadLeadChat(threadId);
  }, [threadId, loadLeadChat]);

  // Slot changed — drop the previous worker's ref immediately so cwd/header
  // never render with stale data while sessionFor refreshes. A relative file ref
  // clicked in that window would otherwise resolve against the prior worktree.
  useEffect(() => {
    setSessionLookup({ slotKey, status: "loading", ref: null, error: null });
  }, [slotKey]);

  // Resolve the worker's session ref (worktree/branch/tool/session_id/native_id).
  // Polls while not live so a backgrounded worker's status stays fresh.
  useEffect(() => {
    setRail(viewing?.sidePanel ?? "info");
    if (directionId == null || repoId == null) {
      setSessionLookup({ slotKey, status: "ready", ref: null, error: null });
      return;
    }
    let alive = true;
    const load = () =>
      api
        .sessionFor(directionId, repoId)
        .then((r) => {
          if (alive) {
            setSessionLookup({ slotKey, status: "ready", ref: r, error: null });
            if (r && r.session_id != null) hydrateWorkerMeta(r.session_id, r);
          }
        })
        .catch((e: unknown) => {
          if (!alive) return;
          setSessionLookup((current) => {
            if (current.slotKey !== slotKey) return current;
            const status = failChatHistoryLoad(current.status);
            if (status === "ready") return current;
            return { slotKey, status: "error", ref: null, error: String(e) };
          });
        });
    void load();
    const h = setInterval(load, 2000);
    return () => {
      alive = false;
      clearInterval(h);
    };
  }, [directionId, repoId, slotKey, viewing?.sidePanel, hydrateWorkerMeta, sessionLookupRetry]);

  // Enabled skills for the panel (workspace-level, CLI-agnostic). Re-fetch when
  // skills change so the silent skill-refresh is reflected without a restart.
  useEffect(() => {
    if (activeWorkspaceId == null) return;
    void api
      .workspaceSkills(activeWorkspaceId)
      .then((s) => setSkills(s.filter((x) => !x.overridden)))
      .catch(() => {});
  }, [activeWorkspaceId, skillsDirtyAt]);

  // 带外 meta:codex/opencode 补 model/window/MCP(+ opencode usage);claude 只补 cwd
  // 的 skill(model/window/MCP 走事件流,session_meta 留空不覆盖)。开页 + 每次 turn 状态
  // 变 + skills 变(刷新按钮)各拉一次。该 effect 按 live.status 触发,turn 起/止都会跑;
  // running 期的请求读的是上一条 assistant 行(旧 usage),若它晚于 idle 期的请求返回,会用
  // 旧值盖掉新的 contextTokens。用 alive 标志丢弃被取代的旧请求(也防 thread 切换串台)。
  useEffect(() => {
    const tool = ref?.tool;
    const metaSid = live?.info.session_id ?? ref?.session_id ?? null;
    if (directionId == null || repoId == null || metaSid == null) return;
    if (tool == null) return;
    let alive = true;
    void api
      .sessionMeta(directionId, repoId)
      .then((s) => {
        if (alive) mergeWorkerMeta(metaSid, s);
      })
      .catch(() => {});
    return () => {
      alive = false;
    };
  }, [
    directionId,
    repoId,
    ref?.session_id,
    ref?.tool,
    live?.info.session_id,
    live?.status,
    skillsDirtyAt,
    mergeWorkerMeta,
  ]);

  if (viewing == null || directionId == null || repoId == null) return null;

  // Effective session id: the live engine's, else the persisted ref's. resume
  // reuses the same row, so this is stable across attach.
  const sid = live?.info.session_id ?? ref?.session_id ?? null;
  const historyStatus = workerChatHistoryStatus(
    sid,
    activeSessionLookup.status,
    threadId == null ? undefined : leadHistoryStatus[threadId],
  );
  const timelineKey = sid == null
    ? `worker-slot:${directionId}:${repoId}`
    : `worker:${sid}`;
  const turn = sid != null ? workerTurn[sid] : undefined;
  const busy = isInFlight(turn?.state ?? "stopped");
  const msgs =
    threadId != null && sid != null
      ? (leadMessages[threadId] ?? []).filter((m) => m.session_id === sid)
      : [];
  const openAsks = needs.filter((n) => n.direction_id === directionId);
  const nativeId = live?.nativeId ?? ref?.native_id ?? null;
  // Prefer the live engine's worktree — available synchronously on slot switch,
  // unlike the async `ref` — so relative file refs resolve against this worker.
  const cwd = live?.info.worktree ?? ref?.worktree;

  // Conversation rewind (Phase 1) is scoped to claude/codex/opencode workers —
  // the tools with native fork support; others get no rewind affordance at all.
  const canRewind =
    sid != null && (ref?.tool === "claude" || ref?.tool === "codex" || ref?.tool === "opencode");

  // Dialog confirm: the backend truncates from the picked message on (and, for
  // the code modes, restores the worktree), then returns the message's text —
  // prefill the composer with it only when the conversation was rewound (a
  // code-only rewind leaves the chat untouched, so its text would be noise).
  // The "rewound" push reloads the timeline independently (store listener).
  const confirmRewind = async (mode: RewindMode) => {
    if (sid == null || rewindId == null) return;
    const r = await api.chatRewind(sid, rewindId, mode);
    if (mode !== "code") setPrefill((p) => ({ text: r.rewound_text, seq: p.seq + 1 }));
  };

  // 重载会话:先让 flagSessionSkillRefresh 把新启用的 skill 注入 cwd(并标记静默 re-spawn),
  // **注入完成后**再 bump skillsDirtyAt —— 上面的带外 meta effect 据此重扫 cwd。若先 bump,
  // 重扫会抢在 inject_for 之前跑、把陈旧/空列表当权威合并,且之后没有触发再纠正。
  const onReload = () => {
    if (sid == null) {
      markSkillsDirty();
      return;
    }
    void api.flagSessionSkillRefresh(sid).finally(() => markSkillsDirty());
  };

  return (
    <div className="flex min-h-0 min-w-0 flex-1">
      <section className="flex min-w-0 flex-1 flex-col bg-bg">
        <header className="flex items-center gap-2 border-b border-border bg-surface px-3 py-2">
          {ref && (
            <span className="mr-auto flex shrink-0 items-center gap-1.5 whitespace-nowrap rounded-[var(--radius-sm)] bg-bg px-2 py-0.5 text-[11px] font-medium text-ink-muted">
              <ToolIcon tool={ref.tool} size={12} />
              {toolFullName(ref.tool)}
            </span>
          )}
          <span className="hidden min-w-0 truncate font-mono text-[11.5px] text-ink-faint md:block">
            {ref?.branch}
          </span>
          <button
            onClick={() => setRail((r) => (r === "info" ? "none" : "info"))}
            title={t("sessionInfo.title")}
            aria-label={t("sessionInfo.title")}
            className={`grid h-7 w-7 shrink-0 place-items-center rounded-[var(--radius-md)] border transition-colors ${
              rail === "info"
                ? "border-brand bg-brand-ghost text-brand"
                : "border-border text-ink-muted hover:bg-surface hover:text-ink"
            }`}
          >
            <Info size={13} />
          </button>
          <button
            onClick={() => setRail("diff")}
            title={t("diff.tab")}
            aria-label={t("diff.tab")}
            className="grid h-7 w-7 shrink-0 place-items-center rounded-[var(--radius-md)] border border-border text-ink-muted transition-colors hover:bg-surface hover:text-ink"
          >
            <GitCompare size={13} />
          </button>
          <button
            onClick={() => setRail("files")}
            title={t("files.tab")}
            aria-label={t("files.tab")}
            className="grid h-7 w-7 shrink-0 place-items-center rounded-[var(--radius-md)] border border-border text-ink-muted transition-colors hover:bg-surface hover:text-ink"
          >
            <FolderTree size={13} />
          </button>
          {ref && (
            <Inspect
              path={ref.worktree}
              branch={ref.branch}
              nativeId={ref.native_id}
              tool={ref.tool}
              command={ref.command}
              className="h-7 w-7 shrink-0"
            />
          )}
        </header>

        {openAsks.length > 0 && (
          <div className="border-b border-border bg-surface/60 px-3 py-2">
            {openAsks.map((a) => (
              <AskInline key={a.ask_id} text={a.text} onAnswer={(txt) => void answerAsk(a, txt)} />
            ))}
          </div>
        )}

        <div className="flex min-h-0 flex-1 flex-col">
          <ChatTimeline
            messages={msgs}
            historyStatus={historyStatus}
            timelineKey={timelineKey}
            onRetryHistory={() => {
              if (threadId != null) void loadLeadChat(threadId);
              if (activeSessionLookup.status === "error") {
                setSessionLookup({ slotKey, status: "loading", ref: null, error: null });
                setSessionLookupRetry((n) => n + 1);
              }
            }}
            asks={workerAsks}
            busy={busy}
            activity={sid != null ? workerActivity[sid] : undefined}
            onReviewProposal={() => {}}
            cwd={cwd}
            emptyState={<LeadEmptyState mode="default" threadId={null} workspaceId={null} />}
            queue={turn?.queue ?? []}
            onRemove={sid != null ? (id) => void api.chatDequeue(sid, id) : undefined}
            onEdit={sid != null ? (id, text) => void api.chatEditQueued(sid, id, text) : undefined}
            onReorder={sid != null ? (order) => void api.chatReorderQueue(sid, order) : undefined}
            onRewind={canRewind ? (id) => setRewindId(id) : undefined}
          />
          <ChatComposer
            key={prefill.seq}
            initialValue={prefill.text}
            slashCommands={(sid != null ? workerSlash[sid] : undefined) ?? []}
            onNeedSlashCommands={() => sid != null && discoverWorkerSlash(sid)}
            tool={ref?.tool}
            contextMeta={sid != null ? workerMeta[sid] : undefined}
            busy={busy}
            queued={turn?.queue?.length ?? 0}
            placeholder={loadError ?? t("session.message")}
            onSend={(v, images, fs) => sendToWorker(directionId, repoId, v, images, fs)}
            onStop={() => sid != null && void api.chatInterrupt(sid)}
            onRewindPicker={canRewind ? () => setPickerOpen(true) : undefined}
            onTakeOver={async () => {
              if (!ref || !nativeId || sid == null) return false;
              await api.chatStop(sid);
              await navigator.clipboard.writeText(
                resumeCommand(ref.tool, ref.worktree, nativeId, ref.command),
              );
              return true;
            }}
            onOpenApp={
              ref && nativeId && appLink(ref.tool, nativeId)
                ? () => void api.openUrl(appLink(ref.tool, nativeId)!)
                : undefined
            }
          />
        </div>
      </section>

      {rail === "info" && (
        <SessionInfoPanel
          meta={sid != null ? workerMeta[sid] : undefined}
          skills={skills}
          onClose={() => setRail("none")}
          onReload={onReload}
          busy={busy}
        />
      )}
      {ref && (
        <DiffPanel
          cwd={ref.worktree}
          directionId={directionId}
          open={rail === "diff"}
          onClose={() => setRail("info")}
          onAsk={(text) => void sendToWorker(directionId, repoId, text)}
        />
      )}
      {ref && (
        <FileTreePanel
          cwd={ref.worktree}
          open={rail === "files"}
          onClose={() => setRail("info")}
        />
      )}

      <RewindDialog
        open={rewindId != null}
        onOpenChange={(o) => {
          if (!o) setRewindId(null);
        }}
        onConfirm={confirmRewind}
        modes={ALL_REWIND_MODES}
      />
      <RewindPickerDialog
        open={pickerOpen}
        onOpenChange={setPickerOpen}
        messages={msgs}
        onPick={(id) => {
          setPickerOpen(false);
          setRewindId(id);
        }}
      />
    </div>
  );
}

function AskInline({ text, onAnswer }: { text: string; onAnswer: (answer: string) => void }) {
  const { t } = useTranslation();
  const [val, setVal] = useState("");
  const { composition, isComposing } = useImeComposition();
  return (
    <div className="flex items-center gap-2 py-1">
      <span className="min-w-0 flex-1 truncate text-[13px] text-ink">{text}</span>
      <input
        value={val}
        onChange={(e) => setVal(e.target.value)}
        {...composition}
        onKeyDown={(e) => {
          if (e.key === "Enter" && !isComposing(e) && val.trim()) {
            onAnswer(val.trim());
            setVal("");
          }
        }}
        placeholder={t("observe.answerPlaceholder")}
        className="w-64 rounded-[var(--radius-sm)] border border-border bg-bg px-2 py-1 text-[12px] text-ink"
      />
    </div>
  );
}
