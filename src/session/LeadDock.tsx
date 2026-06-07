import { useRef, useState } from "react";
import { AnimatePresence, motion } from "motion/react";
import { useTranslation } from "react-i18next";
import {
  ArrowRight,
  ChevronsLeft,
  ChevronsRight,
  MessagesSquare,
  Send,
  Sparkles,
  SquareTerminal,
} from "lucide-react";
import { useStore } from "../state/store";
import { TerminalPanel } from "../panels/TerminalPanel";
import { LeadTranscript } from "./LeadTranscript";
import { ToolIcon } from "../components/ToolIcon";
import { Button } from "../components/ui/Button";
import { cn } from "../lib/cn";

const EXPO = "cubic-bezier(0.16,1,0.3,1)";

/**
 * The thread's home: a collapsible right dock holding the persistent lead
 * conversation (embedded native TUI) + a composer. When the lead proposes a
 * plan, a card surfaces here; reviewing/creating happens in the board canvas.
 */
export function LeadDock() {
  const {
    leadSession,
    leadCollapsed,
    toggleLeadCollapsed,
    proposal,
    reviewingProposal,
    setReviewingProposal,
  } = useStore();
  const { t } = useTranslation();
  const [view, setView] = useState<"chat" | "terminal">("chat");

  const running = leadSession?.status === "running" || leadSession?.status === "starting";
  const proposalPending =
    proposal?.status === "proposed" &&
    proposal.directions.length > 0 &&
    !reviewingProposal;

  if (leadCollapsed) {
    return (
      <aside
        style={{ transition: `width 200ms ${EXPO}` }}
        className="flex w-11 shrink-0 flex-col items-center border-l border-border bg-surface py-3 motion-reduce:transition-none"
      >
        <button
          onClick={toggleLeadCollapsed}
          aria-label={t("lead.expand")}
          title={t("lead.expand")}
          className="grid h-7 w-7 place-items-center rounded-[var(--radius-md)] text-ink-faint transition-colors hover:bg-brand-ghost hover:text-ink"
        >
          <ChevronsLeft size={15} />
        </button>
        <div className="mt-3 flex flex-1 flex-col items-center gap-2">
          <span
            className={cn(
              "h-1.5 w-1.5 rounded-full",
              proposalPending ? "bg-accent" : running ? "bg-running" : "bg-idle",
              (proposalPending || running) && "animate-pulse",
            )}
          />
          <span
            className="text-[11px] font-semibold uppercase tracking-wider text-ink-faint"
            style={{ writingMode: "vertical-rl" }}
          >
            {t("lead.title")}
          </span>
        </div>
      </aside>
    );
  }

  return (
    <aside
      style={{ transition: `width 200ms ${EXPO}` }}
      className="flex w-[420px] shrink-0 flex-col border-l border-border bg-surface motion-reduce:transition-none"
    >
      <header className="flex items-center gap-2 border-b border-border px-3 py-2.5">
        <span
          className={cn(
            "h-1.5 w-1.5 rounded-full",
            running ? "bg-running" : "bg-idle",
            running && "animate-pulse",
          )}
        />
        <span className="text-[12px] font-semibold text-ink">{t("lead.title")}</span>
        {leadSession && (
          <span className="flex items-center gap-1 rounded-[var(--radius-sm)] bg-raised px-1.5 py-0.5 text-[10px] capitalize text-ink-muted">
            <ToolIcon tool={leadSession.info.tool} size={11} />
            {leadSession.info.tool}
          </span>
        )}
        <div className="ml-auto flex items-center gap-1">
          {leadSession && (
            <div className="flex items-center rounded-[var(--radius-md)] bg-bg p-0.5">
              <ViewTab
                active={view === "chat"}
                onClick={() => setView("chat")}
                title={t("lead.viewChat")}
              >
                <MessagesSquare size={13} />
              </ViewTab>
              <ViewTab
                active={view === "terminal"}
                onClick={() => setView("terminal")}
                title={t("lead.viewTerminal")}
              >
                <SquareTerminal size={13} />
              </ViewTab>
            </div>
          )}
          <button
            onClick={toggleLeadCollapsed}
            aria-label={t("lead.collapse")}
            title={t("lead.collapse")}
            className="grid h-7 w-7 place-items-center rounded-[var(--radius-md)] text-ink-faint transition-colors hover:bg-brand-ghost hover:text-ink"
          >
            <ChevronsRight size={15} />
          </button>
        </div>
      </header>

      {leadSession ? (
        <div className="flex min-h-0 flex-1 flex-col">
          <AnimatePresence>
            {proposalPending && (
              <motion.button
                initial={{ opacity: 0, y: -6 }}
                animate={{ opacity: 1, y: 0 }}
                exit={{ opacity: 0, y: -6 }}
                transition={{ duration: 0.18 }}
                onClick={() => setReviewingProposal(true)}
                className="group mx-2.5 mt-2.5 flex items-center gap-2.5 rounded-[var(--radius-md)] border border-accent/40 bg-accent-ghost px-3 py-2.5 text-left transition-colors hover:border-accent/70"
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
            <LeadTranscript
              cwd={leadSession.info.worktree}
              tool={leadSession.info.tool}
            />
          ) : (
            <motion.div
              key={leadSession.info.session_id}
              initial={{ opacity: 0 }}
              animate={{ opacity: 1 }}
              transition={{ duration: 0.16 }}
              className="min-h-0 flex-1 p-1.5"
            >
              <TerminalPanel sessionId={leadSession.info.session_id} mode="readonly" />
            </motion.div>
          )}
          <Composer />
        </div>
      ) : (
        <LeadStart />
      )}
    </aside>
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

function LeadStart() {
  const { startLead } = useStore();
  const { t } = useTranslation();
  const [busy, setBusy] = useState(false);
  return (
    <div className="flex flex-1 flex-col items-center justify-center px-6 text-center">
      <div className="grid h-11 w-11 place-items-center rounded-[var(--radius-lg)] bg-accent-ghost">
        <Sparkles size={20} className="text-accent" />
      </div>
      <h2 className="mt-3 text-[14px] font-semibold text-ink">{t("lead.startTitle")}</h2>
      <p className="mt-1.5 max-w-[260px] text-[12px] leading-relaxed text-ink-faint">
        {t("lead.startBody")}
      </p>
      <Button
        variant="primary"
        className="mt-4"
        disabled={busy}
        onClick={async () => {
          setBusy(true);
          try {
            await startLead();
          } finally {
            setBusy(false);
          }
        }}
      >
        <Sparkles size={14} />
        {busy ? t("lead.starting") : t("lead.start")}
      </Button>
    </div>
  );
}

function Composer() {
  const { sendToLead } = useStore();
  const { t } = useTranslation();
  const [text, setText] = useState("");
  const ref = useRef<HTMLTextAreaElement>(null);

  async function send() {
    const body = text.trim();
    if (!body) return;
    setText("");
    if (ref.current) ref.current.style.height = "auto";
    await sendToLead(body);
  }

  return (
    <form
      onSubmit={(e) => {
        e.preventDefault();
        void send();
      }}
      className="flex items-end gap-2 border-t border-border p-2.5"
    >
      <textarea
        ref={ref}
        rows={1}
        value={text}
        onChange={(e) => {
          setText(e.currentTarget.value);
          const el = e.currentTarget;
          el.style.height = "auto";
          el.style.height = `${Math.min(el.scrollHeight, 160)}px`;
        }}
        onKeyDown={(e) => {
          if (e.key === "Enter" && !e.shiftKey) {
            e.preventDefault();
            void send();
          }
        }}
        placeholder={t("lead.compose")}
        className="max-h-40 min-h-[36px] flex-1 resize-none rounded-[var(--radius-md)] border border-border bg-bg px-3 py-2 text-[12.5px] leading-relaxed text-ink outline-none transition-colors placeholder:text-ink-faint focus:border-border-strong"
      />
      <Button type="submit" variant="primary" size="icon" disabled={!text.trim()} title={t("lead.send")}>
        <Send size={14} />
      </Button>
    </form>
  );
}
