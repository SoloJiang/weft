import { useEffect, useState } from "react";
import { motion } from "motion/react";
import { useTranslation } from "react-i18next";
import { Layers, Plus, SquarePen, X } from "lucide-react";
import { threadLiveCounts, useStore } from "../state/store";
import type { ThreadOverview } from "../lib/types";
import { Button } from "../components/ui/Button";
import { CreateThreadDialog, CreateWorkspaceDialog } from "../nav/dialogs";
import { cn } from "../lib/cn";

type Phase = "planning" | "working" | "review" | "done";

function progressBarColor(attention: number, failing: number): string {
  if (attention > 0) return "bg-waiting";
  if (failing > 0) return "bg-danger";
  return "bg-brand";
}

const COLUMNS: { key: Phase; label: string; dot: string }[] = [
  { key: "planning", label: "wsboard.planning", dot: "bg-idle" },
  { key: "working", label: "thread.colRunning", dot: "bg-running" },
  { key: "review", label: "thread.colReview", dot: "bg-brand" },
  { key: "done", label: "thread.colDone", dot: "bg-accent" },
];

export function WorkspaceKanban() {
  const {
    overview,
    refreshOverview,
    needs,
    asks,
    checksByDirection,
    selectThread,
  } = useStore();
  const { t } = useTranslation();

  useEffect(() => {
    void refreshOverview();
  }, [refreshOverview]);

  // Phase from the stored direction statuses — deterministic across restarts
  // (no dependency on in-memory sessions). Needs-you is a tag on the card, not
  // a stage: an open ask never moves a card out of its lifecycle column.
  // planning = the thread is still being scoped (no tasks yet); any task not
  // yet through coding = working; only review-and-beyond remains = review.
  const phaseOf = (o: ThreadOverview): Phase => {
    if (o.direction_ids.length === 0) return "planning";
    if (o.statuses.every((s) => s === "done")) return "done";
    if (o.statuses.some((s) => s !== "done" && s !== "review")) return "working";
    return "review";
  };

  // Cards waiting on the human (or with a failing check) bubble to the top of
  // their column — the attention signal without hijacking the stage.
  const urgent = (o: ThreadOverview): boolean =>
    needs.some((n) => o.direction_ids.includes(n.direction_id)) ||
    asks.some((a) => o.direction_ids.includes(Number(a.dir))) ||
    o.direction_ids.some((id) =>
      (checksByDirection[id] ?? []).some((rc) => rc.checks.some((c) => c.status === "fail")),
    );

  if (overview.length === 0) {
    return <EmptyBoard />;
  }

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <div className="min-h-0 flex-1 overflow-auto">
        <div className="flex h-full min-w-0 gap-3 px-5 py-4">
          {COLUMNS.map((col) => {
            const cards = overview
              .filter((o) => phaseOf(o) === col.key)
              .sort((a, b) => Number(urgent(b)) - Number(urgent(a)));
            return (
              <div
                key={col.key}
                className="flex min-w-[260px] max-w-[360px] flex-1 flex-col rounded-[var(--radius-lg)] border border-border bg-surface/35"
              >
                <div className="flex items-center gap-2 border-b border-border px-3 py-2.5">
                  <span
                    className={cn(
                      "h-1.5 w-1.5 rounded-full",
                      col.dot,
                      col.key === "working" && "weft-pulse",
                    )}
                  />
                  <span className="text-[11.5px] font-semibold text-ink-muted">
                    {t(col.label)}
                  </span>
                  <span className="ml-auto font-mono text-[11px] tabular-nums text-ink-faint">
                    {cards.length}
                  </span>
                </div>
                <div className="flex min-h-0 flex-1 flex-col gap-2 p-2">
                  {cards.map((o) => (
                    <ThreadCard
                      key={o.thread_id}
                      o={o}
                      onOpen={() => void selectThread(o.thread_id)}
                    />
                  ))}
                  {cards.length === 0 && (
                    <div className="flex flex-1 items-center justify-center py-6 text-[11px] text-ink-faint/60">
                      {t("thread.colEmpty")}
                    </div>
                  )}
                </div>
              </div>
            );
          })}
        </div>
      </div>
    </div>
  );
}

function EmptyBoard() {
  const { activeWorkspaceId } = useStore();
  const { t } = useTranslation();
  const [dlg, setDlg] = useState<null | "ws" | "thread">(null);
  const hasWs = activeWorkspaceId != null;

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <div className="flex flex-1 flex-col items-center justify-center px-6 text-center">
        <div className="grid h-11 w-11 place-items-center rounded-[var(--radius-lg)] border border-border bg-surface">
          <Layers size={20} className="text-brand" />
        </div>
        <h2 className="mt-3 text-[14px] font-semibold text-ink">
          {hasWs ? t("workspace.emptyTitleHas") : t("workspace.emptyTitleNoWs")}
        </h2>
        <p className="mt-1.5 max-w-sm text-[12px] leading-relaxed text-ink-faint">
          {hasWs ? t("workspace.emptyBodyHas") : t("workspace.emptyBodyNoWs")}
        </p>
        <Button
          variant="primary"
          className="mt-4"
          onClick={() => setDlg(hasWs ? "thread" : "ws")}
        >
          {hasWs ? <SquarePen size={14} /> : <Plus size={14} />}
          {hasWs ? t("nav.newThread") : t("nav.newWorkspace")}
        </Button>

        <CreateWorkspaceDialog open={dlg === "ws"} onOpenChange={(o) => !o && setDlg(null)} />
        <CreateThreadDialog open={dlg === "thread"} onOpenChange={(o) => !o && setDlg(null)} />
      </div>
    </div>
  );
}

function ThreadCard({ o, onOpen }: { o: ThreadOverview; onOpen: () => void }) {
  const { sessions, needs, asks, checksByDirection, openNeeds, leadTurn } = useStore();
  const { t } = useTranslation();
  // Split the in-flight count so a stalled worker OR lead is visible on the card
  // itself (not just the drill-in board): running = green pulse, stalled = amber.
  const { running, stalled } = threadLiveCounts(
    sessions,
    o.direction_ids,
    leadTurn[o.thread_id]?.state,
  );
  const done = o.statuses.filter((s) => s === "done").length;
  // Attention includes THREAD-level needs/asks — a stalled or blocked lead posts
  // with direction_id -1 / dir "lead", so match by thread_id, not only per-direction.
  const attention =
    needs.filter(
      (n) => o.direction_ids.includes(n.direction_id) || n.thread_id === o.thread_id,
    ).length +
    asks.filter(
      (a) => o.direction_ids.includes(Number(a.dir)) || a.thread === o.thread_id,
    ).length;
  const failing = o.direction_ids.filter((id) =>
    (checksByDirection[id] ?? []).some((rc) => rc.checks.some((c) => c.status === "fail")),
  ).length;
  const total = Math.max(o.direction_ids.length, 1);
  const donePct = Math.min(100, Math.round((done / total) * 100));
  const progressColor = progressBarColor(attention, failing);

  return (
    <motion.button
      layout
      onClick={onOpen}
      className={cn(
        "group flex flex-col gap-2.5 rounded-[var(--radius-lg)] border bg-surface p-3 text-left transition-colors hover:border-border-strong hover:bg-raised",
        attention > 0 ? "border-waiting/45" : "border-border",
      )}
    >
      <div className="flex items-start gap-2">
        <span className="min-w-0 flex-1 text-[13px] font-semibold leading-snug text-ink">
          {o.title}
        </span>
        {attention > 0 && (
          <span
            title={t("needs.title")}
            onClick={(e) => {
              e.stopPropagation();
              openNeeds();
            }}
            className="grid h-5 min-w-5 shrink-0 cursor-pointer place-items-center rounded-full bg-waiting text-[10px] font-semibold tabular-nums text-bg transition-opacity hover:opacity-80"
          >
            {attention}
          </span>
        )}
      </div>

      <div className="flex flex-wrap items-center gap-1.5">
        <span className="shrink-0 rounded-full border border-border bg-bg px-1.5 py-0.5 text-[10.5px] text-ink-faint">
          {t(`kind.${o.kind}`, o.kind)}
        </span>
        {o.write_repos.slice(0, 3).map((r) => (
          <span
            key={r.id}
            className="rounded-full border border-border bg-bg px-1.5 py-0.5 font-mono text-[10.5px] text-ink-muted"
          >
            {r.name}
          </span>
        ))}
        {o.write_repos.length > 3 && (
          <span className="rounded-full border border-border bg-bg px-1.5 py-0.5 font-mono text-[10.5px] text-ink-faint">
            +{o.write_repos.length - 3}
          </span>
        )}
      </div>

      {(o.direction_ids.length > 0 || running > 0 || stalled > 0) && (
        <div className="flex items-center gap-2">
          {o.direction_ids.length > 0 && (
            <>
              <div className="h-1 min-w-0 flex-1 overflow-hidden rounded-full bg-bg">
                <span
                  className={cn("block h-full rounded-full", progressColor)}
                  style={{ width: `${donePct}%` }}
                />
              </div>
              <span className="font-mono text-[11px] tabular-nums text-ink-faint">
                {done}/{o.direction_ids.length}
              </span>
            </>
          )}
          {running > 0 && (
            <span
              title={t("workspace.live", { count: running })}
              className="flex items-center gap-1 text-[11px] tabular-nums text-running"
            >
              <span className="weft-pulse h-1.5 w-1.5 rounded-full bg-running" />
              {running}
            </span>
          )}
          {stalled > 0 && (
            <span
              title={t("workspace.stalled", { count: stalled })}
              className="flex items-center gap-1 text-[11px] tabular-nums text-waiting"
            >
              <span className="h-1.5 w-1.5 rounded-full bg-waiting" />
              {stalled}
            </span>
          )}
          {failing > 0 && (
            <span
              title={t("workspace.failing", { count: failing })}
              className="flex items-center gap-1 text-[11px] tabular-nums text-danger"
            >
              <X size={11} />
              {failing}
            </span>
          )}
        </div>
      )}
    </motion.button>
  );
}
