//! Keep-awake: hold a system-level "prevent idle sleep" assertion while any
//! agent session is busy (Settings-controlled) or IM remote standby is on.
//! Display sleep stays allowed.
//! Spec: docs/superpowers/specs/2026-06-11-keep-awake-remote-standby-design.md
//!
//! Two parts: `PowerState` is the pure decision logic (unit-tested); the
//! holder thread owns the OS handle, because keepawake's Windows backend is
//! thread-bound (`SetThreadExecutionState(ES_CONTINUOUS)`) — the handle must
//! be created AND dropped on the same thread.

use std::time::{Duration, Instant};

/// Hold the assertion for this long after the last session went idle, so
/// back-to-back turns (queued sends, coordinator nudge bursts) don't flap it.
const LINGER: Duration = Duration::from_secs(60);

/// Assertion reasons, visible in `pmset -g assertions` / `powercfg /requests`.
const REASON_RUNNING: &str = "Weft: agent session running";
const REASON_STANDBY: &str = "Weft: remote standby (IM)";

/// Pure decision state: should the assertion be held right now?
struct PowerState {
    /// The "prevent sleep while running" setting (re-pushed on every launch).
    enabled: bool,
    /// Any engine busy as of the last event/sweep; held through the linger.
    busy: bool,
    /// When a sweep first saw all engines idle (linger anchor).
    idle_since: Option<Instant>,
    /// IM remote standby: hold while the bridge is enabled so remote commands
    /// always reach an awake machine. Independent of the run toggle.
    standby: bool,
}

impl Default for PowerState {
    fn default() -> Self {
        // Default ON, matching the frontend default ("weft-keep-awake" !== "0").
        Self {
            enabled: true,
            busy: false,
            idle_since: None,
            standby: false,
        }
    }
}

impl PowerState {
    /// A turn just began somewhere: hold immediately.
    fn note_busy(&mut self) {
        self.busy = true;
        self.idle_since = None;
    }

    /// Periodic reconciliation with ground truth + linger expiry.
    fn sweep(&mut self, any_busy: bool, now: Instant) {
        if any_busy {
            self.note_busy();
        } else if self.busy {
            let since = *self.idle_since.get_or_insert(now);
            if now.duration_since(since) >= LINGER {
                self.busy = false;
                self.idle_since = None;
            }
        }
    }

    /// Which assertion to hold right now (None = release). The session-running
    /// reason wins when both apply — it's the more specific cause.
    fn desired(&self) -> Option<&'static str> {
        if self.enabled && self.busy {
            Some(REASON_RUNNING)
        } else if self.standby {
            Some(REASON_STANDBY)
        } else {
            None
        }
    }
}

/// Spawn the thread that owns the OS assertion handle. Send `Some(reason)` to
/// acquire (re-created if the reason changes), `None` to release; repeats are
/// no-ops. The thread exits when every sender is dropped, releasing any held
/// assertion with it.
fn spawn_holder() -> std::sync::mpsc::Sender<Option<&'static str>> {
    let (tx, rx) = std::sync::mpsc::channel::<Option<&'static str>>();
    let spawned = std::thread::Builder::new()
        .name("weft-power-holder".into())
        .spawn(move || {
            // Created and dropped on this thread only (Windows thread affinity).
            let mut held: Option<(&'static str, keepawake::KeepAwake)> = None;
            while let Ok(want) = rx.recv() {
                if held.as_ref().map(|(r, _)| *r) == want {
                    continue; // already in the desired state
                }
                if held.take().is_some() {
                    // release first (reason change or plain release)
                    eprintln!("[weft] keep-awake: assertion released");
                }
                let Some(reason) = want else { continue };
                match keepawake::Builder::default()
                    .idle(true) // PreventUserIdleSystemSleep: 熄屏不受影响
                    .reason(reason)
                    .app_name("Weft")
                    .app_reverse_domain("com.weft.app")
                    .create()
                {
                    Ok(h) => {
                        held = Some((reason, h));
                        eprintln!("[weft] keep-awake: assertion acquired ({reason})");
                    }
                    // Best-effort by design: keep-awake failing must never
                    // affect the sessions themselves.
                    Err(e) => eprintln!("[weft] keep-awake: acquire failed: {e}"),
                }
            }
        });
    if let Err(e) = spawned {
        eprintln!("[weft] keep-awake: holder thread failed to start: {e}");
    }
    tx
}

/// Managed Tauri state. The Settings command flips `enabled`; the chat engine
/// reports turn starts; the 30s sweep loop reconciles and expires the linger;
/// the IM bridge flips `standby`.
pub struct PowerGuard {
    state: std::sync::Mutex<PowerState>,
    tx: std::sync::mpsc::Sender<Option<&'static str>>,
}

impl Default for PowerGuard {
    fn default() -> Self {
        Self {
            state: std::sync::Mutex::new(PowerState::default()),
            tx: spawn_holder(),
        }
    }
}

impl PowerGuard {
    /// The Settings toggle (re-pushed from localStorage on every launch).
    pub fn set_enabled(&self, on: bool) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        st.enabled = on;
        let _ = self.tx.send(st.desired());
    }

    /// A turn just began: hold immediately instead of waiting for a sweep.
    pub fn note_busy(&self) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        st.note_busy();
        let _ = self.tx.send(st.desired());
    }

    /// Periodic reconciliation: ground truth from the engine registry.
    pub fn sweep(&self, any_busy: bool) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        st.sweep(any_busy, Instant::now());
        let _ = self.tx.send(st.desired());
    }

    /// IM remote standby (phase 2): hold while the bridge is enabled. No
    /// linger — intent flips are explicit, not turn-boundary noise.
    pub fn set_standby(&self, on: bool) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        st.standby = on;
        let _ = self.tx.send(st.desired());
    }
}

/// Event hook for the chat engine: a turn just began (instant acquire).
pub fn on_turn_began(app: &tauri::AppHandle) {
    use tauri::Manager as _;
    if let Some(guard) = app.try_state::<PowerGuard>() {
        guard.note_busy();
    }
}

/// IM bridge hook: remote standby intent changed (bridge enable/disable and
/// credential transitions all converge through here).
pub fn set_standby(app: &tauri::AppHandle, on: bool) {
    use tauri::Manager as _;
    if let Some(guard) = app.try_state::<PowerGuard>() {
        guard.set_standby(on);
    }
}

/// Every 30s: is any live engine's turn busy? Feeds `PowerGuard::sweep`, which
/// also expires the linger. The event hooks give instant acquire; this loop is
/// the release path and the safety net — a crashed engine can't leak the
/// assertion past one interval + linger. Deliberately NOT piggybacked on
/// `spawn_watchdog`: that loop short-circuits when both guardrail caps are 0.
pub fn spawn_sweep(app: tauri::AppHandle) {
    use tauri::Manager as _;
    tauri::async_runtime::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(30)).await;
            let engines: Vec<crate::lead_chat::engine::EngineRef> = {
                let state = app.state::<crate::lead_chat::engine::LeadChatState>();
                let g = state.0.lock().unwrap_or_else(|e| e.into_inner());
                g.values().cloned().collect()
            };
            let mut any_busy = false;
            for eng in engines {
                if eng.lock().await.turn.busy {
                    any_busy = true;
                    break;
                }
            }
            if let Some(guard) = app.try_state::<PowerGuard>() {
                guard.sweep(any_busy);
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn busy_state() -> PowerState {
        let mut st = PowerState::default();
        st.note_busy();
        st
    }

    #[test]
    fn disabled_never_desires_hold() {
        let mut st = busy_state();
        st.enabled = false;
        assert!(st.desired().is_none());
    }

    #[test]
    fn busy_desires_hold_when_enabled() {
        assert_eq!(busy_state().desired(), Some(REASON_RUNNING));
        assert!(PowerState::default().desired().is_none());
    }

    #[test]
    fn idle_lingers_then_releases() {
        let mut st = busy_state();
        let t0 = Instant::now();
        st.sweep(false, t0);
        assert!(st.desired().is_some(), "still held during linger");
        st.sweep(false, t0 + LINGER - Duration::from_secs(1));
        assert!(st.desired().is_some(), "still held just before expiry");
        st.sweep(false, t0 + LINGER);
        assert!(st.desired().is_none(), "released after linger");
    }

    #[test]
    fn busy_during_linger_restarts_anchor() {
        let mut st = busy_state();
        let t0 = Instant::now();
        st.sweep(false, t0);
        st.sweep(true, t0 + Duration::from_secs(30)); // busy again mid-linger
        let t1 = t0 + LINGER + Duration::from_secs(10);
        st.sweep(false, t1); // new linger anchored at t1
        assert!(st.desired().is_some(), "linger restarted by the busy sweep");
        st.sweep(false, t1 + LINGER);
        assert!(st.desired().is_none());
    }

    #[test]
    fn standby_holds_independent_of_run_toggle() {
        let mut st = PowerState::default();
        st.enabled = false; // 关掉「运行时防休眠」也不影响远程待命
        st.standby = true;
        assert_eq!(st.desired(), Some(REASON_STANDBY));
        st.standby = false;
        assert_eq!(st.desired(), None, "standby 关闭立即释放，无 linger");
    }

    #[test]
    fn running_reason_wins_over_standby() {
        let mut st = busy_state();
        st.standby = true;
        assert_eq!(st.desired(), Some(REASON_RUNNING));
    }

    #[test]
    fn sweep_never_clears_standby() {
        let mut st = PowerState::default();
        st.standby = true;
        let t0 = Instant::now();
        st.sweep(false, t0);
        st.sweep(false, t0 + LINGER + LINGER);
        assert_eq!(st.desired(), Some(REASON_STANDBY));
    }

    #[test]
    fn holder_thread_tolerates_rapid_toggles() {
        let tx = spawn_holder();
        for on in [
            Some(REASON_RUNNING),
            Some(REASON_RUNNING),
            Some(REASON_STANDBY), // reason 切换：释放旧断言 + 持新 reason
            None,
            Some(REASON_STANDBY),
            None,
        ] {
            tx.send(on).expect("holder thread alive");
        }
        drop(tx); // thread exits, releasing anything held
    }

    #[test]
    fn guard_end_to_end_does_not_panic() {
        let guard = PowerGuard::default();
        guard.note_busy();
        guard.sweep(true);
        guard.sweep(false);
        guard.set_standby(true);
        guard.set_standby(false);
        guard.set_enabled(false);
        guard.set_enabled(true);
    }
}
