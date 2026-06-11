//! Keep-awake: hold a system-level "prevent idle sleep" assertion while any
//! agent session is busy (Settings-controlled). Display sleep stays allowed.
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

/// Pure decision state: should the assertion be held right now?
struct PowerState {
    /// The "prevent sleep while running" setting (re-pushed on every launch).
    enabled: bool,
    /// Any engine busy as of the last event/sweep; held through the linger.
    busy: bool,
    /// When a sweep first saw all engines idle (linger anchor).
    idle_since: Option<Instant>,
}

impl Default for PowerState {
    fn default() -> Self {
        // Default ON, matching the frontend default ("weft-keep-awake" !== "0").
        Self { enabled: true, busy: false, idle_since: None }
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

    fn desired(&self) -> bool {
        self.enabled && self.busy
    }
}

/// Spawn the thread that owns the OS assertion handle. Send `true`/`false` to
/// acquire/release (repeats are no-ops). The thread exits when every sender is
/// dropped, releasing any held assertion with it.
fn spawn_holder() -> std::sync::mpsc::Sender<bool> {
    let (tx, rx) = std::sync::mpsc::channel::<bool>();
    let spawned = std::thread::Builder::new()
        .name("weft-power-holder".into())
        .spawn(move || {
            // Created and dropped on this thread only (Windows thread affinity).
            let mut held: Option<keepawake::KeepAwake> = None;
            while let Ok(want) = rx.recv() {
                if want && held.is_none() {
                    match keepawake::Builder::default()
                        .idle(true) // PreventUserIdleSystemSleep: 熄屏不受影响
                        .reason("Weft: agent session running")
                        .app_name("Weft")
                        .app_reverse_domain("com.weft.app")
                        .create()
                    {
                        Ok(h) => {
                            held = Some(h);
                            eprintln!("[weft] keep-awake: assertion acquired");
                        }
                        // Best-effort by design: keep-awake failing must never
                        // affect the sessions themselves.
                        Err(e) => eprintln!("[weft] keep-awake: acquire failed: {e}"),
                    }
                } else if !want && held.is_some() {
                    held = None;
                    eprintln!("[weft] keep-awake: assertion released");
                }
            }
        });
    if let Err(e) = spawned {
        eprintln!("[weft] keep-awake: holder thread failed to start: {e}");
    }
    tx
}

/// Managed Tauri state. The Settings command flips `enabled`; the chat engine
/// reports turn starts; the 30s sweep loop reconciles and expires the linger.
pub struct PowerGuard {
    state: std::sync::Mutex<PowerState>,
    tx: std::sync::mpsc::Sender<bool>,
}

impl Default for PowerGuard {
    fn default() -> Self {
        Self { state: std::sync::Mutex::new(PowerState::default()), tx: spawn_holder() }
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
}

/// Event hook for the chat engine: a turn just began (instant acquire).
pub fn on_turn_began(app: &tauri::AppHandle) {
    use tauri::Manager as _;
    if let Some(guard) = app.try_state::<PowerGuard>() {
        guard.note_busy();
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
        assert!(!st.desired());
    }

    #[test]
    fn busy_desires_hold_when_enabled() {
        assert!(busy_state().desired());
        assert!(!PowerState::default().desired());
    }

    #[test]
    fn idle_lingers_then_releases() {
        let mut st = busy_state();
        let t0 = Instant::now();
        st.sweep(false, t0);
        assert!(st.desired(), "still held during linger");
        st.sweep(false, t0 + LINGER - Duration::from_secs(1));
        assert!(st.desired(), "still held just before expiry");
        st.sweep(false, t0 + LINGER);
        assert!(!st.desired(), "released after linger");
    }

    #[test]
    fn busy_during_linger_restarts_anchor() {
        let mut st = busy_state();
        let t0 = Instant::now();
        st.sweep(false, t0);
        st.sweep(true, t0 + Duration::from_secs(30)); // busy again mid-linger
        let t1 = t0 + LINGER + Duration::from_secs(10);
        st.sweep(false, t1); // new linger anchored at t1
        assert!(st.desired(), "linger restarted by the busy sweep");
        st.sweep(false, t1 + LINGER);
        assert!(!st.desired());
    }

    #[test]
    fn holder_thread_tolerates_rapid_toggles() {
        let tx = spawn_holder();
        for on in [true, true, false, true, false, false] {
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
        guard.set_enabled(false);
        guard.set_enabled(true);
    }
}
