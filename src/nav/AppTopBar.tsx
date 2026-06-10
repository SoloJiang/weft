import {
  ArrowLeft,
  FolderPlus,
  Languages,
  LayoutGrid,
  MessagesSquare,
  Moon,
  PanelLeftClose,
  PanelLeftOpen,
  Sun,
} from "lucide-react";
import { useState } from "react";
import { motion } from "motion/react";
import { useTranslation } from "react-i18next";
import { currentLang, setLang } from "../i18n";
import { Button } from "../components/ui/Button";
import { ToolIcon, toolFullName } from "../components/ToolIcon";
import { cn } from "../lib/cn";
import { useStore } from "../state/store";
import { useTheme } from "../state/theme";
import { AddRepoDialog } from "./dialogs";

export function AppTopBar() {
  const {
    navCollapsed,
    setNavCollapsed,
    activeThreadId,
    activeSessionId,
    activeWorkspaceId,
    viewing,
    sessions,
    showNeeds,
    homeTab,
    repos,
    threads,
    overview,
    directionsByThread,
    needs,
    asks,
    writeTriggers,
    proposal,
    reviewingProposal,
    setReviewingProposal,
    threadTab,
    setThreadTab,
    backToBoard,
    closeObserve,
  } = useStore();
  const { t } = useTranslation();
  const { theme, toggle } = useTheme();
  const [repoDialogOpen, setRepoDialogOpen] = useState(false);
  const lang = currentLang();
  const thread = threads.find((th) => th.id === activeThreadId);
  const activeSession = activeSessionId != null ? sessions[activeSessionId] : null;
  const inIssue = !!thread && activeSessionId == null && viewing == null && !showNeeds;
  const inSession = activeSession != null && !showNeeds;
  const inObserve = viewing != null && !showNeeds;
  const viewedRepo = viewing ? repos.find((r) => r.id === viewing.repoId) : null;
  const viewedDirection = viewing
    ? Object.values(directionsByThread)
        .flat()
        .find((d) => d.id === viewing.directionId)
    : null;
  const sessionRepo = activeSession ? repos.find((r) => r.id === activeSession.repoId) : null;
  const sessionDirection = activeSession
    ? directionsByThread[activeSession.threadId]?.find((d) => d.id === activeSession.directionId)
    : null;
  const inWorkspaceBoard =
    activeWorkspaceId != null &&
    activeThreadId == null &&
    activeSessionId == null &&
    viewing == null &&
    !showNeeds &&
    homeTab === "board";
  const inWorkspaceRepos =
    activeWorkspaceId != null &&
    activeThreadId == null &&
    activeSessionId == null &&
    viewing == null &&
    !showNeeds &&
    homeTab === "repos";
  const needsCount = needs.length + asks.length + writeTriggers.length;
  const proposalPending =
    proposal?.status === "proposed" && proposal.directions.length > 0 && !reviewingProposal;
  const issueTabs = [
    {
      key: "lead" as const,
      label: t("lead.viewChat"),
      icon: MessagesSquare,
      dot: proposalPending ? "bg-accent" : null,
    },
    { key: "board" as const, label: t("thread.tabBoard"), icon: LayoutGrid, dot: null as string | null },
  ];
  return (
    <header className="flex h-11 shrink-0 items-center gap-1.5 border-b border-border bg-bg px-3">
      <button
        type="button"
        onClick={() => setNavCollapsed(!navCollapsed)}
        aria-label={navCollapsed ? t("nav.expandSidebar") : t("nav.collapseSidebar")}
        title={navCollapsed ? t("nav.expandSidebar") : t("nav.collapseSidebar")}
        className="grid h-7 w-7 shrink-0 place-items-center rounded-[var(--radius-md)] text-ink-faint transition-colors hover:bg-brand-ghost hover:text-ink"
      >
        {navCollapsed ? <PanelLeftOpen size={16} /> : <PanelLeftClose size={16} />}
      </button>

      <div className="flex min-w-0 flex-1 items-center gap-1.5">
        {navCollapsed && (
          <>
            <img src="/weft-mark.svg" alt="" className="h-[18px] w-[18px]" draggable={false} />
            <span className="text-[15px] font-semibold tracking-[-0.01em] text-ink">weft</span>
          </>
        )}
        {inSession && (
          <div className="flex min-w-0 items-center gap-2">
            <button
              type="button"
              onClick={backToBoard}
              aria-label={t("session.back")}
              title={t("session.back")}
              className="grid h-7 w-7 shrink-0 place-items-center rounded-[var(--radius-md)] text-ink-faint transition-colors hover:bg-brand-ghost hover:text-ink"
            >
              <ArrowLeft size={15} />
            </button>
            <span className="flex shrink-0 items-center gap-1.5 rounded-[var(--radius-sm)] bg-surface px-2 py-0.5 text-[11px] font-medium text-ink-muted">
              <ToolIcon tool={activeSession.info.tool} size={12} />
              {toolFullName(activeSession.info.tool)}
            </span>
            <span className="min-w-0 truncate text-[13px] font-semibold text-ink">
              {sessionDirection?.name ?? "task"}
            </span>
            <span className="hidden shrink-0 text-[11.5px] text-ink-faint sm:inline">
              {sessionRepo?.name ?? "working copy"}
            </span>
          </div>
        )}
        {inObserve && (
          <div className="flex min-w-0 items-center gap-2">
            <button
              type="button"
              onClick={closeObserve}
              aria-label={t("session.back")}
              title={t("session.back")}
              className="grid h-7 w-7 shrink-0 place-items-center rounded-[var(--radius-md)] text-ink-faint transition-colors hover:bg-brand-ghost hover:text-ink"
            >
              <ArrowLeft size={15} />
            </button>
            <span className="min-w-0 truncate text-[13px] font-semibold text-ink">
              {viewedDirection?.name ?? "task"}
            </span>
            <span className="hidden shrink-0 text-[11.5px] text-ink-faint sm:inline">
              {viewedRepo?.name ?? "working copy"}
            </span>
          </div>
        )}
        {inIssue && (
          <div className="ml-1 flex min-w-0 items-center gap-2">
            <div className="flex shrink-0 items-center gap-1">
              {issueTabs.map((tab) => {
                const active = threadTab === tab.key;
                return (
                  <button
                    key={tab.key}
                    type="button"
                    onClick={() => {
                      setThreadTab(tab.key);
                      if (tab.key === "board") setReviewingProposal(false);
                    }}
                    className={cn(
                      "relative flex h-9 items-center gap-1.5 px-2.5 text-[12.5px] transition-colors",
                      active ? "text-ink" : "text-ink-faint hover:text-ink-muted",
                    )}
                  >
                    <tab.icon size={13} className={active ? "text-brand" : ""} />
                    {tab.label}
                    {tab.dot && <span className={cn("h-1.5 w-1.5 rounded-full", tab.dot, "animate-pulse")} />}
                    {active && (
                      <motion.span
                        layoutId="topbar-thread-tab"
                        className="absolute inset-x-2 bottom-0 h-[2px] rounded-full bg-brand"
                      />
                    )}
                  </button>
                );
              })}
            </div>
          </div>
        )}
        {inWorkspaceBoard && (
          <div className="flex min-w-0 items-baseline gap-2">
            <span className="truncate text-[13px] font-semibold text-ink">{t("nav.threads")}</span>
            <span className="shrink-0 text-[11.5px] text-ink-faint">
              {t("workspace.threadsCount", { count: overview.length })}
            </span>
          </div>
        )}
        {inWorkspaceRepos && (
          <div className="flex min-w-0 items-baseline gap-2">
            <span className="truncate text-[13px] font-semibold text-ink">
              {t("workspace.tabRepos")}
            </span>
            <span className="shrink-0 text-[11.5px] text-ink-faint">
              {t("nav.repos", { count: repos.length })}
            </span>
            {repos.length > 0 && (
              <span className="hidden shrink-0 items-center gap-1.5 text-[11.5px] text-running sm:inline-flex">
                <span className="h-1.5 w-1.5 rounded-full bg-running" />
                {t("repomap.curatorReady")}
              </span>
            )}
          </div>
        )}
        {showNeeds && (
          <div className="flex min-w-0 items-baseline gap-2">
            <span className="truncate text-[13px] font-semibold text-ink">{t("needs.title")}</span>
            {needsCount > 0 && (
              <span className="shrink-0 text-[11.5px] text-waiting tabular-nums">
                {needsCount}
              </span>
            )}
          </div>
        )}
      </div>

      {inWorkspaceRepos && activeWorkspaceId != null && (
        <Button size="sm" variant="primary" onClick={() => setRepoDialogOpen(true)}>
          <FolderPlus size={14} />
          {t("dialog.addRepo")}
        </Button>
      )}

      <button
        type="button"
        onClick={() => setLang(lang === "zh" ? "en" : "zh")}
        title={t("settings.language")}
        className="grid h-8 min-w-8 place-items-center rounded-[var(--radius-md)] px-2 text-[12px] font-semibold text-ink-muted transition-colors hover:bg-brand-ghost hover:text-ink"
      >
        <span className="flex items-center gap-1.5">
          <Languages size={14} />
          {lang === "zh" ? "中" : "EN"}
        </span>
      </button>

      <button
        type="button"
        onClick={toggle}
        title={t("palette.theme")}
        className="grid h-8 w-8 place-items-center rounded-[var(--radius-md)] text-ink-muted transition-colors hover:bg-brand-ghost hover:text-ink"
      >
        {theme === "dark" ? <Sun size={15} /> : <Moon size={15} />}
      </button>
      <AddRepoDialog open={repoDialogOpen} onOpenChange={setRepoDialogOpen} />
    </header>
  );
}
