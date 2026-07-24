import { useSyncExternalStore } from "react";
import { AnimatePresence, motion, useReducedMotion } from "motion/react";
import { AlertTriangle, Check, CircleAlert, type LucideIcon } from "lucide-react";
import { cn } from "../lib/cn";

/**
 * Tiny app-wide toast for transient confirmations (e.g. "copied"). External
 * store so any action can `toast(msg)` without prop-drilling or a context.
 * Distinct from DangerToast (the once-a-day permission nudge).
 */
export type ToastTone = "success" | "warning" | "danger";

type Toast = { id: number; msg: string; tone: ToastTone };

const TOAST_VIEW: Record<
  ToastTone,
  {
    icon: LucideIcon;
    iconClassName: string;
    containerClassName: string;
    role: "status" | "alert";
    duration: number;
  }
> = {
  success: {
    icon: Check,
    iconClassName: "text-running",
    containerClassName: "border-border",
    role: "status",
    duration: 3_000,
  },
  warning: {
    icon: AlertTriangle,
    iconClassName: "text-waiting",
    containerClassName: "border-waiting/40",
    role: "status",
    duration: 5_000,
  },
  danger: {
    icon: CircleAlert,
    iconClassName: "text-danger",
    containerClassName: "border-danger/40",
    role: "alert",
    duration: 5_000,
  },
};

let toasts: Toast[] = [];
const listeners = new Set<() => void>();
let seq = 0;

function notify() {
  for (const l of listeners) l();
}

export function toast(msg: string, tone: ToastTone = "success") {
  const id = ++seq;
  toasts = [...toasts, { id, msg, tone }];
  notify();
  setTimeout(() => {
    toasts = toasts.filter((t) => t.id !== id);
    notify();
  }, TOAST_VIEW[tone].duration);
}

function subscribe(cb: () => void) {
  listeners.add(cb);
  return () => {
    listeners.delete(cb);
  };
}

export function Toasts() {
  const items = useSyncExternalStore(subscribe, () => toasts);
  const reduce = useReducedMotion();
  return (
    <div className="pointer-events-none fixed bottom-4 left-1/2 z-[100] flex -translate-x-1/2 flex-col items-center gap-2">
      {/* No container live region: each toast's role (status=polite / alert=assertive)
          drives its own announcement, so danger keeps assertiveness and there's no
          redundant double-announce from a nested polite region. */}
      <AnimatePresence initial={false}>
        {items.map((item) => {
          const view = TOAST_VIEW[item.tone];
          const Icon = view.icon;
          return (
            <motion.div
              key={item.id}
              layout={!reduce}
              initial={reduce ? false : { opacity: 0, y: 12 }}
              animate={{ opacity: 1, y: 0 }}
              exit={reduce ? { opacity: 0 } : { opacity: 0, y: 12 }}
              transition={{ duration: 0.2, ease: [0.22, 1, 0.36, 1] }}
              role={view.role}
              className={cn(
                "pointer-events-auto flex items-center gap-2 rounded-[var(--radius-md)] border bg-raised px-3 py-2 text-[12.5px] text-ink shadow-[0_12px_40px_-10px_rgba(0,0,0,0.6)]",
                view.containerClassName,
              )}
            >
              <Icon size={13} className={view.iconClassName} />
              {item.msg}
            </motion.div>
          );
        })}
      </AnimatePresence>
    </div>
  );
}
