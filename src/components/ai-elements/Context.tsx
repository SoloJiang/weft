import type { ReactNode } from "react";
import { PanelsTopLeft } from "lucide-react";
import { cn } from "../../lib/cn";

export function ContextPanel({
  title,
  children,
  className,
}: {
  readonly title: string;
  readonly children: ReactNode;
  readonly className?: string;
}) {
  return (
    <section
      className={cn(
        "rounded-[var(--radius-md)] border border-border bg-surface px-3 py-2 text-[12px] text-ink-muted",
        className,
      )}
    >
      <div className="mb-2 flex items-center gap-2 text-ink">
        <PanelsTopLeft size={13} className="text-brand" />
        <span className="font-medium">{title}</span>
      </div>
      <div className="space-y-1.5">{children}</div>
    </section>
  );
}

export function ContextItem({
  label,
  value,
}: {
  readonly label: string;
  readonly value: ReactNode;
}) {
  return (
    <div className="flex min-w-0 items-center gap-2">
      <span className="shrink-0 text-ink-faint">{label}</span>
      <span className="min-w-0 truncate font-mono text-ink">{value}</span>
    </div>
  );
}
