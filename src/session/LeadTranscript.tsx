import { useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { Wrench } from "lucide-react";
import { api } from "../lib/api";
import type { NormEvent } from "../lib/types";
import { cn } from "../lib/cn";

/**
 * Observe-mode chat: renders the lead's transcript from its sidecar (the tool's
 * own jsonl), normalized to messages + tool calls. App-native React, so it
 * always renders correctly, reflows, and costs nothing close to a live TUI.
 * Polls while mounted; the PTY keeps running underneath regardless.
 */
export function LeadTranscript({ cwd, tool }: { cwd: string; tool: string }) {
  const { t } = useTranslation();
  const [events, setEvents] = useState<NormEvent[]>([]);
  const [loaded, setLoaded] = useState(false);
  const endRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    let alive = true;
    const tick = async () => {
      try {
        const ev = await api.readTranscript(cwd, tool);
        if (alive) {
          setEvents(ev);
          setLoaded(true);
        }
      } catch {
        /* not ready */
      }
    };
    void tick();
    const h = setInterval(tick, 1500);
    return () => {
      alive = false;
      clearInterval(h);
    };
  }, [cwd, tool]);

  useEffect(() => {
    endRef.current?.scrollIntoView({ block: "end" });
  }, [events.length]);

  if (loaded && events.length === 0) {
    return (
      <div className="flex flex-1 items-center justify-center px-6 text-center">
        <p className="text-[12px] leading-relaxed text-ink-faint">
          {t("lead.transcriptEmpty")}
        </p>
      </div>
    );
  }

  return (
    <div className="flex min-h-0 flex-1 flex-col gap-2.5 overflow-y-auto px-3 py-3">
      {events.map((e, i) =>
        e.kind === "tool" ? (
          <div
            key={i}
            className="flex items-center gap-1.5 text-[11px] text-ink-faint"
          >
            <Wrench size={11} className="shrink-0 text-ink-faint/70" />
            <span className="font-medium text-ink-muted">{e.name}</span>
            {e.summary && (
              <span className="truncate font-mono text-ink-faint">{e.summary}</span>
            )}
          </div>
        ) : e.role === "user" ? (
          <div key={i} className="flex justify-end">
            <p className="max-w-[88%] whitespace-pre-wrap break-words rounded-[var(--radius-md)] bg-brand-ghost px-3 py-2 text-[12.5px] leading-relaxed text-ink">
              {e.text}
            </p>
          </div>
        ) : (
          <p
            key={i}
            className={cn(
              "whitespace-pre-wrap break-words text-[12.5px] leading-relaxed text-ink",
            )}
          >
            {e.text}
          </p>
        ),
      )}
      <div ref={endRef} />
    </div>
  );
}
