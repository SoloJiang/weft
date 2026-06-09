import { useEffect } from "react";
import { motion } from "motion/react";
import { useTranslation } from "react-i18next";
import { Layers, X } from "lucide-react";
import { useStore } from "../state/store";
import type { ThreadOverview } from "../lib/types";
import { cn } from "../lib/cn";

/** A thread's phase, derived from its tasks + live state (the workspace board). */
type Phase = "planning" | "working" | "needs" | "review" | "done";

const COLUMNS: { key: Phase; label: string; dot: string }[] = [
  { key: "planning", label: "wsboard.planning", dot: "bg-idle" },
  { key: "working", label: "thread.colRunning", dot: "bg-running" },
  { key: "needs", label: "thread.colNeeds", dot: "bg-waiting" },
  { key: "review", label: "thread.colReview", dot: "bg-brand" },
  { key: "done", label: "thread.colDone", dot: "bg-accent" },
];

/**
 * The workspace-level board (two-level kanban, top level): every thread as a
 * card, grouped by phase — planning (no tasks) → in progress → needs you →
 * review → done. The portfolio at a glance; click a card to drop into its
 * task board.
 */
export function WorkspaceKanban() {
  const { overview, refreshOverview, sessions, needs, asks, checksByDirection, selectThread } =
    useStore();
  const { t } = useTranslation();

  useEffect(() => {
    void refreshOverview();
  }, [refreshOverview]);

  const phaseOf = (o: ThreadOverview): Phase => {
    // Direction-level only, so the card matches the board's Needs-you column
    // (a thread-level lead ask, dir="", lives in Activity, not a task column).
    const attention =
      needs.some((n) => o.direction_ids.includes(n.direction_id)) ||
      asks.some((a) => o.direction_ids.includes(Number(a.dir)));
    const failing = o.direction_ids.some((id) =>
      (checksByDirection[id] ?? []).some((rc) => rc.checks.some((c) => c.status === "fail")),
    );
    if (attention || failing) return "needs";
    const live = Object.values(sessions).some(
      (s) =>
        (s.status === "running" || s.status === "starting") &&
        o.direction_ids.includes(s.directionId),
    );
    if (live) return "working";
    if (o.direction_ids.length === 0) return "planning";
    if (o.done >= o.direction_ids.length) return "done";
    return "review";
  };

  if (overview.length === 0) {
    return (
      <div className="flex flex-1 flex-col items-center justify-center px-6 text-center">
        <div className="grid h-11 w-11 place-items-center rounded-[var(--radius-lg)] border border-border bg-surface">
          <Layers size={20} className="text-ink-faint" />
        </div>
        <h2 className="mt-3 text-[14px] font-semibold text-ink">{t("workspace.emptyTitleHas")}</h2>
        <p className="mt-1.5 max-w-sm text-[12px] leading-relaxed text-ink-faint">
          {t("workspace.emptyBodyHas")}
        </p>
      </div>
    );
  }

  // Cross-thread contention: repos written by ≥2 issues — the workspace-level
  // "hot repo" view single-repo tools can't draw (§4.6). Computed from overview.
  const contention = new Map<string, { name: string; count: number }>();
  for (const o of overview) {
    for (const r of o.write_repos) {
      const e = contention.get(String(r.id)) ?? { name: r.name, count: 0 };
      e.count += 1;
      contention.set(String(r.id), e);
    }
  }
  const hot = [...contention.values()].filter((e) => e.count >= 2);

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      {hot.length > 0 && (
        <div className="flex flex-wrap items-center gap-2 border-b border-waiting/30 bg-waiting/10 px-5 py-2 text-[11.5px]">
          <span className="font-medium text-waiting">{t("workspace.contendedRepos")}</span>
          {hot.map((h) => (
            <span
              key={h.name}
              className="flex items-center gap-1 rounded-full border border-waiting/30 bg-bg px-2 py-0.5 font-mono text-ink-muted"
            >
              {h.name}
              <span className="tabular-nums text-waiting">×{h.count}</span>
            </span>
          ))}
          <span className="text-ink-faint">{t("workspace.reconcile")}</span>
        </div>
      )}
      <div className="min-h-0 flex-1 overflow-auto">
      <div className="flex h-full min-w-fit gap-3 px-5 py-4">
        {COLUMNS.map((col) => {
          const cards = overview.filter((o) => phaseOf(o) === col.key);
          return (
            <div key={col.key} className="flex w-[280px] shrink-0 flex-col gap-2">
              <div className="flex items-center gap-2 px-1 text-[11px] font-semibold uppercase tracking-wider text-ink-faint">
                <span className={cn("h-1.5 w-1.5 rounded-full", col.dot)} />
                {t(col.label)}
                <span className="tabular-nums text-ink-faint/70">{cards.length}</span>
              </div>
              <div className="flex min-h-0 flex-1 flex-col gap-2 rounded-[var(--radius-lg)] bg-surface/40 p-2">
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

function ThreadCard({ o, onOpen }: { o: ThreadOverview; onOpen: () => void }) {
  const { sessions, needs, asks, checksByDirection } = useStore();
  const { t } = useTranslation();
  const live = Object.values(sessions).filter(
    (s) => s.status === "running" && o.direction_ids.includes(s.directionId),
  ).length;
  const attention =
    needs.filter((n) => o.direction_ids.includes(n.direction_id)).length +
    asks.filter((a) => o.direction_ids.includes(Number(a.dir))).length;
  const failing = o.direction_ids.filter((id) =>
    (checksByDirection[id] ?? []).some((rc) => rc.checks.some((c) => c.status === "fail")),
  ).length;

  return (
    <motion.button
      layout
      onClick={onOpen}
      className="flex flex-col gap-2.5 rounded-[var(--radius-lg)] border border-border bg-surface p-3 text-left transition-colors hover:border-border-strong hover:bg-raised"
    >
      <div className="flex items-start gap-2">
        <Layers size={14} className="mt-0.5 shrink-0 text-ink-faint" />
        <span className="min-w-0 flex-1 text-[13.5px] font-medium leading-snug text-ink">
          {o.title}
        </span>
        <span className="shrink-0 rounded bg-bg px-1.5 py-0.5 font-mono text-[10px] uppercase text-ink-faint">
          {t(`kind.${o.kind}`, o.kind)}
        </span>
      </div>

      {o.write_repos.length > 0 && (
        <div className="flex flex-wrap items-center gap-1.5">
          {o.write_repos.map((r) => (
            <span
              key={r.id}
              className="rounded-full bg-bg px-1.5 py-0.5 text-[10px] text-ink-faint"
            >
              {r.name}
            </span>
          ))}
        </div>
      )}

      <div className="flex items-center gap-3 text-[11px] text-ink-faint">
        <span>{t("workspace.directions", { count: o.direction_ids.length })}</span>
        {live > 0 && (
          <span className="flex items-center gap-1 text-running">
            <span className="weft-pulse h-1.5 w-1.5 rounded-full bg-running" />
            {t("workspace.live", { count: live })}
          </span>
        )}
        {failing > 0 && (
          <span className="flex items-center gap-1 text-danger">
            <X size={11} />
            {t("workspace.failing", { count: failing })}
          </span>
        )}
        {attention > 0 && (
          <span className="ml-auto rounded-full bg-waiting/15 px-1.5 py-0.5 font-medium text-waiting">
            {t("workspace.needsYouBadge", { count: attention })}
          </span>
        )}
      </div>
    </motion.button>
  );
}
