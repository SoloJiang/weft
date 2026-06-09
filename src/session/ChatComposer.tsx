import { useEffect, useMemo, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { Check, Send, SlashSquare, Square, SquareTerminal } from "lucide-react";
import { cn } from "../lib/cn";
import { Button } from "../components/ui/Button";

/**
 * The chat-engine composer, shared by the lead console and chat-mode workers.
 * Enter sends (Shift+Enter newline); a leading `/` opens the command palette
 * fed by the CLI's own init-reported slash_commands — skills and commands work
 * headless exactly as in the TUI, with a real picker on top. While a turn runs
 * the Stop button interrupts; extra sends queue (the chip shows how many).
 */
export function ChatComposer({
  slashCommands,
  busy,
  stopped,
  queued,
  stoppedHint,
  onSend,
  onStop,
  onTakeOver,
}: {
  slashCommands: string[];
  busy: boolean;
  stopped: boolean;
  queued: number;
  /** Footer hint while the engine is stopped (sending resumes it). */
  stoppedHint: string;
  onSend: (text: string) => void;
  onStop: () => void;
  /** Stop the engine + copy the terminal resume command; false = unavailable. */
  onTakeOver?: () => Promise<boolean>;
}) {
  const { t } = useTranslation();
  const [text, setText] = useState("");
  const [slashIdx, setSlashIdx] = useState(0);
  const [copied, setCopied] = useState(false);
  const ref = useRef<HTMLTextAreaElement>(null);

  useEffect(() => {
    const el = ref.current;
    if (!el) return;
    el.style.height = "0px";
    el.style.height = `${Math.min(el.scrollHeight, 150)}px`;
  }, [text]);

  // Palette: leading "/" with no space yet → filter the CLI's command list.
  const slashQuery = text.startsWith("/") && !text.includes(" ") ? text.slice(1) : null;
  const slashMatches = useMemo(() => {
    if (slashQuery == null || slashCommands.length === 0) return [];
    const q = slashQuery.toLowerCase();
    return slashCommands.filter((c) => c.toLowerCase().includes(q)).slice(0, 8);
  }, [slashQuery, slashCommands]);
  const paletteOpen = slashMatches.length > 0;

  useEffect(() => setSlashIdx(0), [slashQuery]);

  const send = () => {
    const v = text.trim();
    if (!v) return;
    onSend(v);
    setText("");
  };

  const complete = (cmd: string) => {
    setText(`/${cmd} `);
    ref.current?.focus();
  };

  const takeOver = async () => {
    if (!onTakeOver) return;
    if (await onTakeOver()) {
      setCopied(true);
      window.setTimeout(() => setCopied(false), 2500);
    }
  };

  return (
    <div className="border-t border-border bg-bg px-4 py-3">
      <div className="relative mx-auto max-w-[820px] rounded-[var(--radius-lg)] border border-border bg-surface p-2 shadow-[0_12px_40px_-28px_rgba(0,0,0,0.65)]">
        {paletteOpen && (
          <div className="absolute inset-x-2 bottom-full mb-2 overflow-hidden rounded-[var(--radius-md)] border border-border bg-raised shadow-[0_12px_40px_-20px_rgba(0,0,0,0.6)]">
            {slashMatches.map((cmd, i) => (
              <button
                key={cmd}
                onMouseEnter={() => setSlashIdx(i)}
                onClick={() => complete(cmd)}
                className={cn(
                  "flex w-full items-center gap-2 px-3 py-1.5 text-left font-mono text-[12.5px]",
                  i === slashIdx ? "bg-brand-ghost text-ink" : "text-ink-muted",
                )}
              >
                <SlashSquare size={12} className="shrink-0 text-brand" />/{cmd}
              </button>
            ))}
          </div>
        )}
        <textarea
          ref={ref}
          autoFocus
          rows={1}
          value={text}
          onChange={(e) => setText(e.currentTarget.value)}
          onKeyDown={(e) => {
            if (paletteOpen) {
              if (e.key === "ArrowDown") {
                e.preventDefault();
                setSlashIdx((i) => (i + 1) % slashMatches.length);
                return;
              }
              if (e.key === "ArrowUp") {
                e.preventDefault();
                setSlashIdx((i) => (i - 1 + slashMatches.length) % slashMatches.length);
                return;
              }
              if (e.key === "Tab" || e.key === "Enter") {
                e.preventDefault();
                complete(slashMatches[slashIdx]);
                return;
              }
              if (e.key === "Escape") {
                e.preventDefault();
                setText("");
                return;
              }
            }
            if (e.key === "Enter" && !e.shiftKey) {
              e.preventDefault();
              send();
            }
          }}
          placeholder={t("lead.compose")}
          className="max-h-[150px] min-h-[42px] w-full resize-none bg-transparent px-2 py-1 text-[13px] leading-relaxed text-ink outline-none placeholder:text-ink-faint"
        />
        <div className="flex items-center gap-2 border-t border-border/70 px-1.5 pt-2">
          <span className="hidden truncate text-[11px] text-ink-faint sm:block">
            {stopped ? stoppedHint : t("lead.slashHint")}
          </span>
          <span className="ml-auto" />
          {queued > 0 && (
            <span className="rounded-full bg-bg px-2 py-0.5 text-[10.5px] text-ink-faint">
              {t("lead.queuedN", { count: queued })}
            </span>
          )}
          {onTakeOver && (
            <button
              onClick={() => void takeOver()}
              title={t("lead.takeOverTerminal")}
              className="grid h-7 w-7 place-items-center rounded text-ink-faint transition-colors hover:bg-brand-ghost hover:text-ink"
            >
              {copied ? <Check size={13} className="text-running" /> : <SquareTerminal size={13} />}
            </button>
          )}
          {copied && (
            <span className="text-[10.5px] text-ink-faint">{t("lead.takeOverCopied")}</span>
          )}
          {busy ? (
            <Button size="sm" variant="ghost" onClick={onStop}>
              <Square size={12} />
              {t("lead.stop")}
            </Button>
          ) : null}
          <Button size="sm" variant="primary" disabled={!text.trim()} onClick={send}>
            <Send size={13} />
            {t("lead.send")}
          </Button>
        </div>
      </div>
    </div>
  );
}
