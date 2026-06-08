import { useEffect, useState } from "react";
import { motion } from "motion/react";
import { useTranslation } from "react-i18next";
import {
  Check,
  Layers,
  LayoutGrid,
  MessagesSquare,
  Plus,
  Radio,
  TerminalSquare,
  X,
} from "lucide-react";
import { useStore } from "../state/store";
import type { Direction, RepoChecks, SessionStatus } from "../lib/types";
import { Button } from "../components/ui/Button";
import { StatusDot } from "../components/ui/StatusChip";
import { Inspect } from "../components/Inspect";
import { ToolIcon } from "../components/ToolIcon";
import { CreateDirectionDialog } from "../nav/dialogs";
import { RailToggle } from "../components/RailToggle";
import { BusDrawer } from "./BusDrawer";
import { LeadTab } from "../session/LeadTab";
import { cn } from "../lib/cn";

const TOOL_LABEL: Record<string, string> = {
  claude: "Claude",
  codex: "Codex",
  opencode: "OpenCode",
};

/** Task lifecycle column. "needs" is a weft overlay (an open ask / failing
 *  check); the rest are the agent/human-set stored status (§4.6). */
type TaskState = "queued" | "working" | "needs" | "review" | "done";

const COLUMNS: { key: TaskState; label: string; dot: string }[] = [
  { key: "queued", label: "thread.colQueued", dot: "bg-idle" },
  { key: "working", label: "thread.colRunning", dot: "bg-running" },
  { key: "needs", label: "thread.colNeeds", dot: "bg-waiting" },
  { key: "review", label: "thread.colReview", dot: "bg-brand" },
  { key: "done", label: "thread.colDone", dot: "bg-accent" },
];

export function ThreadBoard() {
  const {
    threads,
    activeThreadId,
    directionsByThread,
    proposal,
    reviewingProposal,
    setReviewingProposal,
    leadSession,
    messages,
    showBus,
    setShowBus,
    needs,
    asks,
    checksByDirection,
    setTaskStatus,
  } = useStore();
  const { t } = useTranslation();
  const thread = threads.find((th) => th.id === activeThreadId);
  const [newDir, setNewDir] = useState(false);
  const [tab, setTab] = useState<"board" | "lead">("board");
  // drag-to-restatus a task between columns
  const [dragId, setDragId] = useState<number | null>(null);
  const [overCol, setOverCol] = useState<TaskState | null>(null);
  useEffect(() => {
    setTab("board");
    setReviewingProposal(false);
  }, [activeThreadId, setReviewingProposal]);
  if (!thread) return null;
  const dirs = directionsByThread[thread.id] ?? [];
  const reviewing = reviewingProposal && proposal?.status === "proposed";
  const proposalPending =
    proposal?.status === "proposed" && proposal.directions.length > 0 && !reviewingProposal;
  const leadRunning =
    leadSession?.status === "running" || leadSession?.status === "starting";

  // Column = the stored, agent/human-set status; an open ask/need or a failing
  // check overlays the task into Needs-you (the exception lane weft owns).
  const statusOf = (d: Direction): TaskState => {
    const need =
      needs.some((n) => n.direction_id === d.id) ||
      asks.some((a) => a.dir === String(d.id));
    const failing = (checksByDirection[d.id] ?? []).some((rc) =>
      rc.checks.some((c) => c.status === "fail"),
    );
    if (need || failing) return "needs";
    const s = d.status;
    if (s === "working" || s === "review" || s === "done") return s;
    return "queued";
  };

  const TABS = [
    { key: "board" as const, label: t("thread.tabBoard"), icon: LayoutGrid, dot: null as string | null },
    {
      key: "lead" as const,
      label: t("lead.title"),
      icon: MessagesSquare,
      dot: proposalPending ? "bg-accent" : leadRunning ? "bg-running" : null,
    },
  ];

  return (
    <section className="flex min-w-0 flex-1 flex-col overflow-hidden bg-bg">
      <header className="flex items-center gap-3 border-b border-border px-5 py-2.5">
        <RailToggle />
        <div className="flex shrink-0 items-center gap-1">
          {TABS.map((tb) => {
            const active = tab === tb.key;
            return (
              <button
                key={tb.key}
                onClick={() => {
                  setTab(tb.key);
                  if (tb.key === "board") setReviewingProposal(false);
                }}
                className={cn(
                  "relative flex items-center gap-1.5 whitespace-nowrap rounded-[var(--radius-md)] px-2.5 py-1.5 text-[12.5px] transition-colors",
                  active ? "text-ink" : "text-ink-faint hover:text-ink-muted",
                )}
              >
                <tb.icon size={13} className={active ? "text-brand" : ""} />
                {tb.label}
                {tb.dot && (
                  <span className={cn("h-1.5 w-1.5 rounded-full", tb.dot, "animate-pulse")} />
                )}
                {active && (
                  <motion.span
                    layoutId="thread-tab"
                    className="absolute inset-x-1.5 -bottom-[9px] h-[2px] rounded-full bg-brand"
                  />
                )}
              </button>
            );
          })}
        </div>

        <div className="ml-auto flex shrink-0 items-center gap-2">
          <span className="rounded bg-surface px-1.5 py-0.5 font-mono text-[10px] uppercase text-ink-faint">
            {t(`kind.${thread.kind}`, thread.kind)}
          </span>
          <button
            onClick={() => setShowBus(!showBus)}
            className="flex items-center gap-1.5 whitespace-nowrap rounded-[var(--radius-md)] border border-border px-2.5 py-1.5 text-[12px] text-ink-muted transition-colors hover:bg-surface hover:text-ink"
          >
            <Radio size={13} className="text-brand" />
            {t("bus.activity")}
            {messages.length > 0 && (
              <span className="tabular-nums text-ink-faint">{messages.length}</span>
            )}
          </button>
          {tab === "board" && !reviewing && dirs.length > 0 && (
            <Button variant="primary" onClick={() => setNewDir(true)}>
              <Plus size={14} />
              {t("thread.newDirection")}
            </Button>
          )}
        </div>
      </header>

      <div className="flex min-h-0 flex-1 flex-col">
        {tab === "lead" ? (
          <LeadTab onReview={() => setTab("board")} />
        ) : dirs.length === 0 ? (
          <EmptyDiscuss onTalk={() => setTab("lead")} />
        ) : (
          <div className="min-h-0 flex-1 overflow-auto">
            <div className="flex h-full min-w-fit gap-3 px-5 py-4">
              {COLUMNS.map((col) => {
                const cards = dirs.filter((d) => statusOf(d) === col.key);
                // "needs" is weft-derived (open asks / failing checks), not a
                // status a human sets — so it isn't a drop target.
                const droppable = col.key !== "needs";
                const isOver = droppable && overCol === col.key && dragId != null;
                return (
                  <div key={col.key} className="flex w-[300px] shrink-0 flex-col gap-2">
                    <div className="flex items-center gap-2 px-1 text-[11px] font-semibold uppercase tracking-wider text-ink-faint">
                      <span className={cn("h-1.5 w-1.5 rounded-full", col.dot)} />
                      {t(col.label)}
                      <span className="tabular-nums text-ink-faint/70">{cards.length}</span>
                      {col.key === "queued" && (
                        <button
                          onClick={() => setNewDir(true)}
                          aria-label={t("thread.addDirection")}
                          className="ml-auto grid h-5 w-5 place-items-center rounded text-ink-faint transition-colors hover:bg-brand-ghost hover:text-ink"
                        >
                          <Plus size={13} />
                        </button>
                      )}
                    </div>
                    <div
                      onDragOver={(e) => {
                        if (!droppable || dragId == null) return;
                        e.preventDefault();
                        setOverCol(col.key);
                      }}
                      onDragLeave={() => setOverCol((c) => (c === col.key ? null : c))}
                      onDrop={(e) => {
                        e.preventDefault();
                        if (droppable && dragId != null) void setTaskStatus(dragId, col.key);
                        setDragId(null);
                        setOverCol(null);
                      }}
                      className={cn(
                        "flex min-h-0 flex-1 flex-col gap-2 rounded-[var(--radius-lg)] p-2 transition-colors",
                        isOver
                          ? "bg-brand-ghost ring-1 ring-inset ring-brand/40"
                          : "bg-surface/40",
                      )}
                    >
                      {cards.map((d) => (
                        <div
                          key={d.id}
                          draggable
                          onDragStart={(e) => {
                            setDragId(d.id);
                            e.dataTransfer.effectAllowed = "move";
                          }}
                          onDragEnd={() => {
                            setDragId(null);
                            setOverCol(null);
                          }}
                          className={cn("cursor-grab active:cursor-grabbing", dragId === d.id && "opacity-40")}
                        >
                          <DirectionCard direction={d} />
                        </div>
                      ))}
                      {cards.length === 0 && (
                        <div className="flex flex-1 items-center justify-center py-6 text-[11px] text-ink-faint/60">
                          {isOver ? t("thread.dropHere") : t("thread.colEmpty")}
                        </div>
                      )}
                    </div>
                  </div>
                );
              })}
            </div>
          </div>
        )}
      </div>

      <BusDrawer directions={dirs} />
      <CreateDirectionDialog open={newDir} onOpenChange={setNewDir} threadId={thread.id} />
    </section>
  );
}

function EmptyDiscuss({ onTalk }: { onTalk: () => void }) {
  const { t } = useTranslation();
  return (
    <div className="flex h-full flex-col items-center justify-center px-6 text-center">
      <div className="grid h-11 w-11 place-items-center rounded-[var(--radius-lg)] border border-border bg-surface">
        <Layers size={20} className="text-ink-faint" />
      </div>
      <h2 className="mt-3 text-[14px] font-semibold text-ink">{t("thread.discussTitle")}</h2>
      <p className="mt-1.5 max-w-sm text-[12px] leading-relaxed text-ink-faint">
        {t("thread.discussBody")}
      </p>
      <Button variant="primary" className="mt-4" onClick={onTalk}>
        <MessagesSquare size={14} />
        {t("lead.title")}
      </Button>
    </div>
  );
}

function DirectionCard({ direction }: { direction: Direction }) {
  const { worktreesByDirection, repos, sessions, openSession, checksByDirection } = useStore();
  const writes = worktreesByDirection[direction.id] ?? [];
  const checks = checksByDirection[direction.id];

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
          <ToolIcon tool={direction.tool} size={12} />
          {TOOL_LABEL[direction.tool] ?? direction.tool}
        </span>
      </div>

      {/* write repos — openable session slots. Each is an isolated working copy;
          the real path/branch lives in Inspect (§4.7), not on the card face. */}
      <ul className="flex flex-col gap-0.5 px-1.5 py-1.5">
        {writes.map((w) => {
          const repo = repos.find((r) => r.id === w.repo_id);
          const sess = Object.values(sessions).find(
            (s) => s.directionId === direction.id && s.repoId === w.repo_id,
          );
          return (
            <li
              key={w.id}
              className="group flex items-center gap-0.5 rounded-[var(--radius-md)] transition-colors hover:bg-brand-ghost"
            >
              <button
                onClick={() => void openSession(direction.id, w.repo_id)}
                className="flex min-w-0 flex-1 items-center gap-2 px-2 py-1.5 text-left"
              >
                <span className="grid h-5 w-5 place-items-center rounded bg-raised">
                  <TerminalSquare size={12} className="text-brand" />
                </span>
                <span className="truncate text-[12px] text-ink">
                  {repo?.name ?? `repo ${w.repo_id}`}
                </span>
                {sess && (
                  <span className="ml-auto flex items-center">
                    <StatusDot status={sess.status as SessionStatus} />
                  </span>
                )}
              </button>
              <Inspect
                path={w.path}
                branch={w.branch}
                nativeId={sess?.nativeId}
                tool={sess?.info.tool ?? direction.tool}
                size={13}
                className="mr-1 h-6 w-6 shrink-0 opacity-0 group-hover:opacity-100"
              />
            </li>
          );
        })}
      </ul>

      {checks && checks.length > 0 && (
        <div className="flex flex-col gap-1.5 border-t border-border px-3 py-2">
          {checks.map((rc) => (
            <ChecksRow key={rc.repo} rc={rc} />
          ))}
        </div>
      )}
    </motion.div>
  );
}

function ChecksRow({ rc }: { rc: RepoChecks }) {
  const { t } = useTranslation();
  if (rc.checks.length === 0) {
    return (
      <div className="flex items-center gap-2 text-[11px]">
        <span className="truncate text-ink-muted">{rc.repo}</span>
        <span className="text-ink-faint">{t("thread.noChecks")}</span>
      </div>
    );
  }
  return (
    <div className="flex flex-wrap items-center gap-1.5 text-[11px]">
      <span className="mr-0.5 truncate text-ink-muted">{rc.repo}</span>
      {rc.checks.map((c) => {
        const pass = c.status === "pass";
        return (
          <span
            key={c.name}
            title={pass ? `${c.name}: passed` : c.output_tail || `${c.name}: failed (exit ${c.code})`}
            className={cn(
              "flex items-center gap-1 rounded-full px-1.5 py-0.5 font-medium",
              pass ? "bg-running/15 text-running" : "bg-[oklch(0.64_0.2_25/0.15)] text-danger",
            )}
          >
            {pass ? <Check size={10} /> : <X size={10} />}
            {c.name}
          </span>
        );
      })}
    </div>
  );
}

