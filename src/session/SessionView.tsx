import { useEffect, useState } from "react";
import { motion } from "motion/react";
import { useTranslation } from "react-i18next";
import {
  ArrowLeft,
  GitCompare,
  MessagesSquare,
  RotateCcw,
  Square,
  SquareTerminal,
} from "lucide-react";
import { useStore } from "../state/store";
import type { SessionStatus } from "../lib/types";
import { TerminalPanel } from "../panels/TerminalPanel";
import { Transcript } from "./Transcript";
import { DiffDrawer } from "./DiffDrawer";
import { StatusChip } from "../components/ui/StatusChip";
import { Button } from "../components/ui/Button";
import { Inspect } from "../components/Inspect";
import { ToolIcon } from "../components/ToolIcon";
import { cn } from "../lib/cn";

export function SessionView() {
  const {
    sessions,
    activeSessionId,
    resumeSession,
    killSession,
    backToBoard,
    repos,
    directionsByThread,
  } = useStore();
  const { t } = useTranslation();
  const active = activeSessionId != null ? sessions[activeSessionId] : null;
  const tool = active?.info.tool;
  // Observe by default (chat); all three tools have a sidecar transcript now.
  const transcripted = tool === "claude" || tool === "codex" || tool === "opencode";
  const [view, setView] = useState<"chat" | "terminal">(
    transcripted ? "chat" : "terminal",
  );
  const [showDiff, setShowDiff] = useState(false);
  useEffect(() => {
    setView(transcripted ? "chat" : "terminal");
    setShowDiff(false);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [active?.info.session_id, tool]);

  if (!active) return null;

  const { info, status, nativeId } = active;
  const isLead = active.kind === "lead";
  const running = status === "running" || status === "starting";
  // Product words, not plumbing: "<repo> · <direction>". The real worktree
  // path / branch / native id live in Inspect (§4.7).
  const repoName =
    repos.find((r) => r.id === active.repoId)?.name ?? "working copy";
  const dirName =
    directionsByThread[active.threadId]?.find((d) => d.id === active.directionId)?.name ??
    "task";

  return (
    <section className="flex min-w-0 flex-1 flex-col bg-bg">
      {/* session header */}
      <header className="flex items-center gap-3 border-b border-border bg-surface px-3 py-2">
        <button
          onClick={backToBoard}
          aria-label={t("session.back")}
          className="-ml-1 grid h-7 w-7 place-items-center rounded-[var(--radius-md)] text-ink-faint transition-colors hover:bg-brand-ghost hover:text-ink"
        >
          <ArrowLeft size={15} />
        </button>
        <span className="flex items-center gap-1.5 rounded-[var(--radius-sm)] bg-raised px-2 py-0.5 text-[11px] font-medium capitalize text-ink-muted">
          <ToolIcon tool={info.tool} size={12} />
          {info.tool}
        </span>
        {isLead ? (
          <span className="flex items-center gap-1.5 text-[13px]">
            <span className="rounded-full bg-accent-ghost px-2 py-0.5 text-[11px] font-medium text-accent">
              {t("session.lead")}
            </span>
            <span className="text-ink-muted">{t("session.leadPlanning")}</span>
          </span>
        ) : (
          <span className="flex min-w-0 items-center gap-1.5 text-[13px] text-ink">
            <span className="truncate font-medium">{repoName}</span>
            <span className="text-ink-faint">·</span>
            <span className="truncate text-ink-muted">{dirName}</span>
          </span>
        )}

        <div className="ml-auto flex items-center gap-2">
          <div className="flex items-center rounded-[var(--radius-md)] bg-bg p-0.5">
            <ViewTab active={view === "chat"} onClick={() => setView("chat")} title={t("lead.viewChat")}>
              <MessagesSquare size={13} />
            </ViewTab>
            <ViewTab active={view === "terminal"} onClick={() => setView("terminal")} title={t("lead.viewTerminal")}>
              <SquareTerminal size={13} />
            </ViewTab>
          </div>
          {!isLead && (
            <button
              onClick={() => setShowDiff(true)}
              title={t("diff.tab")}
              className="flex h-7 items-center gap-1.5 rounded-[var(--radius-md)] border border-border px-2.5 text-[12px] text-ink-muted transition-colors hover:bg-surface hover:text-ink"
            >
              <GitCompare size={13} />
              {t("diff.tab")}
            </button>
          )}
          <StatusChip status={status as SessionStatus} />
          {running ? (
            <Button size="sm" variant="danger" onClick={() => void killSession(info.session_id)}>
              <Square size={11} />
              {t("session.kill")}
            </Button>
          ) : (
            <Button
              size="sm"
              variant="default"
              onClick={() => void resumeSession(info.session_id)}
              disabled={!nativeId}
              title={nativeId ? t("session.resumeReady") : t("session.starting")}
            >
              <RotateCcw size={12} />
              {t("session.resume")}
            </Button>
          )}
          <Inspect
            path={info.worktree}
            branch={info.branch}
            nativeId={nativeId}
            className="h-7 w-7"
          />
        </div>
      </header>

      {view === "chat" ? (
        <Transcript cwd={info.worktree} tool={info.tool} running={running} />
      ) : (
        /* embedded native TUI — keyed so each session gets a fresh terminal */
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

      {!isLead && (
        <DiffDrawer cwd={info.worktree} open={showDiff} onClose={() => setShowDiff(false)} />
      )}
    </section>
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

