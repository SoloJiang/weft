import { useCallback, useEffect, useRef, useState } from "react";
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
 *
 * issue #160 round-15 P2: round-6's merge only ran one direction — nothing
 * ever cleared `localStopFailed` again. When a human fixes the problem by
 * re-enabling computer use from Settings, the backend clears its OWN sticky
 * flag (`clear_emergency_stop` resets `STOP_PERSIST_FAILED`), so polling
 * picks that up and `serverStopFailed` goes back to false — but a Stop that
 * failed from THIS button's own local `.catch` left `localStopFailed` stuck
 * true forever, pinning the banner in the emphasized error state even though
 * computer use is enabled again and nothing is actually wrong anymore. Fixed
 * by watching for a true -> false TRANSITION of the server flag across
 * successful polls (tracked in `lastServerStopFailedRef`, updated only when
 * a poll actually resolves — never on a rejection, preserving round-13's
 * fail-safe below) and clearing `localStopFailed` only on that transition.
 * A transition, not a bare "server says false", because a purely local
 * failure (the invoke rejects before ever reaching the backend) never flips
 * the server flag true in the first place — so a steady stream of `false`
 * polls must NOT be read as "resolved" and clear it out from under a still-
 * unretried local failure.
 *
 * issue #160 round-16 P2: round-15's transition still requires a fulfilled
 * poll to have observed the server flag AT true before it can see the
 * true -> false edge. The poll interval is 3s; if a human fixes a failed-to-
 * persist Stop by jumping straight to Settings and re-enabling computer use,
 * the whole true -> false round trip can complete between two ticks with no
 * poll ever landing on `true` in between. `lastServerStopFailedRef` never
 * sees a true, the transition never fires, and `localStopFailed` is stuck
 * forever even though everything is actually fine again. Fixed by also
 * polling `get_computer_use_enabled` every tick: the only backend path that
 * can turn `computer_use_enabled` back to true is a human explicitly
 * re-enabling it from Settings, and that path (`clear_emergency_stop`) is
 * the SAME command that resets the backend's own persist-failed flag — so a
 * fulfilled `enabled === true` poll is itself proof any prior persist
 * failure has already been handled, and clears `localStopFailed` directly
 * (mirroring `lastServerStopFailedRef` to `false` alongside it, so the two
 * tracked flags don't disagree about what the server last confirmed).
 * Round-15's transition check stays too — it still covers the case where
 * enabled is already false (e.g. round-13's fail-safe below never treats an
 * enabled poll as license to peek past a still-disabled state), and now
 * mostly acts as a harmless second path to the same clear. Round-13's
 * fail-safe applies here unchanged: a rejected enabled poll is a no-op, and
 * `enabled === false` never clears anything — a disabled state means
 * whatever local failure is recorded is still real.
 *
 * issue #160 round-17 P1: the poll above is a `setInterval` firing every 3s
 * regardless of whether the PREVIOUS tick's three invokes have resolved yet.
 * If one tick runs slower than the interval, a newer tick can start while the
 * older one is still in flight, and their `Promise.allSettled` results can
 * land in either order — a stale, slower `null` control-state result from the
 * OLDER tick arriving AFTER a newer "has holder" result would hide the
 * banner/Stop button/Escape listener while control is still actually active
 * (and symmetrically, a stale `enabled: true` could clear a fresher local
 * Stop failure). Fixed with `tickInFlightRef` (skip a tick entirely if the
 * previous one hasn't finished — single-flight, so at most one tick's results
 * are ever applied, which rules out the reordering outright) plus
 * `tickSeqRef` as defense in depth (each tick that actually runs claims a
 * sequence number and refuses to write any state if it's no longer the
 * latest by the time its `await` returns) — the same belt/suspenders shape
 * as `alive`, except `alive` only guards against the effect unmounting, not
 * against two ticks from the SAME mount racing each other.
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
  // issue #160 round-15 P2: last CONFIRMED (successfully polled) server
  // value, used only to detect a true -> false transition — see the class
  // doc comment above for why a transition (and not a bare "is false")
  // is what's required to also clear `localStopFailed`. Only ever written
  // from inside a fulfilled poll, so it shares round-13's fail-safe: a
  // rejected poll leaves it (and everything else) untouched.
  const lastServerStopFailedRef = useRef(false);
  // issue #160 round-17 P1: guards against a NEWER tick's results landing
  // before an OLDER tick's (the invokes are network calls; nothing orders
  // them relative to each other once more than one tick is in flight). See
  // the two comments at their use sites below, and the class doc comment
  // above for the failure this closes.
  const tickInFlightRef = useRef(false);
  const tickSeqRef = useRef(0);

  useEffect(() => {
    let alive = true;
    const tick = async () => {
      // round-17 P1 layer 1 — single-flight: if the previous tick's invokes
      // haven't resolved yet, skip this tick entirely rather than let a
      // second set of invokes race the first. At most one tick's results are
      // ever in flight at a time, which by itself rules out any reordering.
      if (tickInFlightRef.current) return;
      tickInFlightRef.current = true;
      // round-17 P1 layer 2 — generational guard (defense in depth): claim a
      // sequence number for this tick. Layer 1 already guarantees this check
      // can never actually fail today; it's here so a stale promise can't
      // land state if someone later removes single-flight, or if this effect
      // is ever re-run while a tick from the prior mount is still pending.
      const seq = ++tickSeqRef.current;
      try {
        const [controlResult, persistResult, enabledResult] = await Promise.allSettled([
          api.getComputerControlState(),
          api.getComputerStopPersistFailed(),
          api.getComputerUseEnabled(),
        ]);
        if (!alive || seq !== tickSeqRef.current) return;
        // issue #160 round-13 P1: a transient invoke rejection must NEVER hide the
        // Stop banner / warning — that would remove the human's kill-switch surface
        // (and the WebView Escape listener) while the backend was never shown to
        // have cleared. Only a SUCCESSFUL poll updates each piece of state; a
        // rejected one keeps the last known value, so the banner stays up until a
        // real poll confirms control and the persistence warning are both gone.
        // This applies to the round-16 enabled poll too: a rejection is a no-op.
        if (controlResult.status === "fulfilled") setState(controlResult.value);
        if (persistResult.status === "fulfilled") {
          const next = persistResult.value;
          // round-15 P2: only a confirmed true -> false transition means "the
          // backend just cleared a persist failure" — clear the local flag too.
          if (lastServerStopFailedRef.current && !next) setLocalStopFailed(false);
          lastServerStopFailedRef.current = next;
          setServerStopFailed(next);
        }
        // round-16 P2: a fulfilled poll landing on enabled === true means a human
        // just re-enabled computer use from Settings, which is the same backend
        // path that clears the persist-failed flag — so it's recovery confirmation
        // on its own, independent of whether round-15's transition above ever saw
        // the intervening `true`. enabled === false clears nothing here: a
        // disabled state doesn't tell us a prior local failure was handled.
        if (enabledResult.status === "fulfilled" && enabledResult.value) {
          setLocalStopFailed(false);
          lastServerStopFailedRef.current = false;
        }
      } finally {
        tickInFlightRef.current = false;
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
