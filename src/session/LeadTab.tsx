import { useEffect, useRef, useState } from "react";
import { AnimatePresence, motion } from "motion/react";
import { useTranslation } from "react-i18next";
import { ArrowRight, MessagesSquare, RotateCcw, Send, Sparkles, Square, SquareTerminal } from "lucide-react";
import { useStore, type OpenSession } from "../state/store";
import type { SessionStatus } from "../lib/types";
import { TerminalPanel } from "../panels/TerminalPanel";
import { Transcript } from "./Transcript";
import { StatusChip } from "../components/ui/StatusChip";
import { Button } from "../components/ui/Button";
import { Inspect } from "../components/Inspect";
import { ToolIcon } from "../components/ToolIcon";
import { cn } from "../lib/cn";

/**
 * The thread's lead conversation as a full tab, mirroring a worker session:
 * observe (Chat) by default, switch to interactive (Terminal). When the lead
 * proposes a plan, a card surfaces here; reviewing happens on the Board tab.
 */
export function LeadTab({ onReview }: { onReview: () => void }) {
  const {
    leadSession,
    startLead,
    killSession,
    proposal,
    reviewingProposal,
    setReviewingProposal,
    activeThreadId,
    sessions,
  } = useStore();
  const { t } = useTranslation();
  const [view, setView] = useState<"chat" | "terminal">("chat");
  // Auto-start at most ONCE per thread. Keying off leadSession alone loops if the
  // lead exits right after spawning (null → start → exit → null → start …).
  const attemptedRef = useRef<number | null>(null);

  useEffect(() => {
    if (leadSession || activeThreadId == null) return;
    if (attemptedRef.current === activeThreadId) return;
    attemptedRef.current = activeThreadId;
    void startLead();
  }, [leadSession, activeThreadId, startLead]);

  if (!leadSession) {
    // The lead finished its turn (PTY exited). Keep its transcript readable, with
    // a Restart to continue the conversation — don't hide the work behind a button.
    const exitedLead = Object.values(sessions).find(
      (s) => s.kind === "lead" && s.threadId === activeThreadId && s.status === "exited",
    );
    if (exitedLead) {
      return (
        <LeadStopped
          session={exitedLead}
          onRestart={() => {
            attemptedRef.current = activeThreadId;
            void startLead();
          }}
        />
      );
    }
    return <LeadStarting />;
  }

  const { info, status, nativeId } = leadSession;
  const running = status === "running" || status === "starting";
  const proposalPending =
    proposal?.status === "proposed" &&
    proposal.directions.length > 0 &&
    !reviewingProposal;

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <div className="flex items-center gap-2 border-b border-border bg-surface px-3 py-2">
        <span className="flex shrink-0 items-center gap-1.5 whitespace-nowrap rounded-[var(--radius-sm)] bg-raised px-2 py-0.5 text-[11px] capitalize text-ink-muted">
          <ToolIcon tool={info.tool} size={12} />
          {info.tool}
        </span>
        <div className="ml-auto flex shrink-0 items-center gap-2">
          <div className="flex items-center rounded-[var(--radius-md)] bg-bg p-0.5">
            <ViewTab active={view === "chat"} onClick={() => setView("chat")} title={t("lead.viewChat")}>
              <MessagesSquare size={13} />
            </ViewTab>
            <ViewTab active={view === "terminal"} onClick={() => setView("terminal")} title={t("lead.viewTerminal")}>
              <SquareTerminal size={13} />
            </ViewTab>
          </div>
          <StatusChip status={status as SessionStatus} />
          {running && (
            <Button size="sm" variant="danger" onClick={() => void killSession(info.session_id)}>
              <Square size={11} />
              {t("session.kill")}
            </Button>
          )}
          <Inspect
            path={info.worktree}
            nativeId={nativeId}
            tool={info.tool}
            className="h-7 w-7"
          />
        </div>
      </div>

      <AnimatePresence>
        {proposalPending && (
          <motion.button
            initial={{ opacity: 0, y: -6 }}
            animate={{ opacity: 1, y: 0 }}
            exit={{ opacity: 0, y: -6 }}
            transition={{ duration: 0.18 }}
            onClick={() => {
              setReviewingProposal(true);
              onReview();
            }}
            className="group mx-3 mt-3 flex items-center gap-2.5 rounded-[var(--radius-md)] border border-accent/40 bg-accent-ghost px-3 py-2.5 text-left transition-colors hover:border-accent/70"
          >
            <Sparkles size={15} className="shrink-0 text-accent" />
            <div className="min-w-0 flex-1">
              <p className="text-[12.5px] font-medium text-ink">
                {t("lead.proposalReady", { count: proposal!.directions.length })}
              </p>
              <p className="truncate text-[11px] text-ink-muted">
                {proposal!.rationale || t("lead.reviewCreate")}
              </p>
            </div>
            <span className="flex shrink-0 items-center gap-1 text-[11px] font-medium text-accent">
              {t("lead.reviewCreate")}
              <ArrowRight size={12} className="transition-transform group-hover:translate-x-0.5" />
            </span>
          </motion.button>
        )}
      </AnimatePresence>

      {view === "chat" ? (
        <div className="flex min-h-0 flex-1 flex-col">
          <Transcript cwd={info.worktree} tool={info.tool} running={running} />
          {running && <LeadComposer />}
        </div>
      ) : (
        <motion.div
          key={info.session_id}
          initial={{ opacity: 0 }}
          animate={{ opacity: 1 }}
          transition={{ duration: 0.16 }}
          className="min-h-0 flex-1 p-1.5"
        >
          <TerminalPanel sessionId={info.session_id} />
        </motion.div>
      )}
    </div>
  );
}

/**
 * The composer for the Lead conversation — what makes "home is a conversation"
 * (§ M-C) actually conversational from the Chat view, instead of forcing the
 * human into the raw Terminal tab to type. Sends via the store's bracketed-paste
 * path so multi-line prompts land intact in the TUI. Enter sends; Shift+Enter
 * adds a line.
 */
function LeadComposer() {
  const { sendToLead } = useStore();
  const { t } = useTranslation();
  const [text, setText] = useState("");
  const ref = useRef<HTMLTextAreaElement>(null);

  // Auto-grow to content (capped), so a long prompt is visible as you type.
  useEffect(() => {
    const el = ref.current;
    if (!el) return;
    el.style.height = "0px";
    el.style.height = `${Math.min(el.scrollHeight, 160)}px`;
  }, [text]);

  const send = () => {
    const v = text.trim();
    if (!v) return;
    void sendToLead(v);
    setText("");
  };

  return (
    <div className="border-t border-border bg-surface px-2.5 py-2">
      <div className="flex items-end gap-2 rounded-[var(--radius-md)] border border-border bg-bg px-2.5 py-1.5 transition-colors focus-within:border-brand/50">
        <textarea
          ref={ref}
          value={text}
          onChange={(e) => setText(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter" && !e.shiftKey) {
              e.preventDefault();
              send();
            }
          }}
          rows={1}
          placeholder={t("lead.compose")}
          className="max-h-40 min-h-[20px] flex-1 resize-none bg-transparent text-[13px] leading-snug text-ink outline-none placeholder:text-ink-faint"
        />
        <button
          onClick={send}
          disabled={!text.trim()}
          aria-label={t("lead.send")}
          title={t("lead.send")}
          className="grid h-7 w-7 shrink-0 place-items-center rounded-[var(--radius-sm)] bg-brand text-brand-ink transition-opacity hover:opacity-90 disabled:opacity-30"
        >
          <Send size={13} />
        </button>
      </div>
    </div>
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
        active ? "bg-raised text-ink shadow-[0_1px_2px_rgba(0,0,0,0.3)]" : "text-ink-faint hover:text-ink-muted",
      )}
    >
      {children}
    </button>
  );
}

function LeadStarting() {
  const { t } = useTranslation();
  return (
    <div className="flex flex-1 flex-col items-center justify-center px-6 text-center">
      <div className="grid h-11 w-11 place-items-center rounded-[var(--radius-lg)] bg-accent-ghost">
        <Sparkles size={20} className="animate-pulse text-accent" />
      </div>
      <p className="mt-3 text-[13px] text-ink-muted">{t("lead.starting")}</p>
    </div>
  );
}

/** An exited lead: its transcript stays readable; Restart continues the thread. */
function LeadStopped({
  session,
  onRestart,
}: {
  session: OpenSession;
  onRestart: () => void;
}) {
  const { t } = useTranslation();
  const { info, nativeId } = session;
  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <div className="flex items-center gap-2 border-b border-border bg-surface px-3 py-2">
        <span className="flex shrink-0 items-center gap-1.5 whitespace-nowrap rounded-[var(--radius-sm)] bg-raised px-2 py-0.5 text-[11px] capitalize text-ink-muted">
          <ToolIcon tool={info.tool} size={12} />
          {info.tool}
        </span>
        <span className="text-[11px] text-ink-faint">{t("lead.stopped")}</span>
        <div className="ml-auto flex shrink-0 items-center gap-2">
          <Button size="sm" variant="primary" onClick={onRestart}>
            <RotateCcw size={12} />
            {t("lead.restart")}
          </Button>
          <Inspect path={info.worktree} nativeId={nativeId} tool={info.tool} className="h-7 w-7" />
        </div>
      </div>
      <Transcript cwd={info.worktree} tool={info.tool} running={false} />
    </div>
  );
}
