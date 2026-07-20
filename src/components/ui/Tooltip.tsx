import type { ReactNode } from "react";
import { cn } from "../../lib/cn";

type TooltipAlign = "center" | "start" | "end";

/** Horizontal anchor → bubble placement. `center` suits compact triggers
 *  (icon buttons); for wide triggers it drops the bubble over the middle of an
 *  invisible span, visually attached to nothing — pin those to the edge the
 *  reading anchors on instead (`start`/`end`). */
const alignClass: Record<TooltipAlign, string> = {
  center: "left-1/2 -translate-x-1/2",
  start: "left-0",
  end: "right-0",
};

/** A zero-dependency hover tooltip: wraps its child, shows `label` above. */
export function Tooltip({
  label,
  children,
  className,
  align = "center",
}: {
  label: string;
  children: ReactNode;
  /** Extra classes for the wrapper (e.g. `w-full` so the child can stretch). */
  className?: string;
  /** Horizontal anchor. `start` pins the bubble to the trigger's left edge
   *  (wide left-aligned triggers like the context gauge); `end` pins it to the
   *  right edge so a long label on a right-aligned button grows leftward
   *  instead of poking past the panel/window edge (and inflating the row's
   *  scrollWidth). */
  align?: TooltipAlign;
}) {
  return (
    <span className={cn("group/tip relative inline-flex", className)}>
      {children}
      <span
        role="tooltip"
        className={cn(
          // Stay single-line (whitespace-nowrap → width = full label, no
          // shrink-to-fit collapse in the tiny inline-flex wrapper).
          "pointer-events-none absolute bottom-full z-50 mb-1.5 whitespace-nowrap rounded-[var(--radius-sm)] border border-border bg-raised px-2 py-1 text-[11px] text-ink opacity-0 shadow-[0_8px_24px_-12px_rgba(0,0,0,0.6)] transition-opacity duration-100 group-hover/tip:opacity-100",
          alignClass[align],
        )}
      >
        {label}
      </span>
    </span>
  );
}
