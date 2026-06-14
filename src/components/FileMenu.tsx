import { useEffect, useSyncExternalStore, type ReactNode } from "react";
import { createPortal } from "react-dom";
import { useTranslation } from "react-i18next";
import { Copy, FileText, FolderOpen } from "lucide-react";
import {
  closeFileMenu,
  copyFilePath,
  fileMenuSnapshot,
  openFileRef,
  revealFileRef,
  subscribeFileMenu,
} from "../lib/fileLinks";

const MENU_W = 220;
const MENU_H = 124;

/**
 * The right-click menu for a chat file reference (open with default app, open
 * the containing folder, copy path). One app-wide instance driven by an external
 * store — same pattern as the toast — so any `FilePathRef` can summon it without
 * prop drilling.
 */
export function FileMenu() {
  const m = useSyncExternalStore(subscribeFileMenu, fileMenuSnapshot, () => null);
  const { t } = useTranslation();

  useEffect(() => {
    if (!m) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") closeFileMenu();
    };
    const onScroll = () => closeFileMenu();
    window.addEventListener("keydown", onKey);
    window.addEventListener("scroll", onScroll, true);
    return () => {
      window.removeEventListener("keydown", onKey);
      window.removeEventListener("scroll", onScroll, true);
    };
  }, [m]);

  if (!m) return null;
  const left = Math.max(8, Math.min(m.x, window.innerWidth - MENU_W - 8));
  const top = Math.max(8, Math.min(m.y, window.innerHeight - MENU_H - 8));

  return createPortal(
    <>
      <div
        className="fixed inset-0 z-[90]"
        onClick={closeFileMenu}
        onContextMenu={(e) => {
          e.preventDefault();
          closeFileMenu();
        }}
      />
      <div
        data-state="open"
        className="weft-pop fixed z-[91] min-w-[200px] rounded-[var(--radius-md)] border border-border bg-raised p-1 shadow-[0_8px_24px_-8px_rgba(0,0,0,0.5)]"
        style={{ left, top }}
      >
        <MenuItem
          icon={<FileText size={13} />}
          label={t("fileLink.openWith")}
          onClick={() => {
            closeFileMenu();
            void openFileRef(m.token, m.cwd, m.isUrl);
          }}
        />
        <MenuItem
          icon={<FolderOpen size={13} />}
          label={t("fileLink.openFolder")}
          onClick={() => {
            closeFileMenu();
            void revealFileRef(m.token, m.cwd, m.isUrl);
          }}
        />
        <MenuItem
          icon={<Copy size={13} />}
          label={t("fileLink.copyPath")}
          onClick={() => {
            closeFileMenu();
            copyFilePath(m.token, m.isUrl);
          }}
        />
      </div>
    </>,
    document.body,
  );
}

function MenuItem({
  icon,
  label,
  onClick,
}: {
  icon: ReactNode;
  label: string;
  onClick: () => void;
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      className="flex w-full items-center gap-2 rounded-[var(--radius-sm)] px-2 py-1.5 text-left text-[12px] text-ink-muted outline-none hover:bg-brand-ghost hover:text-ink"
    >
      <span className="text-ink-faint">{icon}</span>
      {label}
    </button>
  );
}

/**
 * Toggle `body[data-cmd]` while ⌘/Ctrl is held so quiet file refs reveal their
 * underline + pointer only when the user means to open one (VS Code parity).
 * Mounted once at the app shell.
 */
export function useCmdAffordance() {
  useEffect(() => {
    const set = (on: boolean) => {
      document.body.dataset.cmd = on ? "1" : "";
    };
    const down = (e: KeyboardEvent) => {
      if (e.key === "Meta" || e.key === "Control") set(true);
    };
    const up = (e: KeyboardEvent) => {
      if (e.key === "Meta" || e.key === "Control") set(false);
    };
    const clear = () => set(false);
    window.addEventListener("keydown", down);
    window.addEventListener("keyup", up);
    window.addEventListener("blur", clear);
    return () => {
      window.removeEventListener("keydown", down);
      window.removeEventListener("keyup", up);
      window.removeEventListener("blur", clear);
      set(false);
    };
  }, []);
}
