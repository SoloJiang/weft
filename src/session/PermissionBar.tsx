import { useStore } from "../state/store";
import type { PermissionAsk } from "../lib/types";
import { PermissionConfirmationCard } from "../components/ConfirmationCard";

/**
 * Approvals at the conversation: when this session's agent is blocked on a
 * tool permission (Ask Bridge), answer it right here —
 * the conversation is the console, no detour through Needs-you required.
 */
export function PermissionBar({ asks }: { asks: PermissionAsk[] }) {
  const { answerPermission } = useStore();
  if (asks.length === 0) return null;
  const ask = asks[0];
  return (
    <PermissionConfirmationCard
      ask={ask}
      onAnswer={(askId, answer) => void answerPermission(askId, answer)}
      className="flex-row flex-wrap items-center gap-2 rounded-none border-x-0 border-t-0 border-b border-waiting/40 bg-waiting/10 px-3 py-2 text-[12.5px]"
      actionsClassName="shrink-0"
    />
  );
}
