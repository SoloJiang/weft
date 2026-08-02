import { useCallback, useEffect, useState } from "react";
import { AlertOctagon } from "lucide-react";
import { AnimatePresence, motion, useReducedMotion } from "motion/react";
import type { TFunction } from "i18next";
import { useTranslation } from "react-i18next";
import { api } from "../lib/api";
import { needsBarMotion } from "../lib/motion";
import { Button } from "./ui/Button";

const POLL_MS = 3000;

type ControlState = { thread: number; dir: string; expires_at_ms: number };

// One discriminated status the banner renders from, instead of re-deriving
// "is it visible / is it the error variant" booleans at each call site (see
// CLAUDE.md on multi-way UI state).
type BannerStatus = "hidden" | "active" | "stopFailed";

type BannerView = { containerClassName: string; textClassName: string };

const BANNER_VIEW: Record<Exclude<BannerStatus, "hidden">, BannerView> = {
  active: {
    containerClassName: "border-danger/35 bg-danger/10",
    textClassName: "font-medium text-danger",
  },
  // Stop failed to persist: same alert, more emphasized (stronger tint/border,
  // bolder text) than the plain "someone's in control" state.
  stopFailed: {
    containerClassName: "border-danger/60 bg-danger/20",
    textClassName: "font-semibold text-danger",
  },
};

function bannerStatus(state: ControlState | null, stopFailed: boolean): BannerStatus {
  if (stopFailed) return "stopFailed";
  if (state !== null) return "active";
  return "hidden";
}

function bannerText(status: BannerStatus, state: ControlState | null, t: TFunction): string {
  switch (status) {
    case "stopFailed":
      return t("settings.computerControlStopFailed");
    case "active":
      return t("settings.computerControlActive", {
        thread: state?.thread ?? 0,
        dir: state?.dir ?? "",
      });
    case "hidden":
      return "";
    default:
      // Unreachable under the closed BannerStatus union (BANNER_VIEW's Record
      // already forces a new status to be handled) — fail safe to no text
      // rather than let an unhandled case throw.
      return "";
  }
}

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
 * back to Settings.
 *
 * The backend can apply the in-process latch (this run is already safe)
 * while still failing to *persist* the setting — e.g. sqlite read-only,
 * full disk, or otherwise unavailable — and reject the call to say so. That
 * half-success must never be swallowed: a stale `computer_use_enabled: true`
 * left on disk quietly re-arms computer use on the next restart, with nobody
 * told the stop didn't stick. So `stop` is not optimistic — the banner only
 * clears on a confirmed successful call, and a rejection flips it into a
 * standing, more-emphasized error state (cleared only by a successful
 * retry), never a silently swallowed `.catch`. Esc and the Stop button both
 * funnel through this same handler, so they get identical error handling.
 *
 * issue #160 round-6 review P2 #6: that local `.catch`-driven error state
 * only ever covers a Stop issued from THIS button. The OS-level global
 * Escape shortcut (`computer::register_global_escape`) triggers the exact
 * same `emergency_stop` from a spawned backend task with nobody watching
 * its result — a persist failure there had no UI to surface it at all,
 * since once the lease clears there's no holder left for the polling loop
 * to show either. The poll below now ALSO fetches
 * `get_computer_stop_persist_failed` every tick, so a failure from EITHER
 * path (this button's local catch, or the backend's own sticky flag) flips
 * the SAME error-state banner — reusing round-4's `stopFailed` UI/copy
 * rather than inventing a second one, since the underlying meaning
 * ("the disabled setting may not have persisted") is identical either way.
 * Mounted once near the top of the shell, above the content area, alongside
 * `ProcessQuotaBar`.
 */
export function ComputerControlBanner() {
  const { t } = useTranslation();
  const reduce = useReducedMotion();
  const [state, setState] = useState<ControlState | null>(null);
  // Two independent sources for the SAME error state (issue #160 round-6
  // review P2 #6) — a local one (this button's own call failed) and a
  // server one (ANY emergency_stop, including the OS-level Escape path,
  // failed to persist and hasn't since been cleared by a successful
  // re-enable). Combined below into the one discriminated `status`; neither
  // is re-derived anywhere else.
  const [localStopFailed, setLocalStopFailed] = useState(false);
  const [serverStopFailed, setServerStopFailed] = useState(false);
  const [stopping, setStopping] = useState(false);

  useEffect(() => {
    let alive = true;
    const tick = async () => {
      const [controlResult, persistResult] = await Promise.allSettled([
        api.getComputerControlState(),
        api.getComputerStopPersistFailed(),
      ]);
      if (!alive) return;
      // issue #160 round-13 P1: a transient invoke rejection must NEVER hide the
      // Stop banner / warning — that would remove the human's kill-switch surface
      // (and the WebView Escape listener) while the backend was never shown to
      // have cleared. Only a SUCCESSFUL poll updates each piece of state; a
      // rejected one keeps the last known value, so the banner stays up until a
      // real poll confirms control and the persistence warning are both gone.
      if (controlResult.status === "fulfilled") setState(controlResult.value);
      if (persistResult.status === "fulfilled") setServerStopFailed(persistResult.value);
    };
    void tick();
    const h = setInterval(() => void tick(), POLL_MS);
    return () => {
      alive = false;
      clearInterval(h);
    };
  }, []);

  const stop = useCallback(() => {
    setStopping(true);
    api.computerEmergencyStop().then(
      () => {
        setStopping(false);
        setLocalStopFailed(false);
        setState(null);
      },
      () => {
        setStopping(false);
        setLocalStopFailed(true);
      },
    );
  }, []);

  const status = bannerStatus(state, localStopFailed || serverStopFailed);

  useEffect(() => {
    if (status === "hidden") return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== "Escape") return;
      stop();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [status, stop]);

  const view = status === "hidden" ? null : BANNER_VIEW[status];

  return (
    <AnimatePresence initial={false}>
      {view !== null && (
        <motion.div
          key="computer-control-banner"
          {...needsBarMotion(Boolean(reduce))}
          role="alert"
          className={`shrink-0 overflow-hidden border-b ${view.containerClassName}`}
        >
          <div className="flex min-h-10 items-center gap-2 px-5 py-2 text-[12px]">
            <AlertOctagon size={14} className="shrink-0 text-danger" />
            <span className={`min-w-0 flex-1 truncate ${view.textClassName}`}>
              {bannerText(status, state, t)}
            </span>
            <Button
              type="button"
              variant="danger"
              size="sm"
              onClick={stop}
              disabled={stopping}
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
