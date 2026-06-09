import { useEffect, useState } from "react";
import { motion } from "motion/react";
import { useTranslation } from "react-i18next";
import {
  GitCompare,
  Keyboard,
  MessagesSquare,
  ShieldQuestion,
  Square,
  SquareTerminal,
} from "lucide-react";
import { useStore } from "../state/store";
import { api } from "../lib/api";
import type { SessionStatus } from "../lib/types";
import { TerminalPanel } from "../panels/TerminalPanel";
import { Transcript } from "./Transcript";
import { ChatTimeline } from "./ChatTimeline";
import { ChatComposer } from "./ChatComposer";
import { resumeCommand } from "../lib/resume";
import { DiffPanel } from "./DiffPanel";
import { StatusChip } from "../components/ui/StatusChip";
import { Button } from "../components/ui/Button";
import { Composer } from "../components/Composer";
import { Inspect } from "../components/Inspect";
import { toolFullName } from "../components/ToolIcon";
import { Dialog, DialogContent } from "../components/ui/Dialog";
import { cn } from "../lib/cn";

const KEYMAP = [
  { key: "session.keymapKeyCommand", owner: "product", action: "session.keymapCommand" },
  { key: "session.keymapKeyComposer", owner: "product", action: "session.keymapComposer" },
  { key: "session.keymapKeyPanels", owner: "product", action: "session.keymapPanels" },
  { key: "session.keymapKeyInterrupt", owner: "terminal", action: "session.keymapInterrupt" },
  { key: "session.keymapKeyPass", owner: "terminal", action: "session.keymapPass" },
] as const;

export function SessionView() {
  const {
    sessions,
    activeSessionId,
    killSession,
    sendToSession,
    leadMessages,
    workerTurn,
    workerSlash,
    workerActivity,
    loadLeadChat,
  } = useStore();
  const { t } = useTranslation();
  const active = activeSessionId != null ? sessions[activeSessionId] : null;
  const tool = active?.info.tool;
  const isChat = active?.mode === "chat";
  // Observe by default (chat); all three tools have a sidecar transcript now.
  const transcripted = tool === "claude" || tool === "codex" || tool === "opencode";
  const [view, setView] = useState<"chat" | "terminal">(
    transcripted ? "chat" : "terminal",
  );
  const [showDiff, setShowDiff] = useState(false);
  const [showKeys, setShowKeys] = useState(false);
  // Bumped on each send so the transcript refreshes + snaps to bottom at once.
  const [sentNonce, setSentNonce] = useState(0);
  useEffect(() => {
    setView(transcripted ? "chat" : "terminal");
    setShowDiff(false);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [active?.info.session_id, tool]);

  // Chat-mode workers: hydrate the thread's timeline (their rows live there).
  const chatThreadId = isChat ? active?.threadId : null;
  useEffect(() => {
    if (chatThreadId != null) void loadLeadChat(chatThreadId);
  }, [chatThreadId, loadLeadChat]);

  if (!active) return null;

  const { info, status, nativeId } = active;
  const isLead = active.kind === "lead";
  const running = status === "running" || status === "starting";
  // Product words, not plumbing: "<repo> · <direction>". The real worktree
  // path / branch / native id live in Inspect (§4.7).
  return (
    <div className="flex min-w-0 flex-1">
      <section className="flex min-w-0 flex-1 flex-col bg-bg">
        <header className="flex items-center gap-2 border-b border-border bg-surface px-3 py-2">
          {!isLead && !isChat && (
            <div className="flex items-center rounded-[var(--radius-md)] bg-bg p-0.5">
              <ViewTab active={view === "chat"} onClick={() => setView("chat")} title={t("lead.viewChat")}>
                <MessagesSquare size={13} />
              </ViewTab>
              <ViewTab active={view === "terminal"} onClick={() => setView("terminal")} title={t("lead.viewTerminal")}>
                <SquareTerminal size={13} />
              </ViewTab>
            </div>
          )}
          <StatusChip status={status as SessionStatus} />
          <span className="hidden min-w-0 truncate font-mono text-[11.5px] text-ink-faint md:block">
            {info.branch}
          </span>
          <span className="min-w-0 flex-1" />
          {!isLead && (
            <button
              onClick={() => setShowDiff(true)}
              title={t("diff.tab")}
              aria-label={t("diff.tab")}
              className="grid h-7 w-7 shrink-0 place-items-center rounded-[var(--radius-md)] border border-border text-ink-muted transition-colors hover:bg-surface hover:text-ink"
            >
              <GitCompare size={13} />
            </button>
          )}
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
              onClick={() =>
                void (isChat ? api.chatStop(info.session_id) : killSession(info.session_id))
              }
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

        {/* §4.3 approval bar: mirror the native y/N prompt as buttons that write to
            the PTY (a convenience over the native prompt, never a replacement). */}
        {status === "waiting-approval" && (
          <div className="flex items-center gap-2 border-b border-waiting/40 bg-waiting/10 px-3 py-2 text-[12.5px]">
            <ShieldQuestion size={14} className="shrink-0 text-waiting" />
            <span className="text-ink-muted">
              <span className="text-ink">{toolFullName(info.tool)}</span> {t("needs.wantsPermission")}
            </span>
            <div className="ml-auto flex shrink-0 items-center gap-1.5">
              <Button size="sm" variant="primary" onClick={() => void api.writePty(info.session_id, "y\n")}>
                {t("common.allow")} · y
              </Button>
              <Button size="sm" variant="default" onClick={() => void api.writePty(info.session_id, "n\n")}>
                {t("common.deny")} · n
              </Button>
            </div>
          </div>
        )}

        {isChat ? (
          /* chat-engine worker: weft-owned timeline + composer, no PTY at all */
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
            />
          </div>
        ) : isLead || view === "chat" ? (
          <div className="flex min-h-0 flex-1 flex-col">
            <Transcript
              cwd={info.worktree}
              tool={info.tool}
              running={running}
              refreshSignal={sentNonce}
            />
            {running && (
              <div className="border-t border-border bg-surface px-2.5 py-2">
                <Composer
                  multiline
                  placeholder={isLead ? t("lead.compose") : t("session.message")}
                  onSend={(v) => {
                    void sendToSession(info.session_id, v);
                    setSentNonce((n) => n + 1);
                  }}
                />
              </div>
            )}
          </div>
        ) : (
          /* embedded native TUI — keyed so each session gets a fresh terminal */
          <motion.div
            key={info.session_id}
            initial={{ opacity: 0 }}
            animate={{ opacity: 1 }}
            transition={{ duration: 0.16 }}
            className="min-h-0 flex-1 p-3"
          >
            <TerminalFrame cwd={info.worktree} focused={running}>
              <TerminalPanel sessionId={info.session_id} />
            </TerminalFrame>
          </motion.div>
        )}
      </section>

      {!isLead && (
        <DiffPanel cwd={info.worktree} open={showDiff} onClose={() => setShowDiff(false)} />
      )}
      <KeymapDialog open={showKeys} onOpenChange={setShowKeys} />
    </div>
  );
}

function TerminalFrame({
  cwd,
  focused,
  children,
}: {
  cwd: string;
  focused: boolean;
  children: React.ReactNode;
}) {
  const { t } = useTranslation();
  return (
    <div
      className={cn(
        "flex h-full min-h-0 flex-col overflow-hidden rounded-[var(--radius-lg)] border bg-[#1a1814]",
        focused ? "border-brand/55" : "border-border",
      )}
    >
      <div className="flex h-9 shrink-0 items-center gap-1.5 border-b border-white/10 bg-[#211f1a] px-3">
        <span className="h-2.5 w-2.5 rounded-full bg-[#ff6b57]" />
        <span className="h-2.5 w-2.5 rounded-full bg-[#ffbf4a]" />
        <span className="h-2.5 w-2.5 rounded-full bg-[#36c56f]" />
        <span className="ml-2 min-w-0 truncate font-mono text-[11px] text-[#b9b4aa]">
          {cwd}
        </span>
        {focused && (
          <span className="ml-auto inline-flex shrink-0 items-center gap-1.5 rounded-full border border-brand/35 bg-brand-ghost px-2 py-0.5 text-[11px] text-brand">
            <span className="h-1.5 w-1.5 rounded-full bg-brand" />
            {t("session.typingHere")}
          </span>
        )}
      </div>
      <div className="min-h-0 flex-1 p-2">
        {children}
      </div>
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
              <span
                className={cn(
                  "shrink-0 rounded-full px-2 py-0.5 text-[11px]",
                  row.owner === "product"
                    ? "bg-brand-ghost text-brand"
                    : "bg-accent-ghost text-accent",
                )}
              >
                {t(`session.keymapOwner_${row.owner}`)}
              </span>
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

function ViewTab({
  active,
  onClick,
  title,
  children,
}: {
  active: boolean;
  onClick: () => void;
  title: string;
  children: React.ReactNode;
}) {
  return (
    <button
      onClick={onClick}
      title={title}
      aria-label={title}
      aria-pressed={active}
      className={cn(
        "grid h-6 w-7 place-items-center rounded-[var(--radius-sm)] transition-colors",
        active
          ? "bg-raised text-ink shadow-[0_1px_2px_rgba(0,0,0,0.3)]"
          : "text-ink-faint hover:text-ink-muted",
      )}
    >
      {children}
    </button>
  );
}
