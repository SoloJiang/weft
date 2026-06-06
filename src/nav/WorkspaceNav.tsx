import { useState } from "react";
import { motion } from "motion/react";
import {
  Check,
  ChevronRight,
  FolderGit2,
  HelpCircle,
  Moon,
  Plus,
  Sun,
  Trash2,
} from "lucide-react";
import { useStore } from "../state/store";
import { useTheme } from "../state/theme";
import type { Thread } from "../lib/types";
import { cn } from "../lib/cn";
import {
  AddRepoDialog,
  CreateThreadDialog,
  CreateWorkspaceDialog,
} from "./dialogs";

export function WorkspaceNav() {
  const {
    workspaces,
    activeWorkspaceId,
    repos,
    threads,
    selectWorkspace,
    openRepoMap,
    showRepoMap,
  } = useStore();
  const [dlg, setDlg] = useState<null | "ws" | "repo" | "thread">(null);
  const active = workspaces.find((w) => w.id === activeWorkspaceId);

  return (
    <nav className="flex h-full w-72 shrink-0 flex-col border-r border-border bg-surface">
      <div className="flex items-center gap-2 px-3 pb-2 pt-3">
        <span className="flex select-none items-center gap-1.5">
          <img src="/weft-mark.svg" alt="" className="h-[18px] w-[18px]" draggable={false} />
          <span className="text-[15px] font-semibold tracking-[-0.01em] text-ink">weft</span>
        </span>
        <span className="text-ink-faint">/</span>
        <WorkspacePicker
          workspaces={workspaces}
          activeId={activeWorkspaceId}
          onSelect={(id) => void selectWorkspace(id)}
          onNew={() => setDlg("ws")}
        />
      </div>

      <NeedsButton />

      <div
        className={cn(
          "group mx-2 mb-1 flex items-center rounded-[var(--radius-md)] transition-colors",
          showRepoMap ? "bg-brand-ghost" : "hover:bg-brand-ghost",
        )}
      >
        <button
          onClick={openRepoMap}
          disabled={!active}
          title="Open the repo map"
          className="flex min-w-0 flex-1 items-center gap-2 px-2 py-1.5 text-left text-[12px] text-ink-muted disabled:opacity-40"
        >
          <FolderGit2 size={13} className="text-ink-faint" />
          {repos.length} {repos.length === 1 ? "repo" : "repos"}
        </button>
        <button
          onClick={() => setDlg("repo")}
          disabled={!active}
          aria-label="Add repo"
          className="mr-1 grid h-6 w-6 place-items-center rounded text-ink-faint opacity-0 transition-opacity hover:text-ink group-hover:opacity-100 disabled:opacity-40"
        >
          <Plus size={14} />
        </button>
      </div>

      <div className="mx-2 my-1 border-t border-border" />

      <div className="flex items-center justify-between px-3 py-1.5">
        <span className="text-[11px] font-semibold uppercase tracking-wider text-ink-faint">
          Threads
        </span>
        <button
          onClick={() => setDlg("thread")}
          disabled={!active}
          aria-label="New thread"
          className="grid h-5 w-5 place-items-center rounded text-ink-faint transition-colors hover:bg-brand-ghost hover:text-ink disabled:opacity-40"
        >
          <Plus size={14} />
        </button>
      </div>

      <div className="min-h-0 flex-1 overflow-y-auto px-2 pb-3">
        {threads.length === 0 ? (
          <p className="px-2 py-6 text-center text-[12px] leading-relaxed text-ink-faint">
            {active
              ? "No threads yet. Create one to start a work line."
              : "Create a workspace to begin."}
          </p>
        ) : (
          <ul className="flex flex-col gap-0.5">
            {threads.map((t) => (
              <ThreadRow key={t.id} thread={t} />
            ))}
          </ul>
        )}
      </div>

      <footer className="flex items-center justify-between border-t border-border px-3 py-2">
        <span className="text-[11px] text-ink-faint">Local · no server</span>
        <ThemeToggle />
      </footer>

      <CreateWorkspaceDialog open={dlg === "ws"} onOpenChange={(o) => !o && setDlg(null)} />
      <AddRepoDialog open={dlg === "repo"} onOpenChange={(o) => !o && setDlg(null)} />
      <CreateThreadDialog open={dlg === "thread"} onOpenChange={(o) => !o && setDlg(null)} />
    </nav>
  );
}

function NeedsButton() {
  const { needs, asks, showNeeds, openNeeds } = useStore();
  const count = needs.length + asks.length;
  const has = count > 0;

  return (
    <button
      onClick={openNeeds}
      aria-label={has ? `${count} waiting on you` : "Needs you"}
      className={cn(
        "group mx-2 mb-1 mt-1 flex items-center gap-2 rounded-[var(--radius-md)] px-2 py-1.5 text-left transition-colors",
        showNeeds
          ? has
            ? "bg-waiting/15 text-ink"
            : "bg-raised text-ink"
          : has
            ? "bg-waiting/10 text-ink hover:bg-waiting/15"
            : "text-ink-faint hover:bg-brand-ghost hover:text-ink-muted",
      )}
    >
      {has ? (
        <span className="grid h-3.5 w-3.5 place-items-center">
          <HelpCircle size={13} className="text-waiting" />
        </span>
      ) : (
        <Check size={13} className="text-ink-faint" />
      )}
      <span className="text-[13px] font-medium">
        {has ? "Needs you" : "Nothing needs you"}
      </span>
      {has && (
        <span className="ml-auto rounded-full bg-waiting/20 px-1.5 py-0.5 text-[10px] font-semibold tabular-nums text-waiting">
          {count}
        </span>
      )}
    </button>
  );
}

function ThemeToggle() {
  const { theme, toggle } = useTheme();
  const dark = theme === "dark";
  return (
    <button
      onClick={toggle}
      aria-label={dark ? "Switch to light theme" : "Switch to dark theme"}
      title={dark ? "Light theme" : "Dark theme"}
      className="grid h-6 w-6 place-items-center rounded-[var(--radius-md)] text-ink-faint transition-colors hover:bg-brand-ghost hover:text-ink"
    >
      {dark ? <Sun size={14} /> : <Moon size={14} />}
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
  } = useStore();
  const isActive = activeThreadId === thread.id;
  const dirCount = directionsByThread[thread.id]?.length;
  const liveCount = Object.values(sessions).filter(
    (s) =>
      s.status === "running" &&
      directionsByThread[thread.id]?.some((d) => d.id === s.directionId),
  ).length;

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
          {dirCount != null && dirCount > 0 && (
            <span className="text-[10px] tabular-nums text-ink-faint">{dirCount}</span>
          )}
        </span>
      </button>
      <button
        onClick={() => void deleteThread(thread.id)}
        aria-label="Delete thread"
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
  onSelect,
  onNew,
}: {
  workspaces: { id: number; name: string }[];
  activeId: number | null;
  onSelect: (id: number) => void;
  onNew: () => void;
}) {
  const active = workspaces.find((w) => w.id === activeId);
  return (
    <details className="group relative flex-1">
      <summary className="flex cursor-pointer list-none items-center justify-between gap-1 rounded-[var(--radius-md)] px-2 py-1 text-[13px] font-medium text-ink hover:bg-brand-ghost">
        <span className="truncate">{active?.name ?? "No workspace"}</span>
        <ChevronRight
          size={13}
          className="text-ink-faint transition-transform group-open:rotate-90"
        />
      </summary>
      <div className="absolute left-0 top-full z-50 mt-1 w-52 rounded-[var(--radius-md)] border border-border bg-raised p-1 shadow-[0_8px_24px_-8px_rgba(0,0,0,0.6)]">
        {workspaces.map((w) => (
          <button
            key={w.id}
            onClick={(ev) => {
              onSelect(w.id);
              (ev.currentTarget.closest("details") as HTMLDetailsElement).open = false;
            }}
            className={cn(
              "flex w-full items-center rounded-[var(--radius-sm)] px-2 py-1.5 text-left text-[13px]",
              w.id === activeId
                ? "bg-brand-ghost text-ink"
                : "text-ink-muted hover:bg-brand-ghost hover:text-ink",
            )}
          >
            {w.name}
          </button>
        ))}
        <div className="my-1 border-t border-border" />
        <button
          onClick={(ev) => {
            onNew();
            (ev.currentTarget.closest("details") as HTMLDetailsElement).open = false;
          }}
          className="flex w-full items-center gap-2 rounded-[var(--radius-sm)] px-2 py-1.5 text-left text-[13px] text-ink-muted hover:bg-brand-ghost hover:text-ink"
        >
          <Plus size={13} /> New workspace
        </button>
      </div>
    </details>
  );
}
