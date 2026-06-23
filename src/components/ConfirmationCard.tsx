import type { ToolUIPart } from "ai";
import type { ComponentProps, ReactNode } from "react";
import { createContext, useContext, useEffect, useMemo, useRef } from "react";
import { useTranslation } from "react-i18next";
import { MoreHorizontal, ShieldQuestion } from "lucide-react";
import type { PermissionAsk } from "../lib/types";
import { cn } from "../lib/cn";
import { Button } from "./ui/Button";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from "./ui/DropdownMenu";
import { ToolIcon, toolFullName } from "./ToolIcon";

type ToolUIPartApproval =
  | { id: string; approved?: never; reason?: never }
  | { id: string; approved: boolean; reason?: string }
  | { id: string; approved: true; reason?: string }
  | { id: string; approved: false; reason?: string }
  | undefined;

export type PermissionAnswer = "allow" | "always" | "full" | "deny";

interface ConfirmationContextValue {
  readonly approval: ToolUIPartApproval;
  readonly state: ToolUIPart["state"];
}

const ConfirmationContext = createContext<ConfirmationContextValue | null>(
  null,
);

const useConfirmation = () => {
  const context = useContext(ConfirmationContext);
  if (!context) {
    throw new Error("Confirmation components must be used within Confirmation");
  }
  return context;
};

function Alert({ className, ...props }: ComponentProps<"div">) {
  return (
    <div
      data-slot="alert"
      role="alert"
      className={cn(
        "relative grid w-full gap-1 rounded-none border border-border bg-waiting/10 px-3 py-2 text-left text-[12.5px] text-ink",
        className,
      )}
      {...props}
    />
  );
}

function AlertDescription({ className, ...props }: ComponentProps<"div">) {
  return (
    <div
      data-slot="alert-description"
      className={cn("text-[12.5px] text-ink-muted", className)}
      {...props}
    />
  );
}

type ConfirmationProps = ComponentProps<typeof Alert> & {
  readonly approval?: ToolUIPartApproval;
  readonly state: ToolUIPart["state"];
};

export function Confirmation({
  className,
  approval,
  state,
  ...props
}: ConfirmationProps) {
  const contextValue = useMemo(() => ({ approval, state }), [approval, state]);

  if (!approval || state === "input-streaming" || state === "input-available") {
    return null;
  }

  return (
    <ConfirmationContext.Provider value={contextValue}>
      <Alert className={cn("flex flex-col gap-2", className)} {...props} />
    </ConfirmationContext.Provider>
  );
}

export function ConfirmationTitle({
  className,
  ...props
}: ComponentProps<typeof AlertDescription>) {
  return <AlertDescription className={cn("inline", className)} {...props} />;
}

export function ConfirmationActions({
  className,
  ...props
}: ComponentProps<"div">) {
  const { state } = useConfirmation();
  if (state !== "approval-requested") return null;
  return (
    <div
      className={cn("flex items-center justify-end gap-2 self-end", className)}
      {...props}
    />
  );
}

export function ConfirmationAction(props: ComponentProps<typeof Button>) {
  return <Button type="button" {...props} />;
}

type PermissionConfirmationCardProps = {
  readonly ask: PermissionAsk;
  readonly onAnswer: (askId: number, answer: PermissionAnswer) => void;
  readonly className?: string;
  readonly titleClassName?: string;
  readonly actionsClassName?: string;
  readonly context?: ReactNode;
  readonly timestamp?: ReactNode;
  readonly showToolIcon?: boolean;
  readonly summaryMode?: "inline" | "block";
  /** Bind keyboard shortcuts (Enter/⌘↩/Esc). Only for a single active in-session ask. */
  readonly enableShortcuts?: boolean;
};

export function PermissionConfirmationCard({
  ask,
  onAnswer,
  className,
  titleClassName,
  actionsClassName,
  context,
  timestamp,
  showToolIcon = false,
  summaryMode = "inline",
  enableShortcuts = false,
}: PermissionConfirmationCardProps) {
  const { t } = useTranslation();
  const detailTitle = ask.detail || ask.summary;
  const isBlockSummary = summaryMode === "block";
  const rootRef = useRef<HTMLDivElement>(null);

  // On the in-session card (a single active ask) the keyboard answers it:
  // Enter = allow, ⌘/Ctrl+Enter = always, Esc = deny. Runs in the capture phase
  // and stops propagation so the card preempts other window keydown handlers —
  // e.g. an open Diff/FileTree panel also closes on Escape; without this, Esc
  // would deny AND close the panel. Skipped when focus is on an interactive
  // element, mid-IME, or the card is hidden (offsetParent null — a host may keep
  // a compact session mounted under `hidden`, which must not capture keys).
  useEffect(() => {
    if (!enableShortcuts) return;
    const onKey = (e: KeyboardEvent) => {
      // One physical press = one answer: ignore IME composition and key-repeat
      // so a held key can't resolve this ask and then the next one it exposes.
      if (e.isComposing || e.repeat) return;
      if (rootRef.current?.offsetParent == null) return;
      const el = e.target as HTMLElement | null;
      // Skip interactive targets (composer, buttons) and anything inside an open
      // menu/dialog/listbox — e.g. the ⋯ More dropdown owns Enter/Escape, so the
      // shortcut must not answer the ask out from under it.
      const interactive =
        !!el &&
        (el.tagName === "INPUT" ||
          el.tagName === "TEXTAREA" ||
          el.tagName === "BUTTON" ||
          el.tagName === "A" ||
          el.isContentEditable ||
          !!el.closest('[role="menu"],[role="menuitem"],[role="listbox"],[role="dialog"]'));
      if (interactive) return;
      const act = (answer: PermissionAnswer) => {
        e.preventDefault();
        e.stopImmediatePropagation();
        onAnswer(ask.id, answer);
      };
      if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) act("always");
      else if (e.key === "Enter") act("allow");
      else if (e.key === "Escape") act("deny");
    };
    window.addEventListener("keydown", onKey, true);
    return () => window.removeEventListener("keydown", onKey, true);
  }, [enableShortcuts, ask.id, onAnswer]);

  return (
    <Confirmation
      ref={rootRef}
      approval={{ id: String(ask.id) }}
      state="approval-requested"
      className={cn(
        "border-waiting/40 bg-waiting/10 text-[12.5px]",
        className,
      )}
    >
      <div className="flex min-w-0 items-center gap-2">
        <ShieldQuestion size={14} className="shrink-0 text-waiting" />
        {showToolIcon && <ToolIcon tool={ask.tool} size={13} />}
        <ConfirmationTitle
          className={cn("min-w-0 flex-1 truncate text-ink-muted", titleClassName)}
        >
          <span className="text-ink">{toolFullName(ask.tool)}</span>{" "}
          {t("needs.wantsPermission")}
          {!isBlockSummary && ask.summary && (
            <span className="ml-1.5 font-mono text-[11.5px] text-ink">
              {ask.summary}
            </span>
          )}
        </ConfirmationTitle>
        {timestamp}
      </div>
      {context}
      {isBlockSummary && ask.summary && (
        <p
          className="truncate font-mono text-[13px] text-ink"
          title={detailTitle}
        >
          {ask.summary}
        </p>
      )}
      <ConfirmationActions className={actionsClassName}>
        <ConfirmationAction
          size="sm"
          variant="primary"
          title={t("needs.allowTitle")}
          onClick={() => onAnswer(ask.id, "allow")}
        >
          {t("common.allow")}
        </ConfirmationAction>
        <DropdownMenu>
          <DropdownMenuTrigger asChild>
            <Button
              type="button"
              size="icon"
              variant="default"
              title={t("needs.more")}
              aria-label={t("needs.more")}
            >
              <MoreHorizontal size={15} />
            </Button>
          </DropdownMenuTrigger>
          <DropdownMenuContent>
            <DropdownMenuItem
              title={t("needs.alwaysTitle")}
              onSelect={() => onAnswer(ask.id, "always")}
            >
              {t("needs.always")}
            </DropdownMenuItem>
            <DropdownMenuItem
              title={t("needs.fullAccessTitle")}
              onSelect={() => onAnswer(ask.id, "full")}
            >
              {t("needs.fullAccess")}
            </DropdownMenuItem>
          </DropdownMenuContent>
        </DropdownMenu>
        <ConfirmationAction
          size="sm"
          variant="ghost"
          title={t("needs.denyTitle")}
          onClick={() => onAnswer(ask.id, "deny")}
        >
          {t("common.deny")}
        </ConfirmationAction>
      </ConfirmationActions>
    </Confirmation>
  );
}
