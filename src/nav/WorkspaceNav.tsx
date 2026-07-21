import { useState } from "react";
import { motion } from "motion/react";
import { useTranslation } from "react-i18next";
import * as DM from "@radix-ui/react-dropdown-menu";
import {
  Check,
  ChevronDown,
  FolderGit2,
  FolderPlus,
  HelpCircle,
  LayoutGrid,
  Pencil,
  PanelLeftClose,
  Plus,
  Search,
  Settings,
  SquarePen,
  Trash2,
} from "lucide-react";
import { isLiveStatus, useStore } from "../state/store";
import type { Thread } from "../lib/types";
import { cn } from "../lib/cn";
import { openCommandPalette } from "../components/CommandPalette";
import {
  AddRepoDialog,
  CreateThreadDialog,
  CreateWorkspaceDialog,
  DeleteWorkspaceDialog,
  RenameDialog,
} from "./dialogs";

export function WorkspaceNav() {
  const {
    workspaces,
    activeWorkspaceId,
    threads,
    selectWorkspace,
    renameWorkspace,
    deleteWorkspace,
    renameThread,
    backToWorkspace,
    needsByWorkspace,
    homeTab,
    setHomeTab,
    openSettings,
    activeThreadId,
    showNeeds,
    openNeeds,
    needs,
    asks,
    writeTriggers,
    setNavCollapsed,
  } = useStore();
  // Live workspace-wide pending count for the Needs-you focal entry.
  const needsCount = needs.length + asks.length + writeTriggers.length;
  const [dlg, setDlg] = useState<null | "ws" | "repo" | "thread">(null);
  // Both rename surfaces store only an id and derive `initial` from the live
  // slice — so concurrent updates flow through instead of being captured.
  const [renamingWsId, setRenamingWsId] = useState<number | null>(null);
  const [deletingWsId, setDeletingWsId] = useState<number | null>(null);
  const [renamingThreadId, setRenamingThreadId] = useState<number | null>(null);
  const active = workspaces.find((w) => w.id === activeWorkspaceId);
  const renamingWs =
    renamingWsId != null ? workspaces.find((w) => w.id === renamingWsId) ?? null : null;
  const deletingWs =
    deletingWsId != null ? workspaces.find((w) => w.id === deletingWsId) ?? null : null;
  const renamingThread =
    renamingThreadId != null ? threads.find((th) => th.id === renamingThreadId) ?? null : null;
  // The hidden curator-chat thread is in `threads` (so its chat can render) but
  // is not a board issue — keep it out of the nav list.
  const issueThreads = threads.filter((th) => th.kind !== "curator");
  const { t } = useTranslation();
  // Any OTHER workspace waiting on the human → flag it on the switcher.
  const otherNeeds = workspaces.some(
    (w) => w.id !== activeWorkspaceId && (needsByWorkspace[w.id] ?? 0) > 0,
  );
  // On the workspace home (no thread / session open) — for highlighting the views.
  const onHome = activeThreadId == null && !showNeeds;

  return (
    <nav className="flex h-full w-60 shrink-0 flex-col border-r border-border bg-surface">
      <div className="flex flex-col gap-2 px-3 pb-2.5 pt-3">
        <div className="flex items-center justify-between gap-2">
          <button
            onClick={backToWorkspace}
            title={t("nav.home")}
            className="flex min-w-0 select-none items-center gap-1.5 rounded-[var(--radius-sm)] px-1 py-0.5 transition-colors hover:bg-brand-ghost"
          >
            <img src="/weft-mark.svg" alt="" className="h-[18px] w-[18px]" draggable={false} />
            <span className="truncate text-[15px] font-semibold tracking-[-0.01em] text-ink">Weft</span>
          </button>
          <button
            type="button"
            onClick={() => setNavCollapsed(true)}
            aria-label={t("nav.collapseSidebar")}
            title={t("nav.collapseSidebar")}
            className="grid h-7 w-7 shrink-0 place-items-center rounded-[var(--radius-md)] text-ink-faint transition-colors hover:bg-brand-ghost hover:text-ink"
          >
            <PanelLeftClose size={16} />
          </button>
        </div>
        <div className="rounded-[var(--radius-md)] border border-border bg-bg/55 p-1">
          <WorkspacePicker
            workspaces={workspaces}
            activeId={activeWorkspaceId}
            needsByWorkspace={needsByWorkspace}
            otherNeeds={otherNeeds}
            onSelect={(id) => void selectWorkspace(id)}
            onNew={() => setDlg("ws")}
            onRename={(w) => setRenamingWsId(w.id)}
            onDelete={(w) => setDeletingWsId(w.id)}
          />
        </div>
      </div>

      <div className="mx-2 mb-1 border-t border-border" />

      {/* search / jump — the ⌘K command palette trigger */}
      <div className="px-2 pt-1">
        <button
          onClick={openCommandPalette}
          className="flex w-full items-center gap-2 rounded-[var(--radius-md)] border border-border bg-raised px-2 py-1.5 text-[13px] text-ink-faint transition-colors hover:border-brand/40 hover:text-ink-muted"
        >
          <Search size={14} className="shrink-0" />
          <span>{t("palette.search")}</span>
          <kbd className="ml-auto rounded border border-border bg-surface px-1.5 py-px font-mono text-[10px] text-ink-faint">
            ⌘K
          </kbd>
        </button>
      </div>

      {/* primary actions */}
      {active ? (
        <>
          <div className="flex flex-col gap-0.5 px-2 py-1">
            <button
              onClick={() => setDlg("thread")}
              className="flex items-center gap-2 rounded-[var(--radius-md)] px-2 py-1.5 text-[13px] font-medium text-ink transition-colors hover:bg-brand-ghost"
            >
              <SquarePen size={14} className="text-brand" />
              {t("nav.newThread")}
            </button>
            <button
              onClick={() => setDlg("repo")}
              className="flex w-full items-center gap-2 rounded-[var(--radius-md)] px-2 py-1.5 text-[13px] text-ink-muted transition-colors hover:bg-brand-ghost hover:text-ink"
            >
              <FolderPlus size={14} className="text-ink-faint" />
              {t("dialog.addRepo")}
            </button>
          </div>

          <div className="mx-2 my-1 border-t border-border" />

          {/* workspace views — the home tabs, moved into the rail (Linear-style).
              Needs-you leads: the exception queue is the focal surface (PRODUCT §7),
              reachable from anywhere with the live workspace-wide pending count. */}
          <ul className="flex flex-col gap-0.5 px-2 py-1">
            <WsNavItem
              icon={HelpCircle}
              label={t("needs.title")}
              attnCount={needsCount}
              active={showNeeds}
              onClick={() => openNeeds()}
            />
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
            {issueThreads.length === 0 ? (
              <p className="px-2 py-6 text-center text-[12px] leading-relaxed text-ink-faint">
                {t("nav.noThreads")}
              </p>
            ) : (
              <ul className="flex flex-col gap-0.5">
                {issueThreads.map((t) => (
                  <ThreadRow key={t.id} thread={t} onRename={setRenamingThreadId} />
                ))}
              </ul>
            )}
          </div>
        </>
      ) : (
        <>
          <div className="px-2 py-1">
            <button
              onClick={() => setDlg("ws")}
              className="flex w-full items-center gap-2 rounded-[var(--radius-md)] px-2 py-1.5 text-[13px] font-medium text-ink transition-colors hover:bg-brand-ghost"
            >
              <Plus size={14} className="text-brand" />
              {t("nav.newWorkspace")}
            </button>
          </div>
          <div className="flex-1" />
        </>
      )}

      <footer className="border-t border-border p-2">
        <button
          onClick={openSettings}
          className={cn(
            "flex w-full items-center gap-2 rounded-[var(--radius-md)] px-2 py-1.5 text-[13px] transition-colors hover:bg-brand-ghost hover:text-ink",
            onHome && homeTab === "settings" ? "bg-brand-ghost text-ink" : "text-ink-muted",
          )}
        >
          <Settings
            size={14}
            className={onHome && homeTab === "settings" ? "text-brand" : "text-ink-faint"}
          />
          {t("settings.title")}
        </button>
      </footer>

      <CreateWorkspaceDialog open={dlg === "ws"} onOpenChange={(o) => !o && setDlg(null)} />
      <CreateThreadDialog open={dlg === "thread"} onOpenChange={(o) => !o && setDlg(null)} />
      <AddRepoDialog open={dlg === "repo"} onOpenChange={(o) => !o && setDlg(null)} />
      {renamingWs && (
        <RenameDialog
          open={renamingWsId != null}
          onOpenChange={(o) => !o && setRenamingWsId(null)}
          title={t("nav.renameWorkspace")}
          label={t("dialog.workspaceName")}
          initial={renamingWs.name}
          onSubmit={(v) => renameWorkspace(renamingWs.id, v)}
        />
      )}
      <DeleteWorkspaceDialog
        workspace={deletingWs}
        onOpenChange={(o) => !o && setDeletingWsId(null)}
        onConfirm={() => {
          if (!deletingWs) return Promise.resolve();
          return deleteWorkspace(deletingWs.id);
        }}
      />
      {renamingThread && (
        <RenameDialog
          open={renamingThreadId != null}
          onOpenChange={(o) => !o && setRenamingThreadId(null)}
          title={t("nav.renameThread")}
          label={t("dialog.threadTitle")}
          initial={renamingThread.title}
          onSubmit={(v) => renameThread(renamingThread.id, v)}
        />
      )}
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
        "group relative flex items-center rounded-[var(--radius-md)] transition-colors",
        active ? "bg-brand-ghost" : "hover:bg-brand-ghost",
      )}
    >
      {active && (
        <motion.span
          layoutId="nav-workspace-active"
          className="absolute left-0 top-1/2 h-5 w-[2px] -translate-y-1/2 rounded-full bg-brand"
        />
      )}
      <button
        onClick={onClick}
        aria-current={active ? "page" : undefined}
        className={cn(
          "flex min-w-0 flex-1 items-center gap-2 px-2 py-1.5 pl-2.5 text-left text-[13px]",
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

function ThreadRow({ thread, onRename }: { thread: Thread; onRename: (id: number) => void }) {
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
      isLiveStatus(s.status) &&
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
          "relative flex w-full items-center gap-2 rounded-[var(--radius-md)] px-2 py-1.5 text-left transition-[padding,background-color]",
          // reserve space on hover so the pencil + trash overlay doesn't sit on
          // top of the needsYou / dirCount badges
          "group-hover:pr-[3.25rem]",
          isActive ? "bg-brand-ghost text-ink" : "text-ink-muted hover:bg-brand-ghost hover:text-ink",
        )}
      >
        {isActive && (
          <motion.span
            layoutId="nav-thread-active"
            className="absolute left-0 top-1/2 h-5 w-[2px] -translate-y-1/2 rounded-full bg-brand"
          />
        )}
        <span className="shrink-0 font-mono text-[11px] font-medium tabular-nums text-ink-faint">
          #{thread.id}
        </span>
        <span className="truncate text-[13px]">{thread.title}</span>
        {liveCount > 0 && (
          <span className="flex items-center gap-1 text-[10px] text-running">
            <span className="weft-pulse h-1.5 w-1.5 rounded-full bg-running" />
            {liveCount}
          </span>
        )}
        <span className="ml-auto flex items-center gap-1.5 transition-opacity group-hover:opacity-0">
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
        onClick={() => onRename(thread.id)}
        aria-label={t("nav.renameThread")}
        className="absolute right-7 top-1/2 grid h-5 w-5 -translate-y-1/2 place-items-center rounded text-ink-faint opacity-0 transition-opacity hover:bg-brand-ghost hover:text-ink group-hover:opacity-100"
      >
        <Pencil size={12} />
      </button>
      <button
        onClick={() => void deleteThread(thread.id)}
        aria-label={t("nav.deleteThread")}
        className="absolute right-1.5 top-1/2 grid h-5 w-5 -translate-y-1/2 place-items-center rounded text-ink-faint opacity-0 transition-opacity hover:bg-[oklch(0.64_0.2_25/0.15)] hover:text-danger group-hover:opacity-100"
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
  onRename,
  onDelete,
}: {
  workspaces: { id: number; name: string }[];
  activeId: number | null;
  needsByWorkspace: Record<number, number>;
  otherNeeds: boolean;
  onSelect: (id: number) => void;
  onNew: () => void;
  onRename: (w: { id: number; name: string }) => void;
  onDelete: (w: { id: number; name: string }) => void;
}) {
  const active = workspaces.find((w) => w.id === activeId);
  const { t } = useTranslation();
  return (
    <DM.Root>
      <DM.Trigger className="flex w-full min-w-0 items-center gap-1 rounded-[var(--radius-md)] px-2 py-1.5 text-[13px] font-medium text-ink outline-none transition-colors hover:bg-brand-ghost data-[state=open]:bg-brand-ghost">
        <span className="min-w-0 flex-1 truncate text-left">{active?.name ?? t("nav.noWorkspace")}</span>
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
                  "group flex cursor-pointer items-center gap-2 rounded-[var(--radius-sm)] px-2 py-1.5 text-[13px] outline-none data-[highlighted]:bg-brand-ghost data-[highlighted]:text-ink",
                  isActive ? "text-ink" : "text-ink-muted",
                )}
              >
                <Check size={13} className={cn("shrink-0", isActive ? "text-brand" : "text-transparent")} />
                <span className="min-w-0 flex-1 truncate">{w.name}</span>
                {/* only flag OTHER workspaces — the current one is shown in-app */}
                {!isActive && count > 0 && (
                  <span className="rounded-full bg-waiting/20 px-1.5 py-0.5 text-[10px] font-semibold tabular-nums text-waiting">
                    {count}
                  </span>
                )}
                <button
                  type="button"
                  // Mouse-only affordance: keyboard users have a dedicated
                  // "Rename current workspace" menu item below the list. We
                  // hide this from keyboard/AT so we don't nest a focusable
                  // button inside a DM.Item (role=menuitem).
                  tabIndex={-1}
                  aria-hidden="true"
                  title={t("nav.renameWorkspace")}
                  onPointerDown={(e) => e.stopPropagation()}
                  onPointerUp={(e) => e.stopPropagation()}
                  onClick={(e) => {
                    e.stopPropagation();
                    onRename(w);
                  }}
                  className="grid h-5 w-5 shrink-0 place-items-center rounded text-ink-faint opacity-0 transition-opacity hover:text-ink group-hover:opacity-100 group-data-[highlighted]:opacity-100"
                >
                  <Pencil size={12} />
                </button>
              </DM.Item>
            );
          })}
          <DM.Separator className="my-1 h-px bg-border" />
          {active && (
            <>
              <DM.Item
                onSelect={() => onRename(active)}
                className="flex cursor-pointer items-center gap-2 rounded-[var(--radius-sm)] px-2 py-1.5 text-[13px] text-ink-muted outline-none data-[highlighted]:bg-brand-ghost data-[highlighted]:text-ink"
              >
                <Pencil size={13} /> {t("nav.renameWorkspace")}
              </DM.Item>
              <DM.Item
                onSelect={() => onDelete(active)}
                className="flex cursor-pointer items-center gap-2 rounded-[var(--radius-sm)] px-2 py-1.5 text-[13px] text-danger outline-none data-[highlighted]:bg-[oklch(0.64_0.2_25/0.15)]"
              >
                <Trash2 size={13} /> {t("nav.deleteWorkspace")}
              </DM.Item>
            </>
          )}
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
