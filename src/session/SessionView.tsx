import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { GitCompare, Keyboard, Square } from "lucide-react";
import { useStore } from "../state/store";
import { api } from "../lib/api";
import type { SessionStatus } from "../lib/types";
import { ChatTimeline } from "./ChatTimeline";
import { ChatComposer } from "./ChatComposer";
import { appLink, resumeCommand } from "../lib/resume";
import { DiffPanel } from "./DiffPanel";
import { StatusChip } from "../components/ui/StatusChip";
import { Button } from "../components/ui/Button";
import { Inspect } from "../components/Inspect";
import { Dialog, DialogContent } from "../components/ui/Dialog";

const KEYMAP = [
  { key: "session.keymapKeyCommand", action: "session.keymapCommand" },
  { key: "session.keymapKeyComposer", action: "session.keymapComposer" },
  { key: "session.keymapKeyPanels", action: "session.keymapPanels" },
] as const;

export function SessionView() {
  const {
    sessions,
    activeSessionId,
    leadMessages,
    workerTurn,
    workerSlash,
    workerActivity,
    loadLeadChat,
  } = useStore();
  const { t } = useTranslation();
  const active = activeSessionId != null ? sessions[activeSessionId] : null;
  const [showDiff, setShowDiff] = useState(false);
  const [showKeys, setShowKeys] = useState(false);
  useEffect(() => {
    setShowDiff(false);
  }, [active?.info.session_id]);

  // Workers run on the chat engine: hydrate the thread's timeline (their rows
  // live there).
  const chatThreadId = active?.threadId ?? null;
  useEffect(() => {
    if (chatThreadId != null) void loadLeadChat(chatThreadId);
  }, [chatThreadId, loadLeadChat]);

  if (!active) return null;

  const { info, status, nativeId } = active;
  const running = status === "running";
  // Product words, not plumbing: "<repo> · <direction>". The real worktree
  // path / branch / native id live in Inspect (§4.7).
  return (
    <div className="flex min-w-0 flex-1">
      <section className="flex min-w-0 flex-1 flex-col bg-bg">
        <header className="flex items-center gap-2 border-b border-border bg-surface px-3 py-2">
          <StatusChip status={status as SessionStatus} />
          <span className="hidden min-w-0 truncate font-mono text-[11.5px] text-ink-faint md:block">
            {info.branch}
          </span>
          <span className="min-w-0 flex-1" />
          <button
            onClick={() => setShowDiff(true)}
            title={t("diff.tab")}
            aria-label={t("diff.tab")}
            className="grid h-7 w-7 shrink-0 place-items-center rounded-[var(--radius-md)] border border-border text-ink-muted transition-colors hover:bg-surface hover:text-ink"
          >
            <GitCompare size={13} />
          </button>
          <button
            onClick={() => setShowKeys(true)}
            title={t("session.keymap")}
            aria-label={t("session.keymap")}
            className="grid h-7 w-7 shrink-0 place-items-center rounded-[var(--radius-md)] border border-border text-ink-muted transition-colors hover:bg-surface hover:text-ink"
          >
            <Keyboard size={13} />
          </button>
          {running && (
            <Button
              size="sm"
              variant="danger"
              onClick={() => void api.chatStop(info.session_id)}
            >
              <Square size={11} />
              {t("session.kill")}
            </Button>
          )}
          <Inspect
            path={info.worktree}
            branch={info.branch}
            nativeId={nativeId}
            tool={info.tool}
            className="h-7 w-7 shrink-0"
          />
        </header>

        {/* chat-engine worker: weft-owned timeline + composer */}
        <div className="flex min-h-0 flex-1 flex-col">
          <ChatTimeline
            messages={(leadMessages[active.threadId] ?? []).filter(
              (m) => m.session_id === info.session_id,
            )}
            busy={(workerTurn[info.session_id]?.state ?? "stopped") === "busy"}
            activity={workerActivity[info.session_id]}
            onReviewProposal={() => {}}
          />
          <ChatComposer
            slashCommands={workerSlash[info.session_id] ?? []}
            busy={(workerTurn[info.session_id]?.state ?? "stopped") === "busy"}
            stopped={(workerTurn[info.session_id]?.state ?? "stopped") === "stopped"}
            queued={workerTurn[info.session_id]?.queued ?? 0}
            stoppedHint={t("session.chatStopped")}
            placeholder={t("session.message")}
            onSend={(v, images, files) => void api.chatSend(info.session_id, v, images, files)}
            onStop={() => void api.chatInterrupt(info.session_id)}
            onTakeOver={async () => {
              if (!nativeId) return false;
              await api.chatStop(info.session_id);
              await navigator.clipboard.writeText(
                resumeCommand(info.tool, info.worktree, nativeId),
              );
              return true;
            }}
            onOpenApp={
              nativeId && appLink(info.tool, nativeId)
                ? () => void api.openUrl(appLink(info.tool, nativeId)!)
                : undefined
            }
          />
        </div>
      </section>

      <DiffPanel
        cwd={info.worktree}
        open={showDiff}
        onClose={() => setShowDiff(false)}
        onAsk={(text) => void api.chatSend(info.session_id, text)}
      />
      <KeymapDialog open={showKeys} onOpenChange={setShowKeys} />
    </div>
  );
}

function KeymapDialog({
  open,
  onOpenChange,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
}) {
  const { t } = useTranslation();
  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent
        title={t("session.keymap")}
        description={t("session.keymapDesc")}
        className="w-[min(560px,calc(100vw-2rem))]"
      >
        <div className="flex flex-col gap-1.5">
          {KEYMAP.map((row) => (
            <div
              key={row.key}
              className="flex items-center gap-2 rounded-[var(--radius-md)] border border-border bg-bg px-2.5 py-2 text-[12px]"
            >
              <kbd className="min-w-[92px] rounded border border-border bg-raised px-2 py-1 text-center font-mono text-[11px] text-ink">
                {t(row.key)}
              </kbd>
              <span className="min-w-0 flex-1 text-ink-muted">
                {t(row.action)}
              </span>
            </div>
          ))}
        </div>
      </DialogContent>
    </Dialog>
  );
}
