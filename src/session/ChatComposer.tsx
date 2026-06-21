import {
  createContext,
  forwardRef,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useRef,
  useState,
  type ReactNode,
  type TextareaHTMLAttributes,
} from "react";
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
import type { ImageAttachment, SlashCmd } from "../lib/types";
import { api } from "../lib/api";
import { cn } from "../lib/cn";
import { useClickOutside } from "../lib/useClickOutside";
import { Button } from "../components/ui/Button";
import { Tooltip } from "../components/ui/Tooltip";

// -----------------------------------------------------------------------------// AI SDK Elements-style PromptInput primitives (self-contained, Weft-skinned)// -----------------------------------------------------------------------------

interface PromptInputAttachmentLike {
  id?: string;
  name?: string;
  url?: string;
  contentType?: string;
}

interface PromptInputContextValue {
  value: string;
  setValue: (value: string) => void;
  isLoading: boolean;
  disabled: boolean;
  onSubmit?: () => void;
}

const PromptInputContext = createContext<PromptInputContextValue | null>(
  null,
);

function usePromptInput() {
  const ctx = useContext(PromptInputContext);
  if (!ctx) {
    throw new Error("usePromptInput must be used within a PromptInputProvider");
  }
  return ctx;
}

function PromptInputProvider({
  children,
  initialValue = "",
  isLoading = false,
  disabled = false,
  onSubmit,
}: {
  children: ReactNode;
  initialValue?: string;
  isLoading?: boolean;
  disabled?: boolean;
  onSubmit?: () => void;
}) {
  const [value, setValue] = useState(initialValue);
  const ctx = useMemo(
    () => ({ value, setValue, isLoading, disabled, onSubmit }),
    [value, isLoading, disabled, onSubmit],
  );
  return (
    <PromptInputContext.Provider value={ctx}>
      {children}
    </PromptInputContext.Provider>
  );
}

function PromptInput({
  children,
  className,
  onSubmit,
}: {
  children: ReactNode;
  className?: string;
  onSubmit?: (event: React.FormEvent) => void;
}) {
  const ctx = useContext(PromptInputContext);
  const handleSubmit = useCallback(
    (event: React.FormEvent) => {
      event.preventDefault();
      ctx?.onSubmit?.();
      onSubmit?.(event);
    },
    [ctx, onSubmit],
  );
  return (
    <form
      onSubmit={handleSubmit}
      className={cn("flex flex-col gap-2", className)}
    >
      {children}
    </form>
  );
}

const PromptInputTextarea = forwardRef<
  HTMLTextAreaElement,
  Omit<
    TextareaHTMLAttributes<HTMLTextAreaElement>,
    "value" | "onChange"
  >
>(({ className, onKeyDown, ...props }, ref) => {
  const ctx = usePromptInput();
  const handleChange = useCallback(
    (event: React.ChangeEvent<HTMLTextAreaElement>) => {
      ctx.setValue(event.target.value);
    },
    [ctx],
  );
  const handleKeyDown = useCallback(
    (event: React.KeyboardEvent<HTMLTextAreaElement>) => {
      onKeyDown?.(event);
      if (event.defaultPrevented) return;
      if (
        event.key === "Enter" &&
        !event.shiftKey &&
        !event.nativeEvent.isComposing
      ) {
        event.preventDefault();
        ctx.onSubmit?.();
      }
    },
    [ctx, onKeyDown],
  );
  return (
    <textarea
      ref={ref}
      value={ctx.value}
      onChange={handleChange}
      onKeyDown={handleKeyDown}
      disabled={ctx.disabled}
      className={cn(
        "min-h-[44px] flex-1 resize-none bg-transparent px-3 py-2.5 text-[13px] leading-relaxed text-ink outline-none placeholder:text-ink-faint",
        className,
      )}
      {...props}
    />
  );
});
PromptInputTextarea.displayName = "PromptInputTextarea";

function PromptInputActions({
  children,
  className,
}: {
  children: ReactNode;
  className?: string;
}) {
  return (
    <div
      className={cn(
        "flex items-center justify-between gap-2",
        className,
      )}
    >
      {children}
    </div>
  );
}

function PromptInputTools({
  children,
  className,
}: {
  children: ReactNode;
  className?: string;
}) {
  return (
    <div className={cn("flex flex-wrap items-center gap-1", className)}>
      {children}
    </div>
  );
}

function InputGroup({
  children,
  className,
}: {
  children: ReactNode;
  className?: string;
}) {
  return (
    <div
      className={cn(
        "flex w-full items-stretch rounded-[var(--radius-md)] border border-border bg-bg transition-colors duration-150 focus-within:border-brand focus-within:ring-2 focus-within:ring-brand/30 hover:border-border-strong",
        className,
      )}
    >
      {children}
    </div>
  );
}

function InputGroupButton({
  children,
  onClick,
  disabled,
  title,
  type = "button",
}: {
  children: ReactNode;
  onClick?: () => void;
  disabled?: boolean;
  title?: string;
  type?: "button" | "submit";
}) {
  return (
    <div className="flex items-end p-2">
      <Button
        type={type}
        size="icon"
        variant="primary"
        disabled={disabled}
        title={title}
        onClick={onClick}
        className="h-8 w-8"
      >
        {children}
      </Button>
    </div>
  );
}

function Spinner({ className }: { className?: string }) {
  return (
    <svg
      className={cn("animate-spin", className)}
      xmlns="http://www.w3.org/2000/svg"
      fill="none"
      viewBox="0 0 24 24"
      aria-label="Loading"
    >
      <circle
        className="opacity-25"
        cx="12"
        cy="12"
        r="10"
        stroke="currentColor"
        strokeWidth="4"
      />
      <path
        className="opacity-75"
        fill="currentColor"
        d="M4 12a8 8 0 018-8V0C5.373 0 0 5.373 0 12h4zm2 5.291A7.962 7.962 0 014 12H0c0 3.042 1.135 5.824 3 7.938l3-2.647z"
      />
    </svg>
  );
}

function PromptInputSubmit({
  children,
  disabled,
  title,
}: {
  children?: ReactNode;
  disabled?: boolean;
  title?: string;
}) {
  const ctx = usePromptInput();
  return (
    <InputGroupButton
      type="submit"
      disabled={ctx.disabled || disabled || !ctx.value.trim()}
      title={title}
    >
      {ctx.isLoading ? <Spinner className="h-4 w-4" /> : children}
    </InputGroupButton>
  );
}

function PromptInputButton({
  children,
  className,
  disabled,
  title,
  onClick,
  tooltip,
}: {
  children: ReactNode;
  className?: string;
  disabled?: boolean;
  title?: string;
  onClick?: () => void;
  tooltip?: string;
}) {
  const ctx = usePromptInput();
  const button = (
    <Button
      type="button"
      size="icon"
      variant="ghost"
      disabled={ctx.disabled || disabled}
      title={title}
      onClick={onClick}
      className={cn("h-7 w-7", className)}
    >
      {children}
    </Button>
  );
  if (tooltip) {
    return <Tooltip label={tooltip}>{button}</Tooltip>;
  }
  return button;
}

function PromptInputAttachment({
  attachment,
  onRemove,
}: {
  attachment: PromptInputAttachmentLike;
  onRemove?: () => void;
}) {
  const isImage = attachment.contentType?.startsWith("image/");
  const name = attachment.name ?? "attachment";
  return (
    <div
      className={cn(
        "group/attachment relative flex items-center gap-2 overflow-hidden rounded-[var(--radius-md)] border border-border bg-raised p-1.5 pr-7 text-[12px] max-w-[180px]",
      )}
      title={name}
    >
      {isImage && attachment.url ? (
        <img
          src={attachment.url}
          alt={name}
          className="h-8 w-8 rounded-[var(--radius-sm)] object-cover"
        />
      ) : (
        <div className="flex h-8 w-8 shrink-0 items-center justify-center rounded-[var(--radius-sm)] bg-bg text-ink-muted">
          <FileText size={16} />
        </div>
      )}
      <span className="truncate text-ink-muted">{name}</span>
      {onRemove && (
        <Button
          type="button"
          size="icon"
          variant="ghost"
          onClick={onRemove}
          className="absolute right-0.5 top-1/2 h-5 w-5 -translate-y-1/2 opacity-0 transition-opacity group-hover/attachment:opacity-100"
          aria-label={`Remove ${name}`}
        >
          <X size={12} />
        </Button>
      )}
    </div>
  );
}

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
    <PromptInputProvider disabled={busy}>
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
      const exact =
        slashQuery != null
          ? slashMatches.find((x) => x.name === slashQuery)
          : undefined;
      if (exact && exact.source === "cli") {
        send();
      } else {
        complete(exact ?? slashMatches[activeSlashIdx]);
      }
      return;
    }
    if (e.key === "Escape") {
      e.preventDefault();
      setText("");
    }
  };

  const allAttachments = useMemo(() => {
    const imageAttachments = images.map((img, idx) => ({
      id: `img-${idx}`,
      name: t("lead.pastedImage"),
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
      return (
        <PromptInputButton
          onClick={guardedStop}
          tooltip={t("lead.stop")}
          className="text-danger hover:bg-danger/10 hover:text-danger"
        >
          <Square size={14} />
        </PromptInputButton>
      );
    }
    return (
      <PromptInputSubmit title={t("lead.send")}>
        <Send size={14} />
      </PromptInputSubmit>
    );
  }

  return (
    <div className="border-t border-border bg-bg px-4 py-3">
      <PromptInput
        onSubmit={() => {
          if (paletteOpen) return;
          send();
        }}
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
                />
              ))}
            </div>
          )}

          <InputGroup className="min-h-[42px]">
            <PromptInputTextarea
              ref={ref}
              autoFocus
              onFocus={() => setDismissed(false)}
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
            {queued > 0 && (
              <span className="rounded-full bg-bg px-2 py-0.5 text-[10.5px] text-ink-faint">
                {t("lead.queuedN", { count: queued })}
              </span>
            )}
            {extraActions}
            {onOpenApp && (
              <PromptInputButton
                onClick={onOpenApp}
                tooltip={t("lead.openInApp")}
              >
                <ExternalLink size={13} />
              </PromptInputButton>
            )}
            {onTakeOver && (
              <PromptInputButton
                onClick={() => void takeOver()}
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
