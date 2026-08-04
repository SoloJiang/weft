import { useTranslation } from "react-i18next";
import type { IssueReadiness, ReadinessReason, ReadinessReasonCode } from "../lib/types";

interface ReadinessPresentation {
  color: string;
  dot: string;
  labelKey: string;
}

/** The one frontend mapping from backend readiness discriminators to UI. */
const PRESENTATION: Record<IssueReadiness, ReadinessPresentation> = {
  review_ready: {
    color: "text-accent ring-accent/25",
    dot: "bg-accent",
    labelKey: "readiness.status.review_ready",
  },
  blocked: {
    color: "text-danger ring-danger/25",
    dot: "bg-danger",
    labelKey: "readiness.status.blocked",
  },
  needs_you: {
    color: "text-waiting ring-waiting/30",
    dot: "bg-waiting",
    labelKey: "readiness.status.needs_you",
  },
  unknown: {
    color: "text-ink-muted ring-border",
    dot: "bg-ink-faint",
    labelKey: "readiness.status.unknown",
  },
  failed: {
    color: "text-danger ring-danger/35",
    dot: "bg-danger",
    labelKey: "readiness.status.failed",
  },
};

const REASON_KEYS: Record<ReadinessReasonCode, string> = {
  no_active_lanes: "readiness.reason.no_active_lanes",
  upstream_unmet: "readiness.reason.upstream_unmet",
  evidence_missing: "readiness.reason.evidence_missing",
  remote_unknown: "readiness.reason.remote_unknown",
  execution_drifted: "readiness.reason.execution_drifted",
  policy_gate_pending: "readiness.reason.policy_gate_pending",
  open_need: "readiness.reason.open_need",
  checks_failing: "readiness.reason.checks_failing",
  checks_unknown: "readiness.reason.checks_unknown",
  worker_failed: "readiness.reason.worker_failed",
  in_progress: "readiness.reason.in_progress",
  pr_ci_pending: "readiness.reason.pr_ci_pending",
  pr_ci_failing: "readiness.reason.pr_ci_failing",
  pr_review_changes_requested: "readiness.reason.pr_review_changes_requested",
  pr_threads_unresolved: "readiness.reason.pr_threads_unresolved",
  pr_conflict: "readiness.reason.pr_conflict",
  pr_closed_unmerged: "readiness.reason.pr_closed_unmerged",
};

const LOADING_REASON: ReadinessReason = {
  code: "evidence_missing",
  direction_id: null,
};

export function ReadinessChip({
  readiness,
  reasons,
  className,
}: {
  readiness?: IssueReadiness;
  reasons?: ReadinessReason[];
  className?: string;
}) {
  const { t } = useTranslation();
  const resolvedReadiness = readiness ?? "unknown";
  const presentation = PRESENTATION[resolvedReadiness];
  let displayedReasons = reasons ?? [];
  if (readiness == null && displayedReasons.length === 0) {
    displayedReasons = [LOADING_REASON];
  }
  const reasonText = displayedReasons.map((reason) => t(REASON_KEYS[reason.code])).join(" · ");
  const classNames = [
    "inline-flex min-w-0 items-center gap-1.5 rounded-full bg-raised px-2 py-0.5 text-[10.5px] font-medium ring-1 ring-inset",
    presentation.color,
  ];
  if (className) {
    classNames.push(className);
  }

  return (
    <span className={classNames.join(" ")} title={reasonText}>
      <span className={`h-1.5 w-1.5 shrink-0 rounded-full ${presentation.dot}`} />
      <span className="shrink-0">{t(presentation.labelKey)}</span>
      {reasonText && <span className="min-w-0 truncate text-ink-faint">{reasonText}</span>}
    </span>
  );
}
