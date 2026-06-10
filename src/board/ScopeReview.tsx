import { useMemo, useState } from "react";
import { motion } from "motion/react";
import { useTranslation } from "react-i18next";
import {
  AlertTriangle,
  ArrowLeft,
  CircleDashed,
  Clock,
  FolderGit2,
  GitBranch,
  Sparkles,
} from "lucide-react";
import { useStore } from "../state/store";
import type { ResolvedDirection } from "../lib/types";
import { Button } from "../components/ui/Button";
import { ToolIcon, toolFullName } from "../components/ToolIcon";
import { cn } from "../lib/cn";

type ScopeLane =
  | {
      key: string;
      role: "write";
      repoName: string;
      repoKnown: boolean;
      direction: ResolvedDirection;
      order: number;
    }
  | {
      key: string;
      role: "none";
      repoName: string;
      repoKnown: true;
    };

export function ScopeReview({ onBack }: { onBack: () => void }) {
  const { proposal, confirmProposal, threads, activeThreadId, repos } = useStore();
  const { t } = useTranslation();
  const [confirming, setConfirming] = useState(false);
  const dirs = proposal?.directions ?? [];
  const thread = threads.find((th) => th.id === activeThreadId);

  const lanes = useMemo(() => {
    const writeNames = new Set<string>();
    const writeLanes: ScopeLane[] = [];
    dirs.forEach((direction, dirIndex) => {
      // One write repo per direction (scope rework): repo + required reason.
      const entry = direction.repo;
      writeNames.add(entry.repo_name);
      writeLanes.push({
        key: `${dirIndex}-${entry.repo_name}`,
        role: "write",
        repoName: entry.repo_name,
        repoKnown: entry.known,
        direction,
        order: dirIndex + 1,
      });
    });
    const noneLanes: ScopeLane[] = repos
      .filter((repo) => !writeNames.has(repo.name))
      .map((repo) => ({
        key: `none-${repo.id}`,
        role: "none" as const,
        repoName: repo.name,
        repoKnown: true as const,
      }));
    return [...writeLanes, ...noneLanes];
  }, [dirs, repos]);

  if (!proposal) return null;

  const writeCount = lanes.filter((lane) => lane.role === "write").length;
  const noneCount = lanes.filter((lane) => lane.role === "none").length;

  async function confirm() {
    setConfirming(true);
    try {
      await confirmProposal();
    } finally {
      setConfirming(false);
    }
  }

  return (
    <div className="flex min-h-0 flex-1 flex-col overflow-hidden bg-bg">
      <div className="min-h-0 flex-1 overflow-auto px-5 py-5">
        <div className="mx-auto flex w-full max-w-[920px] flex-col gap-4">
          <div className="rounded-[var(--radius-lg)] border border-border bg-surface px-4 py-3">
            <div className="flex items-center gap-3">
              <span className="grid h-8 w-8 shrink-0 place-items-center rounded-[var(--radius-md)] bg-accent-ghost text-accent">
                <Sparkles size={16} />
              </span>
              <div className="min-w-0 flex-1">
                <div className="text-[11px] font-medium text-ink-faint">
                  {t("scope.inputTask")}
                </div>
                <div className="truncate text-[15px] font-semibold text-ink">
                  {thread?.title ?? t("palette.issue")}
                </div>
              </div>
              <span className="rounded-full border border-brand/35 bg-brand-ghost px-2 py-0.5 text-[11px] font-medium text-brand">
                {thread?.kind ? t(`kind.${thread.kind}`, thread.kind) : t("palette.issue")}
              </span>
              <span className="hidden rounded-full border border-border bg-bg px-2 py-0.5 text-[11px] text-ink-faint sm:inline">
                {t("scope.notMaterialized")}
              </span>
            </div>
          </div>

          {proposal.rationale && (
            <div className="rounded-[var(--radius-lg)] border border-border bg-surface/60 px-4 py-3">
              <div className="mb-1 text-[10.5px] font-semibold uppercase tracking-wide text-ink-faint">
                {t("scope.rationale")}
              </div>
              <p className="text-[12.5px] leading-relaxed text-ink-muted">
                {proposal.rationale}
              </p>
            </div>
          )}

          <div className="relative py-1">
            <div className="absolute left-[29px] top-0 h-full w-px bg-border" />
            <div className="absolute left-[29px] top-3 h-[calc(100%-24px)] w-px bg-[var(--c-warp-line)] opacity-60" />
            <div className="mb-2 ml-12 text-[10.5px] font-semibold uppercase tracking-wide text-ink-faint">
              {t("scope.inferred")}
            </div>
            <div className="flex flex-col gap-2">
              {lanes.map((lane, index) => (
                <ScopeLaneRow key={lane.key} lane={lane} index={index} />
              ))}
            </div>
          </div>
        </div>
      </div>

      <div className="border-t border-border bg-bg/95 px-5 py-3 backdrop-blur">
        <div className="mx-auto flex w-full max-w-[920px] flex-wrap items-center gap-3">
          <SummaryPill tone="write" label={t("scope.writeSummary", { count: writeCount })} />
          <SummaryPill tone="none" label={t("scope.noneSummary", { count: noneCount })} />
          <span className="ml-auto text-[11.5px] text-ink-faint">{t("scope.onlyGate")}</span>
          <button
            onClick={onBack}
            className="flex h-8 items-center gap-1.5 rounded-[var(--radius-md)] px-2.5 text-[12.5px] text-ink-muted transition-colors hover:bg-surface hover:text-ink"
          >
            <ArrowLeft size={13} />
            {t("scope.back")}
          </button>
          <Button variant="primary" onClick={() => void confirm()} disabled={confirming || dirs.length === 0}>
            <GitBranch size={14} />
            {confirming ? t("scope.confirming") : t("scope.confirm", { count: dirs.length })}
          </Button>
        </div>
      </div>
    </div>
  );
}

function ScopeLaneRow({ lane, index }: { lane: ScopeLane; index: number }) {
  const { t } = useTranslation();
  const write = lane.role === "write";
  return (
    <motion.div
      initial={{ opacity: 0, y: 6 }}
      animate={{ opacity: 1, y: 0 }}
      transition={{ duration: 0.18, delay: index * 0.025 }}
      className={cn(
        "relative grid grid-cols-[58px_minmax(140px,190px)_112px_minmax(0,1fr)] items-center gap-3 rounded-[var(--radius-lg)] border px-3 py-2.5",
        write
          ? "border-accent/35 bg-surface"
          : "border-border bg-surface/35 text-ink-faint",
      )}
    >
      <div className="relative flex items-center gap-3">
        <span
          className={cn(
            "z-10 grid h-7 w-7 shrink-0 place-items-center rounded-full border bg-bg font-mono text-[11px]",
            write ? "border-accent/50 text-accent" : "border-border text-ink-faint",
          )}
        >
          {write ? lane.order : "·"}
        </span>
        <span
          className={cn(
            "h-px flex-1",
            write ? "bg-accent/70" : "border-t border-dashed border-border-strong",
          )}
        />
      </div>

      <div className="min-w-0">
        <div className={cn("truncate font-mono text-[12.5px]", write ? "text-ink" : "text-ink-faint")}>
          {lane.repoName}
        </div>
        <div className="mt-0.5 truncate text-[10.5px] text-ink-faint">
          {write ? t("scope.createsCopy") : t("scope.noCopy")}
        </div>
      </div>

      <span
        className={cn(
          "inline-flex h-6 items-center justify-center gap-1.5 rounded-full px-2 text-[11px] font-medium",
          write ? "bg-accent-ghost text-accent" : "border border-border bg-bg text-ink-faint",
        )}
      >
        {write ? <FolderGit2 size={11} /> : <CircleDashed size={11} />}
        {write ? t("scope.writes") : t("scope.notInvolved")}
      </span>

      {write ? (
        <div className="flex min-w-0 items-center gap-2">
          <div className="min-w-0 flex-1">
            <div className="truncate text-[12.5px] font-medium text-ink">
              {lane.direction.name}
            </div>
            {!lane.repoKnown && (
              <div className="mt-0.5 flex items-center gap-1 text-[10.5px] text-waiting">
                <AlertTriangle size={10} />
                {t("scope.unknownRepo")}
              </div>
            )}
          </div>
          <span className="flex shrink-0 items-center gap-1.5 rounded-[var(--radius-sm)] bg-bg px-2 py-0.5 text-[11px] text-ink-muted">
            <ToolIcon tool={lane.direction.tool} size={12} />
            {toolFullName(lane.direction.tool)}
          </span>
          <span className="hidden shrink-0 rounded-full border border-border bg-bg px-2 py-0.5 text-[10.5px] text-ink-faint sm:inline">
            {lane.direction.mandate === "impl-only"
              ? t("scope.mandateImpl")
              : t("scope.mandatePlan")}
          </span>
          {index > 0 && (
            <span className="hidden shrink-0 items-center gap-1 rounded-full border border-brand/30 bg-brand-ghost px-2 py-0.5 text-[10.5px] text-brand lg:flex">
              <Clock size={11} />
              {t("scope.afterPrevious")}
            </span>
          )}
        </div>
      ) : (
        <div className="truncate text-[12px] text-ink-faint">
          {t("scope.noTouchReason")}
        </div>
      )}
    </motion.div>
  );
}

function SummaryPill({ tone, label }: { tone: "write" | "none"; label: string }) {
  return (
    <span className="inline-flex items-center gap-1.5 rounded-full border border-border bg-surface px-2.5 py-1 text-[11.5px] text-ink-muted">
      <span
        className={cn(
          "h-1.5 w-1.5 rounded-full",
          tone === "write" ? "bg-accent" : "bg-ink-faint",
        )}
      />
      {label}
    </span>
  );
}
