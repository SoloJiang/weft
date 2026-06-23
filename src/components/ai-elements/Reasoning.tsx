import type { ReactNode } from "react";
import { BrainCircuit } from "lucide-react";
import { cn } from "../../lib/cn";
import { Shimmer } from "./Shimmer";

export function Reasoning({
  title,
  children,
  active = false,
  className,
}: {
  readonly title: string;
  readonly children?: ReactNode;
  readonly active?: boolean;
  readonly className?: string;
}) {
  return (
    <section
      className={cn(
        "rounded-[var(--radius-md)] border border-border bg-raised/70 px-3 py-2 text-[12px] text-ink-muted",
        className,
      )}
    >
      <div className="flex items-center gap-2 text-ink">
        <BrainCircuit size={13} className={active ? "text-running" : "text-ink-faint"} />
        <span className="font-medium">{title}</span>
        {active && <Shimmer className="ml-auto h-2 w-16" />}
      </div>
      {children && <div className="mt-2 leading-relaxed">{children}</div>}
    </section>
  );
}
