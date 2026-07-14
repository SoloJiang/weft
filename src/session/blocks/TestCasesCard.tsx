import { useTranslation } from "react-i18next";
import { ListTree, ArrowRight } from "lucide-react";

import { Button } from "../../components/ui/Button";

/**
 * Timeline summary card for the issue's test-case document (kind="test_cases").
 * The document's single source of truth is the test_plan table — this card only
 * shows the summary the engine derived at emit time, plus the panel entry point.
 */
export function TestCasesCard({
  title,
  branches,
  caseCount,
  onOpen,
}: {
  title: string;
  branches: string[];
  caseCount: number;
  /** Open the TestPlanPanel; absent on read-only hosts (worker timelines). */
  onOpen?: () => void;
}) {
  const { t } = useTranslation();
  return (
    <div className="rounded-[var(--radius-lg)] border border-border bg-surface px-3.5 py-3">
      <div className="flex items-center gap-2">
        <span className="grid h-6 w-6 shrink-0 place-items-center rounded-[var(--radius-sm)] bg-brand-ghost text-brand">
          <ListTree size={13} />
        </span>
        <span className="min-w-0 truncate text-sm font-medium text-ink">
          {title || t("testPlan.defaultTitle")}
        </span>
        <span className="ml-auto shrink-0 text-[11px] tabular-nums text-ink-faint">
          {t("testPlan.caseCount", { count: caseCount })}
        </span>
      </div>
      {branches.length > 0 && (
        <div className="mt-2 flex flex-wrap gap-1.5">
          {branches.map((b) => (
            <span
              key={b}
              className="max-w-[24ch] truncate rounded-[var(--radius-sm)] border border-border bg-bg px-2 py-0.5 text-[11.5px] text-ink-muted"
            >
              {b}
            </span>
          ))}
        </div>
      )}
      {onOpen && (
        <div className="mt-2.5">
          <Button variant="default" size="sm" onClick={onOpen}>
            {t("testPlan.open")}
            <ArrowRight size={12} />
          </Button>
        </div>
      )}
    </div>
  );
}
