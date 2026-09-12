import { useEffect, useRef, useState } from "react";
import { motion } from "motion/react";
import { useTranslation } from "react-i18next";
import { Layers, Plus, SquarePen, X } from "lucide-react";
import { listen } from "@tauri-apps/api/event";
import { attentionThreadId, useStore } from "../state/store";
import { selectThreadActivity } from "../state/threadActivity";
import type { AttentionItem, IssueReadinessDto, ThreadOverview } from "../lib/types";
import { api } from "../lib/api";
import { Button } from "../components/ui/Button";
import { ThreadActivity } from "../components/ui/ThreadActivity";
import { ReadinessChip } from "../components/ReadinessChip";
import { CreateThreadDialog, CreateWorkspaceDialog } from "../nav/dialogs";
import { InheritedAccessChip } from "../components/InheritedAccessChip";
import { ReadOnlyTrustChip } from "../components/ReadOnlyTrustChip";
import { inheritedAccessOf } from "../lib/grants";
import { cn } from "../lib/cn";
import {
  beginReadinessRefresh,
  buildReadinessKey,
  completeReadinessRefresh,
  failReadinessRefresh,
  isReadinessResponseApplicable,
  selectVisibleReadiness,
  type ReadinessFetchState,
  type StoredReadiness,
} from "../lib/readinessKey";
import {
  deriveIssueStatus,
  ISSUE_BOARD_STATUSES,
  type IssueBoardStatus,
} from "../lib/issue-board";
import { issueBoardSignalKey, issueBoardSignalReason } from "../lib/attention-reason";

type Phase = IssueBoardStatus;
type PrChangedEvent = { thread_id: number };

const COLUMN_META: Record<IssueBoardStatus, { label: string; dot: string }> = {
  queued: { label: "wsboard.queued", dot: "bg-idle" },
  working: { label: "wsboard.working", dot: "bg-running" },
  review: { label: "wsboard.review", dot: "bg-brand" },
  done: { label: "wsboard.done", dot: "bg-accent" },
};

function threadAttentionCount(o: ThreadOverview, items: AttentionItem[]): number {
  return items.filter((item) => attentionThreadId(item) === o.thread_id).length;
}

function threadAttentionIds(o: ThreadOverview, items: AttentionItem[]): string[] {
  // Keep lead/issue-level attention in the refresh signature even though it
  // maps to no direction card; backend readiness models it as a virtual lane.
  return items
    .filter((item) => attentionThreadId(item) === o.thread_id)
    .map((item) => item.id);
}

function progressBarColor(attention: number, failing: number): string {
  if (attention > 0) return "bg-waiting";
  if (failing > 0) return "bg-danger";
  return "bg-brand";
}

const COLUMNS: { key: Phase; label: string; dot: string }[] = ISSUE_BOARD_STATUSES.map((key) => ({
  key,
  ...COLUMN_META[key],
}));

export function WorkspaceKanban() {
  const {
    overview,
    refreshOverview,
    attentionItems,
    checksByDirection,
    selectThread,
  } = useStore();
  const { t } = useTranslation();
  const [readinessPollRevision, setReadinessPollRevision] = useState(0);

  useEffect(() => {
    void refreshOverview();
  }, [refreshOverview]);

  useEffect(() => {
    const poll = setInterval(() => {
      setReadinessPollRevision((revision) => revision + 1);
    }, 60_000);
    return () => {
      clearInterval(poll);
    };
  }, []);

  // Phase from the stored direction statuses — same rollup as weft-codex
  // `deriveIssueStatus`. Needs-you is a tag on the card, not a stage.
  const phaseOf = (o: ThreadOverview): Phase => deriveIssueStatus(o.statuses);

  // Cards waiting on the human (or with a failing check) bubble to the top of
  // their column — the attention signal without hijacking the stage. Same
  // thread-level accounting as the card badge, so lead questions sort up too.
  const urgent = (o: ThreadOverview): boolean =>
    threadAttentionCount(o, attentionItems) > 0 ||
    (o.attention_reasons ?? []).some(Boolean) ||
    o.direction_ids.some((id) =>
      (checksByDirection[id] ?? []).some((rc) => rc.checks.some((c) => c.status === "fail")),
    );

  if (overview.length === 0) {
    return <EmptyBoard />;
  }

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <div className="min-h-0 flex-1 overflow-auto">
        <div className="flex h-full min-w-0 gap-3 px-5 py-4">
          {COLUMNS.map((col) => {
            const cards = overview
              .filter((o) => phaseOf(o) === col.key)
              .sort((a, b) => Number(urgent(b)) - Number(urgent(a)));
            return (
              <div
                key={col.key}
                className="flex min-w-[260px] max-w-[360px] flex-1 flex-col rounded-[var(--radius-lg)] border border-border bg-surface/35"
              >
                <div className="flex items-center gap-2 border-b border-border px-3 py-2.5">
                  <span
                    className={cn(
                      "h-1.5 w-1.5 rounded-full",
                      col.dot,
                      col.key === "working" && "weft-pulse",
                    )}
                  />
                  <span className="text-[11.5px] font-semibold text-ink-muted">
                    {t(col.label)}
                  </span>
                  <span className="ml-auto font-mono text-[11px] tabular-nums text-ink-faint">
                    {cards.length}
                  </span>
                </div>
                <div className="flex min-h-0 flex-1 flex-col gap-2 p-2">
                  {cards.map((o) => (
                    <ThreadCard
                      key={o.thread_id}
                      o={o}
                      onOpen={() => void selectThread(o.thread_id)}
                      readinessPollRevision={readinessPollRevision}
                    />
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
    </div>
  );
}

function EmptyBoard() {
  const { activeWorkspaceId } = useStore();
  const { t } = useTranslation();
  const [dlg, setDlg] = useState<null | "ws" | "thread">(null);
  const hasWs = activeWorkspaceId != null;

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <div className="flex flex-1 flex-col items-center justify-center px-6 text-center">
        <div className="grid h-11 w-11 place-items-center rounded-[var(--radius-lg)] border border-border bg-surface">
          <Layers size={20} className="text-brand" />
        </div>
        <h2 className="mt-3 text-[14px] font-semibold text-ink">
          {hasWs ? t("workspace.emptyTitleHas") : t("workspace.emptyTitleNoWs")}
        </h2>
        <p className="mt-1.5 max-w-sm text-[12px] leading-relaxed text-ink-faint">
          {hasWs ? t("workspace.emptyBodyHas") : t("workspace.emptyBodyNoWs")}
        </p>
        <Button
          variant="primary"
          className="mt-4"
          onClick={() => setDlg(hasWs ? "thread" : "ws")}
        >
          {hasWs ? <SquarePen size={14} /> : <Plus size={14} />}
          {hasWs ? t("nav.newThread") : t("nav.newWorkspace")}
        </Button>

        <CreateWorkspaceDialog open={dlg === "ws"} onOpenChange={(o) => !o && setDlg(null)} />
        <CreateThreadDialog open={dlg === "thread"} onOpenChange={(o) => !o && setDlg(null)} />
      </div>
    </div>
  );
}

function ThreadCard({
  o,
  onOpen,
  readinessPollRevision,
}: {
  o: ThreadOverview;
  onOpen: () => void;
  readinessPollRevision: number;
}) {
  const {
    sessions,
    attentionItems,
    checksByDirection,
    openNeeds,
    leadTurn,
    authGrants,
    readOnlyGrants,
  } = useStore();
  const { t } = useTranslation();
  const [storedReadiness, setStoredReadiness] = useState<StoredReadiness<IssueReadinessDto> | null>(
    null,
  );
  const [prReadinessRevision, setPrReadinessRevision] = useState(0);
  const threadIdRef = useRef(o.thread_id);
  const readinessRequestRevisionRef = useRef(0);
  threadIdRef.current = o.thread_id;
  const readinessDirections = o.direction_ids.map((id, index) => ({
    id,
    status: o.statuses[index] ?? "",
  }));
  const planReadinessSignature =
    o.plan_status === null ? null : `${o.plan_status}:${o.plan_created_at ?? ""}`;
  const readinessKey = buildReadinessKey({
    directions: readinessDirections,
    attentionIds: threadAttentionIds(o, attentionItems),
    worktrees: (o.readiness_worktrees ?? []).map((worktree) => ({
      directionId: worktree.direction_id,
      worktreeId: worktree.worktree_id,
      exists: worktree.exists,
    })),
    workerSessions: Object.values(sessions).map((session) => ({
      directionId: session.directionId,
      repoId: session.repoId,
      sessionId: session.info.session_id,
      status: session.status,
    })),
    planStatus: planReadinessSignature,
    prRevision: prReadinessRevision,
  });
  const visibleReadiness = selectVisibleReadiness(storedReadiness, o.thread_id, readinessKey);

  useEffect(() => {
    const request = {
      threadId: o.thread_id,
      revision: readinessRequestRevisionRef.current + 1,
    };
    const requestKey = readinessKey;
    readinessRequestRevisionRef.current = request.revision;
    setStoredReadiness({
      threadId: request.threadId,
      key: requestKey,
      state: beginReadinessRefresh(),
    });
    let cancelled = false;
    const storeResponse = (state: ReadinessFetchState<IssueReadinessDto>) => {
      setStoredReadiness((current) => {
        if (
          !isReadinessResponseApplicable(
            request,
            threadIdRef.current,
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
      .then((next) => {
        if (cancelled) {
          return;
        }
        storeResponse(completeReadinessRefresh(next));
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
  }, [o.thread_id, readinessKey, readinessPollRevision]);

  useEffect(() => {
    let cancelled = false;
    let unlisten: (() => void) | undefined;
    void listen<PrChangedEvent>("pr://changed", (event) => {
      if (!cancelled && event.payload.thread_id === threadIdRef.current) {
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

  const activity = selectThreadActivity({
    workerSessions: Object.values(sessions),
    directionIds: o.direction_ids,
    leadState: leadTurn[o.thread_id]?.state,
  });
  // Full access and always-allow rules both count: either is a standing grant
  // that carries over (#89 makes Always persist across restarts too), so the
  // marker and its one-click revoke must cover both. The chip re-derives the
  // kind-accurate copy from this same helper.
  const inherited = inheritedAccessOf(authGrants, o.thread_id) !== null;
  // Issue #103: dispatch approval propagated a read-only auto-allow to the
  // whole issue. A SEPARATE, narrower grant from Full/Always above — in-memory
  // only, never survives a restart, never covers Write/NetworkOrCredential/
  // Unknown — so it gets its own marker rather than folding into `inherited`.
  const readOnlyTrusted = readOnlyGrants.issue.includes(o.thread_id);
  const done = o.statuses.filter((s) => s === "done").length;
  const attention = threadAttentionCount(o, attentionItems);
  const directionReasons = (o.attention_reasons ?? []).filter(Boolean);
  const dispatchSignalOptions = {
    leadAttention: false,
    leadReason: "",
    directionReasons,
  };
  const dispatchSignal = directionReasons.length
    ? t(issueBoardSignalKey(dispatchSignalOptions))
    : "";
  const dispatchReason = issueBoardSignalReason(dispatchSignalOptions);
  const flagged = attention > 0 || directionReasons.length > 0;
  const failing = o.direction_ids.filter((id) =>
    (checksByDirection[id] ?? []).some((rc) => rc.checks.some((c) => c.status === "fail")),
  ).length;
  const total = Math.max(o.direction_ids.length, 1);
  const donePct = Math.min(100, Math.round((done / total) * 100));
  const progressColor = progressBarColor(attention, failing);

  return (
    <motion.button
      layout
      onClick={onOpen}
      className={cn(
        "group flex flex-col gap-2.5 rounded-[var(--radius-lg)] border bg-surface p-3 text-left transition-colors hover:border-border-strong hover:bg-raised",
        flagged ? "border-waiting/45" : "border-border",
      )}
    >
      <div className="flex items-start gap-2">
        <span className="min-w-0 flex-1 text-[13px] font-semibold leading-snug text-ink">
          {o.title}
        </span>
        {attention > 0 && (
          <span
            title={t("needs.title")}
            onClick={(e) => {
              e.stopPropagation();
              openNeeds();
            }}
            className="grid h-5 min-w-5 shrink-0 cursor-pointer place-items-center rounded-full bg-waiting text-[10px] font-semibold tabular-nums text-bg transition-opacity hover:opacity-80"
          >
            {attention}
          </span>
        )}
      </div>

      <div className="flex flex-wrap items-center gap-1.5">
        <span className="shrink-0 rounded-full border border-border bg-bg px-1.5 py-0.5 text-[10.5px] text-ink-faint">
          {t(`kind.${o.kind}`, o.kind)}
        </span>
        {o.write_repos.slice(0, 3).map((r) => (
          <span
            key={r.id}
            className="rounded-full border border-border bg-bg px-1.5 py-0.5 font-mono text-[10.5px] text-ink-muted"
          >
            {r.name}
          </span>
        ))}
        {o.write_repos.length > 3 && (
          <span className="rounded-full border border-border bg-bg px-1.5 py-0.5 font-mono text-[10.5px] text-ink-faint">
            +{o.write_repos.length - 3}
          </span>
        )}
        <ReadinessChip
          state={visibleReadiness}
          className="max-w-full"
        />
        {inherited && <InheritedAccessChip threadId={o.thread_id} />}
        {readOnlyTrusted && <ReadOnlyTrustChip threadId={o.thread_id} />}
        {dispatchSignal ? (
          <span
            className="max-w-full truncate rounded-full border border-waiting/30 bg-waiting/10 px-1.5 py-0.5 text-[10.5px] text-waiting"
            title={dispatchSignal}
            data-attention-reason={dispatchReason || undefined}
          >
            {dispatchSignal}
          </span>
        ) : null}
      </div>

      {(o.direction_ids.length > 0 || activity.kind !== "idle") && (
        <div className="flex items-center gap-2">
          {o.direction_ids.length > 0 && (
            <>
              <div className="h-1 min-w-0 flex-1 overflow-hidden rounded-full bg-bg">
                <span
                  className={cn("block h-full rounded-full", progressColor)}
                  style={{ width: `${donePct}%` }}
                />
              </div>
              <span className="font-mono text-[11px] tabular-nums text-ink-faint">
                {done}/{o.direction_ids.length}
              </span>
            </>
          )}
          <ThreadActivity activity={activity} className="text-[11px]" />
          {failing > 0 && (
            <span
              title={t("workspace.failing", { count: failing })}
              className="flex items-center gap-1 text-[11px] tabular-nums text-danger"
            >
              <X size={11} />
              {failing}
            </span>
          )}
        </div>
      )}
    </motion.button>
  );
}
