import { useState } from "react";
import { useTranslation } from "react-i18next";
import { AlertTriangle, Check, GitBranch, ListTree } from "lucide-react";

import { Button } from "../../components/ui/Button";
import { Markdown } from "../../components/Markdown";

/** Coarse direction preview inside a plan card (not the real proposal — the
 *  Needs-you direction card stays the authoritative confirmation surface). */
export interface PlanCardSplitItem {
  name: string;
  repo: string;
  reason?: string;
}

export interface PlanCardBlockProps {
  title: string;
  requirements: string[];
  /** Issue-level technical plan, markdown. */
  approach: string;
  split: PlanCardSplitItem[];
  /** Open risks that survived the lead's blind-spot pass. */
  risks: string[];
  /** When true, the approve button is disabled and a hint explains why. */
  readOnly: boolean;
  /** Timeline cwd so file refs in the approach markdown resolve/open, matching
   *  regular assistant messages. */
  cwd?: string;
  /** Approve → post plan_decision to the lead + persist the settled state.
   *  The row collapses via the resolve push, so no local settled state here. */
  onApprove: () => Promise<void>;
  /** Test cases this plan is built against (0 = none / not shown). Makes the
   *  "cases inform the plan" thesis visible and jumps to the panel. */
  testCaseCount?: number;
  onOpenTestCases?: () => void;
}

export function PlanCardBlock({
  title,
  requirements,
  approach,
  split,
  risks,
  readOnly,
  cwd,
  onApprove,
  testCaseCount = 0,
  onOpenTestCases,
}: PlanCardBlockProps) {
  const { t } = useTranslation();
  const [busy, setBusy] = useState(false);
  const approve = async () => {
    setBusy(true);
    try {
      await onApprove();
    } finally {
      setBusy(false);
    }
  };
  return (
    <div className="rounded-[var(--radius-lg)] border border-border bg-surface px-3.5 py-3">
      <div className="flex items-center gap-2">
        <span className="shrink-0 rounded-[var(--radius-sm)] bg-brand-ghost px-1.5 py-0.5 text-[10px] font-medium text-brand">
          {t("planCard.label")}
        </span>
        {title ? <span className="min-w-0 truncate text-sm font-medium text-ink">{title}</span> : null}
      </div>
      {requirements.length > 0 ? (
        <section className="mt-3">
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
        <section className="mt-3">
          <div className="text-[11px] font-medium uppercase tracking-wide text-ink-faint">
            {t("planCard.approach")}
          </div>
          <div className="mt-1.5 max-w-[62ch] text-xs leading-relaxed text-ink-muted">
            <Markdown text={approach} cwd={cwd} />
          </div>
        </section>
      ) : null}
      {split.length > 0 ? (
        <section className="mt-3">
          <div className="text-[11px] font-medium uppercase tracking-wide text-ink-faint">
            {t("planCard.split")}
          </div>
          <ul className="mt-1.5 grid gap-1 text-xs text-ink-muted">
            {split.map((s, i) => (
              <li key={`${i}-${s.name}`} className="flex items-start gap-2">
                <GitBranch size={13} className="mt-px shrink-0 text-ink-faint" />
                <span className="min-w-0 leading-relaxed">
                  <span className="font-medium text-ink">{s.name}</span>
                  <span className="mx-1 rounded-[var(--radius-sm)] bg-bg px-1 py-px font-mono text-[10px] text-ink-faint">
                    {s.repo}
                  </span>
                  {s.reason ? <span>{s.reason}</span> : null}
                </span>
              </li>
            ))}
          </ul>
        </section>
      ) : null}
      {risks.length > 0 ? (
        <section className="mt-3">
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
      {testCaseCount > 0 && onOpenTestCases ? (
        <button
          type="button"
          onClick={onOpenTestCases}
          className="mt-3 flex w-full items-center gap-2 rounded-[var(--radius-md)] border border-border bg-bg px-2.5 py-1.5 text-left text-xs text-ink-muted transition-colors hover:border-brand/40 hover:text-ink"
        >
          <ListTree size={13} className="shrink-0 text-brand" />
          <span className="min-w-0 flex-1">{t("planCard.basedOnCases", { count: testCaseCount })}</span>
          <span className="shrink-0 text-ink-faint">{t("planCard.viewCases")}</span>
        </button>
      ) : null}
      <div className="mt-3 flex flex-wrap items-center gap-2">
        <Button variant="default" size="sm" disabled={readOnly || busy} onClick={() => void approve()}>
          {busy ? "…" : t("planCard.approve")}
          {!busy && <Check size={12} />}
        </Button>
        {!readOnly ? (
          <span className="text-xs text-ink-faint">{t("planCard.reviseHint")}</span>
        ) : null}
      </div>
      {readOnly ? (
        <div className="mt-2 text-xs text-ink-faint">{t("planCard.readOnlyHint")}</div>
      ) : null}
    </div>
  );
}
