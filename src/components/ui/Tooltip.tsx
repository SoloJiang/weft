import type { ReactNode } from "react";
import { cn } from "../../lib/cn";

/** A zero-dependency hover tooltip: wraps its child, shows `label` above. */
export function Tooltip({
  label,
  children,
  className,
}: {
  label: string;
  children: ReactNode;
  /** Extra classes for the wrapper (e.g. `w-full` so the child can stretch). */
  className?: string;
}) {
  return (
    <span className={cn("group/tip relative inline-flex", className)}>
      {children}
      <span
        role="tooltip"
        className="pointer-events-none absolute bottom-full left-1/2 z-50 mb-1.5 -translate-x-1/2 whitespace-nowrap rounded-[var(--radius-sm)] border border-border bg-raised px-2 py-1 text-[11px] text-ink opacity-0 shadow-[0_8px_24px_-12px_rgba(0,0,0,0.6)] transition-opacity duration-100 group-hover/tip:opacity-100"
      >
        {label}
      </span>
    </span>
  );
}
