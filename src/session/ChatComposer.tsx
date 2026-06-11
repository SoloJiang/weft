import { useEffect, useMemo, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import {
  Check,
  ExternalLink,
  FileText,
  Paperclip,
  Send,
  SlashSquare,
  Square,
  SquareTerminal,
  X,
} from "lucide-react";
import type { ImageAttachment } from "../lib/types";
import { api } from "../lib/api";
import { cn } from "../lib/cn";
import { Button } from "../components/ui/Button";
import { Tooltip } from "../components/ui/Tooltip";

interface PendingImage extends ImageAttachment {
  /** data URI for the thumbnail. */
  preview: string;
}

/**
 * The chat-engine composer, shared by the lead console and chat-mode workers.
 * Enter sends (Shift+Enter newline); a leading `/` opens the command palette
 * fed by the CLI's own init-reported slash_commands — built-ins, plugin
 * commands and skills alike, exactly what the agent supports headless. Images
 * paste straight in (or attach via the clip); files attach as paths the agent
 * reads itself. While a turn runs the Stop button interrupts and extra sends
 * queue — same semantics as the native TUI's mid-turn input queue.
 */
type SlashItem = { name: string; label: string; source: "local" | "cli" };

export function ChatComposer({
  slashCommands,
  localSlash,
  onLocalSlash,
  busy,
  stopped,
  queued,
  stoppedHint,
  onSend,
  onStop,
  onTakeOver,
  onOpenApp,
  extraActions,
  placeholder,
  onNeedSlashCommands,
}: {
  slashCommands: string[];
  /** Extra "local" slash items, prepended to the palette under a divider.
   *  The host handles the action: when the user picks one, onLocalSlash is
   *  called and the composer text is cleared (it is NOT sent). */
  localSlash?: { name: string; label: string }[];
  onLocalSlash?: (name: string) => void;
  busy: boolean;
  stopped: boolean;
  queued: number;
  /** Footer hint while the engine is stopped (sending resumes it). */
  stoppedHint: string;
  onSend: (text: string, images: ImageAttachment[], files: string[]) => void;
  onStop: () => void;
  /** Stop the engine + copy the terminal resume command; false = unavailable. */
  onTakeOver?: () => Promise<boolean>;
  /** Open the vendor's own app on this session (codex deep link). */
  onOpenApp?: () => void;
  /** Host-injected action icons (diff, inspect …) for the toolbar row. */
  extraActions?: React.ReactNode;
  /** Input placeholder — defaults to the lead's; workers pass their own. */
  placeholder?: string;
  /** Called when "/" is typed but the command list is empty — refresh it. */
  onNeedSlashCommands?: () => void;
}) {
  const { t } = useTranslation();
  const [text, setText] = useState("");
  const [images, setImages] = useState<PendingImage[]>([]);
  const [files, setFiles] = useState<string[]>([]);
  const [slashIdx, setSlashIdx] = useState(0);
  const [copied, setCopied] = useState(false);
  const ref = useRef<HTMLTextAreaElement>(null);

  useEffect(() => {
    const el = ref.current;
    if (!el) return;
    el.style.height = "0px";
    el.style.height = `${Math.min(el.scrollHeight, 150)}px`;
  }, [text]);

  // Palette: leading "/" with no space yet → filter the CLI's command list,
  // plus host-injected local items (e.g. /add-repo, /new-repo, /clone-repo).
  // Prefix matches outrank substring matches so built-ins like /clear surface
  // as soon as you type them, not buried under plugin skills. Locals lead.
  const slashQuery = text.startsWith("/") && !text.includes(" ") ? text.slice(1) : null;
  const slashMatches = useMemo<SlashItem[]>(() => {
    if (slashQuery == null) return [];
    if (slashCommands.length === 0 && (!localSlash || localSlash.length === 0)) return [];
    const q = slashQuery.toLowerCase();
    const bucket = (items: SlashItem[]) => {
      const exact: SlashItem[] = [];
      const prefix: SlashItem[] = [];
      const within: SlashItem[] = [];
      for (const it of items) {
        const lc = it.name.toLowerCase();
        if (lc === q) exact.push(it);
        else if (lc.startsWith(q)) prefix.push(it);
        else if (lc.includes(q)) within.push(it);
      }
      return { exact, prefix, within };
    };
    const locals: SlashItem[] = (localSlash ?? []).map((x) => ({ ...x, source: "local" }));
    const clis: SlashItem[] = slashCommands.map((name) => ({
      name,
      label: name,
      source: "cli",
    }));
    const L = bucket(locals);
    const C = bucket(clis);
    return [
      ...L.exact,
      ...L.prefix,
      ...L.within,
      ...C.exact,
      ...C.prefix,
      ...C.within,
    ].slice(0, 16);
  }, [slashQuery, slashCommands, localSlash]);
  const paletteOpen = slashMatches.length > 0;

  useEffect(() => setSlashIdx(0), [slashQuery]);

  // Typing "/" before the engine reported its command list: ask for a refresh
  // (once per palette attempt) so the palette appears as soon as data exists.
  const askedSlashRef = useRef(false);
  useEffect(() => {
    if (slashQuery == null) {
      askedSlashRef.current = false;
      return;
    }
    if (slashCommands.length === 0 && !askedSlashRef.current) {
      askedSlashRef.current = true;
      onNeedSlashCommands?.();
    }
  }, [slashQuery, slashCommands.length, onNeedSlashCommands]);

  const send = () => {
    const v = text.trim();
    if (!v && images.length === 0 && files.length === 0) return;
    onSend(v, images.map(({ media_type, data }) => ({ media_type, data })), files);
    setText("");
    setImages([]);
    setFiles([]);
  };

  const complete = (item: SlashItem) => {
    if (item.source === "local") {
      onLocalSlash?.(item.name);
      setText("");
      return;
    }
    setText(`/${item.name} `);
    ref.current?.focus();
  };

  const addImageBlob = (blob: Blob) => {
    const reader = new FileReader();
    reader.onload = () => {
      const uri = String(reader.result ?? "");
      const m = uri.match(/^data:([^;]+);base64,(.*)$/s);
      if (!m) return;
      setImages((arr) => [...arr, { media_type: m[1], data: m[2], preview: uri }]);
    };
    reader.readAsDataURL(blob);
  };

  const onPaste = (e: React.ClipboardEvent) => {
    for (const item of e.clipboardData.items) {
      if (item.type.startsWith("image/")) {
        const blob = item.getAsFile();
        if (blob) {
          e.preventDefault();
          addImageBlob(blob);
        }
      }
    }
  };

  const attachFiles = async () => {
    const picked = await api.pickFiles(t("lead.attachFiles"));
    if (picked.length === 0) return;
    // Picked files attach as paths — the agent reads them (images included)
    // with its own tools. Pasted images go inline as base64 blocks instead.
    setFiles((arr) => [...arr, ...picked.filter((p) => !arr.includes(p))]);
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
          <div className="absolute inset-x-2 bottom-full mb-2 max-h-64 overflow-y-auto rounded-[var(--radius-md)] border border-border bg-raised shadow-[0_12px_40px_-20px_rgba(0,0,0,0.6)]">
            {slashMatches.map((item, i) => {
              const divider =
                i > 0 && slashMatches[i - 1].source !== item.source
                  ? "border-t border-border/50"
                  : "";
              return (
                <button
                  key={`${item.source}:${item.name}`}
                  onMouseEnter={() => setSlashIdx(i)}
                  onClick={() => complete(item)}
                  className={cn(
                    "flex w-full items-center gap-2 px-3 py-1.5 text-left font-mono text-[12.5px]",
                    i === slashIdx ? "bg-brand-ghost text-ink" : "text-ink-muted",
                    divider,
                  )}
                >
                  <SlashSquare size={12} className="shrink-0 text-brand" />
                  <span>/{item.name}</span>
                  {item.source === "local" && (
                    <span className="ml-auto truncate text-[10.5px] text-ink-faint">
                      {item.label}
                    </span>
                  )}
                </button>
              );
            })}
          </div>
        )}

        {(images.length > 0 || files.length > 0) && (
          <div className="flex flex-wrap items-center gap-1.5 px-1.5 pb-1.5">
            {images.map((img, i) => (
              <span key={i} className="group/att relative">
                <img
                  src={img.preview}
                  alt=""
                  className="h-12 w-12 rounded-[var(--radius-md)] border border-border object-cover"
                />
                <button
                  onClick={() => setImages((arr) => arr.filter((_, j) => j !== i))}
                  aria-label={t("common.close")}
                  className="absolute -right-1.5 -top-1.5 grid h-4 w-4 place-items-center rounded-full border border-border bg-raised text-ink-faint opacity-0 transition-opacity hover:text-ink group-hover/att:opacity-100"
                >
                  <X size={9} />
                </button>
              </span>
            ))}
            {files.map((f) => (
              <span
                key={f}
                className="inline-flex items-center gap-1 rounded-full border border-border bg-bg px-2 py-1 font-mono text-[10.5px] text-ink-muted"
              >
                <FileText size={10} className="shrink-0" />
                {f.split("/").pop()}
                <button
                  onClick={() => setFiles((arr) => arr.filter((x) => x !== f))}
                  aria-label={t("common.close")}
                  className="text-ink-faint hover:text-ink"
                >
                  <X size={10} />
                </button>
              </span>
            ))}
          </div>
        )}

        <textarea
          ref={ref}
          autoFocus
          rows={1}
          value={text}
          onChange={(e) => setText(e.currentTarget.value)}
          onPaste={onPaste}
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
              if (e.key === "Tab") {
                e.preventDefault();
                complete(slashMatches[slashIdx]);
                return;
              }
              if (e.key === "Enter") {
                e.preventDefault();
                // Enter SENDS when you've typed a complete command (exact match,
                // regardless of highlight); it completes otherwise. Local items
                // are host-side actions, so even an exact local match still
                // routes through complete() to fire the host callback.
                const exact =
                  slashQuery != null
                    ? slashMatches.find((x) => x.name === slashQuery)
                    : undefined;
                if (exact && exact.source === "cli") {
                  send();
                } else {
                  complete(exact ?? slashMatches[slashIdx]);
                }
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
          placeholder={placeholder ?? t("lead.compose")}
          className="max-h-[150px] min-h-[42px] w-full resize-none bg-transparent px-2 py-1 text-[13px] leading-relaxed text-ink outline-none placeholder:text-ink-faint"
        />
        <div className="flex items-center gap-2 border-t border-border/70 px-1.5 pt-2">
          <Tooltip label={t("lead.attachFiles")}>
            <button
              onClick={() => void attachFiles()}
              aria-label={t("lead.attachFiles")}
              className="grid h-7 w-7 place-items-center rounded text-ink-faint transition-colors hover:bg-brand-ghost hover:text-ink"
            >
              <Paperclip size={13} />
            </button>
          </Tooltip>
          <span className="hidden truncate text-[11px] text-ink-faint sm:block">
            {stopped ? stoppedHint : busy ? t("lead.busyHint") : t("lead.slashHint")}
          </span>
          <span className="ml-auto" />
          {queued > 0 && (
            <span className="rounded-full bg-bg px-2 py-0.5 text-[10.5px] text-ink-faint">
              {t("lead.queuedN", { count: queued })}
            </span>
          )}
          {extraActions}
          {onOpenApp && (
            <Tooltip label={t("lead.openInApp")}>
              <button
                onClick={onOpenApp}
                aria-label={t("lead.openInApp")}
                className="grid h-7 w-7 place-items-center rounded text-ink-faint transition-colors hover:bg-brand-ghost hover:text-ink"
              >
                <ExternalLink size={13} />
              </button>
            </Tooltip>
          )}
          {onTakeOver && (
            <Tooltip label={copied ? t("lead.takeOverCopied") : t("lead.takeOverTip")}>
              <button
                onClick={() => void takeOver()}
                aria-label={t("lead.takeOverTerminal")}
                className="grid h-7 w-7 place-items-center rounded text-ink-faint transition-colors hover:bg-brand-ghost hover:text-ink"
              >
                {copied ? (
                  <Check size={13} className="text-running" />
                ) : (
                  <SquareTerminal size={13} />
                )}
              </button>
            </Tooltip>
          )}
          {busy ? (
            <Button size="sm" variant="ghost" onClick={onStop}>
              <Square size={12} />
              {t("lead.stop")}
            </Button>
          ) : null}
          <Button
            size="sm"
            variant="primary"
            disabled={!text.trim() && images.length === 0 && files.length === 0}
            onClick={send}
          >
            <Send size={13} />
            {t("lead.send")}
          </Button>
        </div>
      </div>
    </div>
  );
}
