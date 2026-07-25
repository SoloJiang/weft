import type { ToolUIPart } from "ai";
import type { ComponentProps, ReactNode } from "react";
import { createContext, useContext, useEffect, useMemo, useRef } from "react";
import { useTranslation } from "react-i18next";
import { MoreHorizontal, ShieldQuestion } from "lucide-react";
import type { PermissionAsk, RiskLevel } from "../lib/types";
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

/** A visible (never hover-only) preview of an ask's full raw detail — see
 *  `hasHiddenDetail` below for why this must not be opt-in. Bounded height with
 *  its own scroll so a very long command/path list can't blow out the card.
 *  `formatDetail` is memoized on `detail` (round-2 review, issue #101
 *  P2-optional): a JSON parse+stringify on every render is wasted work once
 *  the ask stops changing — most re-renders here are from unrelated store
 *  updates (timestamp ticking, sibling asks), not a new `detail`. The
 *  truncation note is rendered here (not inside `formatDetail`, which is a
 *  plain function with no i18n access) so it goes through `t()` like every
 *  other user-facing string in this file. */
function DetailPreview({ detail }: { readonly detail: string }) {
  const { t } = useTranslation();
  const { text, omittedChars } = useMemo(() => formatDetail(detail), [detail]);
  return (
    <pre className="mt-1 max-h-28 overflow-auto whitespace-pre-wrap break-words rounded-[var(--radius-md)] border border-border/60 bg-bg px-2 py-1.5 font-mono text-[11px] leading-relaxed text-ink-muted">
      {text}
      {omittedChars > 0 && (
        <span className="mt-1 block text-ink-faint">
          {t("needs.detailTruncated", { n: omittedChars })}
        </span>
      )}
    </pre>
  );
}

/** Hard cap on rendered detail length (round-2 review, issue #101
 *  P2-optional). `DetailPreview`'s `max-h-28 overflow-auto` already crops
 *  the VISIBLE height, but that's CSS, not content — the underlying string
 *  was otherwise unbounded, so a pathologically large args blob (a giant
 *  pasted file, a huge code arg) still cost a full parse+stringify and put
 *  the whole thing in the DOM. `formatDetail` enforces this cap in TWO
 *  places (round-3 review: the first pass only capped the OUTPUT, so
 *  `JSON.parse`/`JSON.stringify` still ran on the full, untruncated input
 *  every time — see that function). The truncation note (see
 *  `DetailPreview`) makes clear there's more than what's shown. */
const MAX_DETAIL_CHARS = 20_000;

/** `detail` is the raw arg payload (issue #101: for an MCP ask this is the
 *  FULL args JSON, compact/single-line — see `hasHiddenDetail`). When it
 *  parses as JSON, pretty-print it so nested args read as an indented tree
 *  instead of one long wrapped line; anything else (a command, a path) is
 *  shown verbatim. Purely a rendering transform of the SAME `detail` string
 *  already carried end-to-end since #119 — no new data, no new channel.
 *  Returns the (possibly truncated) text plus how many characters were cut,
 *  so the caller can render a translated truncation note — this function
 *  itself returns no user-facing string.
 *
 *  Checks the RAW length BEFORE attempting `JSON.parse` (round-3 review):
 *  parsing+re-stringifying only to truncate the OUTPUT still pays the full
 *  parse cost on a payload already past the cap — exactly the cost this cap
 *  exists to avoid for the common "huge blob" case (a large `write_file`
 *  arg, a big pasted code arg). Upstream (`ask.rs`/`bus/server.rs`/
 *  `engine.rs`) has no length cap of its own on `detail`/`args_text`, so
 *  this really can be arbitrarily large. A SEPARATE cap after formatting
 *  still applies for the rarer case where a moderately-sized raw payload
 *  pretty-prints into something larger than the cap (deep nesting adds
 *  indentation on every line). */
function formatDetail(detail: string): { text: string; omittedChars: number } {
  if (detail.length > MAX_DETAIL_CHARS) {
    return {
      text: detail.slice(0, MAX_DETAIL_CHARS),
      omittedChars: detail.length - MAX_DETAIL_CHARS,
    };
  }
  let formatted: string;
  try {
    formatted = JSON.stringify(JSON.parse(detail), null, 2);
  } catch {
    formatted = detail;
  }
  if (formatted.length <= MAX_DETAIL_CHARS) {
    return { text: formatted, omittedChars: 0 };
  }
  return {
    text: formatted.slice(0, MAX_DETAIL_CHARS),
    omittedChars: formatted.length - MAX_DETAIL_CHARS,
  };
}

/** issue #101: a permission ask's danger tier, mapped to the project's
 *  existing semantic colors (never a bespoke palette) — mirrors
 *  `StatusChip`'s `Record<Status, style>` shape: exhaustive by construction,
 *  so a new `RiskLevel` variant is a compile error here until handled.
 *  `read_only` reuses `success` (calm/safe); `write` reuses `approval`
 *  (already this codebase's "an approval that involves changing something"
 *  hue — see `WriteTriggerRow`); `network_or_credential` is the most severe,
 *  `danger`; `unknown` is deliberately neutral `idle` — neither reassuring
 *  green nor alarming red, because the honest answer is "we can't tell". */
const RISK_STYLE: Record<RiskLevel, { color: string; ring: string }> = {
  read_only: { color: "text-success", ring: "ring-success/30" },
  write: { color: "text-approval", ring: "ring-approval/30" },
  network_or_credential: { color: "text-danger", ring: "ring-danger/30" },
  unknown: { color: "text-idle", ring: "ring-idle/25" },
};

const RISK_LABEL_KEYS: Record<RiskLevel, string> = {
  read_only: "needs.riskReadOnly",
  write: "needs.riskWrite",
  network_or_credential: "needs.riskNetworkOrCredential",
  unknown: "needs.riskUnknown",
};

const RISK_TITLE_KEYS: Record<RiskLevel, string> = {
  read_only: "needs.riskReadOnlyTitle",
  write: "needs.riskWriteTitle",
  network_or_credential: "needs.riskNetworkOrCredentialTitle",
  unknown: "needs.riskUnknownTitle",
};

/** The one-glance danger-tier pill (issue #101): leftmost in the card's
 *  header, right after the generic ShieldQuestion icon, so scanning a pile of
 *  cards in an authorization storm reads color FIRST. Text label alongside
 *  the color (not color alone) — color-blind-safe and legible without a
 *  legend. */
function RiskBadge({ risk }: { readonly risk: RiskLevel }) {
  const { t } = useTranslation();
  const s = RISK_STYLE[risk];
  return (
    <span
      title={t(RISK_TITLE_KEYS[risk])}
      className={cn(
        "inline-flex shrink-0 items-center rounded-full bg-raised px-1.5 py-0.5",
        "text-[10px] font-medium leading-none ring-1 ring-inset",
        s.color,
        s.ring,
      )}
    >
      {t(RISK_LABEL_KEYS[risk])}
    </span>
  );
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
  // `summary` may truncate (a multi-line command's first line — issue #89's
  // cross-engine normalization — or, for an MCP ask, just the bare tool name
  // while `detail` carries the full args JSON — issue #101). Surfacing the
  // rest ONLY via the hover `title` is not an informed decision: this card's
  // Enter/⌘Enter shortcuts (below) can approve before a human ever hovers, so
  // anything `detail` adds beyond what `summary` already shows must be
  // visible BY DEFAULT via `DetailPreview`, not opt-in. A plain `"\n" in
  // detail` check (issue #89's original test) missed the MCP case: its args
  // are a SINGLE-LINE JSON blob, so a multi-line-only check never triggered —
  // the exact gap issue #101 reported. Checking whether `summary` already
  // CONTAINS `detail` generalizes correctly across all three ask shapes: a
  // single-line command's detail equals its summary's tail (no hidden info,
  // stays collapsed); a file op's detail (the bare path) is already shown
  // in full inside `summary` (stays collapsed); an MCP ask's detail (the
  // full args) is never a substring of its bare-tool-name summary (always
  // expands).
  const hasHiddenDetail = ask.detail !== "" && !ask.summary.includes(ask.detail);
  const rootRef = useRef<HTMLDivElement>(null);

  // On the in-session card (a single active ask) the keyboard answers it:
  // Enter = allow, ⌘/Ctrl+Enter = always, Esc = deny. Runs in the capture phase
  // and stops propagation so the card preempts other window keydown handlers
  // (e.g. an open Diff/FileTree panel that also closes on Escape — otherwise Esc
  // would deny AND close the panel). It fires ONLY when the card is visible and
  // focus is neutral (the document body): if the user is interacting with ANY
  // widget — composer, buttons, the ⋯ menu, the file tree, side panels, … — that
  // widget keeps its own Enter/Escape. Allow-listing neutral focus (rather than
  // blocklisting widget roles, which can never be exhaustive) is what makes this
  // robust.
  useEffect(() => {
    if (!enableShortcuts) return;
    const onKey = (e: KeyboardEvent) => {
      // One physical press = one answer: ignore IME composition and key-repeat so
      // a held key can't resolve this ask and then the next one it exposes.
      if (e.isComposing || e.repeat) return;
      if (rootRef.current?.offsetParent == null) return;
      const ae = document.activeElement;
      if (ae && ae !== document.body && ae !== document.documentElement) return;
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
      {/* `flex-1` matters in the in-session ROW layout: it stretches the title
          block so the actions pin to the card's right edge — a short summary
          ("/bin/echo hi") otherwise left the buttons floating mid-card. In the
          default column layout it has no visible effect. `items-start` (rather
          than `items-center`) lets that column grow taller without squashing
          the icon/timestamp when a DetailPreview is showing below the title. */}
      <div className="flex min-w-0 flex-1 items-start gap-2">
        <ShieldQuestion size={14} className="mt-0.5 shrink-0 text-waiting" />
        <RiskBadge risk={ask.risk} />
        {showToolIcon && (
          <ToolIcon tool={ask.tool} size={13} className="mt-0.5 shrink-0" />
        )}
        {/* Own flex-col wrapper (not just ConfirmationTitle's own classes): the
            preview must be a SIBLING of the truncating title, never its
            descendant — nesting it inside would inherit `truncate`'s
            `overflow-hidden`/`nowrap` and clip the very content it exists to
            reveal. */}
        <div className="min-w-0 flex-1">
          <ConfirmationTitle
            className={cn("truncate text-ink-muted", titleClassName)}
          >
            <span className="text-ink">{toolFullName(ask.tool)}</span>{" "}
            {t("needs.wantsPermission")}
            {!isBlockSummary && ask.summary && (
              <span
                className="ml-1.5 font-mono text-[11.5px] text-ink"
                title={detailTitle}
              >
                {ask.summary === "acp.permission_required"
                  ? t("needs.acpPermissionRequired")
                  : ask.summary}
              </span>
            )}
          </ConfirmationTitle>
          {!isBlockSummary && hasHiddenDetail && (
            <DetailPreview detail={ask.detail} />
          )}
        </div>
        {timestamp}
      </div>
      {context}
      {isBlockSummary && ask.summary && (
        <p
          className="truncate font-mono text-[13px] text-ink"
          title={detailTitle}
        >
          {ask.summary === "acp.permission_required"
            ? t("needs.acpPermissionRequired")
            : ask.summary}
        </p>
      )}
      {isBlockSummary && hasHiddenDetail && <DetailPreview detail={ask.detail} />}
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
