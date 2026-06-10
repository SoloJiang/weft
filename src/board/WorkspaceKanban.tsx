import { useEffect, useState } from "react";
import { motion } from "motion/react";
import { useTranslation } from "react-i18next";
import { Bot, Layers, Plus, SquarePen, X } from "lucide-react";
import { useStore } from "../state/store";
import type { ThreadOverview } from "../lib/types";
import { Button } from "../components/ui/Button";
import { CreateThreadDialog, CreateWorkspaceDialog } from "../nav/dialogs";
import { cn } from "../lib/cn";

type Phase = "planning" | "working" | "needs" | "review" | "done";

const COLUMNS: { key: Phase; label: string; dot: string }[] = [
  { key: "planning", label: "wsboard.planning", dot: "bg-idle" },
  { key: "working", label: "thread.colRunning", dot: "bg-running" },
  { key: "needs", label: "thread.colNeeds", dot: "bg-waiting" },
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
  // (no dependency on in-memory sessions). Needs-you overlays everything;
  // planning = the thread is still being scoped (no tasks yet); any task not
  // yet through coding = working; only review-and-beyond remains = review.
  const phaseOf = (o: ThreadOverview): Phase => {
    const attention =
      needs.some((n) => o.direction_ids.includes(n.direction_id)) ||
      asks.some((a) => o.direction_ids.includes(Number(a.dir)));
    const failing = o.direction_ids.some((id) =>
      (checksByDirection[id] ?? []).some((rc) => rc.checks.some((c) => c.status === "fail")),
    );
    if (attention || failing) return "needs";
    if (o.direction_ids.length === 0) return "planning";
    if (o.statuses.every((s) => s === "done")) return "done";
    if (o.statuses.some((s) => s !== "done" && s !== "review")) return "working";
    return "review";
  };

  if (overview.length === 0) {
    return <EmptyBoard />;
  }

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
              <span className="tabular-nums text-waiting">x{h.count}</span>
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
              <div
                key={col.key}
                className="flex w-[292px] shrink-0 flex-col rounded-[var(--radius-lg)] border border-border bg-surface/35"
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
  const { sessions, needs, asks, checksByDirection } = useStore();
  const { t } = useTranslation();
  const live = Object.values(sessions).filter(
    (s) => s.status === "running" && o.direction_ids.includes(s.directionId),
  ).length;
  const done = o.statuses.filter((s) => s === "done").length;
  const attention =
    needs.filter((n) => o.direction_ids.includes(n.direction_id)).length +
    asks.filter((a) => o.direction_ids.includes(Number(a.dir))).length;
  const failing = o.direction_ids.filter((id) =>
    (checksByDirection[id] ?? []).some((rc) => rc.checks.some((c) => c.status === "fail")),
  ).length;
  const total = Math.max(o.direction_ids.length, 1);
  const donePct = Math.min(100, Math.round((done / total) * 100));

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
        <Layers
          size={14}
          className={cn("mt-0.5 shrink-0", attention > 0 ? "text-waiting" : "text-ink-faint")}
        />
        <span className="min-w-0 flex-1 text-[13px] font-semibold leading-snug text-ink">
          {o.title}
        </span>
        {attention > 0 && (
          <span className="grid h-5 min-w-5 shrink-0 place-items-center rounded-full bg-waiting text-[10px] font-semibold tabular-nums text-bg">
            {attention}
          </span>
        )}
      </div>

      <div className="flex items-center gap-2">
        <span className="shrink-0 rounded-full border border-border bg-bg px-1.5 py-0.5 text-[10.5px] text-ink-faint">
          {t(`kind.${o.kind}`, o.kind)}
        </span>
        <span className="flex items-center gap-1 text-[11px] text-ink-faint">
          <Bot size={11} className="text-brand" />
          {t("workspace.aTask")}
        </span>
        <span className="ml-auto font-mono text-[11px] tabular-nums text-ink-faint">
          {done}/{o.direction_ids.length || 0}
        </span>
      </div>

      {o.write_repos.length > 0 && (
        <div className="flex flex-wrap items-center gap-1.5 pt-0.5">
          {o.write_repos.map((r) => (
            <span
              key={r.id}
              className="rounded-full border border-border bg-bg px-1.5 py-0.5 font-mono text-[10.5px] text-ink-muted"
            >
              {r.name}
            </span>
          ))}
        </div>
      )}

      <div className="h-1 overflow-hidden rounded-full bg-bg">
        <span
          className={cn(
            "block h-full rounded-full",
            attention > 0 ? "bg-waiting" : failing > 0 ? "bg-danger" : "bg-brand",
          )}
          style={{ width: `${donePct}%` }}
        />
      </div>

      <div className="flex items-center gap-2 text-[11px] text-ink-faint">
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
        {attention === 0 && live === 0 && failing === 0 && (
          <span className="ml-auto flex items-center gap-1 text-ink-faint">
            <span className="h-1.5 w-1.5 rounded-full bg-brand" />
            {t("workspace.auto")}
          </span>
        )}
      </div>
    </motion.button>
  );
}
