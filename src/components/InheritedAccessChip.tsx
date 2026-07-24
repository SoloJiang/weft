import { useState } from "react";
import { ShieldCheck } from "lucide-react";
import { useTranslation } from "react-i18next";
import { useStore } from "../state/store";
import { inheritedAccessOf, type InheritedKind } from "../lib/grants";
import { Dialog, DialogContent, DialogClose } from "./ui/Dialog";
import { Button } from "./ui/Button";

/** Tooltip + revoke-dialog copy per inherited kind, so the chip never claims
 *  "Full access" for an Always-only issue. Exhaustive by construction: a new
 *  kind fails to compile until it gets copy here (and in en.ts + zh.ts). */
const COPY: Record<InheritedKind, { title: string; body: string }> = {
  full: { title: "grants.inheritedTitleFull", body: "grants.revokeBodyFull" },
  always: { title: "grants.inheritedTitleAlways", body: "grants.revokeBodyAlways" },
  both: { title: "grants.inheritedTitleBoth", body: "grants.revokeBodyBoth" },
};

/**
 * Board marker for an issue whose Full-access / Always-allow rules persisted
 * across a restart (the Ask Bridge re-seeded them). It is the safety net that
 * pairs with persistence: the human can SEE that access was inherited and revoke
 * it in one click, instead of it silently carrying over with no way to undo.
 *
 * Rendered inside the kanban card's `<button>`, so the trigger is a `<span
 * role="button">` (a nested `<button>` would be invalid DOM) whose own click
 * stops propagation to avoid opening the card. The dialog's controls (overlay,
 * close, Cancel) sit in a portal but still bubble synthetic clicks through the
 * owner tree back to the card — `DialogContent` seals those so acting inside the
 * dialog never opens the issue behind it.
 */
export function InheritedAccessChip({ threadId }: { threadId: number }) {
  const { authGrants, revokeAuthGrant } = useStore();
  const { t } = useTranslation();
  const [open, setOpen] = useState(false);
  // Same derivation as the kanban card's render gate; if a concurrent reconcile
  // empties the grants while the dialog is up, render nothing.
  const info = inheritedAccessOf(authGrants, threadId);

  if (!info) return null;
  const copy = COPY[info.kind];

  const revoke = () => {
    // Close the dialog first so it animates out; the optimistic store update then
    // clears the grant and unmounts this chip. (Revoking first would flip the
    // parent's `inherited` to false and close the dialog by unmounting it — abrupt.)
    setOpen(false);
    // dir=null → clear the whole issue's grants (Full AND Always) in one call.
    void revokeAuthGrant(threadId, null, null);
  };

  return (
    <Dialog open={open} onOpenChange={setOpen}>
      <span
        role="button"
        tabIndex={0}
        title={t(copy.title, { count: info.alwaysCount })}
        onClick={(e) => {
          e.stopPropagation();
          setOpen(true);
        }}
        onKeyDown={(e) => {
          if (e.key === "Enter" || e.key === " ") {
            e.preventDefault();
            e.stopPropagation();
            setOpen(true);
          }
        }}
        className="inline-flex shrink-0 cursor-pointer items-center gap-1 rounded-full border border-waiting/40 bg-waiting/10 px-1.5 py-0.5 text-[10.5px] font-medium text-waiting transition-colors hover:bg-waiting/20"
      >
        <ShieldCheck size={11} />
        {t("grants.inherited")}
      </span>
      <DialogContent
        title={t("grants.revokeTitle")}
        description={t(copy.body, { count: info.alwaysCount })}
      >
        <div className="flex justify-end gap-2">
          <DialogClose asChild>
            <Button variant="ghost" size="sm">
              {t("common.cancel")}
            </Button>
          </DialogClose>
          <Button
            variant="danger"
            size="sm"
            onClick={(e) => {
              e.stopPropagation();
              revoke();
            }}
          >
            {t("grants.revokeConfirm")}
          </Button>
        </div>
      </DialogContent>
    </Dialog>
  );
}
