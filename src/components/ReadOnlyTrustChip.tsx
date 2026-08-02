import { useState } from "react";
import { Eye } from "lucide-react";
import { useTranslation } from "react-i18next";
import { useStore } from "../state/store";
import { Dialog, DialogContent, DialogClose } from "./ui/Dialog";
import { Button } from "./ui/Button";

/**
 * Board marker for an issue whose dispatch approval propagated issue #103's
 * read-only auto-allow to every worker under it (`confirmProposal` /
 * scope confirmation on the backend). Sibling to `InheritedAccessChip`, but
 * a DIFFERENT grant with a DIFFERENT lifetime: this one is in-memory only —
 * it never survives a restart (contrast Full/Always) — and it only ever
 * covers a `read_only`-tier ask, never Write/NetworkOrCredential/Unknown. The
 * human can see it and revoke it in one click, same shape as
 * `InheritedAccessChip`'s revoke dialog.
 *
 * Rendered inside the kanban card's `<button>`, so the trigger is a `<span
 * role="button">` (a nested `<button>` would be invalid DOM) whose own click
 * stops propagation to avoid opening the card.
 */
export function ReadOnlyTrustChip({ threadId }: { threadId: number }) {
  const { readOnlyGrants, revokeReadOnlyGrant } = useStore();
  const { t } = useTranslation();
  const [open, setOpen] = useState(false);

  if (!readOnlyGrants.issue.includes(threadId)) return null;

  const revoke = () => {
    setOpen(false);
    // dir=null → revoke the whole issue's propagation.
    void revokeReadOnlyGrant(threadId, null);
  };

  return (
    <Dialog open={open} onOpenChange={setOpen}>
      <span
        role="button"
        tabIndex={0}
        title={t("grants.readOnlyIssueTitle")}
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
        className="inline-flex shrink-0 cursor-pointer items-center gap-1 rounded-full border border-success/40 bg-success/10 px-1.5 py-0.5 text-[10.5px] font-medium text-success transition-colors hover:bg-success/20"
      >
        <Eye size={11} />
        {t("grants.readOnlyIssue")}
      </span>
      <DialogContent
        title={t("grants.revokeReadOnlyTitle")}
        description={t("grants.revokeReadOnlyIssueBody")}
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
