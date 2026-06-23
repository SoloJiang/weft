import type { ReactNode } from "react";
import { cn } from "../../lib/cn";

export function SuggestionChips({
  label,
  suggestions,
  className,
}: {
  readonly label: string;
  readonly suggestions: readonly string[];
  readonly className?: string;
}) {
  if (suggestions.length === 0) return null;
  return (
    <div className={cn("mt-4", className)}>
      <p className="mb-2 text-[11px] font-medium uppercase tracking-wide text-ink-faint">
        {label}
      </p>
      <div className="flex flex-wrap justify-center gap-1.5">
        {suggestions.map((suggestion) => (
          <span
            key={suggestion}
            className="rounded-full border border-border bg-raised px-2.5 py-1 text-[11.5px] text-ink-muted"
          >
            {suggestion}
          </span>
        ))}
      </div>
    </div>
  );
}

export function OnboardingCue({
  eyebrow,
  title,
  body,
  icon,
  className,
}: {
  readonly eyebrow: string;
  readonly title: string;
  readonly body: string;
  readonly icon: ReactNode;
  readonly className?: string;
}) {
  return (
    <div
      className={cn(
        "rounded-[var(--radius-lg)] border border-border bg-surface px-4 py-3 text-left shadow-[0_12px_34px_-28px_rgba(0,0,0,0.65)]",
        className,
      )}
    >
      <div className="flex items-start gap-3">
        <span className="grid h-8 w-8 shrink-0 place-items-center rounded-[var(--radius-md)] bg-brand-ghost text-brand">
          {icon}
        </span>
        <div className="min-w-0">
          <p className="text-[10.5px] font-medium uppercase tracking-wide text-ink-faint">
            {eyebrow}
          </p>
          <p className="mt-1 text-[13px] font-semibold text-ink">{title}</p>
          <p className="mt-1 text-[12px] leading-relaxed text-ink-muted">{body}</p>
        </div>
      </div>
    </div>
  );
}
