import { useEffect, useState } from "react";
import { motion } from "motion/react";
import { useTranslation } from "react-i18next";
import { Columns3, FolderGit2, Layers, Plus, Rows3, SquarePen, X } from "lucide-react";
import { useStore } from "../state/store";
import type { ThreadOverview } from "../lib/types";
import { Button } from "../components/ui/Button";
import { RailToggle } from "../components/RailToggle";
import { CreateThreadDialog, CreateWorkspaceDialog } from "../nav/dialogs";
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
  // Two lenses on the same portfolio: by phase (lifecycle columns) or by repo
  // (cross-issue swimlanes — the contention view single-repo tools can't draw).
  const [view, setView] = useState<"phase" | "repo">("phase");

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
    return <EmptyBoard />;
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
      <div className="flex items-center gap-2 border-b border-border px-5 py-2.5">
        <RailToggle />
        <div className="ml-auto flex items-center rounded-[var(--radius-md)] bg-surface p-0.5">
          <ViewToggle active={view === "phase"} onClick={() => setView("phase")} label={t("wsboard.byPhase")}>
            <Columns3 size={13} />
          </ViewToggle>
          <ViewToggle active={view === "repo"} onClick={() => setView("repo")} label={t("wsboard.byRepo")}>
            <Rows3 size={13} />
          </ViewToggle>
        </div>
      </div>
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
      {view === "repo" ? (
        <RepoSwimlanes overview={overview} onOpen={(id) => void selectThread(id)} />
      ) : (
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
      )}
    </div>
  );
}

function ViewToggle({
  active,
  onClick,
  label,
  children,
}: {
  active: boolean;
  onClick: () => void;
  label: string;
  children: React.ReactNode;
}) {
  return (
    <button
      onClick={onClick}
      title={label}
      aria-pressed={active}
      className={cn(
        "flex items-center gap-1.5 rounded-[var(--radius-sm)] px-2 py-1 text-[12px] transition-colors",
        active ? "bg-raised text-ink shadow-[0_1px_2px_rgba(0,0,0,0.3)]" : "text-ink-faint hover:text-ink-muted",
      )}
    >
      {children}
      {label}
    </button>
  );
}

/** Cross-issue swimlanes (M-D): one lane per repo, listing the issues that write
 *  it. Overlap = the cross-repo contention that single-repo tools can't show —
 *  the same signal as the hot-repo banner, laid out as the full map. */
function RepoSwimlanes({
  overview,
  onOpen,
}: {
  overview: ThreadOverview[];
  onOpen: (threadId: number) => void;
}) {
  const { t } = useTranslation();
  const lanes = new Map<number, { name: string; threads: ThreadOverview[] }>();
  const unscoped: ThreadOverview[] = [];
  for (const o of overview) {
    if (o.write_repos.length === 0) {
      unscoped.push(o);
      continue;
    }
    for (const r of o.write_repos) {
      const lane = lanes.get(r.id) ?? { name: r.name, threads: [] };
      lane.threads.push(o);
      lanes.set(r.id, lane);
    }
  }
  // Hot lanes (most issues) first — that's where reconciling will be needed.
  const ordered = [...lanes.entries()].sort((a, b) => b[1].threads.length - a[1].threads.length);

  return (
    <div className="min-h-0 flex-1 overflow-y-auto">
      <div className="mx-auto flex w-full max-w-[860px] flex-col gap-2.5 px-5 py-4">
        <p className="px-1 text-[11.5px] text-ink-faint">{t("wsboard.repoLanesHint")}</p>
        {ordered.map(([id, lane]) => {
          const hot = lane.threads.length >= 2;
          return (
            <div
              key={id}
              className={cn(
                "flex flex-col gap-2 rounded-[var(--radius-lg)] border bg-surface/40 p-3 sm:flex-row sm:items-start",
                hot ? "border-waiting/40" : "border-border",
              )}
            >
              <div className="flex w-40 shrink-0 items-center gap-1.5">
                <FolderGit2 size={13} className={hot ? "text-waiting" : "text-ink-faint"} />
                <span className="truncate font-mono text-[12px] text-ink">{lane.name}</span>
                <span
                  className={cn(
                    "ml-auto rounded-full px-1.5 py-0.5 text-[10px] font-semibold tabular-nums sm:ml-0",
                    hot ? "bg-waiting/15 text-waiting" : "text-ink-faint",
                  )}
                >
                  {lane.threads.length}
                </span>
              </div>
              <div className="flex min-w-0 flex-1 flex-wrap gap-1.5">
                {lane.threads.map((o) => (
                  <button
                    key={o.thread_id}
                    onClick={() => onOpen(o.thread_id)}
                    className="flex max-w-full items-center gap-1.5 rounded-[var(--radius-md)] border border-border bg-bg px-2 py-1 text-left text-[12px] text-ink-muted transition-colors hover:border-border-strong hover:text-ink"
                  >
                    <Layers size={11} className="shrink-0 text-ink-faint" />
                    <span className="truncate">{o.title}</span>
                  </button>
                ))}
              </div>
            </div>
          );
        })}
        {unscoped.length > 0 && (
          <div className="flex flex-col gap-2 rounded-[var(--radius-lg)] border border-dashed border-border bg-surface/20 p-3 sm:flex-row sm:items-start">
            <div className="flex w-40 shrink-0 items-center gap-1.5 text-ink-faint">
              <Layers size={13} />
              <span className="truncate text-[12px]">{t("wsboard.unscoped")}</span>
              <span className="ml-auto text-[10px] tabular-nums sm:ml-0">{unscoped.length}</span>
            </div>
            <div className="flex min-w-0 flex-1 flex-wrap gap-1.5">
              {unscoped.map((o) => (
                <button
                  key={o.thread_id}
                  onClick={() => onOpen(o.thread_id)}
                  className="flex max-w-full items-center gap-1.5 rounded-[var(--radius-md)] border border-border bg-bg px-2 py-1 text-left text-[12px] text-ink-muted transition-colors hover:border-border-strong hover:text-ink"
                >
                  <Layers size={11} className="shrink-0 text-ink-faint" />
                  <span className="truncate">{o.title}</span>
                </button>
              ))}
            </div>
          </div>
        )}
      </div>
    </div>
  );
}

/** First-run / empty board: distinguishes "no workspace" from "no issues yet"
 *  and gives the one CTA that moves you forward — not just a dead-end message. */
function EmptyBoard() {
  const { activeWorkspaceId } = useStore();
  const { t } = useTranslation();
  const [dlg, setDlg] = useState<null | "ws" | "thread">(null);
  const hasWs = activeWorkspaceId != null;

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <div className="flex items-center border-b border-transparent px-5 py-2.5">
        <RailToggle />
      </div>
      <div className="flex flex-1 flex-col items-center justify-center px-6 text-center">
        <div className="grid h-11 w-11 place-items-center rounded-[var(--radius-lg)] border border-border bg-surface">
          <Layers size={20} className="text-ink-faint" />
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
