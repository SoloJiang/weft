import { useCallback, useEffect, useRef, useState, type ReactNode } from "react";
import { Virtuoso, type VirtuosoHandle } from "react-virtuoso";
import { useTranslation } from "react-i18next";
import { ArrowRight, Check, CheckCheck, CircleAlert, Copy, Sparkles, Undo2, type LucideIcon } from "lucide-react";
import type { LeadMessage, PermissionAsk, QueuedItem, ResolvedProposal } from "../lib/types";
import { receiptStateOf, type ReceiptState } from "../state/leadSnapshot";
import { Markdown, STREAM_CARET_CLASS } from "../components/Markdown";
import { QueueStack } from "./QueueStack";
import {
  Attachment,
  Message,
  Tool,
  ToolActivity,
  type AiToolStatus,
} from "../components/ai-elements";
import { cn } from "../lib/cn";
import {
  cleanToolName,
  compactToolTarget,
  toolDoneLabelKey,
  toolIcon,
  toolLabelKey,
  toolAllowsFileTarget,
} from "./transcriptBits";
import { groupTimeline, nodeKey, topLevelRows, type TimelineNode } from "./collabBranches";
import { ActionCardBlock, type ActionCardAction } from "./blocks/ActionCardBlock";
import { PlanCardBlock, type PlanCardSplitItem } from "./blocks/PlanCardBlock";
import { TestCasesCard } from "./blocks/TestCasesCard";
import { api } from "../lib/api";
import { currentLang } from "../i18n";
import { toast } from "../components/Toast";
import { PermissionBar } from "./PermissionBar";
import type { useRepoActions } from "./useRepoActions";
import type { ChatHistoryStatus } from "../state/chatHistory";
import { ToolIcon, toolFullName } from "../components/ToolIcon";
import { switchKindOf } from "./engineSwitch";
import { routeReasonKey } from "../lib/engineRoutingDisplay";

type RunAction = ReturnType<typeof useRepoActions>["run"];

const CHAT_BOTTOM_THRESHOLD = 80;

function pinNativeScrollerToBottom(scroller: HTMLElement) {
  const bottom = Math.max(0, scroller.scrollHeight - scroller.clientHeight);
  if (Math.abs(scroller.scrollTop - bottom) > 0.5) scroller.scrollTop = bottom;
}

/**
 * The chat-engine timeline: renders weft-owned LeadMessage rows (no polling,
 * no jsonl). Structured cards (proposal/approval/worker events) live inline in
 * the flow, where they happened — the conversation IS the console. Tool calls
 * are `kind:"tool"` rows, inline and expandable, in the order they ran; the
 * bottom activity line is only the generic "working" pulse between rows.
 *
 * The lead host wires up runAction/promptText so action_card buttons trigger
 * the real repo flows; worker hosts (Observe/Session) omit them and any
 * historical action_card rows fall back to read-only display.
 */
export function ChatTimeline({
  messages,
  historyStatus,
  timelineKey,
  onRetryHistory,
  busy,
  activity,
  onReviewProposal,
  proposal,
  runAction,
  actionsBusy,
  threadId,
  workspaceId,
  promptText,
  cwd,
  emptyState,
  asks = [],
  queue = [],
  onRemove = () => {},
  onEdit = () => {},
  onReorder = () => {},
  onRewind,
  onOpenTestPlan,
  testCaseCount = 0,
}: {
  messages: LeadMessage[];
  /** Persisted history must be ready before the virtual list first mounts. */
  historyStatus: ChatHistoryStatus;
  /** Identity of the actual conversation, not merely its parent thread. */
  timelineKey: string;
  onRetryHistory?: () => void;
  busy: boolean;
  /** Pending (not-yet-sent) queued messages, shown in the bottom stack. */
  queue?: QueuedItem[];
  onRemove?: (id: number) => void;
  onEdit?: (id: number, text: string) => void;
  onReorder?: (order: number[]) => void;
  /** Rewind the conversation to just before a completed user message; omit →
   *  no rewind affordance on user rows. */
  onRewind?: (id: number) => void;
  /** Open the test-plan panel from a test_cases card (lead host only). */
  onOpenTestPlan?: () => void;
  /** Live leaf-case count of the issue's test_plan (0 = none). Sourced from the
   *  table by the host so the plan card matches what the panel opens, not the
   *  stale test_cases summary row (a user edit posts no new summary). */
  testCaseCount?: number;
  /** The tool call executing right now (transient), if any. */
  activity?: { name: string; summary: string } | null;
  onReviewProposal: () => void;
  /** The active thread's live plan, binding the LATEST proposal card to its
   *  open/confirmed state. Omit (worker hosts) → proposal cards render settled. */
  proposal?: ResolvedProposal | null;
  /** Lead-only: dispatch a repo action card. Omit → cards render read-only. */
  runAction?: RunAction;
  actionsBusy?: Record<string, boolean>;
  threadId?: number | null;
  workspaceId?: number | null;
  promptText?: (title: string, placeholder?: string) => Promise<string | null>;
  /** Session working dir — resolves relative file paths agents mention. */
  cwd?: string;
  /** Empty-state slot: the host injects whatever to show when the timeline is empty
   *  (lead/worker pass a LeadEmptyState; the curator panel passes its own line). The
   *  timeline itself stays empty-state-agnostic. */
  emptyState?: ReactNode;
  /** This session's pending permission asks — rendered as an inline card at the
   *  bottom of the conversation (the agent's position), not a top banner. */
  asks?: PermissionAsk[];
}) {
  const virtuosoRef = useRef<VirtuosoHandle>(null);
  const followBottomRef = useRef(true);
  const scrollerElementRef = useRef<HTMLElement | null>(null);
  const rootRef = useRef<HTMLDivElement>(null);
  const setScrollerElement = useCallback((element: HTMLElement | Window | null) => {
    scrollerElementRef.current = element instanceof HTMLElement ? element : null;
  }, []);

  // Tool calls render inline as expandable `kind:"tool"` rows for every dialect
  // (claude/opencode/codex alike); only `meta` bookkeeping rows are hidden.
  // Pending queued messages live in the bottom QueueStack, not the timeline.
  const visible = messages.filter((m) => m.kind !== "meta" && m.status !== "queued");

  // Fold each sub-agent's own interleaved output into a collapsible branch
  // hanging off whichever row first revealed its thread id (issue #99) — see
  // collabBranches.ts for the backend signal this groups on (deterministic,
  // not arrival order) and why a dialect without that signal degrades to
  // exactly today's flat rendering with no special-casing here.
  const roots = groupTimeline(visible);
  // What isLastAssistant / isTail / isLatestProposal / hasPendingUserReply
  // scan: the top-level view only — a row a reader would have to expand a
  // collapsed branch to even see must never count as "the newest thing on
  // screen" (review [P1]; see topLevelRows's doc).
  const topLevel = topLevelRows(roots);

  const growthLen = visible
    .filter((m) => m.kind === "text" || m.kind === "tool")
    .reduce((n, m) => n + m.content.length, 0);
  useEffect(() => {
    if (historyStatus !== "ready" || !followBottomRef.current || visible.length === 0) return;
    virtuosoRef.current?.scrollToIndex({ index: "LAST", align: "end", behavior: "auto" });
  }, [visible.length, growthLen, busy, activity, historyStatus]);

  // Conversation identity is finer than thread identity: each worker session
  // under a thread owns independent history and scroll state. Reset the intent
  // whenever that identity changes, then pin once its complete history mounts.
  useEffect(() => {
    if (historyStatus !== "ready") return;
    followBottomRef.current = true;
    requestAnimationFrame(() =>
      virtuosoRef.current?.scrollToIndex({ index: "LAST", align: "end", behavior: "auto" }),
    );
  }, [timelineKey, historyStatus]);

  const showList = visible.length > 0 || busy || asks.length > 0;

  // Re-pin to the latest message when this timeline is REVEALED (its height goes
  // 0 → >0). A chat kept mounted-but-hidden (the curator behind the detail surface,
  // an inactive tab) can't position its virtualized list while `display:none`, so the
  // initial bottom-scroll is lost and switching in would land mid-history. rAF lets
  // Virtuoso lay out at the new size before we scroll.
  useEffect(() => {
    if (historyStatus !== "ready") return;
    const el = rootRef.current;
    if (!el || typeof ResizeObserver === "undefined") return;
    let prevH = el.offsetHeight;
    const ro = new ResizeObserver(() => {
      const h = el.offsetHeight;
      if (prevH === 0 && h > 0) {
        requestAnimationFrame(() =>
          virtuosoRef.current?.scrollToIndex({ index: "LAST", align: "end", behavior: "auto" }),
        );
      }
      prevH = h;
    });
    ro.observe(el);
    return () => ro.disconnect();
  }, [showList, historyStatus]);

  // Bottom intent belongs to the reader, not to the current viewport geometry.
  // Virtuoso's atBottomStateChange also fires when the viewport gets shorter;
  // a queue/permission stack can therefore turn a reader who WAS at bottom into
  // "not at bottom" without any scroll input. Track native scroll events instead:
  // an external flex resize changes clientHeight but does not masquerade as a
  // reader scroll, while wheel/trackpad/scrollbar movement still updates intent.
  useEffect(() => {
    if (historyStatus !== "ready") return;
    const scroller = scrollerElementRef.current;
    if (!scroller) return;
    const recordReaderIntent = () => {
      const bottomDistance = Math.max(
        0,
        scroller.scrollHeight - scroller.scrollTop - scroller.clientHeight,
      );
      followBottomRef.current = bottomDistance <= CHAT_BOTTOM_THRESHOLD;
    };
    scroller.addEventListener("scroll", recordReaderIntent, { passive: true });
    return () => scroller.removeEventListener("scroll", recordReaderIntent);
  }, [timelineKey, showList, historyStatus]);

  // Queue, permission, and activity chrome sits outside Virtuoso and takes flex
  // height from its viewport. Preserve the last row across that resize only when
  // the reader was already following the bottom; an intentionally scrolled-up
  // reader keeps the same scrollTop. The immediate write runs in ResizeObserver's
  // pre-paint phase, and the rAF repeat covers Virtuoso's follow-up measurement.
  useEffect(() => {
    if (historyStatus !== "ready" || typeof ResizeObserver === "undefined") return;
    const scroller = scrollerElementRef.current;
    if (!scroller) return;
    let previousHeight = scroller.clientHeight;
    let frame = 0;
    const pinIfFollowing = () => {
      if (followBottomRef.current && scrollerElementRef.current === scroller) {
        pinNativeScrollerToBottom(scroller);
      }
    };
    const ro = new ResizeObserver(() => {
      const nextHeight = scroller.clientHeight;
      if (nextHeight === previousHeight) return;
      previousHeight = nextHeight;
      if (!followBottomRef.current) return;
      pinIfFollowing();
      cancelAnimationFrame(frame);
      frame = requestAnimationFrame(pinIfFollowing);
    });
    ro.observe(scroller);
    return () => {
      cancelAnimationFrame(frame);
      ro.disconnect();
    };
  }, [timelineKey, showList, historyStatus]);

  if (historyStatus !== "ready") {
    return <ChatHistoryState status={historyStatus} onRetry={onRetryHistory} />;
  }

  if (!showList) {
    return <>{emptyState}</>;
  }

  return (
    <div ref={rootRef} className="flex min-h-0 flex-1 flex-col">
      <Virtuoso<TimelineNode>
        key={timelineKey}
        ref={virtuosoRef}
        scrollerRef={setScrollerElement}
        // overflow-x-hidden: the scroller's inline overflow-y:auto computes
        // overflow-x to auto, so any over-wide row scrolls the WHOLE timeline
        // sideways. The timeline never pans — wide content (code, tables)
        // scrolls inside its own block (see .weft-md pre/table).
        className="weft-chat-virtualizer min-h-0 flex-1 overflow-x-hidden"
        data={roots}
        computeItemKey={(_index, node) => nodeKey(node)}
        initialTopMostItemIndex={
          roots.length > 0 ? { index: roots.length - 1, align: "end" } : undefined
        }
        increaseViewportBy={{ top: 600, bottom: 600 }}
        components={{ Header }}
        itemContent={(_index, node) => (
          // Every row keeps its bottom padding — INCLUDING the last one, which
          // is what separates the streaming tail from the composer / bottom
          // stack. The old `index < length-1` gate left the final message flush
          // against the input box.
          <div className="mx-auto w-full min-w-0 max-w-[820px] px-4 pb-2.5">
            <TimelineNodeRow
              node={node}
              all={topLevel}
              onReviewProposal={onReviewProposal}
              proposal={proposal ?? null}
              runAction={runAction}
              actionsBusy={actionsBusy}
              threadId={threadId ?? null}
              workspaceId={workspaceId ?? null}
              promptText={promptText}
              cwd={cwd}
              queuedCount={queue.length}
              onOpenTestPlan={onOpenTestPlan}
              testCaseCount={testCaseCount}
              onRewind={onRewind}
            />
          </div>
        )}
      />
      {/* The in-flight tool / working indicator sits OUTSIDE the virtualized
          scroller as a fixed bottom bar. Keeping it out of the list makes the
          last message the unambiguous list bottom and keeps the indicator
          visible even while the user scrolls back through history. */}
      {(busy || queue.length > 0 || asks.length > 0) && (
        <div
          data-testid="chat-bottom-stack"
          className="mx-auto w-full max-w-[820px] shrink-0 px-4 pb-3"
        >
          <div className="flex flex-col gap-1.5">
            <PermissionBar asks={asks} />
            {busy && <BusyIndicator activity={activity} cwd={cwd} />}
            <QueueStack
              items={queue}
              onRemove={onRemove}
              onEdit={onEdit}
              onReorder={onReorder}
            />
          </div>
        </div>
      )}
    </div>
  );
}

function ChatHistoryState({
  status,
  onRetry,
}: {
  status: Exclude<ChatHistoryStatus, "ready">;
  onRetry?: () => void;
}) {
  const { t } = useTranslation();
  if (status === "loading") {
    return (
      <div className="grid min-h-0 flex-1 place-items-center text-[12px] text-ink-faint" role="status">
        {t("lead.loading")}
      </div>
    );
  }
  return (
    <div className="flex min-h-0 flex-1 flex-col items-center justify-center gap-2 px-4 text-center">
      <p className="text-[12px] text-ink-muted" role="alert">
        {t("lead.historyLoadError")}
      </p>
      {onRetry && (
        <button
          type="button"
          onClick={onRetry}
          className="rounded-[var(--radius-md)] border border-border px-2.5 py-1 text-[12px] font-medium text-ink-muted transition-colors hover:border-border-strong hover:bg-hover hover:text-ink"
        >
          {t("lead.retryHistory")}
        </button>
      )}
    </div>
  );
}

function BusyIndicator({
  activity,
  cwd,
}: {
  activity?: { name: string; summary: string } | null;
  cwd?: string;
}) {
  const { t } = useTranslation();
  // Empty name = explicit clear from the engine (e.g. thinking → first answer token).
  if (activity?.name) return <ToolStatus name={activity.name} summary={activity.summary} cwd={cwd} />;
  return (
    <div className="flex items-center gap-1.5 px-1 text-[11px] text-ink-faint">
      <span className="h-1.5 w-1.5 animate-pulse rounded-full bg-running" />
      {t("lead.working")}
    </div>
  );
}

function Header() {
  return <div className="h-4" />;
}

function deriveToolStatus(m: LeadMessage, c: Record<string, unknown>): AiToolStatus {
  if (m.status === "streaming") return "streaming";
  if (c.is_error === true || m.status === "error") return "error";
  return "complete";
}

/** The tool call in flight — pulsing, transient, precise about WHAT it calls. */
function ToolStatus({ name, summary, cwd }: { name: string; summary: string; cwd?: string }) {
  const { t } = useTranslation();
  const Icon = toolIcon(name);
  const labelKey = toolLabelKey(name);
  const { target, targetToken, added, removed } = compactToolTarget(name, summary);
  // For unrecognized tools (MCP etc.) the generic "Calling" says nothing —
  // show the cleaned tool identity instead. Thinking streams freeform text in
  // `summary` — show that tail rather than a path-like target.
  const generic = labelKey === "session.toolCalling";
  const thinking = labelKey === "session.toolThinking";
  return (
    <ToolActivity
      icon={Icon}
      label={generic ? cleanToolName(name) : t(labelKey)}
      target={generic || thinking ? undefined : target}
      targetToken={generic || thinking ? undefined : targetToken}
      cwd={cwd}
      summary={generic || thinking ? summary : undefined}
      added={thinking ? undefined : added}
      removed={thinking ? undefined : removed}
    />
  );
}

/** Render a tool input for display: strings verbatim, objects pretty-printed. */
function formatToolValue(v: unknown): string {
  if (v == null) return "";
  if (typeof v === "string") return v;
  try {
    return JSON.stringify(v, null, 2);
  } catch {
    return String(v);
  }
}

function parse(content: string): Record<string, unknown> {
  try {
    return JSON.parse(content) as Record<string, unknown>;
  } catch {
    return {};
  }
}

// Wider sibling to `parse` for sentinel-payload rows (action_card) where the
// JSON may legitimately contain arrays nested at the top — we still only
// accept an object root, but reject scalars/arrays without throwing.
function safeParseObj(content: string): Record<string, unknown> {
  try {
    const v: unknown = JSON.parse(content);
    return v && typeof v === "object" && !Array.isArray(v)
      ? (v as Record<string, unknown>)
      : {};
  } catch {
    return {};
  }
}

function stringArray(value: unknown): string[] {
  if (!Array.isArray(value)) return [];
  return value.filter((item): item is string => typeof item === "string");
}

// Read-only history replay: only the most recent assistant row is interactive.
// Older action_cards stay rendered for context but their buttons are disabled.
// Tool rows are role:"assistant" too: skip only those from m's OWN turn (a card
// and the tools it kicked off share a turn) so they don't read-only the card —
// but a LATER turn's tool rows are genuine newer activity and must disqualify it.
// test_cases summary rows from m's OWN turn are companion artifacts (a plan
// card emitted alongside its test-case doc must stay approvable) — but a LATER
// turn's summary is real progress (e.g. the lead re-derived after an edit) and
// must retire the card like any other newer assistant activity.
function isLastAssistant(m: LeadMessage, all: LeadMessage[]): boolean {
  for (let i = all.length - 1; i >= 0; i--) {
    const row = all[i];
    if ((row.kind === "tool" || row.kind === "test_cases") && row.turn_id === m.turn_id) {
      continue;
    }
    if (row.role === "assistant") return row.id === m.id;
  }
  return false;
}

// One settled card: a muted, non-interactive one-line summary. Shared by the
// proposal / action_card collapse and the permission/question settled-trail
// rows so a resolved interaction reads the same wherever it lands.
function SettledLine({ label }: { label: string }) {
  return (
    <div className="flex items-center gap-2 rounded-[var(--radius-md)] border border-border bg-surface px-3 py-2 text-[12px] text-ink-muted">
      <Check size={13} className="shrink-0 text-ink-faint" />
      <span className="truncate">{label}</span>
    </div>
  );
}

const permissionAnswerLabelKeys = {
  allow: "settled.permissionAllow",
  deny: "settled.permissionDeny",
  always: "settled.permissionAlways",
  full: "settled.permissionFull",
} as const;

type PermissionAnswer = keyof typeof permissionAnswerLabelKeys;

function permissionAnswerOf(answer: string): PermissionAnswer {
  switch (answer) {
    case "deny":
      return "deny";
    case "always":
      return "always";
    case "full":
      return "full";
    default:
      return "allow";
  }
}

// The live plan binds to the MOST RECENT proposal row only: a re-propose
// replaces the stored plan, so older proposal cards are already settled.
function isLatestProposal(m: LeadMessage, all: LeadMessage[]): boolean {
  for (let i = all.length - 1; i >= 0; i--) {
    if (all[i].kind === "proposal") return all[i].id === m.id;
  }
  return false;
}

// A delivery receipt (issue #94) is only worth showing while it's the newest
// information on screen: once a later row exists — a reply, a tool call, even
// a newer user send — that row is louder proof of what happened than a stale
// "delivered"/"processing" tag left on an old bubble. `all` is the TOP-LEVEL
// row list (`topLevelRows`, issue #99) — meta/queued-free AND with any
// collab-branch children folded out — so "last row" means "last row the
// reader can see without expanding anything," exactly the conversation's
// visible tip.
function isTail(m: LeadMessage, all: LeadMessage[]): boolean {
  return all.length > 0 && all[all.length - 1].id === m.id;
}

// Single source of truth for the receipt's visuals — one Record per concern
// (icon, copy, color) keyed by the SAME ReceiptState `receiptStateOf` derives,
// so a new state is a compile error here until every map handles it (no
// re-derived booleans, no nested ternaries).
const RECEIPT_ICON: Record<ReceiptState, LucideIcon | null> = {
  delivered: Check,
  consumed: CheckCheck,
  interrupted: null,
  error: null,
};
const RECEIPT_LABEL_KEYS: Record<ReceiptState, string> = {
  delivered: "lead.receiptDelivered",
  consumed: "lead.receiptConsumed",
  interrupted: "lead.interrupted",
  error: "lead.errored",
};
const RECEIPT_CLASS: Record<ReceiptState, string> = {
  delivered: "text-ink-faint",
  consumed: "text-ink-faint",
  interrupted: "text-waiting",
  error: "text-danger",
};

/** The line under a sent message: "message never reached the agent" (error /
 *  interrupted — always shown, permanent history) vs "reached it, here's
 *  where it stands" (delivered / consumed — shown only at the timeline's
 *  tail, see `isTail`). Distinguishing these at a glance is the point of
 *  issue #94: a stuck "delivered" forever reads very differently from a
 *  silently vanished send. */
function ReceiptLine({ state }: { state: ReceiptState }) {
  const { t } = useTranslation();
  const Icon = RECEIPT_ICON[state];
  return (
    <p className={cn("flex items-center gap-1 self-end text-[11px]", RECEIPT_CLASS[state])}>
      {Icon && <Icon size={11} />}
      {t(RECEIPT_LABEL_KEYS[state])}
    </p>
  );
}

/** Everything a row needs to render EXCEPT which row it is — shared by
 *  `TimelineRow` (a single LeadMessage) and, since issue #99, a
 *  `CollabBranchRow`'s nested children (rendered through the same
 *  `TimelineNodeRow` dispatcher, unchanged prop-for-prop from before branches
 *  existed). `all` is always the TOP-LEVEL row list (`topLevelRows`), never
 *  the raw flat array — see that function's doc. */
interface TimelineRowProps {
  all: LeadMessage[];
  onReviewProposal: () => void;
  proposal: ResolvedProposal | null;
  runAction?: RunAction;
  actionsBusy?: Record<string, boolean>;
  threadId: number | null;
  workspaceId: number | null;
  promptText?: (title: string, placeholder?: string) => Promise<string | null>;
  cwd?: string;
  /** Messages waiting in the engine queue — a pending revision blocks plan approval. */
  queuedCount?: number;
  /** Open the test-plan panel (lead host only; worker timelines render read-only). */
  onOpenTestPlan?: () => void;
  /** Live leaf-case count of the issue's test_plan, for the plan card row. */
  testCaseCount?: number;
  /** Rewind affordance for completed user text rows (worker hosts only). */
  onRewind?: (id: number) => void;
}

/** Dispatches one virtualized timeline entry: a plain row renders exactly as
 *  before, a branch (issue #99) renders as a collapsible sub-agent container
 *  whose nested rows recurse back through this same dispatcher. */
function TimelineNodeRow({
  node,
  ...rest
}: { node: TimelineNode } & TimelineRowProps) {
  if (node.kind === "row") return <TimelineRow m={node.row} {...rest} />;
  return <CollabBranchRow anchor={node.anchor} branchChildren={node.children} {...rest} />;
}

function TimelineRow({
  m,
  all,
  onReviewProposal,
  proposal,
  runAction,
  actionsBusy,
  threadId,
  workspaceId,
  promptText,
  cwd,
  queuedCount = 0,
  onOpenTestPlan,
  testCaseCount = 0,
  onRewind,
}: { m: LeadMessage } & TimelineRowProps) {
  const { t } = useTranslation();
  const c = parse(m.content);

  // Rewind marker: a quiet centered divider left where a conversation rewind
  // truncated the timeline. System-owned, no hover actions — handled before
  // every other kind so it can never fall through to a bubble.
  if (m.kind === "rewind") {
    return (
      <div className="flex items-center gap-3 px-2 py-1">
        <span className="h-px flex-1 bg-border" />
        <span className="shrink-0 text-[11px] text-ink-faint">{t("session.rewoundMarker")}</span>
        <span className="h-px flex-1 bg-border" />
      </div>
    );
  }

  // Engine/model switch marker (issue #96/#98): quiet centered divider, same
  // treatment as the rewind marker above, but names the concrete before/after
  // so "did my switch actually take?" never requires digging through Settings.
  if (m.kind === "engine_switch") {
    return <EngineSwitchMarker content={c} />;
  }

  // A FAILED auto fail-over attempt (issue #97 review P2): without this, a
  // transient error during the switch would leave the user's engine sitting
  // exhausted with no visible sign Weft even tried — same "system-owned,
  // always part of the record" treatment as a successful switch marker.
  if (m.kind === "quota_failover_failed") {
    return <QuotaFailoverFailedMarker content={c} />;
  }

  if (m.kind === "engine_route") {
    return <EngineRouteMarker content={c} />;
  }

  if (m.kind === "engine_route_blocked") {
    return <EngineRouteBlockedMarker content={c} />;
  }

  // issue #110 T3: a completed (success or failure) auto-merge attempt —
  // same durable, system-owned marker treatment as the others above.
  // Structured content only (never pre-composed prose — review round 1
  // Codex P1), so this renders correctly in either UI language.
  if (m.kind === "pr_auto_merge") {
    return <AutoMergeMarker content={c} />;
  }

  if (m.kind === "tool") {
    const content = parse(m.content);
    const name = typeof content.name === "string" ? content.name : "tool";
    const summary = typeof content.summary === "string" ? content.summary : "";
    const output = typeof content.output === "string" ? content.output : "";
    const inputText = formatToolValue(content.input);
    const status = deriveToolStatus(m, content);
    const Icon = toolIcon(name);
    const labelKey = status === "streaming" ? toolLabelKey(name) : toolDoneLabelKey(name);
    const generic = labelKey === "session.toolCalling" || labelKey === "session.toolCalled";
    const { target, targetToken, added, removed } = compactToolTarget(name, summary);
    const showFileTarget = toolAllowsFileTarget(name);
    return (
      <Tool
        icon={Icon}
        label={generic ? cleanToolName(name) : t(labelKey)}
        summary={summary}
        status={status}
        target={target}
        targetToken={showFileTarget ? targetToken : undefined}
        cwd={cwd}
        added={added}
        removed={removed}
        input={inputText}
        output={output}
        inputLabel={t("tool.input")}
        outputLabel={t("tool.output")}
        showMoreLabel={(hiddenLineCount) => t("tool.showMore", { n: hiddenLineCount })}
        showLessLabel={t("tool.showLess")}
      />
    );
  }

  if (m.kind === "action_card") {
    const parsed = safeParseObj(m.content);
    // Resolved (persisted into the row once its repo flow succeeded): collapse to
    // a settled one-line summary — the loop is closed and it can't re-fire, even
    // after a reload.
    if (typeof parsed.resolved === "string" && parsed.resolved) {
      return <SettledLine label={t("actionCard.resolved", { name: parsed.resolved })} />;
    }
    const title = typeof parsed.title === "string" ? parsed.title : "";
    const body = typeof parsed.body === "string" ? parsed.body : undefined;
    // runtime-checked sentinel payload from the lead — schema enforced by
    // src-tauri/src/lead_chat/sentinels.rs before the row is persisted.
    const actions = Array.isArray(parsed.actions)
      ? parsed.actions.filter(isActionCardAction)
      : [];
    const steps = Array.isArray(parsed.steps)
      ? parsed.steps.filter((step): step is string => typeof step === "string")
      : [];
    // Worker hosts (no runAction wired) and historical rows fall back to
    // read-only — buttons render disabled so the card stays in context but
    // can't fire a flow without a handler.
    const readOnly = !runAction || !promptText || !isLastAssistant(m, all);
    const onAction: ((a: ActionCardAction) => void) | undefined =
      runAction && promptText
        ? (a) =>
            void runAction({
              actionId: a.id,
              kind: a.kind,
              ctx: {
                threadId: threadId ?? undefined,
                messageId: m.id,
                preferredWorkspaceId: workspaceId,
              },
              promptText,
            })
        : undefined;
    return (
      <ActionCardBlock
        title={title}
        body={body}
        steps={steps.length > 0 ? steps : undefined}
        actions={actions}
        readOnly={readOnly}
        busy={actionsBusy ?? {}}
        onAction={onAction ?? (() => {})}
      />
    );
  }

  if (m.kind === "test_cases") {
    // Summary payload persisted by the engine (lead_chat::test_plan::summarize);
    // the document itself lives in the test_plan table and the panel reads it.
    const parsed = safeParseObj(m.content);
    return (
      <TestCasesCard
        title={typeof parsed.title === "string" ? parsed.title : ""}
        branches={stringArray(parsed.branches)}
        caseCount={typeof parsed.caseCount === "number" ? parsed.caseCount : 0}
        onOpen={onOpenTestPlan}
      />
    );
  }

  if (m.kind === "plan_card") {
    const parsed = safeParseObj(m.content);
    // Approved (persisted into the row): collapse to a settled one-line summary
    // so the gate reads as closed and can't re-fire after a reload.
    if (typeof parsed.resolved === "string" && parsed.resolved) {
      return <SettledLine label={t("planCard.approved", { name: parsed.resolved })} />;
    }
    const title = typeof parsed.title === "string" ? parsed.title : "";
    // runtime-checked sentinel payload from the lead — engine only guarantees an
    // object root (src-tauri lead_chat::engine::persist_card_row).
    const split = Array.isArray(parsed.split) ? parsed.split.filter(isPlanSplitItem) : [];
    // `runAction` is only wired on the lead host, so its presence doubles as
    // "this timeline may approve"; worker hosts and older turns are read-only.
    // A pending USER reply also stales the card — a newer user row, a queued
    // user row, or anything waiting in the engine queue is a revision request,
    // and a late approval could be read against the revised plan.
    const readOnly =
      !runAction ||
      threadId == null ||
      !isLastAssistant(m, all) ||
      hasPendingUserReply(m, all) ||
      queuedCount > 0;
    const tid = threadId;
    const onApprove = async () => {
      if (tid == null) return;
      // Feedback first, and only collapse the card once the lead actually
      // accepted the delivery — a stopped lead silently drops hidden input, and
      // a card stamped "approved" with no split coming would mislead.
      const delivered = await api.postLeadToolResult(
        tid,
        { tool: "plan_decision", status: "approved", title },
        currentLang(),
      );
      if (!delivered) {
        toast(t("planCard.deliverFailed"));
        return;
      }
      await api.resolveActionCard(m.id, title || t("planCard.label"));
    };
    // The product thesis (cases inform the plan) is otherwise invisible: link
    // the plan card to the issue's test cases when the issue has derived any.
    // `testCaseCount` is the LIVE test_plan count the host fetched (not the stale
    // test_cases summary), so it matches what the panel's View opens even after a
    // user edit; the link only shows when the panel is openable (lead host).
    return (
      <PlanCardBlock
        title={title}
        requirements={stringArray(parsed.requirements)}
        approach={typeof parsed.approach === "string" ? parsed.approach : ""}
        split={split}
        risks={stringArray(parsed.risks)}
        readOnly={readOnly}
        cwd={cwd}
        onApprove={onApprove}
        testCaseCount={onOpenTestPlan ? testCaseCount : 0}
        onOpenTestCases={onOpenTestPlan}
      />
    );
  }

  if (m.kind === "settled") {
    // Durable trail left when a permission/question card was answered — the
    // interactive card itself vanished from its dock; this is its closed record.
    const v = safeParseObj(m.content);
    const variant = typeof v.variant === "string" ? v.variant : "";
    if (variant === "permission") {
      const summary = typeof v.summary === "string" ? v.summary : "";
      const answer = typeof v.answer === "string" ? v.answer : "allow";
      const key = permissionAnswerLabelKeys[permissionAnswerOf(answer)];
      return <SettledLine label={t(key, { summary })} />;
    }
    if (variant === "ask") {
      const text = typeof v.text === "string" ? v.text : "";
      const answer = typeof v.answer === "string" ? v.answer : "";
      return <SettledLine label={t("settled.askAnswered", { text, answer })} />;
    }
    return null;
  }

  if (m.kind === "command") {
    const command = typeof c.command === "string" ? c.command : "";
    const args = typeof c.args === "string" ? c.args.trim() : "";
    const label = [command, args].filter(Boolean).join(" ");
    return (
      <div className="flex justify-end">
        <span className="inline-flex max-w-[72%] items-center gap-1.5 rounded-[var(--radius-md)] border border-brand/25 bg-brand-ghost px-3 py-2 font-mono text-[12.5px] text-ink">
          <span className="truncate">{label}</span>
        </span>
      </div>
    );
  }

  if (m.kind === "proposal") {
    const count = Number(c.count ?? 0);
    // Count 0 = a withdraw/cancel (the lead's cancel_directions, or a stray empty
    // propose routed to withdraw). Render a settled "已撤回" line, never the
    // interactive "查看并创建" card — that opened a dead-end empty ScopeReview.
    if (count === 0) {
      return (
        <SettledLine label={t("lead.proposalWithdrawn", { rationale: String(c.rationale ?? "") })} />
      );
    }
    // A proposal card is "open" (interactive) only while it is the latest
    // proposal AND its live plan is still awaiting review. Once confirmed (or
    // superseded by a re-propose, or replayed in a worker host with no live
    // plan), it collapses to a settled one-line summary so the interaction
    // closes the loop instead of looping back into the review flow.
    // Guard on thread identity: selectThread sets activeThreadId before the
    // getProposal fetch resolves, so `proposal` can briefly belong to the
    // previously-open thread. Without this match a stale proposed plan could
    // re-open a settled card on the new thread (confirmProposal would then act
    // on the wrong plan).
    const open =
      isLatestProposal(m, all) &&
      proposal != null &&
      proposal.thread_id === m.thread_id &&
      proposal.status === "proposed";
    if (!open) {
      return <SettledLine label={t("lead.proposalResolved", { count })} />;
    }
    return (
      <button
        onClick={onReviewProposal}
        // w-full: a button's auto width is fit-content, so without it the
        // nowrap truncate rationale below sets the button's max-content width
        // and drags the whole timeline into horizontal scroll.
        className="group flex w-full items-center gap-2.5 rounded-[var(--radius-md)] border border-accent/40 bg-accent-ghost px-3 py-2.5 text-left transition-colors hover:border-accent/70"
      >
        <Sparkles size={15} className="shrink-0 text-accent" />
        <div className="min-w-0 flex-1">
          <p className="text-[12.5px] font-medium text-ink">
            {t("lead.proposalReady", { count })}
          </p>
          <p className="truncate text-[11px] text-ink-muted">
            {String(c.rationale ?? "") || t("lead.reviewCreate")}
          </p>
        </div>
        <span className="flex shrink-0 items-center gap-1 text-[11px] font-medium text-accent">
          {t("lead.reviewCreate")}
          <ArrowRight size={12} className="transition-transform group-hover:translate-x-0.5" />
        </span>
      </button>
    );
  }

  if (m.role === "user") {
    const images = stringArray(c.images);
    const files = stringArray(c.files);
    const text = String(c.text ?? "");
    // Delivery receipt (issue #94): error/interrupted are permanent history —
    // the send genuinely never landed, so that stays visible forever. Delivered
    // /consumed are transient — only worth a line while this is still the
    // newest thing on screen (see isTail); once a reply (or a newer send)
    // exists, IT is the live proof and the old receipt would just be noise.
    const receipt = receiptStateOf(m);
    const showReceipt =
      receipt === "error" || receipt === "interrupted" || (receipt != null && isTail(m, all));
    return (
      <Message role="user">
        <div className="flex max-w-[72%] flex-col gap-2 rounded-[var(--radius-lg)] border border-brand/25 bg-brand-ghost px-3.5 py-2.5">
          {images.length > 0 && (
            <div className="flex flex-wrap gap-1.5">
              {images.map((src, imageIndex) => (
                <Attachment
                  key={`${src}-${imageIndex}`}
                  kind="image"
                  label={t("lead.imageAttachment", { count: imageIndex + 1 })}
                  src={src}
                />
              ))}
            </div>
          )}
          {files.length > 0 && (
            <div className="flex flex-wrap gap-1.5">
              {files.map((f) => (
                <Attachment
                  key={f}
                  kind="file"
                  label={f}
                />
              ))}
            </div>
          )}
          {text && (
            <p className="whitespace-pre-wrap break-words text-[13px] leading-relaxed text-ink">
              {text}
            </p>
          )}
          {showReceipt && receipt && <ReceiptLine state={receipt} />}
        </div>
        {text && (
          <MessageActionsRow align="end">
            {onRewind && m.kind === "text" && m.status === "complete" && (
              <RewindMessageButton onClick={() => onRewind(m.id)} />
            )}
            <CopyMessageButton text={text} />
          </MessageActionsRow>
        )}
      </Message>
    );
  }

  // assistant / system text
  const terminal = typeof c.terminal === "string" ? c.terminal : "";
  // A terminal reason maps to a fixed notice; anything else renders the streamed
  // text. One lookup keyed by the reason (not an if-chain) — a new reason is a
  // single entry, and the param-carrying case stays uniform via a thunk.
  const terminalNotice: Record<string, () => string> = {
    error_before_output: () => t("lead.terminalErrorBeforeOutput"),
    agent_not_found: () =>
      t("lead.terminalAgentNotFound", { tool: typeof c.tool === "string" ? c.tool : "" }),
    interrupted_before_output: () => t("lead.terminalInterruptedBeforeOutput"),
  };
  const assistantText = terminalNotice[terminal]?.() ?? String(c.text ?? "");
  return (
    <Message role="assistant">
      <div className="min-w-0 max-w-full overflow-hidden rounded-[var(--radius-lg)] border border-border bg-surface px-3.5 py-3 shadow-[0_12px_34px_-28px_rgba(0,0,0,0.65)]">
        {assistantText && (
          <Markdown text={assistantText} cwd={cwd} caret={m.status === "streaming"} />
        )}
        {m.status === "streaming" && !assistantText && (
          <span className={STREAM_CARET_CLASS} />
        )}
        {m.status === "interrupted" && (
          <p className="mt-1.5 text-[11px] text-waiting">{t("lead.interrupted")}</p>
        )}
        {m.status === "error" && (
          <p className="mt-1.5 text-[11px] text-danger">{t("lead.errored")}</p>
        )}
      </div>
      {assistantText && m.status !== "streaming" && (
        <MessageActionsRow align="start">
          <CopyMessageButton text={assistantText} />
        </MessageActionsRow>
      )}
    </Message>
  );
}

/** Durable, visible record of an engine/model switch (issue #96/#98) — the
 *  same quiet centered-divider treatment the rewind marker uses, but names
 *  the concrete before/after (tool identity + model override, when either
 *  changed) so a switch's outcome is honest and permanent in the transcript,
 *  never just a toast that's gone the moment you look away. Same tool +
 *  same model reads as "reloaded" (see `build_switch_digest`'s doc for why
 *  that's a real, useful action — forcing a stuck engine to restart, or
 *  picking up an externally-edited CLI config). */
function EngineSwitchMarker({ content }: { content: Record<string, unknown> }) {
  const { t } = useTranslation();
  const oldTool = typeof content.old_tool === "string" ? content.old_tool : "";
  const newTool = typeof content.new_tool === "string" ? content.new_tool : "";
  const oldModel = typeof content.old_model === "string" ? content.old_model : null;
  const newModel = typeof content.new_model === "string" ? content.new_model : null;
  const sameTool = switchKindOf(oldTool, newTool) === "reload";
  const label = sameTool
    ? t("session.engineReloadedMarker", { tool: toolFullName(newTool) })
    : t("session.engineSwitchedMarker", { from: toolFullName(oldTool), to: toolFullName(newTool) });
  const modelChanged = oldModel !== newModel;
  // issue #97: Weft's own auto fail-over (never a switch the user clicked) is
  // tagged with this reason so it reads unmistakably as automatic — a claimed
  // engine switch must always be visibly honest about WHO triggered it.
  const isQuotaFailover = content.reason === "quota_exceeded";
  const quotaBasis = typeof content.quota_basis === "string" ? content.quota_basis : "";
  return (
    <div className="flex items-center gap-2 px-2 py-1.5">
      <span className="h-px min-w-4 flex-1 bg-border" />
      <span className="flex min-w-0 flex-1 flex-wrap items-center justify-center gap-1.5 text-center text-[11px] text-ink-faint">
        {!sameTool && oldTool && (
          <>
            <ToolIcon tool={oldTool} size={12} />
            <ArrowRight size={10} />
          </>
        )}
        <ToolIcon tool={newTool} size={12} />
        <span>{label}</span>
        {modelChanged && (
          <span className="font-mono text-[10.5px]">
            {newModel ?? t("session.engineModelCleared")}
          </span>
        )}
        {isQuotaFailover && (
          <span className="rounded-full border border-waiting/30 bg-waiting/15 px-1.5 py-0.5 text-[10px] font-medium text-waiting">
            {t("session.engineSwitchedQuotaReason")}
          </span>
        )}
        {isQuotaFailover && quotaBasis && (
          <span className="rounded-full border border-waiting/30 bg-waiting/10 px-1.5 py-0.5 text-[10px] text-waiting">
            {t("session.engineRouteQuotaBasis", { status: quotaStatusLabel(t, quotaBasis) })}
          </span>
        )}
      </span>
      <span className="h-px min-w-4 flex-1 bg-border" />
    </div>
  );
}

function quotaStatusLabel(t: (key: string) => string, status: string): string {
  const keys: Record<string, string> = {
    ok: "settings.resourcesEngineQuotaOk",
    warning: "settings.resourcesEngineQuotaWarning",
    exceeded: "settings.resourcesEngineQuotaExceeded",
    structured_exceeded: "settings.resourcesEngineQuotaExceeded",
  };
  const key = keys[status];
  return key ? t(key) : status;
}

/** Durable record of the server-owned initial engine route decision. This is
 * intentionally compact: users see the selected engine and one stable reason,
 * while the decision remains auditable after the session is reopened. */
function EngineRouteMarker({ content }: { content: Record<string, unknown> }) {
  const { t } = useTranslation();
  const tool = typeof content.tool === "string" ? content.tool : "";
  const source = typeof content.source === "string" ? content.source : "automatic";
  const reason = typeof content.reason === "string" ? content.reason : "automatic_candidate_unavailable";
  const quotaStatus = typeof content.quota_status === "string" ? content.quota_status : "";
  const labelKeys: Record<string, string> = {
    automatic: "session.engineRouteMarker",
    manual: "session.engineRouteManualMarker",
    legacy: "session.engineRouteMarker",
  };
  const labelKey = labelKeys[source] ?? "session.engineRouteMarker";
  const label = source === "legacy" ? t("scope.engineLegacy", { tool: toolFullName(tool) }) : t(labelKey, { tool: toolFullName(tool) });
  return (
    <div className="flex items-center gap-2 px-2 py-1.5">
      <span className="h-px min-w-4 flex-1 bg-border" />
      <span className="flex min-w-0 flex-1 flex-wrap items-center justify-center gap-1.5 text-center text-[11px] text-ink-faint">
        <ToolIcon tool={tool} size={12} />
        <span>{label}</span>
        <span className="truncate" title={t(routeReasonKey(reason))}>
          · {t(routeReasonKey(reason))}
        </span>
        {quotaStatus && (
          <span className="rounded-full border border-border px-1.5 py-0.5 text-[10px]">
            {t("session.engineRouteQuotaBasis", { status: quotaStatusLabel(t, quotaStatus) })}
          </span>
        )}
      </span>
      <span className="h-px min-w-4 flex-1 bg-border" />
    </div>
  );
}

/** Durable, actionable record for a blocked route. It is separate from the
 * selected-route marker so an exhausted pool cannot look like a successful
 * launch after a reload. */
function EngineRouteBlockedMarker({ content }: { content: Record<string, unknown> }) {
  const { t } = useTranslation();
  const tool = typeof content.tool === "string" ? content.tool : "";
  const fallback = typeof content.fallback === "string" ? content.fallback : "";
  const reason = typeof content.reason === "string" ? content.reason : "automatic_candidate_unavailable";
  const quotaStatus = typeof content.quota_status === "string" ? content.quota_status : "";
  return (
    <div className="flex items-center gap-2 px-2 py-1.5">
      <span className="h-px min-w-4 flex-1 bg-border" />
      <span className="flex min-w-0 flex-1 flex-wrap items-center justify-center gap-1.5 text-center text-[11px] text-danger">
        {tool && <ToolIcon tool={tool} size={12} />}
        {tool && fallback && <ArrowRight size={10} />}
        {fallback && <ToolIcon tool={fallback} size={12} />}
        {tool && fallback && (
          <span>
            {t("session.engineRouteBlockedTransition", {
              from: toolFullName(tool),
              to: toolFullName(fallback),
            })}
          </span>
        )}
        <span>{t("session.engineRouteBlockedMarker")}</span>
        <span className="truncate" title={t(routeReasonKey(reason))}>
          · {t(routeReasonKey(reason))}
        </span>
        {quotaStatus && (
          <span className="rounded-full border border-danger/30 px-1.5 py-0.5 text-[10px]">
            {t("session.engineRouteQuotaBasis", { status: quotaStatusLabel(t, quotaStatus) })}
          </span>
        )}
        <span className="truncate">· {t("session.engineRouteBlockedHint")}</span>
      </span>
      <span className="h-px min-w-4 flex-1 bg-border" />
    </div>
  );
}

/** Native `title` tooltips render verbatim with no CSS truncation available —
 *  cap it so a multi-KB CLI stack trace doesn't produce an unusable OS
 *  tooltip (issue #97 review P2 follow-up). */
function truncateForTooltip(text: string, max: number): string {
  const trimmed = text.trim();
  return trimmed.length > max ? `${trimmed.slice(0, max)}…` : trimmed;
}

/** issue #97 review P2: a failed auto fail-over attempt — same quiet centered
 *  divider as `EngineSwitchMarker`, but a danger tone since nothing actually
 *  changed.
 *
 *  Independent re-review follow-up: the old copy ("Auto fail-over X → Y
 *  failed") left the user to infer two of the three things they actually
 *  need — this now spells out "still on {{from}}" outright, reuses
 *  `EngineSwitchMarker`'s own `engineSwitchedQuotaReason` badge (unconditional
 *  here, unlike the success marker's `reason === "quota_exceeded"` check —
 *  every marker of this kind exists BECAUSE quota was exceeded), and puts the
 *  backend's real `content.error` — previously shown nowhere, not even a
 *  tooltip — one hover away via a native `title`. */
function QuotaFailoverFailedMarker({ content }: { content: Record<string, unknown> }) {
  const { t } = useTranslation();
  const tool = typeof content.tool === "string" ? content.tool : "";
  const fallback = typeof content.fallback === "string" ? content.fallback : "";
  const error = typeof content.error === "string" ? content.error : "";
  return (
    <div className="flex items-center gap-2 px-2 py-1.5">
      <span className="h-px min-w-4 flex-1 bg-border" />
      <span className="flex min-w-0 flex-1 flex-wrap items-center justify-center gap-1.5 text-center text-[11px] text-danger">
        <ToolIcon tool={tool} size={12} />
        <ArrowRight size={10} />
        <ToolIcon tool={fallback} size={12} />
        <span>
          {t("session.quotaFailoverFailedMarker", {
            from: toolFullName(tool),
            to: toolFullName(fallback),
          })}
        </span>
        <span className="rounded-full border border-danger/30 bg-danger/10 px-1.5 py-0.5 text-[10px] font-medium text-danger">
          {t("session.engineSwitchedQuotaReason")}
        </span>
        {error && (
          <span
            title={truncateForTooltip(error, 240)}
            className="inline-flex shrink-0 cursor-help text-danger/80"
          >
            <CircleAlert size={12} />
          </span>
        )}
      </span>
      <span className="h-px min-w-4 flex-1 bg-border" />
    </div>
  );
}

const AUTO_MERGE_STATE_KEYS: Record<string, string> = {
  open: "session.autoMergeStateOpen",
  merged: "session.autoMergeStateMerged",
  closed: "session.autoMergeStateClosed",
};

/** issue #110 T3: durable record of one auto-merge attempt, success or
 *  failure — same quiet centered-divider treatment as the markers above,
 *  danger tone only when the attempt actually failed (a successful merge
 *  reads neutral, matching `EngineSwitchMarker`'s own tone for a completed,
 *  non-alarming automatic action). Raw, non-localizable diagnostics
 *  (`reason` / `state_error` — the host's/OS's own passthrough text) render
 *  as a hover tooltip, the same pattern `QuotaFailoverFailedMarker` already
 *  established for its own `error` field, rather than inline untranslated
 *  prose. */
/** The raw diagnostic to hover-reveal, if any. A merged marker never carries
 *  one; otherwise the host's own `reason` wins, and `state_error` only stands
 *  in when the lifecycle itself came back unrecognized. Kept as a function
 *  rather than a chained `?:` so each case reads on its own line. */
function autoMergeDiagnostic(m: {
  merged: boolean;
  reason: string;
  state: string;
  stateError: string;
}): string {
  if (m.merged) return "";
  if (m.reason) return m.reason;
  if (m.state === "unknown") return m.stateError;
  return "";
}

function AutoMergeMarker({ content }: { content: Record<string, unknown> }) {
  const { t } = useTranslation();
  const merged = content.merged === true;
  const abbrev = typeof content.abbrev === "string" ? content.abbrev : "PR";
  const number = typeof content.number === "number" ? content.number : 0;
  const baseRef = typeof content.base_ref === "string" ? content.base_ref : "";
  const reason = typeof content.reason === "string" ? content.reason : "";
  const state = typeof content.state === "string" ? content.state : "unknown";
  const stateError = typeof content.state_error === "string" ? content.state_error : "";
  const attemptsExhausted = content.attempts_exhausted === true;
  const attemptsMax = typeof content.attempts_max === "number" ? content.attempts_max : 0;
  const stateKey = AUTO_MERGE_STATE_KEYS[state] ?? "session.autoMergeStateUnknown";
  const tone = merged ? "text-ink-faint" : "text-danger";
  const diagnostic = autoMergeDiagnostic({ merged, reason, state, stateError });
  return (
    <div className="flex items-center gap-2 px-2 py-1.5">
      <span className="h-px min-w-4 flex-1 bg-border" />
      <span className={cn("flex min-w-0 flex-1 flex-wrap items-center justify-center gap-1.5 text-center text-[11px]", tone)}>
        <span>
          {merged
            ? t("session.autoMergeSucceeded", { abbrev, number, base: baseRef })
            : t("session.autoMergeFailed", { abbrev, number })}
        </span>
        <span className="truncate">· {t(stateKey)}</span>
        {diagnostic && (
          <span title={truncateForTooltip(diagnostic, 240)} className="inline-flex shrink-0 cursor-help text-danger/80">
            <CircleAlert size={12} />
          </span>
        )}
        {attemptsExhausted && (
          <span className="rounded-full border border-danger/30 bg-danger/10 px-1.5 py-0.5 text-[10px] font-medium text-danger">
            {t("session.autoMergeAttemptsExhausted", { count: attemptsMax })}
          </span>
        )}
      </span>
      <span className="h-px min-w-4 flex-1 bg-border" />
    </div>
  );
}

/**
 * Collapsed by default (issue #99): a sub-agent's branch keeps the main
 * timeline to "delegated → conclusion" while its own transcript — everything
 * `groupTimeline` placed under this anchor row — sits one click away, indented
 * like `FileTree`'s nested rows. Built on the SAME `Tool` expand/collapse
 * toggle PR #19 gave ordinary tool rows (no second collapse semantic): the
 * nested list gets `max-h-80 overflow-auto`, IDENTICAL to how `ToolBlock`
 * already bounds a tool row's own input/output (`Tool.tsx`) — a sub-agent that
 * ran dozens of tool calls scrolls inside its own box instead of inserting
 * every child synchronously into the page (review [P2]).
 */
function CollabBranchRow({
  anchor,
  branchChildren,
  ...rest
}: { anchor: LeadMessage; branchChildren: TimelineNode[] } & TimelineRowProps) {
  const { t } = useTranslation();
  const content = parse(anchor.content);
  const name = typeof content.name === "string" ? content.name : "tool";
  const rawSummary = typeof content.summary === "string" ? content.summary : "";
  const status = deriveToolStatus(anchor, content);
  const Icon = toolIcon(name);
  const labelKey = status === "streaming" ? toolLabelKey(name) : toolDoneLabelKey(name);
  const generic = labelKey === "session.toolCalling" || labelKey === "session.toolCalled";
  const { target } = compactToolTarget(name, rawSummary);
  // "结论摘要": prefer the branch's own latest words over the generic backend
  // summary — much more informative once the sub-agent has said anything at
  // all, and freshens live as it keeps streaming.
  const preview = latestTextPreview(branchChildren) ?? target;

  return (
    <Tool
      icon={Icon}
      label={generic ? cleanToolName(name) : t(labelKey)}
      summary={preview}
      status={status}
      cwd={rest.cwd}
      input={formatToolValue(content.input)}
      output={typeof content.output === "string" ? content.output : ""}
      inputLabel={t("tool.input")}
      outputLabel={t("tool.output")}
      showMoreLabel={(hiddenLineCount) => t("tool.showMore", { n: hiddenLineCount })}
      showLessLabel={t("tool.showLess")}
    >
      <div className="max-h-80 space-y-1.5 overflow-auto border-l border-border py-0.5 pl-2.5">
        {branchChildren.map((child) => (
          <TimelineNodeRow key={nodeKey(child)} node={child} {...rest} />
        ))}
      </div>
    </Tool>
  );
}

// The collapsed branch header's "结论摘要": the most recent non-empty text
// anywhere in the branch's subtree, searched depth-first from the end so a
// still-running delegation shows its latest line and a finished one shows its
// conclusion — exactly "起止 + 结论摘要" while collapsed (issue #99).
function latestTextPreview(nodes: TimelineNode[]): string | undefined {
  for (let i = nodes.length - 1; i >= 0; i--) {
    const node = nodes[i];
    if (node.kind === "branch") {
      const nested = latestTextPreview(node.children);
      if (nested) return nested;
      continue;
    }
    if (node.row.kind !== "text") continue;
    const text = String(parse(node.row.content).text ?? "")
      .replace(/\s+/g, " ")
      .trim();
    if (!text) continue;
    return text.length > 100 ? `${text.slice(0, 100)}…` : text;
  }
  return undefined;
}

function isActionCardAction(value: unknown): value is ActionCardAction {
  if (!value || typeof value !== "object") return false;
  const action = value as Record<string, unknown>;
  return (
    typeof action.id === "string" &&
    typeof action.label === "string" &&
    (action.kind === "add" || action.kind === "new" || action.kind === "clone")
  );
}

// True when a human reply is pending against `m` (a plan card): either a user
// row AFTER the card, or a queued user row ANYWHERE — a message sent mid-turn
// is inserted before the card row but delivered after the turn, so approving
// would queue a plan_decision behind that revision and the lead could read the
// stale approval against the revised plan.
function hasPendingUserReply(m: LeadMessage, all: LeadMessage[]): boolean {
  if (all.some((row) => row.role === "user" && row.status === "queued")) return true;
  for (let i = all.length - 1; i >= 0; i--) {
    const row = all[i];
    if (row.id === m.id) return false;
    if (row.role === "user") return true;
  }
  return false;
}

function isPlanSplitItem(value: unknown): value is PlanCardSplitItem {
  if (!value || typeof value !== "object") return false;
  const item = value as Record<string, unknown>;
  return (
    typeof item.name === "string" &&
    typeof item.repo === "string" &&
    (item.reason === undefined || typeof item.reason === "string")
  );
}

/**
 * Per-message action row (copy, rewind, …): small icon buttons under a chat
 * bubble, revealed on hover of the row (the parent carries `group`) or on
 * keyboard focus. The action row reserves a fixed height even while hidden so
 * hovering never changes row geometry and a hover-driven reflow can't jump the
 * scroll position.
 */
function MessageActionsRow({
  align,
  children,
}: {
  align: "start" | "end";
  children: ReactNode;
}) {
  return (
    <div
      className={cn(
        "mt-0.5 flex h-5 w-full items-center gap-0.5 opacity-0 transition-opacity group-hover:opacity-100 focus-within:opacity-100",
        align === "end" ? "justify-end" : "justify-start",
      )}
    >
      {children}
    </div>
  );
}

/** Copies the raw message text (markdown source for assistant turns), matching
 *  the rest of the app's clipboard affordances. */
function CopyMessageButton({ text }: { text: string }) {
  const { t } = useTranslation();
  const [copied, setCopied] = useState(false);
  const onCopy = () => {
    void navigator.clipboard?.writeText(text);
    setCopied(true);
    window.setTimeout(() => setCopied(false), 1600);
  };
  const label = copied ? t("lead.copied") : t("lead.copyMessage");
  return (
    <button
      type="button"
      onClick={onCopy}
      title={label}
      aria-label={label}
      className="inline-flex items-center gap-1 rounded-[var(--radius-sm)] px-1.5 py-0.5 text-[11px] text-ink-faint outline-none transition-colors hover:bg-surface hover:text-ink focus-visible:bg-surface focus-visible:text-ink"
    >
      {copied ? <Check size={12} className="text-running" /> : <Copy size={12} />}
    </button>
  );
}

/** Rewind the conversation to just before this user message (the host confirms
 *  in a dialog, calls chat_rewind/lead_rewind, then prefills the composer). */
function RewindMessageButton({ onClick }: { onClick: () => void }) {
  const { t } = useTranslation();
  const label = t("session.rewindTip");
  return (
    <button
      type="button"
      onClick={onClick}
      title={label}
      aria-label={label}
      className="inline-flex items-center gap-1 rounded-[var(--radius-sm)] px-1.5 py-0.5 text-[11px] text-ink-faint outline-none transition-colors hover:bg-surface hover:text-ink focus-visible:bg-surface focus-visible:text-ink"
    >
      <Undo2 size={12} />
    </button>
  );
}
