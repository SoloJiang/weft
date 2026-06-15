import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { X } from "lucide-react";
import { DiffView } from "./DiffView";
import { cn } from "../lib/cn";

const MIN_W = 360;
const MAX_W = 860;
const clampW = (x: number) => Math.max(MIN_W, Math.min(MAX_W, x));

/**
 * The worktree diff as a real third column (not a floating overlay): opening it
 * animates the session content aside to make room. Drag its left edge to resize
 * (clamped); the width is remembered. Esc closes.
 */
export function DiffPanel({
  cwd,
  directionId,
  open,
  onClose,
  onAsk,
}: {
  cwd: string;
  /** The task whose worktree this is — enables "vs target" mode (see DiffView). */
  directionId?: number | null;
  open: boolean;
  onClose: () => void;
  /** Deliver a diff annotation to the responsible worker (see DiffView). */
  onAsk?: (text: string) => void;
}) {
  const { t } = useTranslation();
  const [w, setW] = useState(() =>
    clampW(Number(localStorage.getItem("weft-diff-w")) || 520),
  );
  const [dragging, setDragging] = useState(false);

  useEffect(() => {
    localStorage.setItem("weft-diff-w", String(w));
  }, [w]);

  useEffect(() => {
    if (!open) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [open, onClose]);

  useEffect(() => {
    if (!dragging) return;
    const move = (e: PointerEvent) => setW(clampW(window.innerWidth - e.clientX));
    const up = () => setDragging(false);
    window.addEventListener("pointermove", move);
    window.addEventListener("pointerup", up);
    document.body.style.cursor = "col-resize";
    document.body.style.userSelect = "none";
    return () => {
      window.removeEventListener("pointermove", move);
      window.removeEventListener("pointerup", up);
      document.body.style.cursor = "";
      document.body.style.userSelect = "";
    };
  }, [dragging]);

  return (
    <div
      style={{ width: open ? w : 0 }}
      className={cn(
        "relative flex shrink-0 overflow-hidden",
        !dragging &&
          "transition-[width] duration-200 ease-out motion-reduce:transition-none",
      )}
    >
      {/* resize handle on the column's left edge */}
      <div
        onPointerDown={(e) => {
          e.preventDefault();
          setDragging(true);
        }}
        className={cn(
          "absolute left-0 top-0 z-10 h-full w-1.5 cursor-col-resize transition-colors",
          dragging ? "bg-brand/40" : "hover:bg-brand/30",
        )}
      />
      {/* fixed-width inner so content doesn't reflow while the column animates */}
      <aside
        style={{ width: w }}
        className="flex h-full shrink-0 flex-col border-l border-border bg-bg"
      >
        <header className="flex items-center gap-2 border-b border-border px-4 py-2.5">
          <span className="text-[12px] font-semibold text-ink">{t("diff.tab")}</span>
          <button
            onClick={onClose}
            aria-label={t("bus.close")}
            className="ml-auto grid h-7 w-7 place-items-center rounded-[var(--radius-md)] text-ink-faint transition-colors hover:bg-brand-ghost hover:text-ink"
          >
            <X size={15} />
          </button>
        </header>
        <DiffView cwd={cwd} directionId={directionId} onAsk={onAsk} />
      </aside>
    </div>
  );
}
