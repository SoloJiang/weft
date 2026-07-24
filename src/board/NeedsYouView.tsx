import { AnimatePresence, motion, useReducedMotion } from "motion/react";
import { useStore } from "../state/store";
import {
  AskRow,
  EmptyNeeds,
  PermissionRow,
  WriteTriggerRow,
} from "./NeedsRows";
import { needsRowMotion } from "../lib/motion";

/**
 * The "Needs-you" surface (PRODUCT §7): every open agent→human question across
 * the workspace, the one thing the human is here to handle. A pure projection of
 * the bus's ask channel — no TUI parsing. Answering routes the reply straight
 * back to the asking direction's inbox.
 */
export function NeedsYouView() {
  const { needs, asks, writeTriggers } = useStore();
  const reduce = useReducedMotion();
  // Row count, not the "needs your action" count (see pendingNeedsCount in
  // state/store.tsx): this only gates empty-vs-list, so it deliberately still
  // counts a self-clearing stall notice — AskRow renders it as a no-action FYI
  // row, and it must stay reachable here even when it's the only item.
  const total = needs.length + asks.length + writeTriggers.length;

  return (
    <section className="flex min-w-0 flex-1 flex-col overflow-hidden bg-bg">
      <div className="min-h-0 flex-1 overflow-y-auto">
        {total === 0 ? (
          <EmptyNeeds />
        ) : (
          <div className="mx-auto flex w-full max-w-[680px] flex-col gap-2.5 px-5 py-5">
            <AnimatePresence initial={false}>
              {writeTriggers.map((wt) => (
                <motion.div
                  key={`wt-${wt.thread_id}-${wt.index}`}
                  {...needsRowMotion(!!reduce)}
                >
                  <WriteTriggerRow item={wt} />
                </motion.div>
              ))}
              {asks.map((ask) => (
                <motion.div
                  key={`ask-${ask.id}`}
                  {...needsRowMotion(!!reduce)}
                >
                  <PermissionRow ask={ask} />
                </motion.div>
              ))}
              {needs.map((item) => (
                <motion.div
                  key={`need-${item.ask_id}`}
                  {...needsRowMotion(!!reduce)}
                >
                  <AskRow item={item} />
                </motion.div>
              ))}
            </AnimatePresence>
          </div>
        )}
      </div>
    </section>
  );
}
