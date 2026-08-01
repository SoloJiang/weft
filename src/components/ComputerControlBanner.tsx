import { useCallback, useEffect, useState } from "react";
import { AlertOctagon } from "lucide-react";
import { AnimatePresence, motion, useReducedMotion } from "motion/react";
import { useTranslation } from "react-i18next";
import { api } from "../lib/api";
import { needsBarMotion } from "../lib/motion";
import { Button } from "./ui/Button";

const POLL_MS = 3000;

type ControlState = { thread: number; dir: string; expires_at_ms: number };

/**
 * issue #160 M2: a global kill-switch banner for whenever an agent currently
 * holds the computer-use control mutex (mouse/keyboard on the real desktop).
 * Polls `get_computer_control_state` every 3s — a bare setInterval, not a
 * store subscription, since this is the only surface that cares and the
 * state is cheap to re-fetch; an invoke failure is treated the same as "no
 * holder" (silently hides the banner rather than erroring). Stop (button or
 * Esc, either while the banner is visible) calls the emergency-stop command,
 * which flips `computer_use_enabled` off and clears the mutex so every
 * subsequent computer tool call fails closed — re-enabling is a manual trip
 * back to Settings. Mounted once near the top of the shell, above the
 * content area, alongside `ProcessQuotaBar`.
 */
export function ComputerControlBanner() {
  const { t } = useTranslation();
  const reduce = useReducedMotion();
  const [state, setState] = useState<ControlState | null>(null);

  useEffect(() => {
    let alive = true;
    const tick = async () => {
      try {
        const next = await api.getComputerControlState();
        if (alive) setState(next);
      } catch {
        if (alive) setState(null);
      }
    };
    void tick();
    const h = setInterval(() => void tick(), POLL_MS);
    return () => {
      alive = false;
      clearInterval(h);
    };
  }, []);

  const stop = useCallback(() => {
    // Optimistic: hide the banner immediately rather than waiting on the
    // round trip — the next poll would confirm it anyway.
    setState(null);
    void api.computerEmergencyStop().catch(() => {});
  }, []);

  useEffect(() => {
    if (state === null) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== "Escape") return;
      stop();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [state, stop]);

  return (
    <AnimatePresence initial={false}>
      {state !== null && (
        <motion.div
          key="computer-control-banner"
          {...needsBarMotion(Boolean(reduce))}
          role="alert"
          className="shrink-0 overflow-hidden border-b border-danger/35 bg-danger/10"
        >
          <div className="flex min-h-10 items-center gap-2 px-5 py-2 text-[12px]">
            <AlertOctagon size={14} className="shrink-0 text-danger" />
            <span className="min-w-0 flex-1 truncate font-medium text-danger">
              {t("settings.computerControlActive", {
                thread: state.thread,
                dir: state.dir,
              })}
            </span>
            <Button
              type="button"
              variant="danger"
              size="sm"
              onClick={stop}
              className="shrink-0"
            >
              {t("settings.computerControlStop")}
            </Button>
          </div>
        </motion.div>
      )}
    </AnimatePresence>
  );
}
