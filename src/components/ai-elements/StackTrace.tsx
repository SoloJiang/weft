import { Bug } from "lucide-react";
import { cn } from "../../lib/cn";

export function StackTrace({
  title,
  message,
  stack,
  className,
}: {
  readonly title: string;
  readonly message: string;
  readonly stack: string | undefined;
  readonly className?: string;
}) {
  const body = stack && stack.trim().length > 0 ? stack : message;
  return (
    <section className={cn("mt-3 w-full text-left", className)}>
      <div className="mb-1.5 flex items-center gap-1.5 text-[11px] font-medium uppercase tracking-wide text-ink-faint">
        <Bug size={12} />
        <span>{title}</span>
      </div>
      <pre className="max-h-48 overflow-auto whitespace-pre-wrap break-words rounded-[var(--radius-md)] border border-border bg-bg px-3 py-2 font-mono text-[11px] leading-relaxed text-danger">
        {body}
      </pre>
    </section>
  );
}
