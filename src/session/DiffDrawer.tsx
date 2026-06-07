import { useEffect } from "react";
import { AnimatePresence, motion } from "motion/react";
import { useTranslation } from "react-i18next";
import { X } from "lucide-react";
import { DiffView } from "./DiffView";

const EXPO = [0.16, 1, 0.3, 1] as const;

/**
 * The worktree diff as an on-demand right drawer (not a tab): the change review
 * slides over the session, like the thread-bus drawer. Esc / backdrop closes.
 */
export function DiffDrawer({
  cwd,
  open,
  onClose,
}: {
  cwd: string;
  open: boolean;
  onClose: () => void;
}) {
  const { t } = useTranslation();

  useEffect(() => {
    if (!open) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [open, onClose]);

  return (
    <AnimatePresence>
      {open && (
        <div className="fixed inset-0 z-50 flex justify-end">
          <motion.div
            initial={{ opacity: 0 }}
            animate={{ opacity: 1 }}
            exit={{ opacity: 0 }}
            transition={{ duration: 0.18 }}
            onClick={onClose}
            className="absolute inset-0 bg-[oklch(0_0_0/0.4)]"
          />
          <motion.aside
            initial={{ x: 36, opacity: 0 }}
            animate={{ x: 0, opacity: 1 }}
            exit={{ x: 36, opacity: 0 }}
            transition={{ duration: 0.22, ease: EXPO }}
            className="relative flex h-full w-[min(680px,82vw)] flex-col border-l border-border bg-bg"
          >
            <header className="flex items-center gap-2 border-b border-border px-4 py-2.5">
              <span className="text-[12px] font-semibold text-ink">{t("diff.tab")}</span>
              <button
                onClick={onClose}
                aria-label={t("bus.close")}
                className="ml-auto grid h-7 w-7 place-items-center rounded-[var(--radius-md)] text-ink-faint transition-colors hover:bg-brand-ghost hover:text-ink"
              >
                <X size={15} />
              </button>
            </header>
            <DiffView cwd={cwd} />
          </motion.aside>
        </div>
      )}
    </AnimatePresence>
  );
}
