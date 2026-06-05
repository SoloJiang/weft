import { motion } from "motion/react";
import {
  ArrowLeft,
  GitBranch,
  RotateCcw,
  Square,
  TerminalSquare,
} from "lucide-react";
import { useStore } from "../state/store";
import type { SessionStatus } from "../lib/types";
import { TerminalPanel } from "../panels/TerminalPanel";
import { StatusChip } from "../components/ui/StatusChip";
import { Button } from "../components/ui/Button";

export function SessionView() {
  const { sessions, activeSessionId, resumeSession, killSession, backToBoard } =
    useStore();
  const active = activeSessionId != null ? sessions[activeSessionId] : null;

  if (!active) return null;

  const { info, status, nativeId } = active;
  const cwdShort = info.worktree.replace(/^.*\/worktrees\//, "…/");

  return (
    <section className="flex min-w-0 flex-1 flex-col bg-bg">
      {/* session header */}
      <header className="flex items-center gap-3 border-b border-border bg-surface px-3 py-2">
        <button
          onClick={backToBoard}
          aria-label="Back to board"
          className="-ml-1 grid h-7 w-7 place-items-center rounded-[var(--radius-md)] text-ink-faint transition-colors hover:bg-brand-ghost hover:text-ink"
        >
          <ArrowLeft size={15} />
        </button>
        <span className="flex items-center gap-1.5 rounded-[var(--radius-sm)] bg-raised px-2 py-0.5 text-[11px] font-medium capitalize text-ink-muted">
          <TerminalSquare size={12} className="text-brand" />
          {info.tool}
        </span>
        <span className="flex items-center gap-1.5 font-mono text-[12px] text-ink-muted">
          <GitBranch size={12} className="text-ink-faint" />
          {info.branch}
        </span>
        <span
          className="truncate font-mono text-[11px] text-ink-faint"
          title={info.worktree}
        >
          {cwdShort}
        </span>

        <div className="ml-auto flex items-center gap-2">
          <StatusChip status={status as SessionStatus} />
          <Button
            size="sm"
            variant="default"
            onClick={() => void resumeSession(info.session_id)}
            disabled={!nativeId}
            title={nativeId ? "Resume in the same worktree" : "Capturing session id…"}
          >
            <RotateCcw size={12} />
            Resume
          </Button>
          <Button
            size="sm"
            variant="danger"
            onClick={() => void killSession(info.session_id)}
          >
            <Square size={11} />
            Kill
          </Button>
        </div>
      </header>

      {/* embedded native TUI — keyed so each session gets a fresh terminal */}
      <motion.div
        key={info.session_id}
        initial={{ opacity: 0 }}
        animate={{ opacity: 1 }}
        transition={{ duration: 0.16 }}
        className="min-h-0 flex-1 p-1.5"
      >
        <TerminalPanel sessionId={info.session_id} />
      </motion.div>
    </section>
  );
}

