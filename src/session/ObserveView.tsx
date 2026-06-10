import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { GitCompare, Play } from "lucide-react";
import { useStore } from "../state/store";
import { api } from "../lib/api";
import type { ObserveRef, SessionStatus } from "../lib/types";
import { Transcript } from "./Transcript";
import { ChatTimeline } from "./ChatTimeline";
import { ChatComposer } from "./ChatComposer";
import { PermissionBar } from "./PermissionBar";
import { appLink, resumeCommand } from "../lib/resume";
import { DiffPanel } from "./DiffPanel";
import { StatusChip } from "../components/ui/StatusChip";
import { Button } from "../components/ui/Button";
import { Tooltip } from "../components/ui/Tooltip";
import { Inspect } from "../components/Inspect";
import { ToolIcon, toolFullName } from "../components/ToolIcon";

export function ObserveView() {
  const {
    viewing,
    driveDirection,
    sessions,
    needs,
    asks,
    answerAsk,
    activeThreadId,
    leadMessages,
    workerTurn,
    workerSlash,
    workerActivity,
    loadLeadChat,
    sendToDirection,
  } = useStore();
  const { t } = useTranslation();
  const [ref, setRef] = useState<ObserveRef | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [driveError, setDriveError] = useState<string | null>(null);
  const [showDiff, setShowDiff] = useState(false);
  const [driving, setDriving] = useState(false);

  const directionId = viewing?.directionId ?? null;
  const repoId = viewing?.repoId ?? null;

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
  }, [directionId, repoId]);

  // Chat-mode sessions render the weft-owned timeline; keep it hydrated.
  useEffect(() => {
    if (activeThreadId != null) void loadLeadChat(activeThreadId);
  }, [activeThreadId, loadLeadChat]);

  if (viewing == null) return null;

  const liveSession = Object.values(sessions).find(
    (s) => s.directionId === directionId && s.repoId === repoId && s.status !== "exited",
  );
  const openAsks = needs.filter((n) => n.direction_id === directionId);

  // A chat-engine worker (any vendor) shows its REAL conversation, not the
  // jsonl projection: live chat session, or persisted chat rows after a
  // restart (chat_send rebuilds the engine on demand).
  const chatSessionId =
    liveSession != null
      ? liveSession.info.session_id
      : ref?.session_id != null
        ? ref.session_id
        : null;
  const chatMsgs =
    chatSessionId != null && activeThreadId != null
      ? (leadMessages[activeThreadId] ?? []).filter((m) => m.session_id === chatSessionId)
      : [];
  const chatMode = chatSessionId != null && (liveSession != null || chatMsgs.length > 0);

  // Label: attach (live) → continue (has native id) → start (never dispatched).
  const driveLabel = liveSession
    ? t("observe.attach")
    : ref?.native_id
      ? t("observe.continue")
      : t("observe.start");
  const uiStatus: SessionStatus =
    (liveSession?.status as SessionStatus) ??
    (ref?.status === "running" ? "running" : "idle");

  const onDrive = async () => {
    if (directionId == null || repoId == null) return;
    setDriving(true);
    setDriveError(null);
    try {
      await driveDirection(directionId, repoId, true);
    } catch (e) {
      setDriveError(String(e));
    } finally {
      setDriving(false);
    }
  };

  return (
    <div className="flex min-h-0 min-w-0 flex-1">
      <section className="flex min-w-0 flex-1 flex-col bg-bg">
        {/* Chat takeover needs no PTY-era bar (status chip / Attach): the
            conversation is the console; diff + inspect ride the composer row. */}
        {!chatMode && (
        <header className="flex items-center justify-end gap-2 border-b border-border bg-surface px-3 py-2">
            {ref && (
              <span className="mr-auto flex shrink-0 items-center gap-1.5 whitespace-nowrap rounded-[var(--radius-sm)] bg-bg px-2 py-0.5 text-[11px] font-medium text-ink-muted">
                <ToolIcon tool={ref.tool} size={12} />
                {toolFullName(ref.tool)}
              </span>
            )}
            {ref && (
              <button
                onClick={() => setShowDiff(true)}
                title={t("diff.tab")}
                aria-label={t("diff.tab")}
                className="grid h-7 w-7 shrink-0 place-items-center rounded-[var(--radius-md)] border border-border text-ink-muted transition-colors hover:bg-surface hover:text-ink"
              >
                <GitCompare size={13} />
              </button>
            )}
            <StatusChip status={uiStatus} />
            <Button size="sm" variant="primary" disabled={driving} onClick={() => void onDrive()}>
              <Play size={11} />
              {driveLabel}
            </Button>
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
        )}

        {driveError && (
          <div className="border-b border-border bg-[oklch(0.64_0.2_25/0.12)] px-3 py-1.5 text-[12px] text-danger">
            {t("observe.driveFailed")}: {driveError}
          </div>
        )}

        {openAsks.length > 0 && (
          <div className="border-b border-border bg-surface/60 px-3 py-2">
            {openAsks.map((a) => (
              <AskInline key={a.ask_id} text={a.text} onAnswer={(txt) => void answerAsk(a, txt)} />
            ))}
          </div>
        )}

        {chatMode && chatSessionId != null ? (
          <>
            <PermissionBar asks={asks.filter((a) => a.dir === String(directionId))} />
            <ChatTimeline
              messages={chatMsgs}
              busy={(workerTurn[chatSessionId]?.state ?? "stopped") === "busy"}
              activity={workerActivity[chatSessionId]}
              onReviewProposal={() => {}}
            />
            <ChatComposer
              slashCommands={workerSlash[chatSessionId] ?? []}
              busy={(workerTurn[chatSessionId]?.state ?? "stopped") === "busy"}
              stopped={(workerTurn[chatSessionId]?.state ?? "stopped") === "stopped"}
              queued={workerTurn[chatSessionId]?.queued ?? 0}
              stoppedHint={t("session.chatStopped")}
              placeholder={t("session.message")}
              onSend={(v, images, files) =>
                void api.chatSend(chatSessionId, v, images, files)
              }
              onStop={() => void api.chatInterrupt(chatSessionId)}
              onTakeOver={async () => {
                if (!ref?.native_id) return false;
                await api.chatStop(chatSessionId);
                await navigator.clipboard.writeText(
                  resumeCommand(ref.tool, ref.worktree, ref.native_id),
                );
                return true;
              }}
              onOpenApp={
                ref?.native_id && appLink(ref.tool, ref.native_id)
                  ? () => void api.openUrl(appLink(ref.tool, ref.native_id!)!)
                  : undefined
              }
              extraActions={
                ref && (
                  <>
                    <Tooltip label={t("diff.tab")}>
                      <button
                        onClick={() => setShowDiff(true)}
                        aria-label={t("diff.tab")}
                        className="grid h-7 w-7 place-items-center rounded text-ink-faint transition-colors hover:bg-brand-ghost hover:text-ink"
                      >
                        <GitCompare size={13} />
                      </button>
                    </Tooltip>
                    <Inspect
                      path={ref.worktree}
                      branch={ref.branch}
                      nativeId={ref.native_id}
                      tool={ref.tool}
                      size={13}
                      className="h-7 w-7 shrink-0"
                    />
                  </>
                )
              }
            />
          </>
        ) : ref ? (
          <Transcript cwd={ref.worktree} tool={ref.tool} running={!!liveSession} />
        ) : (
          <div className="grid flex-1 place-items-center text-[13px] text-ink-faint">
            {loadError ?? t("observe.empty")}
          </div>
        )}
      </section>

      {ref && (
        <DiffPanel
          cwd={ref.worktree}
          open={showDiff}
          onClose={() => setShowDiff(false)}
          onAsk={
            directionId != null && repoId != null
              ? (text) => void sendToDirection(directionId, repoId, text)
              : undefined
          }
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
