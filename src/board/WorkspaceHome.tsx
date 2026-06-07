import { useEffect, useState } from "react";
import { AnimatePresence, motion } from "motion/react";
import { useTranslation } from "react-i18next";
import { Activity, FolderGit2, LayoutGrid, Plus } from "lucide-react";
import { useStore, type OpenSession } from "../state/store";
import { api } from "../lib/api";
import type { NormEvent } from "../lib/types";
import { Button } from "../components/ui/Button";
import { ToolIcon } from "../components/ToolIcon";
import { RepoMapView } from "./RepoMapView";
import { WorkspaceKanban } from "./WorkspaceKanban";
import { RailToggle } from "../components/RailToggle";
import { PermissionRow, AskRow } from "./NeedsYouView";
import { AddRepoDialog, CreateThreadDialog } from "../nav/dialogs";
import { cn } from "../lib/cn";

/**
 * The workspace home (no thread open). A tabbed surface — Overview (live agent
 * activity across all threads) and Repos (the dependency map) — replacing the
 * old thread-card grid that just mirrored the rail. Threads live only in the
 * rail now; this answers "what's happening right now".
 */
export function WorkspaceHome() {
  const { workspaces, activeWorkspaceId, repos, needs, asks, homeTab, setHomeTab } =
    useStore();
  const { t } = useTranslation();
  const [newThread, setNewThread] = useState(false);
  const [addRepo, setAddRepo] = useState(false);
  const ws = workspaces.find((w) => w.id === activeWorkspaceId);
  const needsCount = needs.length + asks.length;

  const tabs = [
    {
      key: "board" as const,
      label: t("thread.tabBoard"),
      icon: LayoutGrid,
      badge: null as { n: number; attn: boolean } | null,
    },
    {
      key: "overview" as const,
      label: t("workspace.tabOverview"),
      icon: Activity,
      badge: needsCount > 0 ? { n: needsCount, attn: true } : null,
    },
    {
      key: "repos" as const,
      label: t("workspace.tabRepos"),
      icon: FolderGit2,
      badge: repos.length > 0 ? { n: repos.length, attn: false } : null,
    },
  ];

  return (
    <section className="flex min-w-0 flex-1 flex-col overflow-hidden bg-bg">
      <header className="flex items-center gap-4 border-b border-border px-5 py-2.5">
        <RailToggle />
        <h1 className="shrink-0 text-[15px] font-semibold tracking-tight text-ink">
          {ws?.name ?? "Workspace"}
        </h1>
        <div className="flex shrink-0 items-center gap-1">
          {tabs.map((tab) => {
            const active = homeTab === tab.key;
            return (
              <button
                key={tab.key}
                onClick={() => setHomeTab(tab.key)}
                className={cn(
                  "relative flex items-center gap-1.5 whitespace-nowrap rounded-[var(--radius-md)] px-2.5 py-1.5 text-[12.5px] transition-colors",
                  active ? "text-ink" : "text-ink-faint hover:text-ink-muted",
                )}
              >
                <tab.icon size={13} className={active ? "text-brand" : ""} />
                {tab.label}
                {tab.badge && (
                  <span
                    className={cn(
                      "rounded-full px-1.5 text-[10px] font-semibold tabular-nums",
                      tab.badge.attn
                        ? "bg-waiting/20 text-waiting"
                        : "bg-raised text-ink-faint",
                    )}
                  >
                    {tab.badge.n}
                  </span>
                )}
                {active && (
                  <motion.span
                    layoutId="home-tab"
                    className="absolute inset-x-1.5 -bottom-[9px] h-[2px] rounded-full bg-brand"
                  />
                )}
              </button>
            );
          })}
        </div>
        <div className="ml-auto flex shrink-0 items-center gap-2">
          <Button variant="default" onClick={() => setAddRepo(true)} disabled={!ws}>
            <Plus size={14} />
            {t("dialog.addRepo")}
          </Button>
          <Button variant="primary" onClick={() => setNewThread(true)} disabled={!ws}>
            <Plus size={14} />
            {t("nav.newThread")}
          </Button>
        </div>
      </header>

      {homeTab === "board" ? (
        <WorkspaceKanban />
      ) : homeTab === "overview" ? (
        <OverviewTab />
      ) : (
        <RepoMapView embedded />
      )}

      <CreateThreadDialog open={newThread} onOpenChange={setNewThread} />
      <AddRepoDialog open={addRepo} onOpenChange={setAddRepo} />
    </section>
  );
}

function OverviewTab() {
  const { sessions, directionsByThread, loadThreadChildren, needs, asks } = useStore();
  const { t } = useTranslation();
  const running = Object.values(sessions).filter(
    (s) => s.status === "running" || s.status === "starting",
  );
  const needsCount = needs.length + asks.length;

  // Ensure task names are available for any thread with a live agent.
  const threadKey = [...new Set(running.map((s) => s.threadId))].sort().join(",");
  useEffect(() => {
    for (const tid of new Set(running.map((s) => s.threadId))) {
      if (tid > 0 && !directionsByThread[tid]) void loadThreadChildren(tid);
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [threadKey]);

  if (running.length === 0 && needsCount === 0) {
    return (
      <div className="flex flex-1 flex-col items-center justify-center px-6 text-center">
        <div className="grid h-11 w-11 place-items-center rounded-[var(--radius-lg)] border border-border bg-surface">
          <Activity size={20} className="text-ink-faint" />
        </div>
        <h2 className="mt-3 text-[14px] font-semibold text-ink">
          {t("workspace.noActivityTitle")}
        </h2>
        <p className="mt-1.5 max-w-sm text-[12px] leading-relaxed text-ink-faint">
          {t("workspace.noActivityBody")}
        </p>
      </div>
    );
  }

  return (
    <div className="min-h-0 flex-1 overflow-y-auto">
      <div className="mx-auto flex w-full max-w-[820px] flex-col gap-5 px-5 py-5">
        {needsCount > 0 && (
          <div className="flex flex-col gap-1.5">
            <SectionLabel dot="bg-waiting" label={t("needs.title")} count={needsCount} />
            <AnimatePresence initial={false}>
              {asks.map((ask) => (
                <motion.div
                  key={`ask-${ask.id}`}
                  layout
                  initial={{ opacity: 0, y: 6 }}
                  animate={{ opacity: 1, y: 0 }}
                  exit={{ opacity: 0, scale: 0.98 }}
                  transition={{ duration: 0.18 }}
                >
                  <PermissionRow ask={ask} />
                </motion.div>
              ))}
              {needs.map((item) => (
                <motion.div
                  key={`need-${item.ask_id}`}
                  layout
                  initial={{ opacity: 0, y: 6 }}
                  animate={{ opacity: 1, y: 0 }}
                  exit={{ opacity: 0, scale: 0.98 }}
                  transition={{ duration: 0.18 }}
                >
                  <AskRow item={item} />
                </motion.div>
              ))}
            </AnimatePresence>
          </div>
        )}

        {running.length > 0 && (
          <div className="flex flex-col gap-1.5">
            <SectionLabel dot="bg-running" label={t("workspace.runningNow")} count={running.length} pulse />
            {running.map((s) => (
              <RunningRow key={s.info.session_id} s={s} />
            ))}
          </div>
        )}
      </div>
    </div>
  );
}

function SectionLabel({
  dot,
  label,
  count,
  pulse,
}: {
  dot: string;
  label: string;
  count: number;
  pulse?: boolean;
}) {
  return (
    <div className="mb-1 flex items-center gap-2 px-1 text-[11px] font-semibold uppercase tracking-wider text-ink-faint">
      <span className={cn("h-1.5 w-1.5 rounded-full", dot, pulse && "weft-pulse")} />
      {label}
      <span className="tabular-nums text-ink-faint/70">{count}</span>
    </div>
  );
}

/** Last transcript event rendered as a one-line "what it's doing". */
function actionText(e: NormEvent | null): string {
  if (!e) return "";
  if (e.kind === "tool") return e.summary ? `${e.name} · ${e.summary}` : e.name;
  return e.text.replace(/\s+/g, " ").slice(0, 120);
}

function RunningRow({ s }: { s: OpenSession }) {
  const { threads, directionsByThread, focusSession } = useStore();
  const { t } = useTranslation();
  const [last, setLast] = useState<NormEvent | null>(null);
  const [stat, setStat] = useState<{ files: number; added: number; removed: number } | null>(null);

  const threadTitle = threads.find((th) => th.id === s.threadId)?.title ?? "";
  const taskName =
    s.kind === "lead"
      ? t("lead.title")
      : directionsByThread[s.threadId]?.find((d) => d.id === s.directionId)?.name ??
        t("workspace.aTask");

  useEffect(() => {
    let alive = true;
    const tick = async () => {
      try {
        const ev = await api.readTranscript(s.info.worktree, s.info.tool);
        if (alive) setLast(ev.length ? ev[ev.length - 1] : null);
      } catch {
        /* ignore */
      }
      if (s.kind === "worker") {
        try {
          const d = await api.worktreeDiff(s.info.worktree);
          if (alive) {
            const added = d.files.reduce((a, f) => a + f.added, 0);
            const removed = d.files.reduce((a, f) => a + f.removed, 0);
            setStat({ files: d.files.length, added, removed });
          }
        } catch {
          /* ignore */
        }
      }
    };
    void tick();
    const h = setInterval(tick, 3000);
    return () => {
      alive = false;
      clearInterval(h);
    };
  }, [s.info.worktree, s.info.tool, s.kind]);

  return (
    <motion.button
      layout
      onClick={() => focusSession(s.info.session_id)}
      className="flex items-center gap-3 rounded-[var(--radius-md)] border border-border bg-surface px-3 py-2.5 text-left transition-colors hover:border-border-strong hover:bg-raised"
    >
      <span className="grid h-7 w-7 shrink-0 place-items-center rounded-[var(--radius-sm)] bg-raised">
        <ToolIcon tool={s.info.tool} size={13} />
      </span>
      <div className="flex min-w-0 flex-1 flex-col">
        <div className="flex items-center gap-1.5 text-[12.5px]">
          <span className="truncate font-medium text-ink">{taskName}</span>
          {threadTitle && (
            <>
              <span className="text-ink-faint">·</span>
              <span className="truncate text-ink-faint">{threadTitle}</span>
            </>
          )}
        </div>
        <span className="truncate text-[11px] text-ink-faint">
          {actionText(last) || t("lead.working")}
        </span>
      </div>
      {stat && (stat.added > 0 || stat.removed > 0) && (
        <span className="shrink-0 tabular-nums text-[11px]">
          <span className="text-running">+{stat.added}</span>{" "}
          <span className="text-danger">−{stat.removed}</span>
        </span>
      )}
      <span className="weft-pulse h-1.5 w-1.5 shrink-0 rounded-full bg-running" />
    </motion.button>
  );
}
