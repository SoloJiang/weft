import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { ArrowRight, GitBranch, GitPullRequest, ScanEye } from "lucide-react";
import type {
  ChangeSetLane,
  CheckEvidence,
  ExecutionReconciliation,
  IssueChangeSet,
  UpstreamEvidence,
} from "../lib/types";
import { api } from "../lib/api";
import { ReadinessChip } from "../components/ReadinessChip";
import { cn } from "../lib/cn";
import {
  changeSetPanelState,
  hasEvidence,
  laneCheckoutView,
  type ChangeSetPanelState,
  type CheckoutRowView,
  type LaneCheckoutView,
} from "./changeSetView";

/**
 * The Issue Change Set (issue #175): one delivery overview for the whole
 * issue — write scope and why, dependency order, declared-vs-observed
 * checkout, evidence trust, host state, and what is left.
 *
 * Every verdict shown here was decided by the backend. This component fetches
 * and renders; `changeSetView` does the derivations. It never recomputes a
 * readiness boolean from the facts beside it.
 */
export function ChangeSetPanel({
  threadId,
  refreshKey,
  onOpenLane,
}: {
  threadId: number;
  /**
   * The board's readiness refresh key. Re-reading on exactly the signals that
   * invalidate readiness — a lane status, a worktree row, a worker session, the
   * plan, a host PR change, the poll tick — keeps this view consistent with the
   * chip beside it instead of inventing a second, divergent invalidation rule.
   */
  refreshKey: string;
  onOpenLane: (directionId: number, repoId: number) => void;
}) {
  const { t } = useTranslation();
  const [fetchStatus, setFetchStatus] = useState<"loading" | "resolved" | "rejected">("loading");
  const [changeSet, setChangeSet] = useState<IssueChangeSet | null>(null);

  useEffect(() => {
    let cancelled = false;
    setFetchStatus("loading");
    api
      .issueChangeSet(threadId)
      .then((result) => {
        if (cancelled) return;
        setChangeSet(result);
        setFetchStatus("resolved");
      })
      .catch(() => {
        if (cancelled) return;
        setFetchStatus("rejected");
      });
    return () => {
      cancelled = true;
    };
    // A refresh never presents a prior verdict as current evidence
    // (`readinessKey.ts`), so a key change blanks back to loading rather than
    // leaving the last read on screen labelled as now.
  }, [threadId, refreshKey]);

  const state = changeSetPanelState(fetchStatus, changeSet);
  return (
    <div className="min-h-0 flex-1 overflow-auto px-5 py-4">
      <ChangeSetBody state={state} onOpenLane={onOpenLane} emptyLabel={t("changeSet.empty")} />
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
            <section key={wave.lanes.map((lane) => lane.direction_id).join("-")} className="flex flex-col gap-2">
              <div className="flex items-center gap-2 text-[10.5px] font-semibold uppercase tracking-wider text-ink-faint">
                <span>{t("changeSet.wave", { index: index + 1 })}</span>
                {index > 0 && <ArrowRight size={11} aria-hidden="true" />}
                {index > 0 && <span className="normal-case tracking-normal font-normal">{t("changeSet.waveWaits")}</span>}
              </div>
              <div className="flex flex-col gap-2">
                {wave.lanes.map((lane) => (
                  <LaneRow key={`${lane.direction_id}:${lane.name}`} lane={lane} onOpenLane={onOpenLane} />
                ))}
              </div>
            </section>
          ))}
        </div>
      );
  }
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

const UPSTREAM_KEYS: Record<UpstreamEvidence, string> = {
  satisfied: "changeSet.upstream.satisfied",
  unmet: "changeSet.upstream.unmet",
  unknown: "changeSet.upstream.unknown",
};

function LaneRow({
  lane,
  onOpenLane,
}: {
  lane: ChangeSetLane;
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
        <span className="ml-auto shrink-0 text-[10.5px] text-ink-faint">{lane.direction_status}</span>
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
        {hasEvidence(lane) ? (
          <span>
            {t("changeSet.evidenceCounts", {
              fresh: lane.evidence.fresh,
              stale: lane.evidence.stale,
              unknown: lane.evidence.unknown,
            })}
          </span>
        ) : (
          <span>{t("changeSet.evidenceNone")}</span>
        )}
        {lane.pull_requests.length > 0 && (
          <span className="inline-flex items-center gap-1">
            <GitPullRequest size={11} aria-hidden="true" />
            {t("changeSet.pullRequests", { count: lane.pull_requests.length })}
          </span>
        )}
      </div>
    </article>
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
