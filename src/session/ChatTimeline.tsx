import { useEffect, useRef, useState } from "react";
import { Virtuoso, type VirtuosoHandle } from "react-virtuoso";
import { useTranslation } from "react-i18next";
import { ArrowRight, ChevronRight, FileText, Sparkles } from "lucide-react";
import type { LeadMessage } from "../lib/types";
import { Markdown } from "../components/Markdown";
import { cn } from "../lib/cn";
import { cleanToolName, compactToolTarget, toolIcon, toolLabelKey } from "./transcriptBits";
import { ActionCardBlock, type ActionCardAction } from "./blocks/ActionCardBlock";
import type { useRepoActions } from "./useRepoActions";

type RunAction = ReturnType<typeof useRepoActions>["run"];
type EmptyStateMode = "default" | "lead-task" | "lead-repo-guide";

/**
 * The chat-engine timeline: renders weft-owned LeadMessage rows (no polling,
 * no jsonl). Structured cards (proposal/approval/worker events) live inline in
 * the flow, where they happened — the conversation IS the console. Tool calls
 * are NOT rows: the one currently running shows as a transient activity line
 * under the stream and disappears when the turn moves on.
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
  runAction,
  actionsBusy,
  threadId,
  workspaceId,
  promptText,
  emptyState = "default",
}: {
  messages: LeadMessage[];
  busy: boolean;
  /** The tool call executing right now (transient), if any. */
  activity?: { name: string; summary: string } | null;
  onReviewProposal: () => void;
  /** Lead-only: dispatch a repo action card. Omit → cards render read-only. */
  runAction?: RunAction;
  actionsBusy?: Record<string, boolean>;
  threadId?: number | null;
  workspaceId?: number | null;
  promptText?: (title: string, placeholder?: string) => Promise<string | null>;
  /** Lead hosts opt into task/repo guidance; workers keep the default empty state. */
  emptyState?: EmptyStateMode;
}) {
  const { t } = useTranslation();
  const virtuosoRef = useRef<VirtuosoHandle>(null);
  const atBottomRef = useRef(true);

  // Tool calls (claude/opencode) render inline as expandable `kind:"tool"` rows;
  // only `meta` bookkeeping rows are hidden. (Codex tools still arrive as the
  // transient activity bar below the list.)
  const visible = messages.filter((m) => m.kind !== "meta");

  // Virtuoso's followOutput only fires on item-COUNT changes, so it misses
  // intra-message streaming growth (text appended to the existing last row).
  // Follow the bottom ourselves on every growth signal, but only while the user
  // is parked at the bottom. atBottomThreshold (set on Virtuoso below) restores
  // the old ~80px tolerance, so a reader a few px short of the exact bottom still
  // auto-follows while one who scrolled up to read history is left alone.
  // scrollToIndex is measurement-aware — it renders and measures the target row
  // before scrolling — so there is no scrollTo(MAX)/rAF drift to correct, and
  // because the activity bar lives OUTSIDE the scroller the last row is the
  // unambiguous bottom.
  const growthLen = visible
    .filter((m) => m.kind === "text" || m.kind === "tool")
    .reduce((n, m) => n + m.content.length, 0);
  useEffect(() => {
    if (!atBottomRef.current || visible.length === 0) return;
    virtuosoRef.current?.scrollToIndex({ index: "LAST", align: "end", behavior: "auto" });
  }, [visible.length, growthLen, busy, activity]);

  if (visible.length === 0 && !busy) {
    return (
      <EmptyLeadState
        mode={emptyState}
        runAction={runAction}
        actionsBusy={actionsBusy}
        threadId={threadId ?? null}
        workspaceId={workspaceId ?? null}
        promptText={promptText}
      />
    );
  }

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <Virtuoso<LeadMessage>
        ref={virtuosoRef}
        className="min-h-0 flex-1"
        data={visible}
        computeItemKey={(_index, m) => m.id}
        // Open at the BOTTOM of the last message (align "end"), so a final
        // message taller than the viewport opens at its latest line, not its top.
        // Omitted while empty (busy-only turn): index 0 is out of range for
        // data=[] and would misinitialize Virtuoso.
        initialTopMostItemIndex={
          visible.length > 0 ? { index: visible.length - 1, align: "end" } : undefined
        }
        // Restore the old 80px "close enough to the bottom" tolerance: a reader a
        // few px up (e.g. a trackpad nudge) still counts as at-bottom and keeps
        // auto-following. Virtuoso's default is only a few px.
        atBottomThreshold={80}
        atBottomStateChange={(atBottom) => {
          atBottomRef.current = atBottom;
        }}
        increaseViewportBy={{ top: 600, bottom: 600 }}
        components={{ Header, Footer }}
        itemContent={(_index, m) => (
          <div className="mx-auto w-full max-w-[820px] px-4 pb-2.5">
            <TimelineRow
              m={m}
              all={visible}
              onReviewProposal={onReviewProposal}
              runAction={runAction}
              actionsBusy={actionsBusy}
              threadId={threadId ?? null}
              workspaceId={workspaceId ?? null}
              promptText={promptText}
            />
          </div>
        )}
      />
      {/* The in-flight tool / working indicator sits OUTSIDE the virtualized
          scroller as a fixed bottom bar. Keeping it out of the list makes the
          last message the unambiguous list bottom (so the follow-scroll target
          is exact) and keeps the indicator visible even while the user scrolls
          back through history. */}
      {busy && (
        <div className="mx-auto w-full max-w-[820px] shrink-0 px-4 pb-3">
          {activity ? (
            <ActivityLine name={activity.name} summary={activity.summary} />
          ) : (
            <div className="flex items-center gap-1.5 px-1 text-[11px] text-ink-faint">
              <span className="h-1.5 w-1.5 animate-pulse rounded-full bg-running" />
              {t("lead.working")}
            </div>
          )}
        </div>
      )}
    </div>
  );
}

/** Top breathing room — mirrors the old scroll container's py-4 top padding. */
function Header() {
  return <div className="h-4" />;
}

/** Bottom breathing room inside the scroller; the activity bar now renders
 *  outside the virtualized list (see ChatTimeline). */
function Footer() {
  return <div className="h-4" />;
}

function EmptyLeadState({
  mode,
  runAction,
  actionsBusy,
  threadId,
  workspaceId,
  promptText,
}: {
  mode: EmptyStateMode;
  runAction?: RunAction;
  actionsBusy?: Record<string, boolean>;
  threadId: number | null;
  workspaceId: number | null;
  promptText?: (title: string, placeholder?: string) => Promise<string | null>;
}) {
  const { t } = useTranslation();

  if (mode === "lead-repo-guide" && runAction && promptText) {
    const actions: ActionCardAction[] = [
      { id: "empty-add-repo", kind: "add", label: t("actionCard.addRepoLabel") },
      { id: "empty-new-repo", kind: "new", label: t("actionCard.newRepoLabel") },
      { id: "empty-clone-repo", kind: "clone", label: t("actionCard.cloneRepoLabel") },
    ];
    const steps = [
      t("lead.repoGuideStepChoose"),
      t("lead.repoGuideStepMap"),
      t("lead.repoGuideStepReturn"),
    ];

    return (
      <div className="flex flex-1 items-center justify-center px-4 py-6">
        <div className="w-full max-w-[620px]">
          <ActionCardBlock
            title={t("lead.repoGuideTitle")}
            body={t("lead.repoGuideBody")}
            steps={steps}
            actions={actions}
            readOnly={false}
            busy={actionsBusy ?? {}}
            onAction={(action) =>
              void runAction({
                actionId: action.id,
                kind: action.kind,
                ctx: {
                  threadId: threadId ?? undefined,
                  preferredWorkspaceId: workspaceId,
                },
                promptText,
              })
            }
          />
        </div>
      </div>
    );
  }

  if (mode === "lead-task") {
    return (
      <div className="flex flex-1 items-center justify-center px-6 text-center">
        <div className="max-w-[420px]">
          <span className="mx-auto grid h-8 w-8 place-items-center rounded-[var(--radius-md)] bg-brand-ghost text-brand">
            <Sparkles size={15} />
          </span>
          <p className="mt-3 text-[13px] font-medium text-ink">{t("lead.taskEmptyTitle")}</p>
          <p className="mt-1.5 text-[12px] leading-relaxed text-ink-faint">
            {t("lead.taskEmptyBody")}
          </p>
        </div>
      </div>
    );
  }

  return (
    <div className="flex flex-1 items-center justify-center px-6 text-center">
      <p className="text-[12px] leading-relaxed text-ink-faint">{t("lead.transcriptEmpty")}</p>
    </div>
  );
}

/** The tool call in flight — pulsing, transient, precise about WHAT it calls. */
function ActivityLine({ name, summary }: { name: string; summary: string }) {
  const { t } = useTranslation();
  const Icon = toolIcon(name);
  const labelKey = toolLabelKey(name);
  const { target, added, removed } = compactToolTarget(name, summary);
  // For unrecognized tools (MCP etc.) the generic "Calling" says nothing —
  // show the cleaned tool identity instead.
  const generic = labelKey === "session.toolCalling";
  return (
    <div className="flex max-w-full items-center gap-2 px-1.5 py-1 text-[13px] text-ink-faint">
      <span className="h-1.5 w-1.5 shrink-0 animate-pulse rounded-full bg-running" />
      <Icon size={15} className="shrink-0 text-ink-faint" />
      <span className="shrink-0 font-medium text-ink-muted">
        {generic ? cleanToolName(name) : t(labelKey)}
      </span>
      {!generic && summary && (
        <span className="min-w-0 truncate font-mono text-brand">{target}</span>
      )}
      {generic && summary && (
        <span className="min-w-0 truncate font-mono text-brand">{summary}</span>
      )}
      {added && <span className="shrink-0 font-mono text-running">+{added}</span>}
      {removed && <span className="shrink-0 font-mono text-danger">-{removed}</span>}
    </div>
  );
}

/**
 * A persisted tool call (claude/opencode): a compact collapsed line — icon +
 * tool label + target + a status dot — that expands to show the full input and
 * the tool's output. `status` mirrors the row: "streaming" = running,
 * "complete"/"error" = finished.
 */
function ToolRow({ m }: { m: LeadMessage }) {
  const { t } = useTranslation();
  const [open, setOpen] = useState(false);
  const c = parse(m.content);
  const name = typeof c.name === "string" ? c.name : "tool";
  const summary = typeof c.summary === "string" ? c.summary : "";
  const output = typeof c.output === "string" ? c.output : "";
  const inputText = formatToolValue(c.input);
  const running = m.status === "streaming";
  const isError = c.is_error === true || m.status === "error";
  const Icon = toolIcon(name);
  const labelKey = toolLabelKey(name);
  const label = labelKey === "session.toolCalling" ? cleanToolName(name) : t(labelKey);
  const { target } = compactToolTarget(name, summary);
  const hasDetail = inputText.length > 0 || output.length > 0;

  return (
    <div className="overflow-hidden rounded-[var(--radius-md)] border border-border bg-surface/60">
      <button
        type="button"
        disabled={!hasDetail}
        onClick={() => setOpen((v) => !v)}
        className={cn(
          "flex w-full items-center gap-2 px-2.5 py-1.5 text-left text-[13px]",
          hasDetail && "hover:bg-surface",
        )}
      >
        <span
          className={cn(
            "h-1.5 w-1.5 shrink-0 rounded-full",
            running ? "animate-pulse bg-running" : isError ? "bg-danger" : "bg-ink-faint",
          )}
        />
        <Icon size={14} className="shrink-0 text-ink-faint" />
        <span className="shrink-0 font-medium text-ink-muted">{label}</span>
        {(target || summary) && (
          <span className="min-w-0 truncate font-mono text-brand">{target || summary}</span>
        )}
        {hasDetail && (
          <ChevronRight
            size={13}
            className={cn(
              "ml-auto shrink-0 text-ink-faint transition-transform",
              open && "rotate-90",
            )}
          />
        )}
      </button>
      {open && hasDetail && (
        <div className="space-y-2 border-t border-border px-2.5 py-2">
          {inputText && <ToolBlock label={t("tool.input")} body={inputText} />}
          {output && (
            <ToolBlock label={t("tool.output")} body={output} tone={isError ? "error" : "default"} />
          )}
        </div>
      )}
    </div>
  );
}

/** A labeled monospace block inside an expanded tool row, with show-more past a
 *  line budget so a huge stdout/diff doesn't blow up the timeline. */
function ToolBlock({
  label,
  body,
  tone = "default",
}: {
  label: string;
  body: string;
  tone?: "default" | "error";
}) {
  const { t } = useTranslation();
  const [expanded, setExpanded] = useState(false);
  const lines = body.split("\n");
  const LIMIT = 20;
  const long = lines.length > LIMIT;
  const shown = expanded || !long ? body : lines.slice(0, LIMIT).join("\n");
  return (
    <div>
      <p className="mb-1 text-[10.5px] font-medium uppercase tracking-wide text-ink-faint">
        {label}
      </p>
      <pre
        className={cn(
          "max-h-80 overflow-auto whitespace-pre-wrap break-words rounded bg-bg px-2 py-1.5 font-mono text-[11.5px] leading-relaxed",
          tone === "error" ? "text-danger" : "text-ink-muted",
        )}
      >
        {shown}
      </pre>
      {long && (
        <button
          type="button"
          onClick={() => setExpanded((v) => !v)}
          className="mt-1 text-[11px] text-brand hover:underline"
        >
          {expanded ? t("tool.showLess") : t("tool.showMore", { n: lines.length - LIMIT })}
        </button>
      )}
    </div>
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

// Read-only history replay: only the most recent assistant row is interactive.
// Older action_cards stay rendered for context but their buttons are disabled.
function isLastAssistant(m: LeadMessage, all: LeadMessage[]): boolean {
  for (let i = all.length - 1; i >= 0; i--) {
    if (all[i].role === "assistant") return all[i].id === m.id;
  }
  return false;
}

function TimelineRow({
  m,
  all,
  onReviewProposal,
  runAction,
  actionsBusy,
  threadId,
  workspaceId,
  promptText,
}: {
  m: LeadMessage;
  all: LeadMessage[];
  onReviewProposal: () => void;
  runAction?: RunAction;
  actionsBusy?: Record<string, boolean>;
  threadId: number | null;
  workspaceId: number | null;
  promptText?: (title: string, placeholder?: string) => Promise<string | null>;
}) {
  const { t } = useTranslation();
  const c = parse(m.content);

  if (m.kind === "tool") {
    return <ToolRow m={m} />;
  }

  if (m.kind === "action_card") {
    const parsed = safeParseObj(m.content);
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

  if (m.kind === "command") {
    const command = typeof c.command === "string" ? c.command : "";
    const args = typeof c.args === "string" ? c.args.trim() : "";
    const label = [command, args].filter(Boolean).join(" ");
    return (
      <div className="flex justify-end">
        <span
          className={cn(
            "inline-flex max-w-[72%] items-center gap-1.5 rounded-[var(--radius-md)] border border-brand/25 bg-brand-ghost px-3 py-2 font-mono text-[12.5px] text-ink",
            m.status === "queued" && "opacity-60",
          )}
        >
          <span className="truncate">{label}</span>
          {m.status === "queued" && <QueuedChip />}
        </span>
      </div>
    );
  }

  if (m.kind === "proposal") {
    const count = Number(c.count ?? 0);
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
    const images = Array.isArray(c.images) ? (c.images as string[]) : [];
    const files = Array.isArray(c.files) ? (c.files as string[]) : [];
    return (
      <div className="flex justify-end">
        <div
          className={cn(
            "flex max-w-[72%] flex-col gap-2 rounded-[var(--radius-lg)] border border-brand/25 bg-brand-ghost px-3.5 py-2.5",
            m.status === "queued" && "opacity-60",
          )}
        >
          {images.length > 0 && (
            <div className="flex flex-wrap gap-1.5">
              {images.map((src, i) => (
                <img
                  key={i}
                  src={src}
                  alt=""
                  className="max-h-32 rounded-[var(--radius-md)] border border-border object-cover"
                />
              ))}
            </div>
          )}
          {files.length > 0 && (
            <div className="flex flex-wrap gap-1.5">
              {files.map((f) => (
                <span
                  key={f}
                  className="inline-flex items-center gap-1 rounded-full bg-bg px-2 py-0.5 font-mono text-[10.5px] text-ink-muted"
                >
                  <FileText size={10} className="shrink-0" />
                  {f.split("/").pop()}
                </span>
              ))}
            </div>
          )}
          {String(c.text ?? "") && (
            <p className="whitespace-pre-wrap break-words text-[13px] leading-relaxed text-ink">
              {String(c.text ?? "")}
            </p>
          )}
          {m.status === "queued" && (
            <span className="self-end">
              <QueuedChip />
            </span>
          )}
          {m.status === "error" && (
            <p className="self-end text-[11px] text-danger">{t("lead.errored")}</p>
          )}
        </div>
      </div>
    );
  }

  // assistant / system text
  const terminal = typeof c.terminal === "string" ? c.terminal : "";
  const assistantText =
    terminal === "error_before_output"
      ? t("lead.terminalErrorBeforeOutput")
      : terminal === "interrupted_before_output"
        ? t("lead.terminalInterruptedBeforeOutput")
        : String(c.text ?? "");
  return (
    <div className="flex items-start gap-2.5">
      <span className="mt-0.5 grid h-7 w-7 shrink-0 place-items-center rounded-[var(--radius-md)] bg-brand-ghost text-brand">
        <Sparkles size={14} />
      </span>
      <div className="min-w-0 flex-1 rounded-[var(--radius-lg)] border border-border bg-surface px-3.5 py-3 shadow-[0_12px_34px_-28px_rgba(0,0,0,0.65)]">
        {assistantText && <Markdown text={assistantText} />}
        {m.status === "streaming" && (
          <span className="ml-0.5 inline-block h-3.5 w-[2px] animate-pulse rounded bg-brand align-text-bottom" />
        )}
        {m.status === "interrupted" && (
          <p className="mt-1.5 text-[11px] text-waiting">{t("lead.interrupted")}</p>
        )}
        {m.status === "error" && (
          <p className="mt-1.5 text-[11px] text-danger">{t("lead.errored")}</p>
        )}
      </div>
    </div>
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

function QueuedChip() {
  const { t } = useTranslation();
  return (
    <span className="rounded-full bg-bg px-1.5 py-px text-[10px] text-ink-faint">
      {t("lead.queuedChip")}
    </span>
  );
}
