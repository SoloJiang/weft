import { useEffect, useRef } from "react";
import { useTranslation } from "react-i18next";
import { ArrowRight, FileText, Slash, Sparkles } from "lucide-react";
import type { LeadMessage } from "../lib/types";
import { Markdown } from "../components/Markdown";
import { cn } from "../lib/cn";
import { cleanToolName, compactToolTarget, toolIcon, toolLabelKey } from "./transcriptBits";

/**
 * The chat-engine timeline: renders weft-owned LeadMessage rows (no polling,
 * no jsonl). Structured cards (proposal/approval/worker events) live inline in
 * the flow, where they happened — the conversation IS the console. Tool calls
 * are NOT rows: the one currently running shows as a transient activity line
 * under the stream and disappears when the turn moves on.
 */
export function ChatTimeline({
  messages,
  busy,
  activity,
  onReviewProposal,
}: {
  messages: LeadMessage[];
  busy: boolean;
  /** The tool call executing right now (transient), if any. */
  activity?: { name: string; summary: string } | null;
  onReviewProposal: () => void;
}) {
  const { t } = useTranslation();
  const endRef = useRef<HTMLDivElement>(null);
  const scrollRef = useRef<HTMLDivElement>(null);
  const atBottomRef = useRef(true);
  const lastTextLen = messages
    .filter((m) => m.kind === "text")
    .reduce((n, m) => n + m.content.length, 0);

  // Stick to the bottom while the user is already there (streaming included);
  // never yank them down when they've scrolled up to read history.
  useEffect(() => {
    if (atBottomRef.current) endRef.current?.scrollIntoView({ block: "end" });
  }, [messages.length, lastTextLen, busy, activity]);

  const onScroll = () => {
    const el = scrollRef.current;
    if (el) atBottomRef.current = el.scrollHeight - el.scrollTop - el.clientHeight < 80;
  };

  // Tool rows (legacy imports / older builds) are hidden: tool calls render
  // only while running, via the activity line below.
  const visible = messages.filter((m) => m.kind !== "meta" && m.kind !== "tool");

  if (visible.length === 0 && !busy) {
    return (
      <div className="flex flex-1 items-center justify-center px-6 text-center">
        <p className="text-[12px] leading-relaxed text-ink-faint">
          {t("lead.transcriptEmpty")}
        </p>
      </div>
    );
  }

  return (
    <div
      ref={scrollRef}
      onScroll={onScroll}
      className="flex min-h-0 flex-1 flex-col gap-2.5 overflow-y-auto px-4 py-4"
    >
      <div className="mx-auto flex w-full max-w-[820px] flex-col gap-2.5">
        {visible.map((m) => (
          <TimelineRow key={m.id} m={m} onReviewProposal={onReviewProposal} />
        ))}
        {busy && activity && <ActivityLine name={activity.name} summary={activity.summary} />}
        {busy && !activity && (
          <div className="flex items-center gap-1.5 px-1 text-[11px] text-ink-faint">
            <span className="h-1.5 w-1.5 animate-pulse rounded-full bg-running" />
            {t("lead.working")}
          </div>
        )}
      </div>
      <div ref={endRef} />
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

function parse(content: string): Record<string, unknown> {
  try {
    return JSON.parse(content) as Record<string, unknown>;
  } catch {
    return {};
  }
}

function TimelineRow({
  m,
  onReviewProposal,
}: {
  m: LeadMessage;
  onReviewProposal: () => void;
}) {
  const { t } = useTranslation();
  const c = parse(m.content);

  if (m.kind === "command") {
    return (
      <div className="flex justify-end">
        <span
          className={cn(
            "inline-flex max-w-[72%] items-center gap-1.5 rounded-[var(--radius-md)] border border-brand/25 bg-brand-ghost px-3 py-2 font-mono text-[12.5px] text-ink",
            m.status === "queued" && "opacity-60",
          )}
        >
          <Slash size={12} className="shrink-0 text-brand" />
          <span className="truncate">
            {String(c.command ?? "")} {String(c.args ?? "")}
          </span>
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
        </div>
      </div>
    );
  }

  // assistant / system text
  return (
    <div className="flex items-start gap-2.5">
      <span className="mt-0.5 grid h-7 w-7 shrink-0 place-items-center rounded-[var(--radius-md)] bg-brand-ghost text-brand">
        <Sparkles size={14} />
      </span>
      <div className="min-w-0 flex-1 rounded-[var(--radius-lg)] border border-border bg-surface px-3.5 py-3 shadow-[0_12px_34px_-28px_rgba(0,0,0,0.65)]">
        <Markdown text={String(c.text ?? "")} />
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

function QueuedChip() {
  const { t } = useTranslation();
  return (
    <span className="rounded-full bg-bg px-1.5 py-px text-[10px] text-ink-faint">
      {t("lead.queuedChip")}
    </span>
  );
}
