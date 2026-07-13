import { useEffect, useRef, useState, type ReactNode } from "react";
import { Virtuoso, type VirtuosoHandle } from "react-virtuoso";
import { useTranslation } from "react-i18next";
import { ArrowRight, Check, Copy, Sparkles } from "lucide-react";
import type { LeadMessage, PermissionAsk, QueuedItem, ResolvedProposal } from "../lib/types";
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
import { ActionCardBlock, type ActionCardAction } from "./blocks/ActionCardBlock";
import { PlanCardBlock, type PlanCardSplitItem } from "./blocks/PlanCardBlock";
import { api } from "../lib/api";
import { currentLang } from "../i18n";
import { toast } from "../components/Toast";
import { PermissionBar } from "./PermissionBar";
import type { useRepoActions } from "./useRepoActions";

type RunAction = ReturnType<typeof useRepoActions>["run"];

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
}: {
  messages: LeadMessage[];
  busy: boolean;
  /** Pending (not-yet-sent) queued messages, shown in the bottom stack. */
  queue?: QueuedItem[];
  onRemove?: (id: number) => void;
  onEdit?: (id: number, text: string) => void;
  onReorder?: (order: number[]) => void;
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
  const atBottomRef = useRef(true);
  const rootRef = useRef<HTMLDivElement>(null);

  // Tool calls render inline as expandable `kind:"tool"` rows for every dialect
  // (claude/opencode/codex alike); only `meta` bookkeeping rows are hidden.
  // Pending queued messages live in the bottom QueueStack, not the timeline.
  const visible = messages.filter((m) => m.kind !== "meta" && m.status !== "queued");

  const growthLen = visible
    .filter((m) => m.kind === "text" || m.kind === "tool")
    .reduce((n, m) => n + m.content.length, 0);
  useEffect(() => {
    if (!atBottomRef.current || visible.length === 0) return;
    virtuosoRef.current?.scrollToIndex({ index: "LAST", align: "end", behavior: "auto" });
  }, [visible.length, growthLen, busy, activity]);

  // Switching THREADS swaps this timeline's data without remounting it (e.g. the lead
  // chat stays mounted as the active thread changes), so Virtuoso keeps the previous
  // thread's scroll position. Reset the at-bottom intent and re-pin to the latest, so
  // switching into a chat always lands at the bottom. (rAF lets the new data lay out;
  // if it loads async the at-bottom reset makes the message-growth effect above scroll
  // once it arrives.)
  useEffect(() => {
    atBottomRef.current = true;
    requestAnimationFrame(() =>
      virtuosoRef.current?.scrollToIndex({ index: "LAST", align: "end", behavior: "auto" }),
    );
  }, [threadId]);

  const showList = visible.length > 0 || busy || asks.length > 0;

  // Re-pin to the latest message when this timeline is REVEALED (its height goes
  // 0 → >0). A chat kept mounted-but-hidden (the curator behind the detail surface,
  // an inactive tab) can't position its virtualized list while `display:none`, so the
  // initial bottom-scroll is lost and switching in would land mid-history. rAF lets
  // Virtuoso lay out at the new size before we scroll.
  useEffect(() => {
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
  }, [showList]);

  if (!showList) {
    return <>{emptyState}</>;
  }

  return (
    <div ref={rootRef} className="flex min-h-0 flex-1 flex-col">
      <Virtuoso<LeadMessage>
        ref={virtuosoRef}
        className="min-h-0 flex-1"
        data={visible}
        computeItemKey={(_index, m) => m.id}
        initialTopMostItemIndex={
          visible.length > 0 ? { index: visible.length - 1, align: "end" } : undefined
        }
        atBottomThreshold={80}
        atBottomStateChange={(atBottom) => {
          atBottomRef.current = atBottom;
        }}
        increaseViewportBy={{ top: 600, bottom: 600 }}
        components={{ Header, Footer }}
        itemContent={(_index, m) => (
          <div className="mx-auto w-full min-w-0 max-w-[820px] px-4 pb-2.5">
            <TimelineRow
              m={m}
              all={visible}
              onReviewProposal={onReviewProposal}
              proposal={proposal ?? null}
              runAction={runAction}
              actionsBusy={actionsBusy}
              threadId={threadId ?? null}
              workspaceId={workspaceId ?? null}
              promptText={promptText}
              cwd={cwd}
            />
          </div>
        )}
      />
      {/* The in-flight tool / working indicator sits OUTSIDE the virtualized
          scroller as a fixed bottom bar. Keeping it out of the list makes the
          last message the unambiguous list bottom and keeps the indicator
          visible even while the user scrolls back through history. */}
      {(busy || queue.length > 0 || asks.length > 0) && (
        <div className="mx-auto w-full max-w-[820px] shrink-0 px-4 pb-3">
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

function BusyIndicator({
  activity,
  cwd,
}: {
  activity?: { name: string; summary: string } | null;
  cwd?: string;
}) {
  const { t } = useTranslation();
  if (activity) return <ToolStatus name={activity.name} summary={activity.summary} cwd={cwd} />;
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

function Footer() {
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
  // show the cleaned tool identity instead.
  const generic = labelKey === "session.toolCalling";
  return (
    <ToolActivity
      icon={Icon}
      label={generic ? cleanToolName(name) : t(labelKey)}
      target={generic ? undefined : target}
      targetToken={generic ? undefined : targetToken}
      cwd={cwd}
      summary={generic ? summary : undefined}
      added={added}
      removed={removed}
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
function isLastAssistant(m: LeadMessage, all: LeadMessage[]): boolean {
  for (let i = all.length - 1; i >= 0; i--) {
    const row = all[i];
    if (row.kind === "tool" && row.turn_id === m.turn_id) continue;
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
}: {
  m: LeadMessage;
  all: LeadMessage[];
  onReviewProposal: () => void;
  proposal: ResolvedProposal | null;
  runAction?: RunAction;
  actionsBusy?: Record<string, boolean>;
  threadId: number | null;
  workspaceId: number | null;
  promptText?: (title: string, placeholder?: string) => Promise<string | null>;
  cwd?: string;
}) {
  const { t } = useTranslation();
  const c = parse(m.content);

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
    // A newer USER turn also stales the card: the reply is a revision request,
    // and a late approval could be read against the revised plan.
    const readOnly =
      !runAction ||
      threadId == null ||
      !isLastAssistant(m, all) ||
      hasNewerUserTurn(m, all);
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
        className="group flex items-center gap-2.5 rounded-[var(--radius-md)] border border-accent/40 bg-accent-ghost px-3 py-2.5 text-left transition-colors hover:border-accent/70"
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
          {m.status === "error" && (
            <p className="self-end text-[11px] text-danger">{t("lead.errored")}</p>
          )}
        </div>
        {text && <CopyMessageButton text={text} align="end" />}
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
        <CopyMessageButton text={assistantText} align="start" />
      )}
    </Message>
  );
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

// True when any user message landed after `m` — for a plan card that means the
// human replied (a revision request), so approving the old card must be blocked:
// the queued plan_decision could otherwise be read against the revised plan.
function hasNewerUserTurn(m: LeadMessage, all: LeadMessage[]): boolean {
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
 * Per-message copy affordance: a small icon button under a chat bubble, revealed
 * on hover of the row (the parent carries `group`) or on keyboard focus. The
 * action row reserves a fixed height even while hidden so hovering never changes
 * row geometry and a hover-driven reflow can't jump the scroll position. Copies
 * the raw message text (markdown source for assistant turns), matching the rest
 * of the app's clipboard affordances.
 */
function CopyMessageButton({ text, align }: { text: string; align: "start" | "end" }) {
  const { t } = useTranslation();
  const [copied, setCopied] = useState(false);
  const onCopy = () => {
    void navigator.clipboard?.writeText(text);
    setCopied(true);
    window.setTimeout(() => setCopied(false), 1600);
  };
  const label = copied ? t("lead.copied") : t("lead.copyMessage");
  return (
    <div
      className={cn(
        "mt-0.5 flex h-5 w-full items-center opacity-0 transition-opacity group-hover:opacity-100 focus-within:opacity-100",
        align === "end" ? "justify-end" : "justify-start",
      )}
    >
      <button
        type="button"
        onClick={onCopy}
        title={label}
        aria-label={label}
        className="inline-flex items-center gap-1 rounded-[var(--radius-sm)] px-1.5 py-0.5 text-[11px] text-ink-faint outline-none transition-colors hover:bg-surface hover:text-ink focus-visible:bg-surface focus-visible:text-ink"
      >
        {copied ? <Check size={12} className="text-running" /> : <Copy size={12} />}
      </button>
    </div>
  );
}
