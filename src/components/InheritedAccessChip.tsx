import { useState } from "react";
import { ShieldCheck } from "lucide-react";
import { useTranslation } from "react-i18next";
import { useStore } from "../state/store";
import { Dialog, DialogContent, DialogClose } from "./ui/Dialog";
import { Button } from "./ui/Button";

/**
 * Board marker for an issue whose Full-access / Always-allow rules persisted
 * across a restart (the Ask Bridge re-seeded them). It is the safety net that
 * pairs with persistence: the human can SEE that access was inherited and revoke
 * it in one click, instead of it silently carrying over with no way to undo.
 *
 * Rendered inside the kanban card's `<button>`, so the trigger is a `<span
 * role="button">` (a nested `<button>` would be invalid DOM), and every handler
 * stops propagation so acting on the chip never opens the issue behind it.
 */
export function InheritedAccessChip({ threadId }: { threadId: number }) {
  const { revokeAuthGrant } = useStore();
  const { t } = useTranslation();
  const [open, setOpen] = useState(false);
  const [busy, setBusy] = useState(false);

  const revoke = async () => {
    setBusy(true);
    try {
      // dir=null → clear the whole issue's grants in one call.
      await revokeAuthGrant(threadId, null, null);
      setOpen(false);
    } finally {
      setBusy(false);
    }
  };

  return (
    <Dialog open={open} onOpenChange={setOpen}>
      <span
        role="button"
        tabIndex={0}
        title={t("grants.inheritedTitle")}
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
        description={t("grants.revokeBody")}
      >
        <div className="flex justify-end gap-2">
          <DialogClose asChild>
            <Button variant="ghost" size="sm" disabled={busy}>
              {t("common.cancel")}
            </Button>
          </DialogClose>
          <Button
            variant="danger"
            size="sm"
            disabled={busy}
            onClick={(e) => {
              e.stopPropagation();
              void revoke();
            }}
          >
            {t("grants.revokeConfirm")}
          </Button>
        </div>
      </DialogContent>
    </Dialog>
  );
}
