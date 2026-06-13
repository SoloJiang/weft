import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { GitCompare } from "lucide-react";
import { useStore } from "../state/store";
import { api } from "../lib/api";
import type { ObserveRef, SessionStatus } from "../lib/types";
import { ChatTimeline } from "./ChatTimeline";
import { ChatComposer } from "./ChatComposer";
import { DiffPanel } from "./DiffPanel";
import { StatusChip } from "../components/ui/StatusChip";
import { Inspect } from "../components/Inspect";
import { ToolIcon, toolFullName } from "../components/ToolIcon";
import { appLink, resumeCommand } from "../lib/resume";

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
    discoverWorkerSlash,
    loadLeadChat,
    needs,
    answerAsk,
    sendToWorker,
    activeThreadId,
  } = useStore();
  const { t } = useTranslation();
  const [ref, setRef] = useState<ObserveRef | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [showDiff, setShowDiff] = useState(false);

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

  // Resolve the worker's session ref (worktree/branch/tool/session_id/native_id).
  // Polls while not live so a backgrounded worker's status stays fresh.
  useEffect(() => {
    setShowDiff(viewing?.diff ?? false);
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
  }, [directionId, repoId, viewing?.diff]);

  if (viewing == null || directionId == null || repoId == null) return null;

  // Effective session id: the live engine's, else the persisted ref's. resume
  // reuses the same row, so this is stable across attach.
  const sid = live?.info.session_id ?? ref?.session_id ?? null;
  const turn = sid != null ? workerTurn[sid] : undefined;
  const busy = (turn?.state ?? "stopped") === "busy";
  const status: SessionStatus =
    (live?.status as SessionStatus) ?? (ref?.status === "running" ? "running" : "idle");
  const msgs =
    threadId != null && sid != null
      ? (leadMessages[threadId] ?? []).filter((m) => m.session_id === sid)
      : [];
  const openAsks = needs.filter((n) => n.direction_id === directionId);
  const nativeId = live?.nativeId ?? ref?.native_id ?? null;

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
            onClick={() => setShowDiff(true)}
            title={t("diff.tab")}
            aria-label={t("diff.tab")}
            className="grid h-7 w-7 shrink-0 place-items-center rounded-[var(--radius-md)] border border-border text-ink-muted transition-colors hover:bg-surface hover:text-ink"
          >
            <GitCompare size={13} />
          </button>
          <StatusChip status={status} />
          {ref && (
            <Inspect
              path={ref.worktree}
              branch={ref.branch}
              nativeId={ref.native_id}
              tool={ref.tool}
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
          />
          <ChatComposer
            slashCommands={(sid != null ? workerSlash[sid] : undefined) ?? []}
            onNeedSlashCommands={() => sid != null && discoverWorkerSlash(sid)}
            busy={busy}
            queued={turn?.queued ?? 0}
            placeholder={loadError ?? t("session.message")}
            onSend={(v, images, fs) => void sendToWorker(directionId, repoId, v, images, fs)}
            onStop={() => sid != null && void api.chatInterrupt(sid)}
            onTakeOver={async () => {
              if (!ref || !nativeId || sid == null) return false;
              await api.chatStop(sid);
              await navigator.clipboard.writeText(
                resumeCommand(ref.tool, ref.worktree, nativeId),
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

      {ref && (
        <DiffPanel
          cwd={ref.worktree}
          open={showDiff}
          onClose={() => setShowDiff(false)}
          onAsk={(text) => void sendToWorker(directionId, repoId, text)}
        />
      )}
    </div>
  );
}

function AskInline({ text, onAnswer }: { text: string; onAnswer: (answer: string) => void }) {
  const { t } = useTranslation();
  const [val, setVal] = useState("");
  return (
    <div className="flex items-center gap-2 py-1">
      <span className="min-w-0 flex-1 truncate text-[13px] text-ink">{text}</span>
      <input
        value={val}
        onChange={(e) => setVal(e.target.value)}
        onKeyDown={(e) => {
          if (e.key === "Enter" && val.trim()) {
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
