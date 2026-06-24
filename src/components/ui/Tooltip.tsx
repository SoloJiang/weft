import type { ReactNode } from "react";
import { cn } from "../../lib/cn";

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
  /** Horizontal anchor. `end` pins the bubble to the trigger's right edge so a
   *  long label on a right-aligned button grows leftward instead of poking past
   *  the panel/window edge (and inflating the row's scrollWidth). */
  align?: "center" | "end";
}) {
  return (
    <span className={cn("group/tip relative inline-flex", className)}>
      {children}
      <span
        role="tooltip"
        className={cn(
          // Stay single-line (whitespace-nowrap → width = full label, no
          // shrink-to-fit collapse in the tiny inline-flex wrapper). `end` anchors
          // the bubble to the trigger's right edge so a long label on a
          // right-aligned button grows leftward instead of past the panel edge.
          "pointer-events-none absolute bottom-full z-50 mb-1.5 whitespace-nowrap rounded-[var(--radius-sm)] border border-border bg-raised px-2 py-1 text-[11px] text-ink opacity-0 shadow-[0_8px_24px_-12px_rgba(0,0,0,0.6)] transition-opacity duration-100 group-hover/tip:opacity-100",
          align === "end" ? "right-0" : "left-1/2 -translate-x-1/2",
        )}
      >
        {label}
      </span>
    </span>
  );
}
