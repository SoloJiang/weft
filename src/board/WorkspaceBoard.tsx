import { useEffect, useMemo, useState } from "react";
import { motion } from "motion/react";
import { Flame, Layers, Plus, X } from "lucide-react";
import { useStore } from "../state/store";
import type { ThreadOverview } from "../lib/types";
import { Button } from "../components/ui/Button";
import { CreateThreadDialog } from "../nav/dialogs";
import { cn } from "../lib/cn";

const KIND_LABEL: Record<string, string> = {
  feature: "Feature",
  bugfix: "Bugfix",
  refactor: "Refactor",
  spike: "Spike",
};

/**
 * The workspace board (ARCHITECTURE §4.6, §5.2): a portfolio of every thread in
 * flight, with cross-thread "hot repo" contention — repos written by 2+ threads,
 * the overlap that git will eventually make you reconcile. The top level of the
 * two-level kanban; click a thread to drop into its direction board.
 */
export function WorkspaceBoard() {
  const { workspaces, activeWorkspaceId, overview, refreshOverview, selectThread } =
    useStore();
  const [newThread, setNewThread] = useState(false);
  const ws = workspaces.find((w) => w.id === activeWorkspaceId);

  useEffect(() => {
    void refreshOverview();
  }, [refreshOverview]);

  // hot repos: written by 2+ threads.
  const hot = useMemo(() => {
    const count = new Map<number, { name: string; threads: number }>();
    for (const t of overview) {
      for (const r of t.write_repos) {
        const cur = count.get(r.id) ?? { name: r.name, threads: 0 };
        cur.threads += 1;
        count.set(r.id, cur);
      }
    }
    return new Map(
      [...count.entries()].filter(([, v]) => v.threads >= 2),
    );
  }, [overview]);

  return (
    <section className="flex min-w-0 flex-1 flex-col overflow-hidden bg-bg">
      <header className="flex items-center gap-3 border-b border-border px-5 py-3">
        <div className="flex min-w-0 flex-col">
          <h1 className="truncate text-[16px] font-semibold tracking-tight text-ink">
            {ws?.name ?? "Workspace"}
          </h1>
          <span className="mt-0.5 text-[12px] text-ink-faint">
            {overview.length} {overview.length === 1 ? "thread" : "threads"} in
            flight · click one to open its directions
          </span>
        </div>
        <Button variant="primary" className="ml-auto" onClick={() => setNewThread(true)} disabled={!ws}>
          <Plus size={14} />
          New thread
        </Button>
      </header>

      <div className="min-h-0 flex-1 overflow-y-auto px-5 py-4">
        {hot.size > 0 && (
          <div className="mb-4 flex flex-wrap items-center gap-2 rounded-[var(--radius-md)] border border-approval/40 bg-approval/10 px-3 py-2 text-[12px]">
            <Flame size={14} className="shrink-0 text-approval" />
            <span className="text-ink-muted">Contended repos:</span>
            {[...hot.values()].map((v) => (
              <span
                key={v.name}
                className="rounded-full bg-approval/15 px-2 py-0.5 font-medium text-approval"
              >
                {v.name} · {v.threads} threads
              </span>
            ))}
            <span className="text-ink-faint">— these branches will need reconciling.</span>
          </div>
        )}

        {overview.length === 0 ? (
          <EmptyWorkspace onAdd={() => setNewThread(true)} hasWs={!!ws} />
        ) : (
          <div className="grid grid-cols-[repeat(auto-fill,minmax(300px,1fr))] gap-3">
            {overview.map((t) => (
              <ThreadCard
                key={t.thread_id}
                t={t}
                hotIds={hot}
                onOpen={() => void selectThread(t.thread_id)}
              />
            ))}
          </div>
        )}
      </div>

      <CreateThreadDialog open={newThread} onOpenChange={setNewThread} />
    </section>
  );
}

function ThreadCard({
  t,
  hotIds,
  onOpen,
}: {
  t: ThreadOverview;
  hotIds: Map<number, unknown>;
  onOpen: () => void;
}) {
  const { sessions, needs, asks, checksByDirection } = useStore();
  const live = Object.values(sessions).filter(
    (s) => s.status === "running" && t.direction_ids.includes(s.directionId),
  ).length;
  const attention =
    needs.filter((n) => n.thread_id === t.thread_id).length +
    asks.filter((a) => a.thread === t.thread_id).length;
  const failing = t.direction_ids.filter((id) =>
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
          {t.title}
        </span>
        <span className="shrink-0 rounded bg-bg px-1.5 py-0.5 font-mono text-[10px] uppercase text-ink-faint">
          {KIND_LABEL[t.kind] ?? t.kind}
        </span>
      </div>

      <div className="flex flex-wrap items-center gap-1.5">
        {t.write_repos.length === 0 ? (
          <span className="text-[11px] text-ink-faint">no scope yet</span>
        ) : (
          t.write_repos.map((r) => {
            const isHot = hotIds.has(r.id);
            return (
              <span
                key={r.id}
                title={isHot ? "Written by another thread too" : undefined}
                className={cn(
                  "flex items-center gap-1 rounded-full px-1.5 py-0.5 text-[10px]",
                  isHot
                    ? "bg-approval/15 text-approval"
                    : "bg-bg text-ink-faint",
                )}
              >
                {isHot && <Flame size={9} />}
                {r.name}
              </span>
            );
          })
        )}
      </div>

      <div className="flex items-center gap-3 text-[11px] text-ink-faint">
        <span>
          {t.direction_ids.length}{" "}
          {t.direction_ids.length === 1 ? "direction" : "directions"}
        </span>
        {live > 0 && (
          <span className="flex items-center gap-1 text-running">
            <span className="weft-pulse h-1.5 w-1.5 rounded-full bg-running" />
            {live} live
          </span>
        )}
        {failing > 0 && (
          <span className="flex items-center gap-1 text-danger">
            <X size={11} />
            {failing} failing
          </span>
        )}
        {attention > 0 && (
          <span className="ml-auto rounded-full bg-waiting/15 px-1.5 py-0.5 font-medium text-waiting">
            {attention} needs you
          </span>
        )}
      </div>
    </motion.button>
  );
}

function EmptyWorkspace({ onAdd, hasWs }: { onAdd: () => void; hasWs: boolean }) {
  return (
    <div className="flex h-full flex-col items-center justify-center text-center">
      <div className="grid h-12 w-12 place-items-center rounded-[var(--radius-lg)] border border-border bg-surface">
        <Layers size={22} className="text-ink-faint" />
      </div>
      <h2 className="mt-4 text-[15px] font-semibold text-ink">
        {hasWs ? "No threads yet" : "No workspace"}
      </h2>
      <p className="mt-1.5 max-w-sm text-[13px] leading-relaxed text-ink-faint">
        {hasWs
          ? "A thread is one work line — a task you split across repos. Start one and weft plans the scope, dispatches agents, and surfaces what needs you."
          : "Create a workspace to begin."}
      </p>
      {hasWs && (
        <Button variant="primary" className="mt-4" onClick={onAdd}>
          <Plus size={14} />
          New thread
        </Button>
      )}
    </div>
  );
}
