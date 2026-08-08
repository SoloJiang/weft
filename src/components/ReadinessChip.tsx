import { CircleAlert } from "lucide-react";
import { useTranslation } from "react-i18next";
import type { ReadinessFetchState } from "../lib/readinessKey";
import type {
  IssueReadiness,
  IssueReadinessDto,
  ReadinessReason,
  ReadinessReasonCode,
} from "../lib/types";

interface ReadinessPresentation {
  color: string;
  dot: string;
  labelKey: string;
  glyph: "dot" | "alert";
}

type ReadinessChipVariant = IssueReadiness | "unavailable";
type ReadinessChipDto = Pick<IssueReadinessDto, "readiness" | "reasons">;

/** The one frontend mapping from backend readiness discriminators to UI. */
const PRESENTATION: Record<ReadinessChipVariant, ReadinessPresentation> = {
  review_ready: {
    color: "text-accent ring-accent/25",
    dot: "bg-accent",
    labelKey: "readiness.status.review_ready",
    glyph: "dot",
  },
  blocked: {
    color: "text-danger ring-danger/25",
    dot: "bg-danger",
    labelKey: "readiness.status.blocked",
    glyph: "dot",
  },
  needs_you: {
    color: "text-waiting ring-waiting/30",
    dot: "bg-waiting",
    labelKey: "readiness.status.needs_you",
    glyph: "dot",
  },
  unknown: {
    color: "text-ink-muted ring-border",
    dot: "bg-ink-faint",
    labelKey: "readiness.status.unknown",
    glyph: "dot",
  },
  failed: {
    color: "text-danger ring-danger/35",
    dot: "bg-danger",
    labelKey: "readiness.status.failed",
    glyph: "dot",
  },
  unavailable: {
    color: "text-danger ring-danger/35",
    dot: "bg-danger",
    labelKey: "readiness.status.unavailable",
    glyph: "alert",
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

interface ChipView {
  variant: ReadinessChipVariant;
  reasons: ReadinessReason[];
}

function chipView(state: ReadinessFetchState<ReadinessChipDto>): ChipView {
  switch (state.kind) {
    case "loading":
      return { variant: "unknown", reasons: [LOADING_REASON] };
    case "failed":
      return { variant: "unavailable", reasons: [] };
    case "ready":
      return { variant: state.dto.readiness, reasons: state.dto.reasons };
  }
}

function ReadinessGlyph({ presentation }: { presentation: ReadinessPresentation }) {
  if (presentation.glyph === "alert") {
    return <CircleAlert size={12} aria-hidden="true" />;
  }
  return <span className={`h-1.5 w-1.5 shrink-0 rounded-full ${presentation.dot}`} />;
}

export function ReadinessChip({
  state,
  className,
}: {
  state: ReadinessFetchState<ReadinessChipDto>;
  className?: string;
}) {
  const { t } = useTranslation();
  const view = chipView(state);
  const presentation = PRESENTATION[view.variant];
  const reasonText = view.reasons.map((reason) => t(REASON_KEYS[reason.code])).join(" · ");
  const classNames = [
    "inline-flex min-w-0 items-center gap-1.5 rounded-full bg-raised px-2 py-0.5 text-[10.5px] font-medium ring-1 ring-inset",
    presentation.color,
  ];
  if (className) {
    classNames.push(className);
  }

  return (
    <span className={classNames.join(" ")} title={reasonText}>
      <ReadinessGlyph presentation={presentation} />
      <span className="shrink-0">{t(presentation.labelKey)}</span>
      {reasonText && <span className="min-w-0 truncate text-ink-faint">{reasonText}</span>}
    </span>
  );
}
