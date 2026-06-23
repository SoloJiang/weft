import {
  createContext,
  forwardRef,
  useCallback,
  useContext,
  useMemo,
  useState,
  type ChangeEvent,
  type FormEvent,
  type KeyboardEvent,
  type ReactNode,
  type TextareaHTMLAttributes,
} from "react";
import { FileText, X } from "lucide-react";
import { Button } from "../ui/Button";
import { Tooltip } from "../ui/Tooltip";
import { cn } from "../../lib/cn";

export type PromptInputAttachmentLike = {
  readonly id?: string;
  readonly name?: string;
  readonly url?: string;
  readonly contentType?: string;
};

type PromptInputContextValue = {
  readonly value: string;
  readonly setValue: (value: string) => void;
  readonly isLoading: boolean;
  readonly disabled: boolean;
  readonly onSubmit?: () => void;
};

const PromptInputContext = createContext<PromptInputContextValue | null>(null);

export function usePromptInput() {
  const ctx = useContext(PromptInputContext);
  if (!ctx) {
    throw new Error("usePromptInput must be used within a PromptInputProvider");
  }
  return ctx;
}

export function PromptInputProvider({
  children,
  initialValue = "",
  isLoading = false,
  disabled = false,
  onSubmit,
}: {
  readonly children: ReactNode;
  readonly initialValue?: string;
  readonly isLoading?: boolean;
  readonly disabled?: boolean;
  readonly onSubmit?: () => void;
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

export function PromptInput({
  children,
  className,
  onSubmit,
}: {
  readonly children: ReactNode;
  readonly className?: string;
  readonly onSubmit?: (event: FormEvent) => void;
}) {
  const ctx = useContext(PromptInputContext);
  const handleSubmit = useCallback(
    (event: FormEvent) => {
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

export const PromptInputTextarea = forwardRef<
  HTMLTextAreaElement,
  Omit<TextareaHTMLAttributes<HTMLTextAreaElement>, "value" | "onChange">
>(({ className, onKeyDown, ...props }, ref) => {
  const ctx = usePromptInput();
  const handleChange = useCallback(
    (event: ChangeEvent<HTMLTextAreaElement>) => {
      ctx.setValue(event.target.value);
    },
    [ctx],
  );
  const handleKeyDown = useCallback(
    (event: KeyboardEvent<HTMLTextAreaElement>) => {
      onKeyDown?.(event);
      if (event.defaultPrevented) return;
      if (
        event.key === "Enter" &&
        !event.shiftKey &&
        !event.nativeEvent.isComposing
      ) {
        event.preventDefault();
        if (ctx.onSubmit) {
          ctx.onSubmit();
          return;
        }
        event.currentTarget.form?.requestSubmit();
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

export function PromptInputActions({
  children,
  className,
}: {
  readonly children: ReactNode;
  readonly className?: string;
}) {
  return (
    <div className={cn("flex items-center justify-between gap-2", className)}>
      {children}
    </div>
  );
}

export function PromptInputTools({
  children,
  className,
}: {
  readonly children: ReactNode;
  readonly className?: string;
}) {
  return (
    <div className={cn("flex flex-wrap items-center gap-1", className)}>
      {children}
    </div>
  );
}

export function InputGroup({
  children,
  className,
}: {
  readonly children: ReactNode;
  readonly className?: string;
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
  readonly children: ReactNode;
  readonly onClick?: () => void;
  readonly disabled?: boolean;
  readonly title?: string;
  readonly type?: "button" | "submit";
}) {
  return (
    <div className="flex items-end p-2">
      <Button
        type={type}
        size="icon"
        variant="primary"
        disabled={disabled}
        title={title}
        aria-label={title}
        onClick={onClick}
        className="h-8 w-8"
      >
        {children}
      </Button>
    </div>
  );
}

export function PromptInputButton({
  children,
  className,
  disabled,
  title,
  onClick,
  tooltip,
}: {
  readonly children: ReactNode;
  readonly className?: string;
  readonly disabled?: boolean;
  readonly title?: string;
  readonly onClick?: () => void;
  readonly tooltip?: string;
}) {
  const ctx = usePromptInput();
  const accessibleLabel = title ?? tooltip;
  const button = (
    <Button
      type="button"
      size="icon"
      variant="ghost"
      disabled={ctx.disabled || disabled}
      title={accessibleLabel}
      aria-label={accessibleLabel}
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

function Spinner({ className, label }: { readonly className?: string; readonly label: string }) {
  return (
    <svg
      className={cn("animate-spin", className)}
      xmlns="http://www.w3.org/2000/svg"
      fill="none"
      viewBox="0 0 24 24"
      aria-label={label}
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

export function PromptInputSubmit({
  children,
  canSubmit,
  disabled,
  loadingLabel,
  title,
}: {
  readonly children?: ReactNode;
  readonly canSubmit?: boolean;
  readonly disabled?: boolean;
  readonly loadingLabel: string;
  readonly title?: string;
}) {
  const ctx = usePromptInput();
  const hasText = ctx.value.trim().length > 0;
  const canSend = canSubmit ?? hasText;
  return (
    <InputGroupButton
      type="submit"
      disabled={ctx.disabled || disabled || !canSend}
      title={title}
    >
      {ctx.isLoading ? <Spinner className="h-4 w-4" label={loadingLabel} /> : children}
    </InputGroupButton>
  );
}

export function PromptInputAttachment({
  attachment,
  onRemove,
  removeLabel,
}: {
  readonly attachment: PromptInputAttachmentLike;
  readonly onRemove?: () => void;
  readonly removeLabel?: string;
}) {
  const isImage = attachment.contentType?.startsWith("image/");
  const name = attachment.name ?? "attachment";
  return (
    <div
      className={cn(
        "group/attachment relative flex max-w-[180px] items-center gap-2 overflow-hidden rounded-[var(--radius-md)] border border-border bg-raised p-1.5 pr-7 text-[12px]",
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
          className="absolute right-0.5 top-1/2 h-5 w-5 -translate-y-1/2 opacity-0 transition-opacity group-hover/attachment:opacity-100 group-focus-within/attachment:opacity-100 focus-visible:opacity-100"
          aria-label={removeLabel}
        >
          <X size={12} />
        </Button>
      )}
    </div>
  );
}
