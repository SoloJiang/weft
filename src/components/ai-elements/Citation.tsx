import type { ReactNode } from "react";
import { Link2 } from "lucide-react";
import { cn } from "../../lib/cn";

export function Citation({
  index,
  children,
  className,
}: {
  readonly index: number;
  readonly children?: ReactNode;
  readonly className?: string;
}) {
  return (
    <span
      className={cn(
        "inline-flex items-center gap-1 rounded-full border border-border bg-raised px-1.5 py-px align-baseline text-[10.5px] font-medium text-brand",
        className,
      )}
    >
      <Link2 size={10} />
      <span>{index}</span>
      {children}
    </span>
  );
}

export function Sources({
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
      <p className="mb-2 font-medium text-ink">{title}</p>
      <div className="flex flex-col gap-1.5">{children}</div>
    </section>
  );
}

export function Source({ children }: { readonly children: ReactNode }) {
  return <div className="min-w-0 truncate text-ink-muted">{children}</div>;
}
