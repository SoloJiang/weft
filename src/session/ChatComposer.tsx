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
import type { ImageAttachment, SlashCmd } from "../lib/types";
import { api } from "../lib/api";
import { toast } from "../components/Toast";
import { cn } from "../lib/cn";
import { useClickOutside } from "../lib/useClickOutside";
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

const IME_ENTER_GRACE_MS = 100;

/** Max messages that can wait in the queue while a turn runs. */
const MAX_QUEUED = 5;

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
}: {
  slashCommands: SlashCmd[];
  /** Extra "local" slash items, prepended to the palette under a divider.
   *  The host handles the action: when the user picks one, onLocalSlash is
   *  called and the composer text is cleared (it is NOT sent). */
  localSlash?: { name: string; label: string }[];
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
}) {
  return (
    <PromptInputProvider>
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
      />
    </PromptInputProvider>
  );
}

interface ChatComposerBodyProps {
  slashCommands: SlashCmd[];
  localSlash?: { name: string; label: string }[];
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
      ...x,
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

  const send = () => {
    const v = text.trim();
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
    if (e.key === "Escape") {
      e.preventDefault();
      setText("");
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
            <PromptInputButton
              onClick={() => void attachFiles()}
              tooltip={t("lead.attachFiles")}
            >
              <Paperclip size={13} />
            </PromptInputButton>
            <span className="hidden truncate text-[11px] text-ink-faint sm:block">
              {t("lead.slashHint")}
            </span>
          </PromptInputTools>

          <div className="ml-auto flex items-center gap-2">
            {extraActions}
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
