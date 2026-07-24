import { useTranslation } from "react-i18next";
import { AlertTriangle, Check } from "lucide-react";

import { Markdown } from "../../components/Markdown";

export interface PlanSummaryProps {
  requirements: string[];
  /** Issue-level technical plan, markdown. */
  approach: string;
  /** Open risks that survived the lead's blind-spot pass. */
  risks: string[];
  /** Timeline cwd so file refs in the approach markdown resolve/open. */
  cwd?: string;
}

/**
 * Requirements + approach + open risks — the part of a plan_card that is pure
 * context (no split, no approve button), shared by two surfaces that must never
 * visually drift apart: the chat plan_card (`PlanCardBlock`) and the merged
 * ScopeReview dialog (issue #104 confirmation-chain compression, which folds
 * this same content into the split/worktree review so "approach + split" reads
 * on one screen instead of requiring a scroll back into chat).
 */
export function PlanSummary({ requirements, approach, risks, cwd }: PlanSummaryProps) {
  const { t } = useTranslation();
  return (
    <>
      {requirements.length > 0 ? (
        <section className="mt-3 first:mt-0">
          <div className="text-[11px] font-medium uppercase tracking-wide text-ink-faint">
            {t("planCard.requirements")}
          </div>
          <ul className="mt-1.5 grid gap-1 text-xs text-ink-muted">
            {requirements.map((r, i) => (
              <li key={`${i}-${r}`} className="flex gap-2">
                <Check size={13} className="mt-px shrink-0 text-ink-faint" />
                <span className="min-w-0 leading-relaxed">{r}</span>
              </li>
            ))}
          </ul>
        </section>
      ) : null}
      {approach ? (
        <section className="mt-3 first:mt-0">
          <div className="text-[11px] font-medium uppercase tracking-wide text-ink-faint">
            {t("planCard.approach")}
          </div>
          <div className="mt-1.5 max-w-[62ch] text-xs leading-relaxed text-ink-muted">
            <Markdown text={approach} cwd={cwd} />
          </div>
        </section>
      ) : null}
      {risks.length > 0 ? (
        <section className="mt-3 first:mt-0">
          <div className="text-[11px] font-medium uppercase tracking-wide text-ink-faint">
            {t("planCard.risks")}
          </div>
          <ul className="mt-1.5 grid gap-1 text-xs text-ink-muted">
            {risks.map((r, i) => (
              <li key={`${i}-${r}`} className="flex gap-2">
                <AlertTriangle size={13} className="mt-px shrink-0 text-amber-500" />
                <span className="min-w-0 leading-relaxed">{r}</span>
              </li>
            ))}
          </ul>
        </section>
      ) : null}
    </>
  );
}
