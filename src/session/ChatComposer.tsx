import { useEffect, useMemo, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import {
  Check,
  ExternalLink,
  Paperclip,
  Send,
  SlashSquare,
  SquareTerminal,
} from "lucide-react";
import type { ImageAttachment, SessionMeta, SlashCmd } from "../lib/types";
import { api } from "../lib/api";
import { toast } from "../components/Toast";
import { cn } from "../lib/cn";
import { useClickOutside } from "../lib/useClickOutside";
import { ToolIcon, toolFullName } from "../components/ToolIcon";
import { Tooltip } from "../components/ui/Tooltip";
import {
  InputGroup,
  PromptInput,
  PromptInputActions,
  PromptInputAttachment,
  PromptInputButton,
  PromptInputProvider,
  PromptInputStop,
  PromptInputSubmit,
  PromptInputTextarea,
  PromptInputTools,
  usePromptInput,
} from "../components/ai-elements";

// -----------------------------------------------------------------------------// ChatComposer// -----------------------------------------------------------------------------

interface PendingImage extends ImageAttachment {
  preview: string;
}

type SlashItem = {
  name: string;
  label: string;
  source: "local" | "cli";
  description?: string;
  argHint?: string;
};

/** A host-provided local slash item. Two shapes drive what picking it does:
 *  - `act: "action"` — the host owns the side effect; the composer fires
 *    `onLocalSlash` and just clears its draft (nothing is sent).
 *  - `act: "prompt"` — a canned message; the composer sends `prompt` through its
 *    OWN send path, so pending attachments and the queue-full guard are handled
 *    exactly like a typed message (never a bare, attachment-dropping send). */
export type LocalSlashSpec =
  | { name: string; label: string; act: "action" }
  | { name: string; label: string; act: "prompt"; prompt: string };

const IME_ENTER_GRACE_MS = 100;

/** Max messages that can wait in the queue while a turn runs. */
const MAX_QUEUED = 5;

/** Window for the Esc-Esc chord that opens the rewind picker: the second Esc
 *  (on an already-empty draft) must land within this of the previous Esc. */
const REWIND_ESC_WINDOW_MS = 700;

export function ChatComposer({
  slashCommands,
  localSlash,
  onLocalSlash,
  busy,
  queued,
  onSend,
  onStop,
  onTakeOver,
  onOpenApp,
  extraActions,
  placeholder,
  onNeedSlashCommands,
  tool,
  contextMeta,
  initialValue,
  onRewindPicker,
}: {
  slashCommands: SlashCmd[];
  /** Extra "local" slash items, prepended to the palette under a divider.
   *  An `act:"action"` item defers to the host via onLocalSlash (draft cleared,
   *  nothing sent); an `act:"prompt"` item is sent by the composer itself so its
   *  canned text rides the normal send path (attachments + queue-full guard). */
  localSlash?: LocalSlashSpec[];
  onLocalSlash?: (name: string) => void;
  busy: boolean;
  queued: number;
  onSend: (
    text: string,
    images: ImageAttachment[],
    files: string[],
  ) => void | Promise<unknown>;
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
  /** Coding agent driving this session — rendered as a badge in the toolbar
   *  row (this surface has no header of its own). */
  tool?: string;
  /** Session meta for the inline context gauge (tokens/window/model). */
  contextMeta?: SessionMeta;
  /** Initial draft text (e.g. a rewound message prefilled for edit/resend).
   *  Only applied at mount — pair with a `key` change to inject a new value. */
  initialValue?: string;
  /** Esc pressed on an already-empty draft twice in quick succession — the host
   *  opens its rewind-target picker. Omit where rewind is unavailable. */
  onRewindPicker?: () => void;
}) {
  return (
    <PromptInputProvider initialValue={initialValue}>
      <ChatComposerBody
        slashCommands={slashCommands}
        localSlash={localSlash}
        onLocalSlash={onLocalSlash}
        busy={busy}
        queued={queued}
        onSend={onSend}
        onStop={onStop}
        onTakeOver={onTakeOver}
        onOpenApp={onOpenApp}
        extraActions={extraActions}
        placeholder={placeholder}
        onNeedSlashCommands={onNeedSlashCommands}
        tool={tool}
        contextMeta={contextMeta}
        onRewindPicker={onRewindPicker}
      />
    </PromptInputProvider>
  );
}

interface ChatComposerBodyProps {
  slashCommands: SlashCmd[];
  localSlash?: LocalSlashSpec[];
  onLocalSlash?: (name: string) => void;
  busy: boolean;
  queued: number;
  onSend: (
    text: string,
    images: ImageAttachment[],
    files: string[],
  ) => void | Promise<unknown>;
  onStop: () => void;
  onTakeOver?: () => Promise<boolean>;
  onOpenApp?: () => void;
  extraActions?: React.ReactNode;
  placeholder?: string;
  onNeedSlashCommands?: () => void;
  tool?: string;
  contextMeta?: SessionMeta;
  onRewindPicker?: () => void;
}

function ChatComposerBody({
  slashCommands,
  localSlash,
  onLocalSlash,
  busy,
  queued,
  onSend,
  onStop,
  onTakeOver,
  onOpenApp,
  extraActions,
  placeholder,
  onNeedSlashCommands,
  tool,
  contextMeta,
  onRewindPicker,
}: ChatComposerBodyProps) {
  const { t } = useTranslation();
  const { value: text, setValue: setText } = usePromptInput();
  const [images, setImages] = useState<PendingImage[]>([]);
  const [files, setFiles] = useState<string[]>([]);
  const [slashIdx, setSlashIdx] = useState(0);
  const [copied, setCopied] = useState(false);
  const [dismissed, setDismissed] = useState(false);

  const ref = useRef<HTMLTextAreaElement>(null);
  const slashActiveRef = useRef<HTMLButtonElement>(null);
  const wrapRef = useRef<HTMLDivElement>(null);
  const askedSlashRef = useRef(false);
  const lastSendRef = useRef(0);
  const lastEscRef = useRef(0);
  const composingRef = useRef(false);
  const lastCompositionEndRef = useRef<number | null>(null);

  const slashQuery =
    text.startsWith("/") && !text.includes(" ") ? text.slice(1) : null;

  const slashMatches = useMemo<SlashItem[]>(() => {
    if (slashQuery == null) return [];
    if (slashCommands.length === 0 && (!localSlash || localSlash.length === 0)) {
      return [];
    }
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
    const locals: SlashItem[] = (localSlash ?? []).map((x) => ({
      name: x.name,
      label: x.label,
      source: "local",
    }));
    const clis: SlashItem[] = slashCommands.map((c) => ({
      name: c.name,
      label: c.name,
      source: "cli",
      description: c.description,
      argHint: c.arg_hint,
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

  const paletteOpen = slashMatches.length > 0 && !dismissed;
  const activeSlashIdx = slashMatches.length
    ? Math.min(slashIdx, slashMatches.length - 1)
    : 0;

  useEffect(() => {
    setSlashIdx(0);
    setDismissed(false);
  }, [slashQuery]);

  useClickOutside(wrapRef, paletteOpen, () => setDismissed(true));

  useEffect(() => {
    if (paletteOpen) {
      slashActiveRef.current?.scrollIntoView({ block: "nearest" });
    }
  }, [activeSlashIdx, paletteOpen]);

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

  useEffect(() => {
    const el = ref.current;
    if (!el) return;
    el.style.height = "0px";
    el.style.height = `${Math.min(el.scrollHeight, 150)}px`;
  }, [text]);

  // overrideText sends a canned message (a local prompt-slash) instead of the
  // draft — but still through this one path, so attachments, the queue-full
  // guard, and failure-restore behave identically to a typed send.
  const send = (overrideText?: string) => {
    const v = (overrideText ?? text).trim();
    if (!v && images.length === 0 && files.length === 0) return;
    // Queue is capped while a turn runs: intercept, keep the draft, tell the user.
    if (busy && queued >= MAX_QUEUED) {
      toast(t("lead.queueFull"));
      return;
    }
    lastSendRef.current = Date.now();
    const imgs = images.map(({ media_type, data }) => ({ media_type, data }));
    const prevText = text;
    const prevImages = images;
    const prevFiles = files;
    setText("");
    setImages([]);
    setFiles([]);
    Promise.resolve(onSend(v, imgs, prevFiles)).catch(() => {
      setText(prevText);
      setImages(prevImages);
      setFiles(prevFiles);
    });
  };

  const guardedStop = () => {
    if (Date.now() - lastSendRef.current < 400) return;
    onStop();
  };

  const complete = (item: SlashItem) => {
    if (item.source === "local") {
      const spec = localSlash?.find((s) => s.name === item.name);
      // A "prompt" item sends its canned text through the real send path so
      // queue-full is surfaced and the composer's own attachments ride along
      // (not dropped, not left to leak into the next message). "action" items
      // defer to the host.
      if (spec?.act === "prompt") {
        // send() clears its own draft on success and KEEPS it when the queue is
        // full (it toasts and returns before clearing), so don't clear here —
        // otherwise a full queue would silently drop the `/…` command.
        send(spec.prompt);
        return;
      }
      onLocalSlash?.(item.name);
      setText("");
      return;
    }
    setText(`/${item.name} `);
    ref.current?.focus();
  };

  const submitComposer = () => {
    if (!paletteOpen) {
      send();
      return;
    }
    const exact =
      slashQuery != null
        ? slashMatches.find((item) => item.name === slashQuery)
        : undefined;
    if (exact?.source === "cli") {
      send();
      return;
    }
    const item = exact ?? slashMatches[activeSlashIdx];
    if (item) complete(item);
  };

  const addImageBlob = (blob: Blob) => {
    const reader = new FileReader();
    reader.onload = () => {
      const uri = String(reader.result ?? "");
      const m = uri.match(/^data:([^;]+);base64,(.*)$/s);
      if (!m) return;
      setImages((arr) => [
        ...arr,
        { media_type: m[1], data: m[2], preview: uri },
      ]);
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

  const handleKeyDown = (e: React.KeyboardEvent<HTMLTextAreaElement>) => {
    if (shouldBlockImeEnter(e)) {
      blockImeEnter(e);
      return;
    }
    // Esc clears the draft (also dismissing an open slash palette, since its
    // query is the draft). A second Esc on an already-empty draft inside the
    // window opens the host's rewind picker; the timestamp is stamped on every
    // Esc so a clear immediately followed by an empty-Esc chains into it.
    if (e.key === "Escape") {
      e.preventDefault();
      const now = Date.now();
      if (text === "") {
        if (now - lastEscRef.current < REWIND_ESC_WINDOW_MS) onRewindPicker?.();
      } else {
        setText("");
      }
      lastEscRef.current = now;
      return;
    }
    if (!paletteOpen) return;

    if (e.key === "ArrowDown") {
      e.preventDefault();
      setSlashIdx((i) => (i + 1) % slashMatches.length);
      return;
    }
    if (e.key === "ArrowUp") {
      e.preventDefault();
      setSlashIdx(
        (i) => (i - 1 + slashMatches.length) % slashMatches.length,
      );
      return;
    }
    if (e.key === "Tab") {
      e.preventDefault();
      complete(slashMatches[activeSlashIdx]);
      return;
    }
    if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      submitComposer();
      return;
    }
  };

  const shouldBlockImeEnter = (e: React.KeyboardEvent<HTMLTextAreaElement>) => {
    if (e.key !== "Enter") return false;
    if (e.nativeEvent.isComposing || composingRef.current) return true;
    const endedAt = lastCompositionEndRef.current;
    if (endedAt == null) return false;
    return performance.now() - endedAt < IME_ENTER_GRACE_MS;
  };

  const blockImeEnter = (e: React.KeyboardEvent<HTMLTextAreaElement>) => {
    e.preventDefault();
    e.stopPropagation();
  };

  const allAttachments = useMemo(() => {
    const imageAttachments = images.map((img, idx) => ({
      id: `img-${idx}`,
      name: t("lead.pastedImage", { count: idx + 1 }),
      url: img.preview,
      contentType: img.media_type,
    }));
    const fileAttachments = files.map((f, idx) => ({
      id: `file-${idx}`,
      name: f.split("/").pop() ?? f,
      contentType: "application/octet-stream",
    }));
    return [...imageAttachments, ...fileAttachments];
  }, [images, files, t]);

  const removeAttachment = (id: string) => {
    if (id.startsWith("img-")) {
      const idx = Number(id.slice(4));
      setImages((arr) => arr.filter((_, j) => j !== idx));
      return;
    }
    if (id.startsWith("file-")) {
      const idx = Number(id.slice(5));
      setFiles((arr) => arr.filter((_, j) => j !== idx));
    }
  };

  function renderSubmitButton() {
    if (busy) {
      return <PromptInputStop onClick={guardedStop} label={t("lead.stop")} />;
    }
    return (
      <PromptInputSubmit
        canSubmit={text.trim().length > 0 || images.length > 0 || files.length > 0}
        loadingLabel={t("lead.loading")}
        title={t("lead.send")}
      >
        <Send size={14} />
      </PromptInputSubmit>
    );
  }

  return (
    <div className="border-t border-border bg-bg px-4 py-3">
      <PromptInput
        onSubmit={submitComposer}
        className="relative mx-auto max-w-[820px] rounded-[var(--radius-lg)] border border-border bg-surface p-2 shadow-[0_12px_40px_-28px_rgba(0,0,0,0.65)]"
      >
        <div ref={wrapRef} className="relative">
          {paletteOpen && (
            <div className="absolute inset-x-2 bottom-full mb-2 max-h-64 overflow-y-auto rounded-[var(--radius-md)] border border-border bg-raised shadow-[0_12px_40px_-20px_rgba(0,0,0,0.6)]">
              {slashMatches.map((item, i) => {
                const divider =
                  i > 0 && slashMatches[i - 1].source !== item.source
                    ? "border-t border-border/50"
                    : "";
                const active = i === activeSlashIdx;
                return (
                  <button
                    key={`${item.source}:${item.name}`}
                    ref={active ? slashActiveRef : undefined}
                    type="button"
                    onMouseEnter={() => setSlashIdx(i)}
                    onClick={() => complete(item)}
                    className={cn(
                      "flex w-full items-center gap-2 px-3 py-1.5 text-left font-mono text-[12.5px]",
                      active ? "bg-brand-ghost text-ink" : "text-ink-muted",
                      divider,
                    )}
                  >
                    <SlashSquare size={12} className="shrink-0 text-brand" />
                    <span className="shrink-0">/{item.name}</span>
                    {item.argHint && (
                      <span className="shrink-0 text-[10.5px] text-ink-faint">
                        {item.argHint}
                      </span>
                    )}
                    {item.source === "local" ? (
                      <span className="ml-auto truncate text-[10.5px] text-ink-faint">
                        {item.label}
                      </span>
                    ) : (
                      item.description && (
                        <span className="ml-auto truncate pl-3 text-[10.5px] text-ink-faint">
                          {item.description}
                        </span>
                      )
                    )}
                  </button>
                );
              })}
            </div>
          )}

          {allAttachments.length > 0 && (
            <div className="flex flex-wrap items-center gap-1.5 px-1.5 pb-1.5">
              {allAttachments.map((att) => (
                <PromptInputAttachment
                  key={att.id}
                  attachment={att}
                  onRemove={() => removeAttachment(att.id)}
                  removeLabel={t("lead.removeAttachment", { name: att.name })}
                />
              ))}
            </div>
          )}

          <InputGroup className="min-h-[42px]">
            <PromptInputTextarea
              ref={ref}
              autoFocus
              onFocus={() => setDismissed(false)}
              onCompositionStart={() => {
                composingRef.current = true;
                lastCompositionEndRef.current = null;
              }}
              onCompositionEnd={() => {
                composingRef.current = false;
                lastCompositionEndRef.current = performance.now();
              }}
              onKeyDownCapture={(e) => {
                if (shouldBlockImeEnter(e)) {
                  blockImeEnter(e);
                }
              }}
              onPaste={onPaste}
              onKeyDown={handleKeyDown}
              placeholder={placeholder ?? t("lead.compose")}
              className="max-h-[150px] min-h-[42px] py-2"
            />
            {renderSubmitButton()}
          </InputGroup>
        </div>

        <PromptInputActions className="flex items-center gap-2 border-t border-border/70 px-1.5 pt-2">
          <PromptInputTools>
            {tool && (
              <span className="flex shrink-0 items-center gap-1.5 whitespace-nowrap rounded-[var(--radius-sm)] bg-bg px-1.5 py-0.5 text-[11px] font-medium text-ink-muted">
                <ToolIcon tool={tool} size={11} />
                {toolFullName(tool)}
              </span>
            )}
            <ContextGauge meta={contextMeta} />
          </PromptInputTools>

          <div className="ml-auto flex items-center gap-2">
            {extraActions}
            <PromptInputButton
              onClick={() => void attachFiles()}
              tooltip={t("lead.attachFiles")}
              tooltipAlign="end"
            >
              <Paperclip size={13} />
            </PromptInputButton>
            {onOpenApp && (
              <PromptInputButton
                onClick={onOpenApp}
                tooltip={t("lead.openInApp")}
                tooltipAlign="end"
              >
                <ExternalLink size={13} />
              </PromptInputButton>
            )}
            {onTakeOver && (
              <PromptInputButton
                onClick={() => void takeOver()}
                tooltipAlign="end"
                tooltip={
                  copied ? t("lead.takeOverCopied") : t("lead.takeOverTip")
                }
              >
                {copied ? (
                  <Check size={13} className="text-running" />
                ) : (
                  <SquareTerminal size={13} />
                )}
              </PromptInputButton>
            )}
          </div>
        </PromptInputActions>
      </PromptInput>
    </div>
  );
}

/** Context readout for the composer toolbar: usage, the model, AND the
 *  reasoning effort inline — the effort is a live behavior knob, so it earns a
 *  visible slot next to the model instead of hiding in a tooltip. The tooltip
 *  carries only the numbers ("57k / 200k", k-scaled, no unit word); with
 *  nothing numeric to add it isn't rendered at all. With tokens but no window
 *  (window probe failed / unknown model) the bare token count still shows —
 *  it's the only usage signal. Before the first usage event only the model
 *  (+ effort) shows; nothing known → hidden. */
function ContextGauge({ meta }: { meta?: SessionMeta }) {
  const ct = meta?.contextTokens;
  const win = meta?.window;
  const model = meta?.model;
  const effort = meta?.reasoningEffort;
  if (ct == null && !model) return null;
  const pct =
    ct != null && win != null && win > 0 ? Math.min(100, Math.round((ct / win) * 100)) : null;
  const fmtK = (n: number) => (n >= 1000 ? `${Math.round(n / 1000)}k` : String(n));
  let usage: string | null = null;
  if (ct != null) {
    usage = win != null ? `${fmtK(ct)} / ${fmtK(win)}` : fmtK(ct);
  }
  const windowOnly = ct == null && win != null ? fmtK(win) : null;
  const detail = usage ?? windowOnly;
  const gauge = (
    <span className="flex min-w-0 items-center gap-1.5 text-[11px] tabular-nums text-ink-faint">
      {pct != null && (
        <span className="h-1 w-9 shrink-0 overflow-hidden rounded-full bg-border">
          <span className="block h-full rounded-full bg-brand" style={{ width: `${pct}%` }} />
        </span>
      )}
      {pct != null && <span className="shrink-0">{pct}%</span>}
      {pct == null && ct != null && <span className="shrink-0">{fmtK(ct)}</span>}
      {model && <span className="min-w-0 truncate font-mono text-[10.5px]">{model}</span>}
      {effort && (
        <span className="shrink-0 font-mono text-[10.5px]">
          {model ? `· ${effort}` : effort}
        </span>
      )}
    </span>
  );
  if (!detail) return gauge;
  // `align="start"`: the wrapper spans bar + model + effort (~200px), so a
  // centered bubble would float over the middle of the composer, detached from
  // the bar it annotates. Left-anchored, it sits right above the gauge bar.
  return (
    <Tooltip label={detail} className="min-w-0" align="start">
      {gauge}
    </Tooltip>
  );
}
