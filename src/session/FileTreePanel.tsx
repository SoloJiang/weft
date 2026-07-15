import { useEffect } from "react";
import { useTranslation } from "react-i18next";
import { X } from "lucide-react";
import { FileTreeView } from "./FileTreeView";
import { useResizablePanel } from "./useResizablePanel";
import { cn } from "../lib/cn";

/**
 * The worktree file tree as a real third column (like DiffPanel): opening it
 * animates the session content aside. Drag its left edge to resize (clamped);
 * the width is remembered. Esc closes.
 */
export function FileTreePanel({
  cwd,
  open,
  onClose,
}: {
  cwd: string;
  open: boolean;
  onClose: () => void;
}) {
  const { t } = useTranslation();
  const { width: w, dragging, startDrag } = useResizablePanel({
    storageKey: "weft-files-w",
    min: 280,
    max: 520,
    default: 320,
  });

  useEffect(() => {
    if (!open) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [open, onClose]);

  return (
    <div
      style={{ width: open ? w : 0 }}
      className={cn(
        "relative flex shrink-0 overflow-hidden",
        !dragging &&
          "transition-[width] duration-200 ease-out motion-reduce:transition-none",
      )}
    >
      <div
        onPointerDown={(e) => {
          e.preventDefault();
          startDrag();
        }}
        className={cn(
          "absolute left-0 top-0 z-10 h-full w-1.5 cursor-col-resize transition-colors",
          dragging ? "bg-brand/40" : "hover:bg-brand/30",
        )}
      />
      <aside
        style={{ width: w }}
        className="flex h-full shrink-0 flex-col border-l border-border bg-bg"
      >
        <header className="flex items-center gap-2 border-b border-border px-4 py-2.5">
          <span className="text-[12px] font-semibold text-ink">{t("files.tab")}</span>
          <button
            onClick={onClose}
            aria-label={t("bus.close")}
            className="ml-auto grid h-7 w-7 place-items-center rounded-[var(--radius-md)] text-ink-faint transition-colors hover:bg-brand-ghost hover:text-ink"
          >
            <X size={15} />
          </button>
        </header>
        <FileTreeView cwd={cwd} open={open} />
      </aside>
    </div>
  );
}
