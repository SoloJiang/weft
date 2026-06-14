import { useTranslation } from "react-i18next";
import { ArrowRight } from "lucide-react";

import { Button } from "../../components/ui/Button";

export interface ActionCardAction {
  id: string;
  label: string;
  kind: "add" | "new" | "clone";
}

export interface ActionCardBlockProps {
  title: string;
  body?: string | null;
  steps?: string[];
  actions: ActionCardAction[];
  /** When true, all buttons are disabled and a hint appears explaining why. */
  readOnly: boolean;
  /** Map of actionId → in-flight (disables the specific button + shows ellipsis). */
  busy: Record<string, boolean>;
  onAction: (action: ActionCardAction) => void;
}

export function ActionCardBlock({
  title,
  body,
  steps,
  actions,
  readOnly,
  busy,
  onAction,
}: ActionCardBlockProps) {
  const { t } = useTranslation();
  return (
    <div className="rounded-[var(--radius-lg)] border border-border bg-surface px-3.5 py-3">
      {title ? <div className="text-sm font-medium text-ink">{title}</div> : null}
      {body ? <div className="mt-1 max-w-[62ch] text-xs leading-relaxed text-ink-muted">{body}</div> : null}
      {steps && steps.length > 0 ? (
        <ol className="mt-3 grid gap-1.5 text-xs text-ink-muted">
          {steps.map((step, i) => (
            <li key={`${i}-${step}`} className="flex gap-2">
              <span className="mt-px grid h-4 w-4 shrink-0 place-items-center rounded-full bg-bg font-mono text-[10px] text-ink-faint">
                {i + 1}
              </span>
              <span className="min-w-0 leading-relaxed">{step}</span>
            </li>
          ))}
        </ol>
      ) : null}
      {actions.length > 0 ? (
        <div className="mt-2.5 flex flex-wrap gap-1.5">
          {actions.map((a) => {
            const isBusy = !!busy[a.id];
            return (
              <Button
                key={a.id}
                variant="default"
                size="sm"
                disabled={readOnly || isBusy}
                onClick={() => onAction(a)}
              >
                {isBusy ? "…" : a.label}
                {!isBusy && <ArrowRight size={12} />}
              </Button>
            );
          })}
        </div>
      ) : null}
      {readOnly ? (
        <div className="mt-2 text-xs text-ink-faint">{t("actionCard.readOnlyHint")}</div>
      ) : null}
    </div>
  );
}
