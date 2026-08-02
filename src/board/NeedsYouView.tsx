import { AnimatePresence, motion, useReducedMotion } from "motion/react";
import { useStore } from "../state/store";
import { AttentionRow, EmptyNeeds } from "./NeedsRows";
import { needsRowMotion } from "../lib/motion";

/**
 * The canonical actionable queue. Its rows, count, Dock and notifications all
 * consume the same backend projection and stable identities.
 */
export function NeedsYouView() {
  const { attentionItems } = useStore();
  const reduce = useReducedMotion();

  return (
    <section className="flex min-w-0 flex-1 flex-col overflow-hidden bg-bg">
      <div className="min-h-0 flex-1 overflow-y-auto">
        {attentionItems.length === 0 ? (
          <EmptyNeeds />
        ) : (
          <div className="mx-auto flex w-full max-w-[680px] flex-col gap-2.5 px-5 py-5">
            <AnimatePresence initial={false}>
              {attentionItems.map((item) => (
                <motion.div key={item.id} {...needsRowMotion(!!reduce)}>
                  <AttentionRow item={item} />
                </motion.div>
              ))}
            </AnimatePresence>
          </div>
        )}
      </div>
    </section>
  );
}
