import { AnimatePresence, motion, useReducedMotion } from "motion/react";
import { useStore } from "../state/store";
import type { PermissionAsk } from "../lib/types";
import { PermissionConfirmationCard } from "../components/ConfirmationCard";
import { needsBarMotion } from "../lib/motion";

/**
 * Approvals at the conversation: when this session's agent is blocked on a tool
 * permission (Ask Bridge), answer it right here — the conversation is the
 * console. This is the single actionable surface for the session's ask; the
 * workspace dock only points here, so the same ask is never shown twice with
 * buttons. Enter/⌘↩/Esc answer it (single active ask). Collapses on answer.
 */
export function PermissionBar({ asks }: { asks: PermissionAsk[] }) {
  const { answerPermission } = useStore();
  const reduce = useReducedMotion();
  const ask = asks[0];
  return (
    <AnimatePresence initial={false}>
      {ask && (
        <motion.div
          key={ask.id}
          {...needsBarMotion(!!reduce)}
          className="shrink-0 overflow-hidden"
        >
          <PermissionConfirmationCard
            ask={ask}
            onAnswer={(askId, answer) => void answerPermission(askId, answer)}
            className="flex-row items-center gap-2 rounded-none border-x-0 border-t-0 border-b border-waiting/40 bg-waiting/10 px-3 py-2 text-[12.5px]"
            actionsClassName="shrink-0"
            enableShortcuts
          />
        </motion.div>
      )}
    </AnimatePresence>
  );
}
