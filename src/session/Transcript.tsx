import { useCallback, useEffect, useRef, useState } from "react";
import type { ReactNode } from "react";
import { useTranslation } from "react-i18next";
import { ChevronRight, Sparkles } from "lucide-react";
import { api } from "../lib/api";
import type { NormEvent } from "../lib/types";
import { Markdown } from "../components/Markdown";
import { cn } from "../lib/cn";
import { compactToolTarget, toolIcon, toolLabelKey } from "./transcriptBits";

/**
 * Observe-mode chat for any agent (lead or worker): renders the session's
 * transcript from its sidecar (the tool's own jsonl), normalized to messages +
 * tool calls. App-native React, so it always renders correctly, reflows, and
 * costs nothing close to a live TUI. Polls while mounted; the agent keeps
 * running underneath regardless.
 */
export function Transcript({
  cwd,
  tool,
  running,
  refreshSignal,
  before,
  className,
  contentClassName,
  variant = "default",
  hideTools = false,
}: {
  cwd: string;
  tool: string;
  running?: boolean;
  /** Bump to force an immediate re-read + snap-to-bottom (e.g. after you send). */
  refreshSignal?: number;
  before?: ReactNode;
  className?: string;
  contentClassName?: string;
  variant?: "default" | "console";
  hideTools?: boolean;
}) {
  const { t } = useTranslation();
  const [events, setEvents] = useState<NormEvent[]>([]);
  const [loaded, setLoaded] = useState(false);
  const [activeTool, setActiveTool] = useState<Extract<NormEvent, { kind: "tool" }> | null>(null);
  const endRef = useRef<HTMLDivElement>(null);
  const scrollRef = useRef<HTMLDivElement>(null);
  const atBottomRef = useRef(true);

  const load = useCallback(async () => {
    try {
      const ev = await api.readTranscript(cwd, tool);
      // A transient empty read (file mid-write / rotated) must not blank an
      // already-populated transcript — keep the last good content.
      setEvents((prev) => (ev.length === 0 && prev.length > 0 ? prev : ev));
      setLoaded(true);
    } catch {
      /* not ready */
    }
  }, [cwd, tool]);

  useEffect(() => {
    // New session (cwd/tool changed): clear so the transient-empty guard above
    // doesn't carry the previous session's transcript over.
    setEvents([]);
    setLoaded(false);
    let alive = true;
    const tick = () => {
      if (alive) void load();
    };
    tick();
    const h = setInterval(tick, 1500);
    return () => {
      alive = false;
      clearInterval(h);
    };
  }, [cwd, tool, load]);

  // A send bumps refreshSignal: re-read now (don't wait up to 1.5s) and snap to
  // the bottom so the human sees their own message land.
  useEffect(() => {
    if (refreshSignal == null) return;
    atBottomRef.current = true;
    void load();
    endRef.current?.scrollIntoView({ block: "end" });
  }, [refreshSignal, load]);

  // Only auto-scroll when the user is already near the bottom — don't yank them
  // up to the latest when they've scrolled back to read history.
  useEffect(() => {
    if (atBottomRef.current) endRef.current?.scrollIntoView({ block: "end" });
  }, [events.length, running]);

  useEffect(() => {
    if (variant !== "console" || !running) {
      setActiveTool(null);
      return;
    }
    const latest = [...events].reverse().find((event) => event.kind === "tool");
    if (!latest) return;
    setActiveTool(latest);
    const h = window.setTimeout(() => setActiveTool(null), 6500);
    return () => window.clearTimeout(h);
  }, [events, running, variant]);

  const onScroll = () => {
    const el = scrollRef.current;
    if (el) atBottomRef.current = el.scrollHeight - el.scrollTop - el.clientHeight < 80;
  };

  const messageEvents =
    variant === "console" ? events.filter((event) => event.kind !== "tool") : events;
  const visibleEvents = hideTools
    ? messageEvents.filter((event) => event.kind !== "tool")
    : messageEvents;

  if (loaded && visibleEvents.length === 0 && !before) {
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
      className={cn("flex min-h-0 flex-1 flex-col gap-2.5 overflow-y-auto px-3 py-3", className)}
    >
      {before}
      <div className={cn("flex flex-col gap-2.5", contentClassName)}>
        {visibleEvents.map((e, i) => (
          <TranscriptEvent key={i} event={e} variant={variant} />
        ))}
        {activeTool && <TranscriptEvent event={activeTool} variant={variant} />}
        {running && variant !== "console" && (
          <div
            className={cn(
              "flex items-center gap-1.5 px-1 text-[11px] text-ink-faint",
            )}
          >
            <span className="h-1.5 w-1.5 animate-pulse rounded-full bg-running" />
            {t("lead.working")}
          </div>
        )}
      </div>
      <div ref={endRef} />
    </div>
  );
}

function TranscriptEvent({
  event,
  variant,
}: {
  event: NormEvent;
  variant: "default" | "console";
}) {
  const { t } = useTranslation();
  if (variant !== "console") {
    if (event.kind === "tool") {
      const Icon = toolIcon(event.name);
      return (
        <div className="flex items-center gap-1.5 text-[11px] text-ink-faint">
          <Icon size={11} className="shrink-0 text-ink-faint/70" />
          <span className="font-medium text-ink-muted">{event.name}</span>
          {event.summary && (
            <span className="truncate font-mono text-ink-faint">{event.summary}</span>
          )}
        </div>
      );
    }
    if (event.role === "user") {
      return (
        <div className="flex justify-end">
          <p className="max-w-[88%] whitespace-pre-wrap break-words rounded-[var(--radius-md)] bg-brand-ghost px-3 py-2 text-[12.5px] leading-relaxed text-ink">
            {event.text}
          </p>
        </div>
      );
    }
    return (
      <div className="break-words">
        <Markdown text={event.text} />
      </div>
    );
  }

  if (event.kind === "tool") {
    const Icon = toolIcon(event.name);
    const { target, added, removed } = compactToolTarget(event.name, event.summary);
    return (
      <div className="flex max-w-full items-center gap-2 px-1.5 py-1 text-[13px] text-ink-faint">
        <Icon size={15} className="shrink-0 text-ink-faint" />
        <span className="shrink-0 font-medium text-ink-muted">{t(toolLabelKey(event.name))}</span>
        <span className="min-w-0 truncate font-mono text-brand">{target}</span>
        {added && <span className="shrink-0 font-mono text-running">+{added}</span>}
        {removed && <span className="shrink-0 font-mono text-danger">-{removed}</span>}
        <ChevronRight size={15} className="shrink-0 text-ink-faint/70" />
      </div>
    );
  }

  if (event.role === "user") {
    return (
      <div className="flex justify-end">
        <p className="max-w-[72%] whitespace-pre-wrap break-words rounded-[var(--radius-lg)] border border-brand/25 bg-brand-ghost px-3.5 py-2.5 text-[13px] leading-relaxed text-ink">
          {event.text}
        </p>
      </div>
    );
  }

  return (
    <div className="flex items-start gap-2.5">
      <span className="mt-0.5 grid h-7 w-7 shrink-0 place-items-center rounded-[var(--radius-md)] bg-brand-ghost text-brand">
        <Sparkles size={14} />
      </span>
      <div className="min-w-0 flex-1 rounded-[var(--radius-lg)] border border-border bg-surface px-3.5 py-3 shadow-[0_12px_34px_-28px_rgba(0,0,0,0.65)]">
        <Markdown text={event.text} />
      </div>
    </div>
  );
}
