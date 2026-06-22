import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { FolderTree, GitCompare, Info } from "lucide-react";
import { useStore } from "../state/store";
import { api } from "../lib/api";
import type { EnabledSkill, ObserveRef } from "../lib/types";
import { ChatTimeline } from "./ChatTimeline";
import { ChatComposer } from "./ChatComposer";
import { DiffPanel } from "./DiffPanel";
import { FileTreePanel } from "./FileTreePanel";
import { SessionInfoPanel } from "./SessionInfoPanel";
import { Inspect } from "../components/Inspect";
import { ToolIcon, toolFullName } from "../components/ToolIcon";
import { appLink, resumeCommand } from "../lib/resume";
import { useImeComposition } from "../lib/useImeComposition";

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
  } = useStore();
  const { t } = useTranslation();
  const [ref, setRef] = useState<ObserveRef | null>(null);
  const [rail, setRail] = useState<"info" | "diff" | "files" | "none">("info");
  const [loadError, setLoadError] = useState<string | null>(null);
  const [skills, setSkills] = useState<EnabledSkill[]>([]);

  const directionId = viewing?.directionId ?? null;
  const repoId = viewing?.repoId ?? null;

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

  // History lives in the thread timeline; hydrate it (worker rows ride along).
  useEffect(() => {
    if (threadId != null) void loadLeadChat(threadId);
  }, [threadId, loadLeadChat]);

  // Slot changed — drop the previous worker's ref immediately so cwd/header
  // never render with stale data while sessionFor refreshes. A relative file ref
  // clicked in that window would otherwise resolve against the prior worktree.
  useEffect(() => {
    setRef(null);
  }, [directionId, repoId]);

  // Resolve the worker's session ref (worktree/branch/tool/session_id/native_id).
  // Polls while not live so a backgrounded worker's status stays fresh.
  useEffect(() => {
    setRail(viewing?.sidePanel ?? "info");
    if (directionId == null || repoId == null) {
      setRef(null);
      return;
    }
    let alive = true;
    const load = () =>
      api
        .sessionFor(directionId, repoId)
        .then((r) => {
          if (alive) {
            setRef(r);
            setLoadError(null);
            if (r && r.session_id != null) hydrateWorkerMeta(r.session_id, r);
          }
        })
        .catch((e: unknown) => {
          if (alive) setLoadError(String(e));
        });
    void load();
    const h = setInterval(load, 2000);
    return () => {
      alive = false;
      clearInterval(h);
    };
  }, [directionId, repoId, viewing?.sidePanel, hydrateWorkerMeta]);

  // Enabled skills for the panel (workspace-level, CLI-agnostic). Re-fetch when
  // skills change so the silent skill-refresh is reflected without a restart.
  useEffect(() => {
    if (activeWorkspaceId == null) return;
    void api
      .workspaceSkills(activeWorkspaceId)
      .then((s) => setSkills(s.filter((x) => !x.overridden)))
      .catch(() => {});
  }, [activeWorkspaceId, skillsDirtyAt]);

  // codex/opencode 的带外 meta(model/window/MCP server,+ opencode 的 usage)。
  // claude 走事件流不用拉。开页 + 每次 turn 状态变(running/idle)各拉一次。
  // 该 effect 按 live.status 触发,turn 起/止都会跑;running 期的请求读的是上一条
  // assistant 行(旧 usage),若它晚于 idle 期的请求返回,会用旧值盖掉新的 contextTokens。
  // 用 alive 标志丢弃被取代的旧请求(也防 thread 切换后旧请求落到新会话)。
  useEffect(() => {
    const tool = ref?.tool;
    const metaSid = live?.info.session_id ?? ref?.session_id ?? null;
    if (directionId == null || repoId == null || metaSid == null) return;
    if (tool == null || tool === "claude") return;
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
    mergeWorkerMeta,
  ]);

  if (viewing == null || directionId == null || repoId == null) return null;

  // Effective session id: the live engine's, else the persisted ref's. resume
  // reuses the same row, so this is stable across attach.
  const sid = live?.info.session_id ?? ref?.session_id ?? null;
  const turn = sid != null ? workerTurn[sid] : undefined;
  const busy = (turn?.state ?? "stopped") === "busy";
  const msgs =
    threadId != null && sid != null
      ? (leadMessages[threadId] ?? []).filter((m) => m.session_id === sid)
      : [];
  const openAsks = needs.filter((n) => n.direction_id === directionId);
  const nativeId = live?.nativeId ?? ref?.native_id ?? null;
  // Prefer the live engine's worktree — available synchronously on slot switch,
  // unlike the async `ref` — so relative file refs resolve against this worker.
  const cwd = live?.info.worktree ?? ref?.worktree;

  // 重载会话:重拉 skills + 标记静默 re-spawn(下条消息拾取新 MCP/skill);
  // codex/opencode 立即重拉 session_meta(server/window 即时刷新)。
  const onReload = () => {
    markSkillsDirty();
    if (sid == null) return;
    void api.flagSessionSkillRefresh(sid);
    if (ref?.tool && ref.tool !== "claude") {
      void api
        .sessionMeta(directionId, repoId)
        .then((s) => mergeWorkerMeta(sid, s))
        .catch(() => {});
    }
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
            busy={busy}
            activity={sid != null ? workerActivity[sid] : undefined}
            onReviewProposal={() => {}}
            cwd={cwd}
            queue={turn?.queue ?? []}
            onRemove={sid != null ? (id) => void api.chatDequeue(sid, id) : undefined}
            onEdit={sid != null ? (id, text) => void api.chatEditQueued(sid, id, text) : undefined}
            onReorder={sid != null ? (order) => void api.chatReorderQueue(sid, order) : undefined}
          />
          <ChatComposer
            slashCommands={(sid != null ? workerSlash[sid] : undefined) ?? []}
            onNeedSlashCommands={() => sid != null && discoverWorkerSlash(sid)}
            busy={busy}
            queue={turn?.queue ?? []}
            placeholder={loadError ?? t("session.message")}
            onSend={(v, images, fs) => sendToWorker(directionId, repoId, v, images, fs)}
            onStop={() => sid != null && void api.chatInterrupt(sid)}
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
