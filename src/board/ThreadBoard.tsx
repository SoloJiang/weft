import { useState } from "react";
import { motion } from "motion/react";
import {
  ArrowRight,
  Eye,
  GitBranch,
  Layers,
  Plus,
  TerminalSquare,
} from "lucide-react";
import { useStore } from "../state/store";
import type { Direction, RepoRef, SessionStatus } from "../lib/types";
import { Button } from "../components/ui/Button";
import { StatusDot } from "../components/ui/StatusChip";
import { CreateDirectionDialog } from "../nav/dialogs";
import { cn } from "../lib/cn";

const KIND_LABEL: Record<string, string> = {
  feature: "Feature",
  bugfix: "Bugfix",
  refactor: "Refactor",
  spike: "Spike",
};

const TOOL_DOT: Record<string, string> = {
  claude: "bg-[oklch(0.7_0.16_40)]",
  codex: "bg-running",
  opencode: "bg-idle",
};
const TOOL_LABEL: Record<string, string> = {
  claude: "Claude",
  codex: "Codex",
  opencode: "OpenCode",
};

export function ThreadBoard() {
  const { threads, activeThreadId, directionsByThread } = useStore();
  const thread = threads.find((t) => t.id === activeThreadId);
  const [newDir, setNewDir] = useState(false);
  if (!thread) return null;
  const dirs = directionsByThread[thread.id] ?? [];

  return (
    <section className="flex min-w-0 flex-1 flex-col overflow-hidden bg-bg">
      <header className="flex items-center gap-3 border-b border-border px-5 py-3">
        <div className="flex min-w-0 flex-col">
          <div className="flex items-center gap-2">
            <h1 className="truncate text-[16px] font-semibold tracking-tight text-ink">
              {thread.title}
            </h1>
            <span className="rounded bg-surface px-1.5 py-0.5 font-mono text-[10px] uppercase text-ink-faint">
              {KIND_LABEL[thread.kind] ?? thread.kind}
            </span>
          </div>
          <span className="mt-0.5 text-[12px] text-ink-faint">
            {dirs.length} {dirs.length === 1 ? "direction" : "directions"} ·
            parallel work lines, each scoped to its own repos
          </span>
        </div>
        <Button variant="primary" className="ml-auto" onClick={() => setNewDir(true)}>
          <Plus size={14} />
          New direction
        </Button>
      </header>

      <div className="min-h-0 flex-1 overflow-y-auto px-5 py-4">
        {dirs.length === 0 ? (
          <EmptyBoard onAdd={() => setNewDir(true)} />
        ) : (
          <div className="grid grid-cols-[repeat(auto-fill,minmax(300px,1fr))] gap-3">
            {dirs.map((d) => (
              <DirectionCard key={d.id} direction={d} />
            ))}
            <button
              onClick={() => setNewDir(true)}
              className="flex min-h-[140px] flex-col items-center justify-center gap-2 rounded-[var(--radius-lg)] border border-dashed border-border text-ink-faint transition-colors hover:border-border-strong hover:bg-surface hover:text-ink-muted"
            >
              <Plus size={18} />
              <span className="text-[12px]">Add direction</span>
            </button>
          </div>
        )}
      </div>

      <CreateDirectionDialog open={newDir} onOpenChange={setNewDir} threadId={thread.id} />
    </section>
  );
}

function DirectionCard({ direction }: { direction: Direction }) {
  const {
    worktreesByDirection,
    directionReposByDirection,
    repos,
    sessions,
    openSession,
  } = useStore();
  const writes = worktreesByDirection[direction.id] ?? [];
  const scope = directionReposByDirection[direction.id] ?? [];
  const reads = scope
    .filter((s) => s.role === "read")
    .map((s) => repos.find((r) => r.id === s.repo_id))
    .filter((r): r is RepoRef => !!r);

  return (
    <motion.div
      layout
      className="flex flex-col rounded-[var(--radius-lg)] border border-border bg-surface"
    >
      <div className="flex items-center gap-2 border-b border-border px-3 py-2.5">
        <span className="flex items-center gap-1.5 text-[13px] font-medium text-ink">
          <Layers size={13} className="text-ink-faint" />
          {direction.name}
        </span>
        <span className="ml-auto flex items-center gap-1.5 rounded-full bg-raised px-2 py-0.5 text-[11px] text-ink-muted">
          <span className={cn("h-1.5 w-1.5 rounded-full", TOOL_DOT[direction.tool] ?? "bg-idle")} />
          {TOOL_LABEL[direction.tool] ?? direction.tool}
        </span>
      </div>

      <div className="flex items-center gap-1.5 px-3 py-1.5">
        <GitBranch size={11} className="shrink-0 text-ink-faint" />
        <span
          className="truncate font-mono text-[10px] text-ink-faint"
          title={direction.branch}
        >
          {direction.branch}
        </span>
      </div>

      {/* write repos — openable session slots */}
      <ul className="flex flex-col gap-0.5 px-1.5 pb-1.5">
        {writes.map((w) => {
          const repo = repos.find((r) => r.id === w.repo_id);
          const sess = Object.values(sessions).find(
            (s) => s.directionId === direction.id && s.repoId === w.repo_id,
          );
          return (
            <li key={w.id}>
              <button
                onClick={() => void openSession(direction.id, w.repo_id)}
                className="group flex w-full items-center gap-2 rounded-[var(--radius-md)] px-2 py-1.5 text-left transition-colors hover:bg-brand-ghost"
              >
                <span className="grid h-5 w-5 place-items-center rounded bg-raised">
                  <TerminalSquare size={12} className="text-brand" />
                </span>
                <span className="truncate text-[12px] text-ink">
                  {repo?.name ?? `repo ${w.repo_id}`}
                </span>
                <span className="rounded bg-bg px-1 py-px font-mono text-[9px] uppercase text-running">
                  write
                </span>
                <span className="ml-auto flex items-center">
                  {sess ? (
                    <StatusDot status={sess.status as SessionStatus} />
                  ) : (
                    <ArrowRight
                      size={13}
                      className="text-ink-faint opacity-0 transition-opacity group-hover:opacity-100"
                    />
                  )}
                </span>
              </button>
            </li>
          );
        })}
      </ul>

      {/* read repos — context, not yet openable (M5) */}
      {reads.length > 0 && (
        <div className="flex flex-wrap items-center gap-1 border-t border-border px-3 py-2">
          <Eye size={11} className="text-ink-faint" />
          {reads.map((r) => (
            <span
              key={r.id}
              className="rounded-full bg-bg px-1.5 py-0.5 text-[10px] text-ink-faint"
            >
              {r.name}
            </span>
          ))}
        </div>
      )}
    </motion.div>
  );
}

function EmptyBoard({ onAdd }: { onAdd: () => void }) {
  return (
    <div className="flex h-full flex-col items-center justify-center text-center">
      <div className="grid h-11 w-11 place-items-center rounded-[var(--radius-lg)] border border-border bg-surface">
        <Layers size={20} className="text-ink-faint" />
      </div>
      <h2 className="mt-3 text-[14px] font-semibold text-ink">No directions yet</h2>
      <p className="mt-1 max-w-xs text-[12px] leading-relaxed text-ink-faint">
        A direction is one parallel work line: a tool, the repos it writes, and
        an isolated worktree per repo. Split this thread into directions to run
        agents side by side.
      </p>
      <Button variant="primary" className="mt-4" onClick={onAdd}>
        <Plus size={14} />
        New direction
      </Button>
    </div>
  );
}
