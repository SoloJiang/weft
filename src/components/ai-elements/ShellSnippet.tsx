import { useState } from "react";
import { Check, Copy, TerminalSquare } from "lucide-react";
import { cn } from "../../lib/cn";

export function ShellSnippet({
  code,
  label,
  copyLabel,
  copiedLabel,
  className,
}: {
  readonly code: string;
  readonly label: string;
  readonly copyLabel: string;
  readonly copiedLabel: string;
  readonly className?: string;
}) {
  const [copied, setCopied] = useState(false);
  const buttonLabel = copied ? copiedLabel : copyLabel;

  const copyCode = () => {
    void navigator.clipboard?.writeText(code);
    setCopied(true);
    window.setTimeout(() => setCopied(false), 1600);
  };

  return (
    <figure
      className={cn(
        "my-2 overflow-hidden rounded-[var(--radius-md)] border border-border bg-bg text-[11.5px]",
        className,
      )}
    >
      <figcaption className="flex items-center gap-1.5 border-b border-border bg-surface px-3 py-1.5 text-[11px] font-medium text-ink-muted">
        <TerminalSquare size={12} className="text-brand" />
        <span>{label}</span>
        <button
          type="button"
          onClick={copyCode}
          aria-label={buttonLabel}
          title={buttonLabel}
          className="ml-auto inline-flex h-5 items-center gap-1 rounded-[var(--radius-sm)] px-1.5 text-[10.5px] text-ink-faint transition-colors hover:bg-raised hover:text-ink focus-visible:bg-raised focus-visible:text-ink"
        >
          {copied ? <Check size={11} className="text-running" /> : <Copy size={11} />}
          <span>{buttonLabel}</span>
        </button>
      </figcaption>
      <div className="whitespace-pre-wrap break-words p-3 font-mono leading-relaxed text-ink">
        <code>{code}</code>
      </div>
    </figure>
  );
}
