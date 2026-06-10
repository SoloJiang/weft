import { useTranslation } from "react-i18next";
import { ShieldQuestion } from "lucide-react";
import { useStore } from "../state/store";
import type { PermissionAsk } from "../lib/types";
import { Button } from "../components/ui/Button";
import { toolFullName } from "../components/ToolIcon";

/**
 * Approvals at the conversation: when this session's agent is blocked on a
 * tool permission (Ask Bridge), answer it right here —
 * the conversation is the console, no detour through Needs-you required.
 */
export function PermissionBar({ asks }: { asks: PermissionAsk[] }) {
  const { answerPermission } = useStore();
  const { t } = useTranslation();
  if (asks.length === 0) return null;
  const ask = asks[0];
  return (
    <div className="flex flex-wrap items-center gap-2 border-b border-waiting/40 bg-waiting/10 px-3 py-2 text-[12.5px]">
      <ShieldQuestion size={14} className="shrink-0 text-waiting" />
      <span className="min-w-0 flex-1 truncate text-ink-muted">
        <span className="text-ink">{toolFullName(ask.tool)}</span> {t("needs.wantsPermission")}
        {ask.summary && <span className="ml-1.5 font-mono text-[11.5px]">{ask.summary}</span>}
      </span>
      <div className="flex shrink-0 items-center gap-1.5">
        <Button size="sm" variant="primary" onClick={() => void answerPermission(ask.id, "allow")}>
          {t("common.allow")}
        </Button>
        <Button
          size="sm"
          variant="default"
          title={t("needs.alwaysTitle")}
          onClick={() => void answerPermission(ask.id, "always")}
        >
          {t("needs.always")}
        </Button>
        <Button
          size="sm"
          variant="default"
          title={t("needs.fullAccessTitle")}
          onClick={() => void answerPermission(ask.id, "full")}
        >
          {t("needs.fullAccess")}
        </Button>
        <Button size="sm" variant="danger" onClick={() => void answerPermission(ask.id, "deny")}>
          {t("common.deny")}
        </Button>
      </div>
    </div>
  );
}
