import { useTranslation } from "react-i18next";
import { ArrowRight, GitBranch, GitPullRequest, ScanEye } from "lucide-react";
import type {
  ChangeSetLane,
  ChangeSetPullRequest,
  CheckEvidence,
  ExecutionReconciliation,
  UpstreamEvidence,
} from "../lib/types";
import type { ReadinessFetchState } from "../lib/readinessKey";
import { ReadinessChip } from "../components/ReadinessChip";
import type { IssueDelivery } from "./issueDelivery";
import { cn } from "../lib/cn";
import {
  changeSetPanelState,
  laneCheckoutView,
  laneEvidenceView,
  laneStatusView,
  type ChangeSetPanelState,
  type CheckoutRowView,
  type ChangeSetWave,
  type LaneCheckoutView,
  type LaneEvidenceView,
  type LaneStatusView,
} from "./changeSetView";

/**
 * The Issue Change Set (issue #175): one delivery overview for the whole
 * issue — write scope and why, dependency order, declared-vs-observed
 * checkout, evidence trust, host state, and what is left.
 *
 * A pure function of the board's single delivery read — the same split
 * `ReadinessChip` and `EvidenceBody` use. It deliberately does NOT fetch:
 * `issue_change_set` and `issue_readiness` run the same collection and the same
 * Git probes, so a second fetch here would contend with the board's own and
 * make this panel disagree with the chip above it (see `issueDelivery.ts`).
 * Every verdict shown was decided by the backend; `changeSetView` only shapes
 * it. Nothing here recomputes a readiness boolean.
 */
export function ChangeSetPanel({
  state,
  onOpenLane,
}: {
  state: ReadinessFetchState<IssueDelivery>;
  onOpenLane: (directionId: number, repoId: number) => void;
}) {
  const { t } = useTranslation();
  return (
    <div className="min-h-0 flex-1 overflow-auto px-5 py-4">
      <ChangeSetBody
        state={changeSetPanelState(state)}
        onOpenLane={onOpenLane}
        emptyLabel={t("changeSet.empty")}
      />
    </div>
  );
}

function ChangeSetBody({
  state,
  onOpenLane,
  emptyLabel,
}: {
  state: ChangeSetPanelState;
  onOpenLane: (directionId: number, repoId: number) => void;
  emptyLabel: string;
}) {
  const { t } = useTranslation();
  switch (state.kind) {
    case "loading":
      return <div className="text-[12px] text-ink-faint">{t("changeSet.loading")}</div>;
    case "error":
      return <div className="text-[12px] text-danger">{t("changeSet.loadFailed")}</div>;
    case "empty":
      return <div className="text-[12px] text-ink-faint">{emptyLabel}</div>;
    case "ready":
      return (
        <div className="flex flex-col gap-4">
          <header className="flex flex-wrap items-center gap-2">
            <h2 className="text-[13px] font-semibold text-ink">{t("changeSet.title")}</h2>
            <ReadinessChip
              state={{ kind: "ready", dto: state.changeSet }}
              className="max-w-[28rem]"
            />
            <span className="text-[11.5px] text-ink-faint">
              {t("changeSet.laneCount", { count: state.changeSet.active_lane_count })}
            </span>
          </header>
          {state.waves.map((wave, index) => (
            // Keyed by position: several lanes can share direction_id 0, so an
            // id-derived key is not unique among siblings.
            <section key={`wave-${index}`} className="flex flex-col gap-2">
              <WaveHeader wave={wave} index={index} />
              <div className="flex flex-col gap-2">
                {wave.lanes.map((lane, position) => (
                  <LaneRow
                    key={`${lane.direction_id}:${lane.name}:${position}`}
                    lane={lane}
                    evidenceScanTruncated={state.changeSet.evidence_scan_truncated}
                    onOpenLane={onOpenLane}
                  />
                ))}
              </div>
            </section>
          ))}
        </div>
      );
  }
}

/** A cycle is not a step, so it never gets a step number or the
 *  "waits on the step above" caption — both would describe an order that does
 *  not exist. */
function WaveHeader({ wave, index }: { wave: ChangeSetWave; index: number }) {
  const { t } = useTranslation();
  if (wave.kind === "cycle") {
    return (
      <div className="flex items-center gap-2 text-[10.5px] font-semibold uppercase tracking-wider text-danger">
        <span>{t("changeSet.cycle")}</span>
        <span className="normal-case font-normal tracking-normal text-ink-faint">
          {t("changeSet.cycleBody")}
        </span>
      </div>
    );
  }
  return (
    <div className="flex items-center gap-2 text-[10.5px] font-semibold uppercase tracking-wider text-ink-faint">
      <span>{t("changeSet.wave", { index: index + 1 })}</span>
      {index > 0 && <ArrowRight size={11} aria-hidden="true" />}
      {index > 0 && (
        <span className="normal-case tracking-normal font-normal">{t("changeSet.waveWaits")}</span>
      )}
    </div>
  );
}

const RECONCILIATION_KEYS: Record<ExecutionReconciliation, string> = {
  matched: "changeSet.reconciliation.matched",
  drifted: "changeSet.reconciliation.drifted",
  unknown: "changeSet.reconciliation.unknown",
};

const CHECKS_KEYS: Record<CheckEvidence, string> = {
  not_applicable: "changeSet.checks.not_applicable",
  not_produced: "changeSet.checks.not_produced",
  passed: "changeSet.checks.passed",
  failing: "changeSet.checks.failing",
};

const STATUS_KEYS: Record<LaneStatusView, string> = {
  queued: "changeSet.status.queued",
  planning: "changeSet.status.planning",
  working: "changeSet.status.working",
  review: "changeSet.status.review",
  done: "changeSet.status.done",
  unknown: "changeSet.status.unknown",
  not_materialized: "changeSet.status.not_materialized",
};

const UPSTREAM_KEYS: Record<UpstreamEvidence, string> = {
  satisfied: "changeSet.upstream.satisfied",
  unmet: "changeSet.upstream.unmet",
  unknown: "changeSet.upstream.unknown",
};

function LaneRow({
  lane,
  evidenceScanTruncated,
  onOpenLane,
}: {
  lane: ChangeSetLane;
  evidenceScanTruncated: boolean;
  onOpenLane: (directionId: number, repoId: number) => void;
}) {
  const { t } = useTranslation();
  const checkout = laneCheckoutView(lane);
  // A virtual lane (unbound PR row, issue-wide ask) has no direction to open,
  // and a lane whose write repo was never resolved has nothing to open it in.
  const openable = lane.direction_id !== 0 && lane.repo_id !== 0;
  return (
    <article className="flex flex-col gap-2 rounded-[var(--radius-lg)] border border-border bg-surface px-3 py-2.5">
      <div className="flex min-w-0 flex-wrap items-center gap-2">
        <span className="min-w-0 truncate text-[12.5px] font-semibold text-ink">{lane.name}</span>
        {lane.repo_name && (
          <span className="shrink-0 rounded-full bg-raised px-1.5 py-0.5 text-[10px] text-ink-muted">
            {lane.repo_name}
          </span>
        )}
        <ReadinessChip state={{ kind: "ready", dto: lane }} className="max-w-[20rem]" />
        <span className="ml-auto shrink-0 text-[10.5px] text-ink-faint">
          {t(STATUS_KEYS[laneStatusView(lane)])}
        </span>
        {openable && (
          <button
            type="button"
            onClick={() => onOpenLane(lane.direction_id, lane.repo_id)}
            title={t("changeSet.openLane")}
            aria-label={t("changeSet.openLane")}
            className="grid h-6 w-6 shrink-0 place-items-center rounded-[var(--radius-md)] text-ink-faint transition-colors hover:bg-brand-ghost hover:text-ink"
          >
            <ScanEye size={13} />
          </button>
        )}
      </div>

      {lane.reason && (
        <p className="text-[11px] leading-relaxed text-ink-muted" title={lane.reason}>
          {t("changeSet.whyRepo")}: {lane.reason}
        </p>
      )}

      <CheckoutView view={checkout} declaredBase={lane.checkout.declared_base} declaredBranch={lane.checkout.declared_branch} />

      <div className="flex flex-wrap items-center gap-x-3 gap-y-1 text-[10.5px] text-ink-faint">
        <span>{t(RECONCILIATION_KEYS[lane.reconciliation])}</span>
        <span>{t(CHECKS_KEYS[lane.checks])}</span>
        <span>{t(UPSTREAM_KEYS[lane.upstream])}</span>
        {lane.depends_on.length > 0 && (
          <span>{t("changeSet.dependsOn", { count: lane.depends_on.length })}</span>
        )}
        <EvidenceCounts view={laneEvidenceView(lane, evidenceScanTruncated)} />
      </div>

      {lane.pull_requests.length > 0 && (
        <div className="flex flex-col gap-0.5">
          {lane.pull_requests.map((pr) => (
            <PullRequestRow key={pr.id} pr={pr} />
          ))}
        </div>
      )}
    </article>
  );
}

/** Evidence trust, mapped exhaustively — "none recorded" is only ever claimed
 *  when the scan was complete enough to support it. */
function EvidenceCounts({ view }: { view: LaneEvidenceView }) {
  const { t } = useTranslation();
  switch (view.kind) {
    case "none":
      return <span>{t("changeSet.evidenceNone")}</span>;
    case "unscanned":
      return <span>{t("changeSet.evidenceUnscanned")}</span>;
    case "counts":
      return (
        <span>
          {t("changeSet.evidenceCounts", {
            fresh: view.fresh,
            stale: view.stale,
            unknown: view.unknown,
          })}
        </span>
      );
  }
}

const CI_KEYS: Record<string, string> = {
  unknown: "changeSet.pr.ciUnknown",
  not_configured: "changeSet.pr.ciNotConfigured",
  pending: "changeSet.pr.ciPending",
  passing: "changeSet.pr.ciPassing",
  failing: "changeSet.pr.ciFailing",
};

const REVIEW_KEYS: Record<string, string> = {
  unknown: "changeSet.pr.reviewUnknown",
  changes_requested: "changeSet.pr.reviewChangesRequested",
  awaiting_approval: "changeSet.pr.reviewAwaiting",
  approved: "changeSet.pr.reviewApproved",
};

const THREADS_KEYS: Record<string, string> = {
  unchecked: "changeSet.pr.threadsUnchecked",
  unknown: "changeSet.pr.threadsUnknown",
  all_resolved: "changeSet.pr.threadsResolved",
  unresolved: "changeSet.pr.threadsUnresolved",
};

const CONFLICT_KEYS: Record<string, string> = {
  unknown: "changeSet.pr.conflictUnknown",
  clean: "changeSet.pr.conflictClean",
  conflicting: "changeSet.pr.conflicting",
};

/**
 * One PR behind the lane's verdict, named and with its axes shown.
 *
 * A count alone ("2 pull requests") cannot tell the reader WHICH one is red,
 * which is the only thing they can act on. Every axis is a tagged union from
 * the backend, so each is mapped through a lookup with an explicit fallback —
 * a state this build does not know renders as unknown, never as a raw token.
 */
function PullRequestRow({ pr }: { pr: ChangeSetPullRequest }) {
  const { t } = useTranslation();
  const axis = (keys: Record<string, string>, state: string) =>
    t(keys[state] ?? "changeSet.pr.stateUnknown");
  const label = pr.number > 0 ? `#${pr.number}` : t("changeSet.pr.unidentified");
  return (
    <div className="flex min-w-0 flex-wrap items-center gap-x-2 gap-y-0.5 text-[10.5px] text-ink-faint">
      <GitPullRequest size={11} aria-hidden="true" />
      <span className="shrink-0 font-medium text-ink-muted">{label}</span>
      {pr.host_slug && <span className="shrink-0">{pr.host_slug}</span>}
      {pr.title && (
        <span className="min-w-0 truncate" title={pr.title}>
          {pr.title}
        </span>
      )}
      <span>{axis(CI_KEYS, pr.ci.state)}</span>
      <span>{axis(REVIEW_KEYS, pr.review.state)}</span>
      <span>{axis(THREADS_KEYS, pr.threads.state)}</span>
      <span>{axis(CONFLICT_KEYS, pr.conflict.state)}</span>
      {pr.probe_failed && <span className="text-waiting">{t("changeSet.pr.probeFailed")}</span>}
    </div>
  );
}

function CheckoutView({
  view,
  declaredBase,
  declaredBranch,
}: {
  view: LaneCheckoutView;
  declaredBase: string;
  declaredBranch: string;
}) {
  const { t } = useTranslation();
  const declared = (
    <div className="flex min-w-0 flex-wrap items-center gap-1.5 text-[10.5px] text-ink-faint">
      <GitBranch size={11} aria-hidden="true" />
      <span className="truncate text-ink-muted">{declaredBranch || t("changeSet.branchUnset")}</span>
      <span>←</span>
      <span className="truncate">{declaredBase || t("changeSet.baseDefault")}</span>
    </div>
  );

  switch (view.kind) {
    case "not_probed":
      return (
        <div className="flex flex-col gap-1">
          {declared}
          <span className="text-[10.5px] text-ink-faint">{t("changeSet.checkout.notProbed")}</span>
        </div>
      );
    case "none_registered":
      return (
        <div className="flex flex-col gap-1">
          {declared}
          <span className="text-[10.5px] text-ink-faint">
            {t("changeSet.checkout.noneRegistered")}
          </span>
        </div>
      );
    case "matched":
    case "drifted":
    case "unknown":
      return (
        <div className="flex flex-col gap-1">
          {declared}
          {view.rows.map((row) => (
            <CheckoutRow key={`${row.checkout.repo_name}:${row.checkout.path}`} row={row} />
          ))}
        </div>
      );
  }
}

function CheckoutRow({ row }: { row: CheckoutRowView }) {
  const { t } = useTranslation();
  const observed = row.checkout.observed;
  if (!observed) {
    return (
      <div className="flex min-w-0 items-center gap-1.5 text-[10.5px] text-danger" title={row.checkout.path}>
        <span className="truncate">{row.checkout.repo_name}</span>
        <span className="truncate">{t("changeSet.checkout.unsampled")}</span>
      </div>
    );
  }
  return (
    <div
      className={cn(
        "flex min-w-0 flex-wrap items-center gap-1.5 text-[10.5px]",
        row.matchesDeclared === false ? "text-waiting" : "text-ink-faint",
      )}
      title={row.checkout.path}
    >
      <span className="truncate">{row.checkout.repo_name}</span>
      <span className="truncate">{observed.branch}</span>
      <span className="truncate font-mono">{observed.head_sha.slice(0, 8)}</span>
      {observed.dirty && <span>{t("changeSet.checkout.dirty")}</span>}
      {row.matchesDeclared === false && <span>{t("changeSet.checkout.differsFromDeclared")}</span>}
    </div>
  );
}
