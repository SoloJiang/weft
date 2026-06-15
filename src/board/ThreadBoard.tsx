import { useState } from "react";
import { motion } from "motion/react";
import { useTranslation } from "react-i18next";
import * as DM from "@radix-ui/react-dropdown-menu";
import {
  Check,
  ChevronDown,
  Copy,
  FolderGit2,
  GitBranch,
  GitCompare,
  Layers,
  MessagesSquare,
  MoreHorizontal,
  Pencil,
  ScanEye,
  TerminalSquare,
  X,
} from "lucide-react";
import { useStore } from "../state/store";
import type { Direction, RepoChecks, SessionStatus } from "../lib/types";
import { Button } from "../components/ui/Button";
import { StatusDot } from "../components/ui/StatusChip";
import { Tooltip } from "../components/ui/Tooltip";
import { ToolIcon, toolFullName } from "../components/ToolIcon";
import { ScopeReview } from "./ScopeReview";
import { RenameDialog } from "../nav/dialogs";
import { LeadTab } from "../session/LeadTab";
import { cn } from "../lib/cn";

/** Task lifecycle column. Needs-you is a tag on the card (amber chip), never
 *  a stage: an open ask leaves the task in its lifecycle column and bubbles it
 *  to the top. Under automation-first, queued/planning/working all mean "weft
 *  is driving it" — one column, with the stored sub-state as a chip. */
type TaskState = "working" | "review" | "done";

const COLUMNS: { key: TaskState; label: string; dot: string }[] = [
  { key: "working", label: "thread.colRunning", dot: "bg-running" },
  { key: "review", label: "thread.colReview", dot: "bg-brand" },
  { key: "done", label: "thread.colDone", dot: "bg-accent" },
];

/** Stored statuses a human may set directly (sub-states of the lifecycle). */
const SETTABLE: { key: string; label: string; dot: string }[] = [
  { key: "planning", label: "thread.statusPlanning", dot: "bg-idle" },
  { key: "working", label: "thread.statusBuilding", dot: "bg-running" },
  { key: "review", label: "thread.colReview", dot: "bg-brand" },
  { key: "done", label: "thread.colDone", dot: "bg-accent" },
];

export function ThreadBoard() {
  const {
    threads,
    activeThreadId,
    directionsByThread,
    proposal,
    reviewingProposal,
    setReviewingProposal,
    threadTab,
    setThreadTab,
    needs,
    asks,
    checksByDirection,
    renameDirection,
  } = useStore();
  const { t } = useTranslation();
  const thread = threads.find((th) => th.id === activeThreadId);
  const [renamingDirectionId, setRenamingDirectionId] = useState<number | null>(null);
  // Resetting the sub-tab here (on mount) is what made backing out of a worker
  // snap to the lead chat: opening the worker unmounts the board, closing it
  // remounts and re-ran this reset. The reset now lives in the store, keyed on a
  // real thread change, so the board tab survives the worker overlay.

  if (!thread) return null;
  const dirs = directionsByThread[thread.id] ?? [];
  // Derive `initial` from the live directions slice rather than capturing it
  // at click time — keeps the dialog in sync with concurrent rename/refresh.
  const renamingDirection =
    renamingDirectionId != null ? dirs.find((d) => d.id === renamingDirectionId) ?? null : null;

  // Column from the stored, agent/human-set status. queued/planning/working
  // share the driving column; an open ask/need or a failing check only tags
  // the card (amber chip) and bubbles it to the top of its column.
  const statusOf = (d: Direction): TaskState => {
    if (d.status === "review" || d.status === "done") return d.status;
    return "working";
  };

  const urgent = (d: Direction): boolean =>
    needs.some((n) => n.direction_id === d.id) ||
    asks.some((a) => a.dir === String(d.id)) ||
    (checksByDirection[d.id] ?? []).some((rc) => rc.checks.some((c) => c.status === "fail"));

  return (
    <section className="flex min-w-0 flex-1 flex-col overflow-hidden bg-bg">
      <div className="flex min-h-0 flex-1 flex-col">
        {threadTab === "lead" ? (
          <LeadTab onReview={() => setThreadTab("board")} />
        ) : reviewingProposal && proposal && proposal.status === "proposed" ? (
          <ScopeReview
            onBack={() => {
              setReviewingProposal(false);
              setThreadTab("lead");
            }}
          />
        ) : dirs.length === 0 ? (
          <EmptyDiscuss onTalk={() => setThreadTab("lead")} />
        ) : (
          <div className="min-h-0 flex-1 overflow-auto">
            <div className="flex h-full min-w-fit gap-3 px-5 py-4">
              {COLUMNS.map((col) => {
                const cards = dirs
                  .filter((d) => statusOf(d) === col.key)
                  .sort((a, b) => Number(urgent(b)) - Number(urgent(a)));
                return (
                  <div key={col.key} className="flex w-[300px] shrink-0 flex-col gap-2">
                    <div className="flex items-center gap-2 px-1 text-[11px] font-semibold uppercase tracking-wider text-ink-faint">
                      <span className={cn("h-1.5 w-1.5 rounded-full", col.dot)} />
                      {t(col.label)}
                      <span className="tabular-nums text-ink-faint/70">{cards.length}</span>
                    </div>
                    <div
                      className="flex min-h-0 flex-1 flex-col gap-2 rounded-[var(--radius-lg)] bg-surface/40 p-2"
                    >
                      {cards.map((d) => (
                        <div key={d.id}>
                          <DirectionCard direction={d} onRename={setRenamingDirectionId} />
                        </div>
                      ))}
                      {cards.length === 0 && (
                        <div className="flex flex-1 items-center justify-center py-6 text-[11px] text-ink-faint/60">
                          {t("thread.colEmpty")}
                        </div>
                      )}
                    </div>
                  </div>
                );
              })}
            </div>
          </div>
        )}
      </div>

      {renamingDirection && (
        <RenameDialog
          open={renamingDirectionId != null}
          onOpenChange={(o) => !o && setRenamingDirectionId(null)}
          title={t("thread.renameTask")}
          label={t("dialog.taskName")}
          initial={renamingDirection.name}
          onSubmit={(v) => renameDirection(renamingDirection.id, v)}
        />
      )}
    </section>
  );
}

function EmptyDiscuss({ onTalk }: { onTalk: () => void }) {
  const { t } = useTranslation();
  return (
    <div className="flex h-full flex-col items-center justify-center px-6 text-center">
      <div className="grid h-11 w-11 place-items-center rounded-[var(--radius-lg)] border border-border bg-surface">
        <Layers size={20} className="text-ink-faint" />
      </div>
      <h2 className="mt-3 text-[14px] font-semibold text-ink">{t("thread.discussTitle")}</h2>
      <p className="mt-1.5 max-w-sm text-[12px] leading-relaxed text-ink-faint">
        {t("thread.discussBody")}
      </p>
      <Button variant="primary" className="mt-4" onClick={onTalk}>
        <MessagesSquare size={14} />
        {t("lead.title")}
      </Button>
    </div>
  );
}

function DirectionCard({
  direction,
  onRename,
}: {
  direction: Direction;
  onRename: (id: number) => void;
}) {
  const {
    worktreesByDirection,
    viewDirection,
    needs,
    asks,
    checksByDirection,
    requestSkillReview,
    openNeeds,
  } = useStore();
  const { t } = useTranslation();
  const writes = worktreesByDirection[direction.id] ?? [];
  const checks = checksByDirection[direction.id];

  const allChecks = (checks ?? []).flatMap((rc) => rc.checks);
  const failed = allChecks.filter((c) => c.status === "fail").length;
  const passed = allChecks.filter((c) => c.status === "pass").length;
  const hasNeed =
    needs.some((n) => n.direction_id === direction.id) ||
    asks.some((a) => a.dir === String(direction.id));
  const firstWrite = writes[0];

  const testsKind =
    failed > 0 ? "fail" : allChecks.length > 0 && passed === allChecks.length ? "pass" : "pend";
  // The review-column primary action is honest: open the actual diff for human
  // eyes (Task→PR is the delivery boundary; weft does not fake a PR step).
  const action = hasNeed
    ? { label: t("thread.handle"), variant: "primary" as const, diff: false }
    : direction.status === "review"
      ? { label: t("thread.viewChanges"), variant: "primary" as const, diff: true }
      : { label: t("thread.openSession"), variant: "default" as const, diff: false };
  const canRunReview = direction.status === "review";

  return (
    <motion.div
      layout
      className={cn(
        "group flex flex-col rounded-[var(--radius-lg)] border bg-surface text-left transition-colors hover:border-border-strong",
        hasNeed ? "border-waiting/45" : "border-border",
      )}
    >
      <div className="flex items-start gap-2.5 px-3 pb-2.5 pt-3">
        <span
          title={toolFullName(direction.tool)}
          className="grid h-6 w-6 shrink-0 place-items-center rounded-[var(--radius-sm)] border border-border bg-bg text-ink-muted"
        >
          <ToolIcon tool={direction.tool} size={14} />
        </span>
        <div className="min-w-0 flex-1">
          <div className="flex items-start gap-2">
            <div className="min-w-0 flex-1 break-words text-[13px] font-semibold leading-snug text-ink">
              {direction.name}
            </div>
            <div className="flex shrink-0 items-center gap-1">
              {hasNeed && (
                <button
                  type="button"
                  title={t("needs.title")}
                  onClick={() => openNeeds()}
                  className="rounded-full bg-waiting/15 px-1.5 py-0.5 text-[10.5px] font-medium text-waiting transition-colors hover:bg-waiting/25"
                >
                  {t("thread.colNeeds")}
                </button>
              )}
              <button
                type="button"
                title={t("thread.renameTask")}
                aria-label={t("thread.renameTask")}
                onClick={() => onRename(direction.id)}
                className="grid h-6 w-6 shrink-0 place-items-center rounded-[var(--radius-sm)] text-ink-faint opacity-0 transition-opacity hover:bg-brand-ghost hover:text-ink group-hover:opacity-100"
              >
                <Pencil size={12} />
              </button>
              <StatusMenu direction={direction} />
            </div>
          </div>
        </div>
      </div>

      {/* One honest trust signal (the real checks) + the details menu (repos /
          branches / paths), then actions. */}
      <div className="flex items-center justify-between gap-2 border-t border-border bg-bg/55 px-3 py-2">
        <div className="flex min-w-0 items-center gap-1.5 overflow-hidden">
          <TrustSignal
            kind={testsKind}
            label={
              allChecks.length > 0
                ? t("thread.testsProgress", { passed, count: allChecks.length })
                : t("thread.testsPending")
            }
          />
          <ProvenanceMenu direction={direction} writes={writes} checks={checks} />
        </div>
        <div className="flex shrink-0 items-center gap-1.5">
          {canRunReview && (
            <Tooltip label={t("thread.reviewTip")}>
              <button
                type="button"
                onClick={() => void requestSkillReview(direction.id, { focus: true })}
                disabled={writes.length === 0}
                aria-label={t("thread.review")}
                className="grid h-7 w-7 shrink-0 place-items-center rounded-[var(--radius-sm)] text-ink-muted outline-none transition-colors hover:bg-brand-ghost hover:text-ink disabled:opacity-40"
              >
                <ScanEye size={13} className="text-brand" />
              </button>
            </Tooltip>
          )}
          <Button
            size="sm"
            variant={action.variant}
            disabled={!firstWrite}
            title={firstWrite ? undefined : t("thread.noWriteCopy")}
            onClick={() =>
              firstWrite &&
              viewDirection(direction.id, firstWrite.repo_id, { diff: action.diff })
            }
          >
            {action.diff ? <GitCompare size={13} /> : <TerminalSquare size={13} />}
            {action.label}
          </Button>
        </div>
      </div>
    </motion.div>
  );
}

/**
 * Task details, demoted to one three-dots icon: a dropdown with the per-repo
 * working copies (name → open the session, plus branch and worktree path to
 * copy) and the check results. Replaces the on-card repo chips so status and
 * review state stay the strongest signals on the card face.
 */
function ProvenanceMenu({
  direction,
  writes,
  checks,
}: {
  direction: Direction;
  writes: { id: number; repo_id: number; branch: string; path: string }[];
  checks?: RepoChecks[];
}) {
  const { t } = useTranslation();
  const { repos, sessions, viewDirection } = useStore();
  // Copy feedback keyed per-field ("b<id>" branch, "p<id>" path) so each row
  // flips its own checkmark.
  const [copied, setCopied] = useState<string | null>(null);
  const copy = (key: string, text: string) => {
    void navigator.clipboard.writeText(text);
    setCopied(key);
    window.setTimeout(() => setCopied((k) => (k === key ? null : k)), 1800);
  };
  return (
    <DM.Root>
      <Tooltip label={t("thread.provenanceTip")}>
        <DM.Trigger
          aria-label={t("thread.provenance")}
          onClick={(e) => e.stopPropagation()}
          className="grid h-7 w-7 shrink-0 place-items-center rounded-[var(--radius-sm)] text-ink-faint outline-none transition-colors hover:bg-brand-ghost hover:text-ink data-[state=open]:bg-brand-ghost data-[state=open]:text-ink"
        >
          <MoreHorizontal size={14} />
        </DM.Trigger>
      </Tooltip>
      <DM.Portal>
        <DM.Content
          align="end"
          sideOffset={4}
          onClick={(e) => e.stopPropagation()}
          className="weft-pop z-[60] w-72 rounded-[var(--radius-md)] border border-border bg-raised p-1 shadow-[0_8px_24px_-8px_rgba(0,0,0,0.5)]"
        >
          {writes.length === 0 ? (
            <div className="px-2 py-1.5 text-[11px] text-ink-faint">
              {t("thread.noWriteCopy")}
            </div>
          ) : (
            writes.map((w) => {
              const repo = repos.find((r) => r.id === w.repo_id);
              const sess = Object.values(sessions).find(
                (s) => s.directionId === direction.id && s.repoId === w.repo_id,
              );
              const name = repo?.name ?? `repo ${w.repo_id}`;
              return (
                <div key={w.id} className="pb-0.5">
                  {/* Repo header → open this repo's session. */}
                  <DM.Item
                    onSelect={() => viewDirection(direction.id, w.repo_id)}
                    className="flex cursor-pointer items-center gap-2 rounded-[var(--radius-sm)] px-2 py-1.5 text-[12px] text-ink outline-none data-[highlighted]:bg-brand-ghost"
                  >
                    <TerminalSquare size={12} className="shrink-0 text-brand" />
                    <span className="min-w-0 flex-1 truncate font-medium">{name}</span>
                    {sess && <StatusDot status={sess.status as SessionStatus} />}
                  </DM.Item>
                  {/* Branch → copy. */}
                  <DM.Item
                    title={t("thread.copyBranch")}
                    onSelect={(e) => {
                      e.preventDefault(); // stay open: copying is not a navigation
                      copy(`b${w.id}`, w.branch);
                    }}
                    className="flex cursor-pointer items-center gap-2 rounded-[var(--radius-sm)] py-1 pl-7 pr-2 text-[11px] text-ink-muted outline-none data-[highlighted]:bg-brand-ghost data-[highlighted]:text-ink"
                  >
                    <GitBranch size={11} className="shrink-0 text-ink-faint" />
                    <span className="min-w-0 flex-1 truncate font-mono">{w.branch}</span>
                    {copied === `b${w.id}` ? (
                      <Check size={11} className="shrink-0 text-running" />
                    ) : (
                      <Copy size={11} className="shrink-0 text-ink-faint" />
                    )}
                  </DM.Item>
                  {/* Worktree path (the repo address) → copy. */}
                  <DM.Item
                    title={w.path}
                    onSelect={(e) => {
                      e.preventDefault();
                      copy(`p${w.id}`, w.path);
                    }}
                    className="flex cursor-pointer items-center gap-2 rounded-[var(--radius-sm)] py-1 pl-7 pr-2 text-[11px] text-ink-muted outline-none data-[highlighted]:bg-brand-ghost data-[highlighted]:text-ink"
                  >
                    <FolderGit2 size={11} className="shrink-0 text-ink-faint" />
                    <span className="min-w-0 flex-1 truncate font-mono">{w.path}</span>
                    {copied === `p${w.id}` ? (
                      <Check size={11} className="shrink-0 text-running" />
                    ) : (
                      <Copy size={11} className="shrink-0 text-ink-faint" />
                    )}
                  </DM.Item>
                </div>
              );
            })
          )}
          {checks && checks.length > 0 && (
            <>
              <DM.Separator className="my-1 h-px bg-border" />
              <div className="flex flex-col gap-1 px-2 py-1.5">
                {checks.map((rc) => (
                  <ChecksRow key={rc.repo} rc={rc} />
                ))}
              </div>
            </>
          )}
        </DM.Content>
      </DM.Portal>
    </DM.Root>
  );
}

type TrustKind = "pass" | "fail" | "pend";

function TrustSignal({ kind, label }: { kind: TrustKind; label: string }) {
  return (
    <span
      className={cn(
        "inline-flex max-w-full items-center gap-1 rounded-full px-1.5 py-0.5 text-[10.5px] font-medium",
        kind === "pass" && "bg-running/15 text-running",
        kind === "fail" && "bg-[oklch(0.64_0.2_25/0.15)] text-danger",
        kind === "pend" && "border border-border bg-bg text-ink-faint",
      )}
    >
      {kind === "pass" ? (
        <Check size={10} className="shrink-0" />
      ) : kind === "fail" ? (
        <X size={10} className="shrink-0" />
      ) : (
        <span className="h-1.5 w-1.5 shrink-0 rounded-full bg-ink-faint/70" />
      )}
      <span className="truncate">{label}</span>
    </span>
  );
}

/** Keyboard/click path to restatus a task. Sets the stored status (§4.6);
 *  Needs-you is a weft-derived tag, not a status, so it isn't offered. */
function StatusMenu({ direction }: { direction: Direction }) {
  const { setTaskStatus } = useStore();
  const { t } = useTranslation();
  const settable = SETTABLE;
  const current = settable.find((c) => c.key === direction.status) ?? settable[0];
  return (
    <DM.Root>
      <DM.Trigger
        title={t("thread.setStatus")}
        aria-label={t("thread.setStatus")}
        onClick={(e) => e.stopPropagation()}
        className="flex items-center gap-1 rounded-full px-1.5 py-0.5 text-ink-faint outline-none transition-colors hover:bg-brand-ghost hover:text-ink data-[state=open]:bg-brand-ghost data-[state=open]:text-ink"
      >
        <span className={cn("h-2 w-2 rounded-full", current.dot)} />
        <ChevronDown size={11} />
      </DM.Trigger>
      <DM.Portal>
        <DM.Content
          align="end"
          sideOffset={4}
          onClick={(e) => e.stopPropagation()}
          className="weft-pop z-[60] w-40 rounded-[var(--radius-md)] border border-border bg-raised p-1 shadow-[0_8px_24px_-8px_rgba(0,0,0,0.5)]"
        >
          {settable.map((c) => (
            <DM.Item
              key={c.key}
              onSelect={() => void setTaskStatus(direction.id, c.key)}
              className="flex cursor-pointer items-center gap-2 rounded-[var(--radius-sm)] px-2 py-1.5 text-[12px] text-ink-muted outline-none data-[highlighted]:bg-brand-ghost data-[highlighted]:text-ink"
            >
              <span className={cn("h-1.5 w-1.5 rounded-full", c.dot)} />
              {t(c.label)}
              {c.key === current.key && <Check size={12} className="ml-auto text-brand" />}
            </DM.Item>
          ))}
        </DM.Content>
      </DM.Portal>
    </DM.Root>
  );
}

function ChecksRow({ rc }: { rc: RepoChecks }) {
  const { t } = useTranslation();
  if (rc.checks.length === 0) {
    return (
      <div className="flex items-center gap-2 text-[11px]">
        <span className="truncate text-ink-muted">{rc.repo}</span>
        <span className="text-ink-faint">{t("thread.noChecks")}</span>
      </div>
    );
  }
  return (
    <div className="flex flex-wrap items-center gap-1.5 text-[11px]">
      <span className="mr-0.5 truncate text-ink-muted">{rc.repo}</span>
      {rc.checks.map((c) => {
        const pass = c.status === "pass";
        return (
          <span
            key={c.name}
            title={pass ? `${c.name}: passed` : c.output_tail || `${c.name}: failed (exit ${c.code})`}
            className={cn(
              "flex items-center gap-1 rounded-full px-1.5 py-0.5 font-medium",
              pass ? "bg-running/15 text-running" : "bg-[oklch(0.64_0.2_25/0.15)] text-danger",
            )}
          >
            {pass ? <Check size={10} /> : <X size={10} />}
            {c.name}
          </span>
        );
      })}
    </div>
  );
}
