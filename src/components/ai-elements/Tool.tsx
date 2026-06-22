import { useState, type ComponentType } from "react";
import { ChevronRight, type LucideProps } from "lucide-react";
import { cn } from "../../lib/cn";

export type AiToolStatus = "streaming" | "complete" | "error";

type ToolIcon = ComponentType<LucideProps>;

export function Tool({
  icon: Icon,
  label,
  status,
  target,
  summary,
  added,
  removed,
  input,
  output,
  inputLabel,
  outputLabel,
  showMoreLabel,
  showLessLabel,
}: {
  readonly icon: ToolIcon;
  readonly label: string;
  readonly status: AiToolStatus;
  readonly target?: string;
  readonly summary?: string;
  readonly added?: string;
  readonly removed?: string;
  readonly input?: string;
  readonly output?: string;
  readonly inputLabel: string;
  readonly outputLabel: string;
  readonly showMoreLabel: (hiddenLineCount: number) => string;
  readonly showLessLabel: string;
}) {
  const [open, setOpen] = useState(false);
  const hasDetail = (input && input.length > 0) || (output && output.length > 0);

  return (
    <div>
      <button
        type="button"
        disabled={!hasDetail}
        onClick={() => setOpen((value) => !value)}
        className={cn(
          "group flex w-full items-center gap-1.5 rounded-[var(--radius-sm)] px-1.5 py-1 text-left text-[12.5px]",
          hasDetail && "hover:bg-surface/60",
        )}
      >
        <Icon
          size={13}
          className={cn(
            "shrink-0",
            status === "streaming" && "animate-pulse text-running",
            status === "error" && "text-danger",
            status === "complete" && "text-ink-faint",
          )}
        />
        <span className="shrink-0 text-ink-muted">{label}</span>
        {(target || summary) && (
          <span className="min-w-0 truncate font-mono text-ink-faint">
            {target || summary}
          </span>
        )}
        {added != null && <span className="shrink-0 font-mono text-running">+{added}</span>}
        {removed != null && <span className="shrink-0 font-mono text-danger">-{removed}</span>}
        {hasDetail && (
          <ChevronRight
            size={12}
            className={cn(
              "ml-auto shrink-0 text-ink-faint/60 transition-transform",
              open && "rotate-90",
            )}
          />
        )}
      </button>
      {open && hasDetail && (
        <div className="space-y-2 py-1.5 pl-[26px] pr-1.5">
          {input && (
            <ToolBlock
              label={inputLabel}
              body={input}
              showMoreLabel={showMoreLabel}
              showLessLabel={showLessLabel}
            />
          )}
          {output && (
            <ToolBlock
              label={outputLabel}
              body={output}
              tone={status === "error" ? "error" : "default"}
              showMoreLabel={showMoreLabel}
              showLessLabel={showLessLabel}
            />
          )}
        </div>
      )}
    </div>
  );
}

export function ToolActivity({
  icon: Icon,
  label,
  target,
  summary,
  added,
  removed,
}: {
  readonly icon: ToolIcon;
  readonly label: string;
  readonly target?: string;
  readonly summary?: string;
  readonly added?: string;
  readonly removed?: string;
}) {
  return (
    <div className="flex max-w-full items-center gap-2 px-1.5 py-1 text-[13px] text-ink-faint">
      <span className="h-1.5 w-1.5 shrink-0 animate-pulse rounded-full bg-running" />
      <Icon size={15} className="shrink-0 text-ink-faint" />
      <span className="shrink-0 font-medium text-ink-muted">{label}</span>
      {target && <span className="min-w-0 truncate font-mono text-brand">{target}</span>}
      {!target && summary && <span className="min-w-0 truncate font-mono text-brand">{summary}</span>}
      {added != null && <span className="shrink-0 font-mono text-running">+{added}</span>}
      {removed != null && <span className="shrink-0 font-mono text-danger">-{removed}</span>}
    </div>
  );
}

function ToolBlock({
  label,
  body,
  showMoreLabel,
  showLessLabel,
  tone = "default",
}: {
  readonly label: string;
  readonly body: string;
  readonly showMoreLabel: (hiddenLineCount: number) => string;
  readonly showLessLabel: string;
  readonly tone?: "default" | "error";
}) {
  const [expanded, setExpanded] = useState(false);
  const lines = body.split("\n");
  const lineLimit = 20;
  const long = lines.length > lineLimit;
  const shown = expanded || !long ? body : lines.slice(0, lineLimit).join("\n");
  return (
    <div>
      <p className="mb-1 text-[10.5px] font-medium uppercase tracking-wide text-ink-faint">
        {label}
      </p>
      <pre
        className={cn(
          "max-h-80 overflow-auto whitespace-pre-wrap break-words rounded bg-bg px-2 py-1.5 font-mono text-[11.5px] leading-relaxed",
          tone === "error" ? "text-danger" : "text-ink-muted",
        )}
      >
        {shown}
      </pre>
      {long && (
        <button
          type="button"
          onClick={() => setExpanded((value) => !value)}
          className="mt-1 text-[11px] text-brand hover:underline"
        >
          {expanded ? showLessLabel : showMoreLabel(lines.length - lineLimit)}
        </button>
      )}
    </div>
  );
}
