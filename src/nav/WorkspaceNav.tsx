import { useState } from "react";
import { motion } from "motion/react";
import { useTranslation } from "react-i18next";
import { ChevronRight, Moon, Plus, Sun, Trash2 } from "lucide-react";
import { useStore } from "../state/store";
import { useTheme } from "../state/theme";
import { setLang } from "../i18n";
import type { Thread } from "../lib/types";
import { cn } from "../lib/cn";
import { CreateThreadDialog, CreateWorkspaceDialog } from "./dialogs";

export function WorkspaceNav() {
  const {
    workspaces,
    activeWorkspaceId,
    threads,
    selectWorkspace,
    backToWorkspace,
    needsByWorkspace,
  } = useStore();
  const [dlg, setDlg] = useState<null | "ws" | "repo" | "thread">(null);
  const active = workspaces.find((w) => w.id === activeWorkspaceId);
  const { t } = useTranslation();
  // Any OTHER workspace waiting on the human → flag it on the switcher.
  const otherNeeds = workspaces.some(
    (w) => w.id !== activeWorkspaceId && (needsByWorkspace[w.id] ?? 0) > 0,
  );

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
      </div>

      <div className="mx-2 mb-1 border-t border-border" />

      <div className="flex items-center justify-between px-3 py-1.5">
        <span className="text-[11px] font-semibold uppercase tracking-wider text-ink-faint">
          {t("nav.threads")}
        </span>
        <button
          onClick={() => setDlg("thread")}
          disabled={!active}
          aria-label={t("nav.newThread")}
          className="grid h-5 w-5 place-items-center rounded text-ink-faint transition-colors hover:bg-brand-ghost hover:text-ink disabled:opacity-40"
        >
          <Plus size={14} />
        </button>
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

      <footer className="flex items-center justify-between border-t border-border px-3 py-2">
        <span className="text-[11px] text-ink-faint">{t("nav.local")}</span>
        <div className="flex items-center gap-1">
          <LangToggle />
          <ThemeToggle />
        </div>
      </footer>

      <CreateWorkspaceDialog open={dlg === "ws"} onOpenChange={(o) => !o && setDlg(null)} />
      <CreateThreadDialog open={dlg === "thread"} onOpenChange={(o) => !o && setDlg(null)} />
    </nav>
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
    <details className="group relative flex-1">
      <summary className="flex cursor-pointer list-none items-center justify-between gap-1 rounded-[var(--radius-md)] px-2 py-1 text-[13px] font-medium text-ink hover:bg-brand-ghost">
        <span className="truncate">{active?.name ?? t("nav.noWorkspace")}</span>
        <span className="flex items-center gap-1.5">
          {otherNeeds && (
            <span
              title={t("nav.otherWorkspaceNeeds")}
              className="h-1.5 w-1.5 rounded-full bg-waiting"
            />
          )}
          <ChevronRight
            size={13}
            className="text-ink-faint transition-transform group-open:rotate-90"
          />
        </span>
      </summary>
      <div className="absolute left-0 top-full z-50 mt-1 w-56 rounded-[var(--radius-md)] border border-border bg-raised p-1 shadow-[0_8px_24px_-8px_rgba(0,0,0,0.6)]">
        {workspaces.map((w) => {
          const count = needsByWorkspace[w.id] ?? 0;
          return (
            <button
              key={w.id}
              onClick={(ev) => {
                onSelect(w.id);
                (ev.currentTarget.closest("details") as HTMLDetailsElement).open = false;
              }}
              className={cn(
                "flex w-full items-center gap-2 rounded-[var(--radius-sm)] px-2 py-1.5 text-left text-[13px]",
                w.id === activeId
                  ? "bg-brand-ghost text-ink"
                  : "text-ink-muted hover:bg-brand-ghost hover:text-ink",
              )}
            >
              <span className="truncate">{w.name}</span>
              {count > 0 && (
                <span className="ml-auto rounded-full bg-waiting/20 px-1.5 py-0.5 text-[10px] font-semibold tabular-nums text-waiting">
                  {count}
                </span>
              )}
            </button>
          );
        })}
        <div className="my-1 border-t border-border" />
        <button
          onClick={(ev) => {
            onNew();
            (ev.currentTarget.closest("details") as HTMLDetailsElement).open = false;
          }}
          className="flex w-full items-center gap-2 rounded-[var(--radius-sm)] px-2 py-1.5 text-left text-[13px] text-ink-muted hover:bg-brand-ghost hover:text-ink"
        >
          <Plus size={13} /> {t("nav.newWorkspace")}
        </button>
      </div>
    </details>
  );
}
