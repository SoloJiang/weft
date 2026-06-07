import { useState } from "react";
import { motion } from "motion/react";
import { useTranslation } from "react-i18next";
import * as DM from "@radix-ui/react-dropdown-menu";
import {
  Activity,
  Check,
  ChevronDown,
  FolderGit2,
  FolderPlus,
  LayoutGrid,
  Moon,
  PanelLeftClose,
  Plus,
  SquarePen,
  Sun,
  Trash2,
} from "lucide-react";
import { useStore } from "../state/store";
import { useTheme } from "../state/theme";
import { setLang } from "../i18n";
import type { Thread } from "../lib/types";
import { cn } from "../lib/cn";
import { AddRepoDialog, CreateThreadDialog, CreateWorkspaceDialog } from "./dialogs";

export function WorkspaceNav() {
  const {
    workspaces,
    activeWorkspaceId,
    threads,
    selectWorkspace,
    backToWorkspace,
    needsByWorkspace,
    setNavCollapsed,
    homeTab,
    setHomeTab,
    activeThreadId,
    showNeeds,
  } = useStore();
  const [dlg, setDlg] = useState<null | "ws" | "repo" | "thread">(null);
  const active = workspaces.find((w) => w.id === activeWorkspaceId);
  const { t } = useTranslation();
  // Any OTHER workspace waiting on the human → flag it on the switcher.
  const otherNeeds = workspaces.some(
    (w) => w.id !== activeWorkspaceId && (needsByWorkspace[w.id] ?? 0) > 0,
  );
  // On the workspace home (no thread / session open) — for highlighting the views.
  const onHome = activeThreadId == null && !showNeeds;

  return (
    <nav className="flex h-full w-72 shrink-0 flex-col border-r border-border bg-surface">
      <div className="flex items-center gap-2 px-3 pb-2.5 pt-3">
        <button
          onClick={backToWorkspace}
          title={t("nav.home")}
          className="flex select-none items-center gap-1.5 rounded-[var(--radius-sm)] px-1 py-0.5 transition-colors hover:bg-brand-ghost"
        >
          <img src="/weft-mark.svg" alt="" className="h-[18px] w-[18px]" draggable={false} />
          <span className="text-[15px] font-semibold tracking-[-0.01em] text-ink">weft</span>
        </button>
        <span className="text-ink-faint">/</span>
        <WorkspacePicker
          workspaces={workspaces}
          activeId={activeWorkspaceId}
          needsByWorkspace={needsByWorkspace}
          otherNeeds={otherNeeds}
          onSelect={(id) => void selectWorkspace(id)}
          onNew={() => setDlg("ws")}
        />
        <button
          onClick={() => setNavCollapsed(true)}
          aria-label={t("nav.collapseSidebar")}
          title={t("nav.collapseSidebar")}
          className="ml-auto grid h-6 w-6 shrink-0 place-items-center rounded-[var(--radius-md)] text-ink-faint transition-colors hover:bg-brand-ghost hover:text-ink"
        >
          <PanelLeftClose size={15} />
        </button>
      </div>

      <div className="mx-2 mb-1 border-t border-border" />

      {/* primary actions */}
      <div className="flex flex-col gap-0.5 px-2 py-1">
        <button
          onClick={() => setDlg("thread")}
          disabled={!active}
          className="flex items-center gap-2 rounded-[var(--radius-md)] px-2 py-1.5 text-[13px] font-medium text-ink transition-colors hover:bg-brand-ghost disabled:opacity-40"
        >
          <SquarePen size={14} className="text-brand" />
          {t("nav.newThread")}
        </button>
        <button
          onClick={() => setDlg("repo")}
          disabled={!active}
          className="flex items-center gap-2 rounded-[var(--radius-md)] px-2 py-1.5 text-[13px] text-ink-muted transition-colors hover:bg-brand-ghost hover:text-ink disabled:opacity-40"
        >
          <FolderPlus size={14} className="text-ink-faint" />
          {t("dialog.addRepo")}
        </button>
      </div>

      <div className="mx-2 my-1 border-t border-border" />

      {/* workspace views — the home tabs, moved into the rail (Linear-style) */}
      <ul className="flex flex-col gap-0.5 px-2 py-1">
        <WsNavItem
          icon={LayoutGrid}
          label={t("thread.tabBoard")}
          active={onHome && homeTab === "board"}
          onClick={() => {
            backToWorkspace();
            setHomeTab("board");
          }}
        />
        <WsNavItem
          icon={Activity}
          label={t("workspace.tabOverview")}
          attnCount={activeWorkspaceId != null ? needsByWorkspace[activeWorkspaceId] ?? 0 : 0}
          active={onHome && homeTab === "overview"}
          onClick={() => {
            backToWorkspace();
            setHomeTab("overview");
          }}
        />
        <WsNavItem
          icon={FolderGit2}
          label={t("workspace.tabRepos")}
          active={onHome && homeTab === "repos"}
          onClick={() => {
            backToWorkspace();
            setHomeTab("repos");
          }}
        />
      </ul>

      <div className="mx-2 my-1 border-t border-border" />

      <div className="px-3 py-1.5">
        <span className="text-[11px] font-semibold uppercase tracking-wider text-ink-faint">
          {t("nav.threads")}
        </span>
      </div>

      <div className="min-h-0 flex-1 overflow-y-auto px-2 pb-3">
        {threads.length === 0 ? (
          <p className="px-2 py-6 text-center text-[12px] leading-relaxed text-ink-faint">
            {active ? t("nav.noThreads") : t("nav.createWorkspaceFirst")}
          </p>
        ) : (
          <ul className="flex flex-col gap-0.5">
            {threads.map((t) => (
              <ThreadRow key={t.id} thread={t} />
            ))}
          </ul>
        )}
      </div>

      <footer className="flex items-center justify-end gap-1 border-t border-border px-3 py-2">
        <LangToggle />
        <ThemeToggle />
      </footer>

      <CreateWorkspaceDialog open={dlg === "ws"} onOpenChange={(o) => !o && setDlg(null)} />
      <CreateThreadDialog open={dlg === "thread"} onOpenChange={(o) => !o && setDlg(null)} />
      <AddRepoDialog open={dlg === "repo"} onOpenChange={(o) => !o && setDlg(null)} />
    </nav>
  );
}

function WsNavItem({
  icon: Icon,
  label,
  count,
  attnCount,
  active,
  onClick,
  onAdd,
  addLabel,
}: {
  icon: typeof LayoutGrid;
  label: string;
  count?: number;
  attnCount?: number;
  active: boolean;
  onClick: () => void;
  onAdd?: () => void;
  addLabel?: string;
}) {
  return (
    <li
      className={cn(
        "group flex items-center rounded-[var(--radius-md)] transition-colors",
        active ? "bg-brand-ghost" : "hover:bg-brand-ghost",
      )}
    >
      <button
        onClick={onClick}
        className={cn(
          "flex min-w-0 flex-1 items-center gap-2 px-2 py-1.5 text-left text-[13px]",
          active ? "text-ink" : "text-ink-muted",
        )}
      >
        <Icon size={14} className={active ? "text-brand" : "text-ink-faint"} />
        <span className="truncate">{label}</span>
        {attnCount != null && attnCount > 0 ? (
          <span className="ml-auto rounded-full bg-waiting/20 px-1.5 py-px text-[10px] font-semibold tabular-nums text-waiting">
            {attnCount}
          </span>
        ) : count != null && count > 0 ? (
          <span className="ml-auto text-[10px] tabular-nums text-ink-faint">{count}</span>
        ) : null}
      </button>
      {onAdd && (
        <button
          onClick={onAdd}
          aria-label={addLabel}
          title={addLabel}
          className="mr-1 grid h-6 w-6 shrink-0 place-items-center rounded text-ink-faint opacity-0 transition-opacity hover:text-ink group-hover:opacity-100"
        >
          <Plus size={14} />
        </button>
      )}
    </li>
  );
}

function ThemeToggle() {
  const { theme, toggle } = useTheme();
  const { t } = useTranslation();
  const dark = theme === "dark";
  return (
    <button
      onClick={toggle}
      aria-label={dark ? t("nav.lightTheme") : t("nav.darkTheme")}
      title={dark ? t("nav.lightTheme") : t("nav.darkTheme")}
      className="grid h-6 w-6 place-items-center rounded-[var(--radius-md)] text-ink-faint transition-colors hover:bg-brand-ghost hover:text-ink"
    >
      {dark ? <Sun size={14} /> : <Moon size={14} />}
    </button>
  );
}

function LangToggle() {
  const { i18n } = useTranslation();
  const zh = i18n.language === "zh";
  return (
    <button
      onClick={() => setLang(zh ? "en" : "zh")}
      title={zh ? "English" : "中文"}
      aria-label="Toggle language"
      className="grid h-6 min-w-[24px] place-items-center rounded-[var(--radius-md)] px-1 text-[11px] font-semibold text-ink-faint transition-colors hover:bg-brand-ghost hover:text-ink"
    >
      {zh ? "EN" : "中"}
    </button>
  );
}

function ThreadRow({ thread }: { thread: Thread }) {
  const {
    activeThreadId,
    directionsByThread,
    selectThread,
    deleteThread,
    sessions,
    needs,
    asks,
  } = useStore();
  const { t } = useTranslation();
  const isActive = activeThreadId === thread.id;
  const dirCount = directionsByThread[thread.id]?.length;
  const liveCount = Object.values(sessions).filter(
    (s) =>
      s.status === "running" &&
      directionsByThread[thread.id]?.some((d) => d.id === s.directionId),
  ).length;
  const needsYou =
    needs.some((n) => n.thread_id === thread.id) ||
    asks.some((a) => a.thread === thread.id);

  return (
    <li className="group relative">
      <button
        onClick={() => void selectThread(thread.id)}
        className={cn(
          "relative flex w-full items-center gap-2 rounded-[var(--radius-md)] px-2 py-1.5 text-left transition-colors",
          isActive ? "bg-brand-ghost text-ink" : "text-ink-muted hover:bg-brand-ghost hover:text-ink",
        )}
      >
        {isActive && (
          <motion.span
            layoutId="nav-thread-active"
            className="absolute left-0 top-1/2 h-5 w-[2px] -translate-y-1/2 rounded-full bg-brand"
          />
        )}
        <span className="truncate text-[13px]">{thread.title}</span>
        {liveCount > 0 && (
          <span className="flex items-center gap-1 text-[10px] text-running">
            <span className="weft-pulse h-1.5 w-1.5 rounded-full bg-running" />
            {liveCount}
          </span>
        )}
        <span className="ml-auto flex items-center gap-1.5">
          {needsYou && (
            <span
              title={t("nav.needsYou")}
              className="h-1.5 w-1.5 rounded-full bg-waiting"
            />
          )}
          {dirCount != null && dirCount > 0 && (
            <span className="text-[10px] tabular-nums text-ink-faint">{dirCount}</span>
          )}
        </span>
      </button>
      <button
        onClick={() => void deleteThread(thread.id)}
        aria-label={t("nav.deleteThread")}
        className="absolute right-1.5 top-1/2 grid h-5 w-5 -translate-y-1/2 place-items-center rounded bg-surface text-ink-faint opacity-0 transition-opacity hover:bg-[oklch(0.64_0.2_25/0.15)] hover:text-danger group-hover:opacity-100"
      >
        <Trash2 size={12} />
      </button>
    </li>
  );
}

function WorkspacePicker({
  workspaces,
  activeId,
  needsByWorkspace,
  otherNeeds,
  onSelect,
  onNew,
}: {
  workspaces: { id: number; name: string }[];
  activeId: number | null;
  needsByWorkspace: Record<number, number>;
  otherNeeds: boolean;
  onSelect: (id: number) => void;
  onNew: () => void;
}) {
  const active = workspaces.find((w) => w.id === activeId);
  const { t } = useTranslation();
  return (
    <DM.Root>
      <DM.Trigger className="flex min-w-0 items-center gap-1 rounded-[var(--radius-md)] px-1.5 py-1 text-[13px] font-medium text-ink outline-none transition-colors hover:bg-brand-ghost data-[state=open]:bg-brand-ghost">
        <span className="max-w-[150px] truncate">{active?.name ?? t("nav.noWorkspace")}</span>
        {otherNeeds && (
          <span title={t("nav.otherWorkspaceNeeds")} className="h-1.5 w-1.5 shrink-0 rounded-full bg-waiting" />
        )}
        <ChevronDown size={13} className="shrink-0 text-ink-faint" />
      </DM.Trigger>
      <DM.Portal>
        <DM.Content
          align="start"
          sideOffset={5}
          className="weft-pop z-[60] w-56 rounded-[var(--radius-md)] border border-border bg-raised p-1 shadow-[0_8px_24px_-8px_rgba(0,0,0,0.6)]"
        >
          {workspaces.map((w) => {
            const count = needsByWorkspace[w.id] ?? 0;
            const isActive = w.id === activeId;
            return (
              <DM.Item
                key={w.id}
                onSelect={() => onSelect(w.id)}
                className={cn(
                  "flex cursor-pointer items-center gap-2 rounded-[var(--radius-sm)] px-2 py-1.5 text-[13px] outline-none data-[highlighted]:bg-brand-ghost data-[highlighted]:text-ink",
                  isActive ? "text-ink" : "text-ink-muted",
                )}
              >
                <Check size={13} className={cn("shrink-0", isActive ? "text-brand" : "text-transparent")} />
                <span className="truncate">{w.name}</span>
                {/* only flag OTHER workspaces — the current one is shown in-app */}
                {!isActive && count > 0 && (
                  <span className="ml-auto rounded-full bg-waiting/20 px-1.5 py-0.5 text-[10px] font-semibold tabular-nums text-waiting">
                    {count}
                  </span>
                )}
              </DM.Item>
            );
          })}
          <DM.Separator className="my-1 h-px bg-border" />
          <DM.Item
            onSelect={onNew}
            className="flex cursor-pointer items-center gap-2 rounded-[var(--radius-sm)] px-2 py-1.5 text-[13px] text-ink-muted outline-none data-[highlighted]:bg-brand-ghost data-[highlighted]:text-ink"
          >
            <Plus size={13} /> {t("nav.newWorkspace")}
          </DM.Item>
        </DM.Content>
      </DM.Portal>
    </DM.Root>
  );
}
