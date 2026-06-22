import { AnimatePresence, motion, useReducedMotion } from "motion/react";
import { useStore } from "../state/store";
import {
  AskRow,
  EmptyNeeds,
  PermissionRow,
  WriteTriggerRow,
} from "./NeedsRows";

/**
 * The "Needs-you" surface (PRODUCT §7): every open agent→human question across
 * the workspace, the one thing the human is here to handle. A pure projection of
 * the bus's ask channel — no TUI parsing. Answering routes the reply straight
 * back to the asking direction's inbox.
 */
export function NeedsYouView() {
  const { needs, asks, writeTriggers } = useStore();
  const reduce = useReducedMotion();
  const total = needs.length + asks.length + writeTriggers.length;

  return (
    <section className="flex min-w-0 flex-1 flex-col bg-bg">
      <div className="min-h-0 flex-1 overflow-y-auto">
        {total === 0 ? (
          <EmptyNeeds />
        ) : (
          <div className="mx-auto flex w-full max-w-[680px] flex-col gap-2.5 px-5 py-5">
            <AnimatePresence initial={false}>
              {writeTriggers.map((wt) => (
                <motion.div
                  key={`wt-${wt.thread_id}-${wt.index}`}
                  layout={!reduce}
                  initial={reduce ? false : { opacity: 0, y: 6 }}
                  animate={{ opacity: 1, y: 0 }}
                  exit={reduce ? { opacity: 0 } : { opacity: 0, height: 0, marginBottom: -10, scale: 0.98 }}
                  transition={{ duration: 0.18, ease: [0.22, 1, 0.36, 1] }}
                >
                  <WriteTriggerRow item={wt} />
                </motion.div>
              ))}
              {asks.map((ask) => (
                <motion.div
                  key={`ask-${ask.id}`}
                  layout={!reduce}
                  initial={reduce ? false : { opacity: 0, y: 6 }}
                  animate={{ opacity: 1, y: 0 }}
                  exit={reduce ? { opacity: 0 } : { opacity: 0, height: 0, marginBottom: -10, scale: 0.98 }}
                  transition={{ duration: 0.18, ease: [0.22, 1, 0.36, 1] }}
                >
                  <PermissionRow ask={ask} />
                </motion.div>
              ))}
              {needs.map((item) => (
                <motion.div
                  key={`need-${item.ask_id}`}
                  layout={!reduce}
                  initial={reduce ? false : { opacity: 0, y: 6 }}
                  animate={{ opacity: 1, y: 0 }}
                  exit={reduce ? { opacity: 0 } : { opacity: 0, height: 0, marginBottom: -10, scale: 0.98 }}
                  transition={{ duration: 0.18, ease: [0.22, 1, 0.36, 1] }}
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
