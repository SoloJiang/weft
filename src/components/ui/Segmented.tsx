import type { ReactNode } from "react";
import { cn } from "../../lib/cn";

/** A compact pill-toggle group for a small, closed set of mutually exclusive
 *  options (theme, language, default tool, …) — reused wherever a Select
 *  would be overkill for 2-4 choices. */
export function Segmented({
  value,
  onChange,
  options,
}: {
  value: string;
  onChange: (v: string) => void;
  options: { value: string; label: string; icon?: ReactNode }[];
}) {
  return (
    <div className="inline-flex items-center gap-0.5 rounded-[var(--radius-md)] bg-bg p-0.5">
      {options.map((o) => (
        <button
          key={o.value}
          type="button"
          onClick={() => onChange(o.value)}
          className={cn(
            "flex h-[28px] items-center gap-1.5 whitespace-nowrap rounded-[var(--radius-sm)] px-3 text-[12px] font-medium transition-colors duration-150",
            value === o.value ? "bg-raised text-ink" : "text-ink-muted hover:text-ink",
          )}
        >
          {o.icon}
          {o.label}
        </button>
      ))}
    </div>
  );
}
