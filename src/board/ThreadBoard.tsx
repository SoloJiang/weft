import { useEffect, useRef, useState } from "react";
import { motion } from "motion/react";
import { useTranslation } from "react-i18next";
import * as DM from "@radix-ui/react-dropdown-menu";
import { listen } from "@tauri-apps/api/event";
import {
  Check,
  ChevronDown,
  Copy,
  FolderGit2,
  FolderTree,
  GitBranch,
  GitCompare,
  Layers,
  MessagesSquare,
  MoreHorizontal,
  Pencil,
  ScanEye,
  TerminalSquare,
  Trash2,
  X,
} from "lucide-react";
import { attentionDirectionId, attentionThreadId, useStore } from "../state/store";
import type {
  Direction,
  IssueReadinessDto,
  LaneReadiness,
  RepoChecks,
  SessionStatus,
  Worktree,
} from "../lib/types";
import { api } from "../lib/api";
import { Button } from "../components/ui/Button";
import { Dialog, DialogPanel } from "../components/ui/Dialog";
import { StatusDot } from "../components/ui/StatusChip";
import { Tooltip } from "../components/ui/Tooltip";
import { EvidencePanel } from "../components/EvidencePanel";
import { ToolIcon, toolFullName } from "../components/ToolIcon";
import { ReadinessChip } from "../components/ReadinessChip";
import { ScopeReview } from "./ScopeReview";
import { DeleteWorktreeDialog, RenameDialog } from "../nav/dialogs";
import { LeadTab } from "../session/LeadTab";
import { cn } from "../lib/cn";
import {
  beginReadinessRefresh,
  buildReadinessWorktreeSignatures,
  buildReadinessKey,
  completeReadinessRefresh,
  failReadinessRefresh,
  isDirectionUrgent,
  isReadinessResponseApplicable,
  selectVisibleReadiness,
  type ReadinessFetchState,
  type StoredReadiness,
} from "../lib/readinessKey";

/** Task lifecycle column. Needs-you is a tag on the card (amber chip), never
 *  a stage: an open ask leaves the task in its lifecycle column and bubbles it
 *  to the top. Under automation-first, queued/planning/working all mean "weft
 *  is driving it" — one column, with the stored sub-state as a chip. */
type TaskState = "working" | "review" | "done";
type PrChangedEvent = { thread_id: number };

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

function deriveTestsKind(failed: number, passed: number, total: number): "fail" | "pass" | "pend" {
  if (failed > 0) return "fail";
  if (total > 0 && passed === total) return "pass";
  return "pend";
}

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
    renameDirection,
    attentionItems,
    checksByDirection,
    worktreesByDirection,
    sessions,
  } = useStore();
  const { t } = useTranslation();
  const thread = threads.find((th) => th.id === activeThreadId);
  const [renamingDirectionId, setRenamingDirectionId] = useState<number | null>(null);
  const [storedIssueReadiness, setStoredIssueReadiness] = useState<
    StoredReadiness<IssueReadinessDto> | null
  >(null);
  const [prReadinessRevision, setPrReadinessRevision] = useState(0);
  const [readinessPollRevision, setReadinessPollRevision] = useState(0);
  const activeThreadIdRef = useRef<number | null>(activeThreadId);
  const readinessRequestRevisionRef = useRef(0);
  activeThreadIdRef.current = activeThreadId;
  const activeDirections = activeThreadId == null ? [] : directionsByThread[activeThreadId] ?? [];
  // Include issue/lead attention too. It has no materialized direction card,
  // but the backend represents it as a virtual readiness lane and must be
  // refreshed immediately rather than waiting for the poll.
  const attentionIds = attentionItems
    .filter((item) => attentionThreadId(item) === activeThreadId)
    .map((item) => item.id);
  const worktreeSignatures = buildReadinessWorktreeSignatures(
    activeDirections,
    worktreesByDirection,
  );
  // `created_at` is the opaque proposal version. A re-proposal can change its
  // policy directions and decisions without changing its lifecycle status.
  let planReadinessSignature: string | null = null;
  if (proposal) {
    planReadinessSignature = `${proposal.status}:${proposal.created_at}`;
  }
  const readinessKey = buildReadinessKey({
    directions: activeDirections,
    attentionIds,
    worktrees: worktreeSignatures,
    workerSessions: Object.values(sessions).map((session) => ({
      directionId: session.directionId,
      repoId: session.repoId,
      sessionId: session.info.session_id,
      status: session.status,
    })),
    planStatus: planReadinessSignature,
    prRevision: prReadinessRevision,
  });
  const visibleReadiness = selectVisibleReadiness(
    storedIssueReadiness,
    activeThreadId,
    readinessKey,
  );
  let issueReadiness: IssueReadinessDto | null = null;
  if (visibleReadiness.kind === "ready") {
    issueReadiness = visibleReadiness.dto;
  }

  useEffect(() => {
    if (activeThreadId == null) {
      readinessRequestRevisionRef.current += 1;
      setStoredIssueReadiness(null);
      return;
    }
    const request = {
      threadId: activeThreadId,
      revision: readinessRequestRevisionRef.current + 1,
    };
    const requestKey = readinessKey;
    readinessRequestRevisionRef.current = request.revision;
    setStoredIssueReadiness({
      threadId: request.threadId,
      key: requestKey,
      state: beginReadinessRefresh(),
    });
    let cancelled = false;
    const storeResponse = (state: ReadinessFetchState<IssueReadinessDto>) => {
      setStoredIssueReadiness((current) => {
        if (
          !isReadinessResponseApplicable(
            request,
            activeThreadIdRef.current,
            readinessRequestRevisionRef.current,
          )
        ) {
          return current;
        }
        return { threadId: request.threadId, key: requestKey, state };
      });
    };
    void api
      .issueReadiness(request.threadId)
      .then((readiness) => {
        if (cancelled) {
          return;
        }
        storeResponse(completeReadinessRefresh(readiness));
      })
      .catch(() => {
        if (cancelled) {
          return;
        }
        storeResponse(failReadinessRefresh());
      });
    return () => {
      cancelled = true;
    };
  }, [activeThreadId, readinessKey, readinessPollRevision]);

  useEffect(() => {
    if (activeThreadId == null) {
      return;
    }
    const poll = setInterval(() => {
      setReadinessPollRevision((revision) => revision + 1);
    }, 60_000);
    return () => {
      clearInterval(poll);
    };
  }, [activeThreadId]);

  useEffect(() => {
    let cancelled = false;
    let unlisten: (() => void) | undefined;
    void listen<PrChangedEvent>("pr://changed", (event) => {
      if (!cancelled && event.payload.thread_id === activeThreadIdRef.current) {
        setPrReadinessRevision((revision) => revision + 1);
      }
    })
      .then((nextUnlisten) => {
        if (cancelled) {
          nextUnlisten();
          return;
        }
        unlisten = nextUnlisten;
        // Close the fetch/listen handoff: a PR change that landed before the
        // async listener registration is recovered by one fresh request after
        // registration. Request revisions reject any older response.
        setPrReadinessRevision((revision) => revision + 1);
      })
      .catch(() => {});
    return () => {
      cancelled = true;
      unlisten?.();
    };
  }, []);
  // Resetting the sub-tab here (on mount) is what made backing out of a worker
  // snap to the lead chat: opening the worker unmounts the board, closing it
  // remounts and re-ran this reset. The reset now lives in the store, keyed on a
  // real thread change, so the board tab survives the worker overlay.

  if (!thread) return null;
  const dirs = activeDirections;
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

  const laneVerdicts = new Map<number, LaneReadiness>();
  for (const lane of issueReadiness?.lanes ?? []) {
    laneVerdicts.set(lane.direction_id, lane.readiness);
  }
  const urgent = (d: Direction): boolean => {
    const verdict = laneVerdicts.get(d.id);
    const hasAttention = attentionItems.some(
      (item) => attentionDirectionId(item) === d.id,
    );
    const hasFailingCheck = (checksByDirection[d.id] ?? []).some((repoChecks) =>
      repoChecks.checks.some((check) => check.status === "fail"),
    );
    // Delivery readiness remains exclusively backend-derived. This only keeps
    // the board's pre-existing sorting affinity stable during refreshes and
    // for non-bus attention/check signals that readiness intentionally skips.
    return isDirectionUrgent({ readiness: verdict, hasAttention, hasFailingCheck });
  };

  // One tab body, one obvious branch each (no nested ternary): the lead chat,
  // the empty-discuss prompt, or the task board columns. Scope review is no
  // longer a fourth branch here — it opens as an in-place dialog over whichever
  // of these is showing, so confirming a split never yanks you off the chat.
  const renderTabBody = () => {
    if (threadTab === "lead") return <LeadTab />;
    if (dirs.length === 0) return <EmptyDiscuss onTalk={() => setThreadTab("lead")} />;
    return (
      <div className="min-h-0 flex-1 overflow-auto">
        <div className="flex h-full min-w-0 gap-3 px-5 py-4">
          {COLUMNS.map((col) => {
            const cards = dirs
              .filter((d) => statusOf(d) === col.key)
              .sort((a, b) => Number(urgent(b)) - Number(urgent(a)));
            return (
              <div key={col.key} className="flex min-w-[260px] max-w-[360px] flex-1 flex-col gap-2">
                <div className="flex items-center gap-2 px-1 text-[11px] font-semibold uppercase tracking-wider text-ink-faint">
                  <span className={cn("h-1.5 w-1.5 rounded-full", col.dot)} />
                  {t(col.label)}
                  <span className="tabular-nums text-ink-faint/70">{cards.length}</span>
                </div>
                <div className="flex min-h-0 flex-1 flex-col gap-2 rounded-[var(--radius-lg)] bg-surface/40 p-2">
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
    );
  };

  return (
    <section className="flex min-w-0 flex-1 flex-col overflow-hidden bg-bg">
      <header className="flex shrink-0 items-center gap-2 border-b border-border px-5 py-2.5">
        <h1 className="min-w-0 flex-1 truncate text-[13px] font-semibold text-ink">{thread.title}</h1>
        <ReadinessChip
          state={visibleReadiness}
          className="max-w-[min(60%,28rem)]"
        />
      </header>
      <div className="flex min-h-0 flex-1 flex-col">{renderTabBody()}</div>

      {/* Scope review, in place: a dialog over the chat rather than a tab swap.
          It reuses ScopeReview verbatim (live store state, its own confirm), so
          there is no bespoke confirm surface to race. confirmProposal clears
          reviewingProposal on success, which closes this; Esc/overlay dismiss
          does the same. */}
      <Dialog
        open={reviewingProposal && !!proposal && proposal.status === "proposed"}
        onOpenChange={(o) => {
          if (!o) setReviewingProposal(false);
        }}
      >
        <DialogPanel title={t("scope.title")}>
          <ScopeReview onClose={() => setReviewingProposal(false)} />
        </DialogPanel>
      </Dialog>

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
    attentionItems,
    checksByDirection,
    requestSkillReview,
    openNeeds,
    deleteWorktree,
  } = useStore();
  const { t } = useTranslation();
  const [wtToDelete, setWtToDelete] = useState<Worktree | null>(null);
  const writes = worktreesByDirection[direction.id] ?? [];
  // Only worktrees whose directory is still on disk back live actions. A row can
  // outlive its directory (reclaimed via the Done-card delete, or removed out of
  // band), and opening a session / diff against a missing cwd just breaks.
  const liveWrites = writes.filter((w) => w.exists);
  const checks = checksByDirection[direction.id];

  const allChecks = (checks ?? []).flatMap((rc) => rc.checks);
  const failed = allChecks.filter((c) => c.status === "fail").length;
  const passed = allChecks.filter((c) => c.status === "pass").length;
  const hasNeed = attentionItems.some((item) => attentionDirectionId(item) === direction.id);
  const firstWrite = liveWrites[0];

  const testsKind = deriveTestsKind(failed, passed, allChecks.length);
  // The review-column primary action is honest: open the actual diff for human
  // eyes (Task→PR is the delivery boundary; weft does not fake a PR step).
  const action = hasNeed
    ? { label: t("thread.handle"), variant: "primary" as const, diff: false }
    : direction.status === "review"
      ? { label: t("thread.viewChanges"), variant: "primary" as const, diff: true }
      : { label: t("thread.openSession"), variant: "default" as const, diff: false };
  const canRunReview = direction.status === "review";

  return (
    <>
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
            {/* The card's own name is the most prominent worker reference on
                it — one click lands on its timeline, same target + resolution
                as the primary action button below (reused, not reimplemented).
                No live worktree yet → same disabled state as that button. */}
            <button
              type="button"
              disabled={!firstWrite}
              title={firstWrite ? t("thread.openSession") : t("thread.noWriteCopy")}
              onClick={() => firstWrite && viewDirection(direction.id, firstWrite.repo_id)}
              className="block min-w-0 flex-1 break-words text-left text-[13px] font-semibold leading-snug text-ink transition-colors hover:text-brand disabled:cursor-default disabled:pointer-events-none"
            >
              {direction.name}
            </button>
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
          <ProvenanceMenu
            direction={direction}
            writes={writes}
            checks={checks}
            onDeleteWorktree={setWtToDelete}
          />
        </div>
        <div className="flex shrink-0 items-center gap-1.5">
          {canRunReview && (
            <Tooltip label={t("thread.reviewTip")}>
              <button
                type="button"
                onClick={() => void requestSkillReview(direction.id, { focus: true })}
                disabled={liveWrites.length === 0}
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
              viewDirection(direction.id, firstWrite.repo_id, {
                sidePanel: action.diff ? "diff" : undefined,
              })
            }
          >
            {action.diff ? <GitCompare size={13} /> : <TerminalSquare size={13} />}
            {action.label}
          </Button>
        </div>
      </div>
    </motion.div>
    <DeleteWorktreeDialog
      worktree={wtToDelete}
      onOpenChange={(o) => {
        if (!o) setWtToDelete(null);
      }}
      onConfirm={async () => {
        if (wtToDelete) await deleteWorktree(wtToDelete.id, direction.id);
      }}
    />
    </>
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
  onDeleteWorktree,
}: {
  direction: Direction;
  writes: Worktree[];
  checks?: RepoChecks[];
  onDeleteWorktree: (w: Worktree) => void;
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
  // Evidence (issue #174) fetches lazily — only while this menu is open —
  // so a closed dropdown never polls the backend.
  const [open, setOpen] = useState(false);
  return (
    <DM.Root open={open} onOpenChange={setOpen}>
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
                  {/* Repo header → open this repo's session (only while the
                      worktree directory is live; a reclaimed/removed one can't). */}
                  <DM.Item
                    onSelect={(e) => {
                      if (!w.exists) {
                        e.preventDefault();
                        return;
                      }
                      viewDirection(direction.id, w.repo_id);
                    }}
                    className={cn(
                      "flex items-center gap-2 rounded-[var(--radius-sm)] px-2 py-1.5 text-[12px] outline-none",
                      w.exists
                        ? "cursor-pointer text-ink data-[highlighted]:bg-brand-ghost"
                        : "cursor-default text-ink-faint",
                    )}
                  >
                    <TerminalSquare
                      size={12}
                      className={cn("shrink-0", w.exists ? "text-brand" : "text-ink-faint")}
                    />
                    <span className="min-w-0 flex-1 truncate font-medium">{name}</span>
                    {w.exists ? (
                      sess && <StatusDot status={sess.status as SessionStatus} />
                    ) : (
                      <span className="shrink-0 text-[10px] text-ink-faint">
                        {t("thread.worktreeRemoved")}
                      </span>
                    )}
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
                  {/* Browse this worktree's files. */}
                  {w.exists && (
                    <DM.Item
                      onSelect={(e) => {
                        e.preventDefault();
                        viewDirection(direction.id, w.repo_id, { sidePanel: "files" });
                      }}
                      className="flex cursor-pointer items-center gap-2 rounded-[var(--radius-sm)] py-1 pl-7 pr-2 text-[11px] text-ink-muted outline-none data-[highlighted]:bg-brand-ghost data-[highlighted]:text-ink"
                    >
                      <FolderTree size={11} className="shrink-0 text-ink-faint" />
                      <span className="min-w-0 flex-1 truncate">{t("thread.browseFiles")}</span>
                    </DM.Item>
                  )}
                  {/* Reclaim a finished task's worktree. Done-only (its work is
                      settled) and only while the directory is still on disk. */}
                  {direction.status === "done" && w.exists && (
                    <DM.Item
                      title={t("thread.deleteWorktree")}
                      onSelect={() => onDeleteWorktree(w)}
                      className="flex cursor-pointer items-center gap-2 rounded-[var(--radius-sm)] py-1 pl-7 pr-2 text-[11px] text-danger outline-none data-[highlighted]:bg-[oklch(0.64_0.2_25/0.12)] data-[highlighted]:text-danger"
                    >
                      <Trash2 size={11} className="shrink-0" />
                      <span className="min-w-0 flex-1 truncate">
                        {t("thread.deleteWorktree")}
                      </span>
                    </DM.Item>
                  )}
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
          <DM.Separator className="my-1 h-px bg-border" />
          <EvidencePanel threadId={direction.thread_id} directionId={direction.id} active={open} />
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
