import { useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import type { LeadMessage, RewindMode } from "../lib/types";
import { Dialog, DialogContent } from "../components/ui/Dialog";
import { Button } from "../components/ui/Button";
import { cn } from "../lib/cn";

/** All copy for one rewind scope — the dialog is one Record lookup per surface
 *  (discriminated state + exhaustive map; a new mode is a compile error until
 *  every string exists in en/zh). */
const REWIND_COPY: Record<
  RewindMode,
  { title: string; body: string; confirm: string; option: string; optionDesc: string }
> = {
  conversation: {
    title: "session.rewindTitle",
    body: "session.rewindBody",
    confirm: "session.rewindConfirm",
    option: "session.rewindModeConversation",
    optionDesc: "session.rewindModeConversationDesc",
  },
  code: {
    title: "session.rewindCodeTitle",
    body: "session.rewindCodeBody",
    confirm: "session.rewindCodeConfirm",
    option: "session.rewindModeCode",
    optionDesc: "session.rewindModeCodeDesc",
  },
  both: {
    title: "session.rewindBothTitle",
    body: "session.rewindBothBody",
    confirm: "session.rewindBothConfirm",
    option: "session.rewindModeBoth",
    optionDesc: "session.rewindModeBothDesc",
  },
};

/** All rewind scopes, in chooser order — worker hosts pass this as `modes`
 *  (module-level so the dialog's reset effect sees a stable identity). */
export const ALL_REWIND_MODES: RewindMode[] = ["conversation", "code", "both"];

/** How many rewind targets the picker lists (most recent first). */
const PICKER_CAP = 20;
/** Preview length of a target's text in the picker. */
const PREVIEW_LEN = 80;

/** Confirm dialog for a rewind (same shape as DeleteWorktreeDialog: open state
 *  driven by the parent, busy/err inside). The picked scope is ONE
 *  discriminated state mapped to copy via REWIND_COPY. The backend's rejection
 *  message (busy turn, missing anchor, unsupported transport) is shown
 *  verbatim — it already says exactly why. */
export function RewindDialog({
  open,
  onOpenChange,
  onConfirm,
  modes = ["conversation"],
}: {
  open: boolean;
  onOpenChange: (o: boolean) => void;
  onConfirm: (mode: RewindMode) => Promise<void>;
  /** Rewind scopes the user may pick. Workers pass all three; the lead console
   *  is conversation-only (its backend command takes no mode), so it keeps the
   *  default and the scope chooser stays hidden. */
  modes?: RewindMode[];
}) {
  const { t } = useTranslation();
  const [mode, setMode] = useState<RewindMode>(modes[0] ?? "conversation");
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  const wasOpen = useRef(false);
  useEffect(() => {
    if (open && !wasOpen.current) {
      setMode(modes[0] ?? "conversation");
      setBusy(false);
      setErr(null);
    }
    wasOpen.current = open;
  }, [open, modes]);

  async function confirm() {
    if (busy) return;
    setBusy(true);
    setErr(null);
    try {
      await onConfirm(mode);
      onOpenChange(false);
    } catch (e) {
      setErr(String(e));
      if (import.meta.env.DEV) console.error("chat rewind failed:", String(e));
      setBusy(false);
    }
  }

  const copy = REWIND_COPY[mode];
  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent title={t(copy.title)}>
        <div className="flex flex-col gap-4">
          {modes.length > 1 && (
            <div className="flex flex-col gap-1.5" role="radiogroup">
              {modes.map((m) => (
                <ModeOption
                  key={m}
                  mode={m}
                  selected={m === mode}
                  onSelect={() => setMode(m)}
                />
              ))}
            </div>
          )}
          <p className="text-[13px] leading-relaxed text-ink-muted">{t(copy.body)}</p>
          {err && <p className="text-[12px] text-danger">{err}</p>}
          <div className="flex justify-end gap-2">
            <Button
              type="button"
              variant="ghost"
              disabled={busy}
              onClick={() => onOpenChange(false)}
            >
              {t("common.cancel")}
            </Button>
            <Button type="button" variant="primary" disabled={busy} onClick={() => void confirm()}>
              {t(copy.confirm)}
            </Button>
          </div>
        </div>
      </DialogContent>
    </Dialog>
  );
}

/** One radio row in the scope chooser: label + what this scope restores. */
function ModeOption({
  mode,
  selected,
  onSelect,
}: {
  mode: RewindMode;
  selected: boolean;
  onSelect: () => void;
}) {
  const { t } = useTranslation();
  const copy = REWIND_COPY[mode];
  return (
    <button
      type="button"
      role="radio"
      aria-checked={selected}
      onClick={onSelect}
      className={cn(
        "flex items-start gap-2.5 rounded-[var(--radius-md)] border px-3 py-2.5 text-left transition-colors",
        selected ? "border-brand bg-brand-ghost" : "border-border hover:bg-hover",
      )}
    >
      <span
        className={cn(
          "mt-0.5 grid h-3.5 w-3.5 shrink-0 place-items-center rounded-full border",
          selected ? "border-brand" : "border-border-strong",
        )}
      >
        {selected && <span className="h-1.5 w-1.5 rounded-full bg-brand" />}
      </span>
      <span className="flex min-w-0 flex-col gap-0.5">
        <span className="text-[13px] font-medium text-ink">{t(copy.option)}</span>
        <span className="text-[12px] leading-relaxed text-ink-muted">{t(copy.optionDesc)}</span>
      </span>
    </button>
  );
}

/** Esc-Esc picker: the session's rewindable messages (completed user text
 *  rows), most recent first. Picking one hands its id to the host, which opens
 *  the RewindDialog for it. */
export function RewindPickerDialog({
  open,
  onOpenChange,
  messages,
  onPick,
}: {
  open: boolean;
  onOpenChange: (o: boolean) => void;
  /** The timeline's messages; rewindable rows are filtered here so every host
   *  offers the same candidates. */
  messages: LeadMessage[];
  onPick: (id: number) => void;
}) {
  const { t } = useTranslation();
  const targets = messages
    .filter((m) => m.role === "user" && m.kind === "text" && m.status === "complete")
    .slice(-PICKER_CAP)
    .reverse();
  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent title={t("session.rewindPickerTitle")}>
        {targets.length === 0 ? (
          <p className="text-[13px] text-ink-muted">{t("session.rewindPickerEmpty")}</p>
        ) : (
          <div className="-mx-1 flex max-h-[320px] flex-col gap-0.5 overflow-y-auto px-1">
            {targets.map((m) => (
              <button
                key={m.id}
                type="button"
                onClick={() => onPick(m.id)}
                className="rounded-[var(--radius-md)] px-3 py-2 text-left text-[13px] leading-relaxed text-ink transition-colors hover:bg-hover"
              >
                {previewOf(m.content)}
              </button>
            ))}
          </div>
        )}
      </DialogContent>
    </Dialog>
  );
}

/** One-line preview of a user text row's content JSON, capped for the list. */
function previewOf(content: string): string {
  try {
    const v = JSON.parse(content) as { text?: unknown };
    const text = typeof v.text === "string" ? v.text : "";
    const oneLine = text.replace(/\s+/g, " ").trim();
    if (oneLine.length > PREVIEW_LEN) return `${oneLine.slice(0, PREVIEW_LEN)}…`;
    return oneLine;
  } catch {
    return "";
  }
}
