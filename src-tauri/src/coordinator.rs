//! Consumes bus Wake events and drives the target's session to read its inbox.
//! This is the honest "push" half of bus + coordinator = near-realtime.
//!
//! Three wake targets: the human (`"you"` → refresh the Needs-you UI), the
//! thread lead (`"lead"` → drive the lead engine), and a worker (a numeric
//! direction id → drive that worker's engine, lazily attaching it if idle).
//!
//! Coalescing is busy-aware, not time-based: per (thread, dir) at most one
//! `deliver` runs at a time, and a wake arriving while one is in flight is
//! re-delivered the instant it finishes (no fixed delay). The engine itself
//! collapses wakes that land mid-turn into a SINGLE inbox-read fired exactly at
//! turn-end (`TurnState::request_bus_read`), so a busy agent reads new messages
//! the moment it frees up — never on a timer, never one redundant turn per post.
//!
//! Single-writer safety: a session the human has taken over in their terminal is
//! persisted `STATUS_STOPPED`; a wake never spawns a competing headless process
//! for it. A cleanly-idle session is still driven (its message goes through).

use crate::bus::{Wake, HUMAN, LEAD};
use crate::lead_chat::engine::STATUS_STOPPED;
use std::collections::HashSet;
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Emitter, Manager};

/// Where a wake's direction routes. The bus identity is either the human, the
/// thread lead, or a worker (its direction id). Anything else is ignored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Route {
    Human,
    Lead,
    Worker(i32),
}

/// Pure classifier over a wake's `dir` string — unit-testable without a runtime.
fn classify(dir: &str) -> Option<Route> {
    if dir == HUMAN {
        Some(Route::Human)
    } else if dir == LEAD {
        Some(Route::Lead)
    } else {
        dir.parse::<i32>().ok().map(Route::Worker)
    }
}

/// Per-key (`"{thread}/{dir}"`) serialization. At most one delivery runs per key
/// at a time so concurrent wakes can't race the lazy-attach; a wake that lands
/// while one is in flight is remembered and re-delivered the instant it ends.
#[derive(Default)]
struct Inflight {
    running: HashSet<String>,
    dirty: HashSet<String>,
}

impl Inflight {
    /// Register a wake. Returns true if the caller should start a delivery loop;
    /// false means one is already running and this wake was coalesced into it.
    fn begin(&mut self, key: &str) -> bool {
        if self.running.contains(key) {
            self.dirty.insert(key.to_string());
            false
        } else {
            self.running.insert(key.to_string());
            true
        }
    }

    /// After one delivery completes. Returns true if a wake coalesced during it
    /// (re-deliver now); false releases the key so the next wake starts fresh.
    fn next(&mut self, key: &str) -> bool {
        if self.dirty.remove(key) {
            true
        } else {
            self.running.remove(key);
            false
        }
    }
}

/// Run the coordinator loop on a dedicated OS thread (the mpsc Receiver is
/// blocking).
pub fn run(app: AppHandle, rx: Receiver<Wake>) {
    std::thread::spawn(move || {
        let inflight: Arc<Mutex<Inflight>> = Arc::new(Mutex::new(Inflight::default()));
        while let Ok(w) = rx.recv() {
            let Some(route) = classify(&w.dir) else {
                continue;
            };
            // A wake addressed to the human means an agent asked a question:
            // nudge the UI to refresh its Needs-you surface, don't touch an
            // engine.
            if route == Route::Human {
                let _ = app.emit("needs-you://changed", w.thread);
                continue;
            }
            // The lead's dir ("lead") repeats across threads, so the thread must
            // be part of the key.
            let key = format!("{}/{}", w.thread, w.dir);
            let start = {
                let mut g = inflight.lock().unwrap_or_else(|e| e.into_inner());
                g.begin(&key)
            };
            if !start {
                continue;
            }
            let app2 = app.clone();
            let inflight2 = inflight.clone();
            tauri::async_runtime::spawn(async move {
                loop {
                    if let Err(e) = deliver(&app2, w.thread, route).await {
                        eprintln!("[weft][coordinator] wake {route:?}@{} failed: {e}", w.thread);
                    }
                    let again = {
                        let mut g = inflight2.lock().unwrap_or_else(|e| e.into_inner());
                        g.next(&key)
                    };
                    if !again {
                        break;
                    }
                }
            });
        }
    });
}

/// True iff `key`'s engine is resident AND its child process is alive — i.e.
/// weft already owns the single-writer slot, so a nudge reuses it instead of
/// spawning. A taken-over session is resident-but-dead-child (or not resident),
/// so this is false for it.
async fn live_resident(app: &AppHandle, key: i64) -> bool {
    let Some(eng) = app
        .state::<crate::lead_chat::engine::LeadChatState>()
        .get(key)
    else {
        return false;
    };
    let mut inner = eng.lock().await;
    inner
        .child
        .as_mut()
        .is_some_and(|c| matches!(c.try_wait(), Ok(None)))
}

/// Deliver a bus-wake to the routed engine via `nudge_bus_read`, which coalesces
/// wakes that land mid-turn into one inbox-read at turn-end. An idle/not-yet-
/// resident worker is lazily attached so a bus post still drives it. A session
/// taken over in the user's terminal (`STATUS_STOPPED`, not currently live under
/// weft) is skipped so we never spawn a competing headless process.
async fn deliver(app: &AppHandle, thread: i32, route: Route) -> anyhow::Result<()> {
    let Some(db) = app.try_state::<crate::store::Db>() else {
        return Ok(());
    };
    let db = crate::store::Db(db.0.clone(), db.1);
    match route {
        // Handled inline in run(); never reaches deliver().
        Route::Human => Ok(()),
        Route::Lead => {
            let key = crate::lead_chat::commands::lead_key(thread);
            let taken_over = crate::store::repo::lead_status(&db, thread).await?.as_deref()
                == Some(STATUS_STOPPED);
            if taken_over && !live_resident(app, key).await {
                return Ok(());
            }
            // Get-or-create the lead engine, then drive it to read its inbox.
            let eng = crate::lead_chat::commands::lead_engine(app, &db, thread, "en").await?;
            crate::lead_chat::engine::nudge_bus_read(app, &db, &eng).await
        }
        Route::Worker(dir) => {
            // Direction ids are global, but a wake belongs to the thread it was
            // posted on. A post to a foreign dir (an id that lives in another
            // thread) must not drive that unrelated worker — it would read its
            // own thread's inbox, not where the message actually landed.
            let Some(d) = crate::store::repo::get_direction(&db, dir).await? else {
                return Ok(());
            };
            if d.thread_id != thread {
                return Ok(());
            }
            let Some(s) = crate::store::repo::latest_session_for_direction(&db, dir).await? else {
                return Ok(());
            };
            let live = live_resident(app, s.id as i64).await;
            // Taken over in the user's terminal: never spawn a competing process.
            // A live resident process means weft re-owns it, so drive on.
            if !live && s.status == STATUS_STOPPED {
                return Ok(());
            }
            let state = app.state::<crate::lead_chat::engine::LeadChatState>();
            let eng = match state.get(s.id as i64) {
                Some(e) => e,
                None => {
                    // Not resident: lazily open the worker so an idle/closed
                    // worker can still be driven by a bus post. Never resurrect a
                    // finished direction — a stray message must not restart it.
                    if d.status == "done" {
                        return Ok(());
                    }
                    let info = crate::lead_chat::commands::chat_open_worker_impl(
                        app, &db, dir, s.repo_id, "en",
                    )
                    .await?;
                    match state.get(info.session_id as i64) {
                        Some(e) => e,
                        None => return Ok(()),
                    }
                }
            };
            crate::lead_chat::engine::nudge_bus_read(app, &db, &eng).await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_routes_each_bus_identity() {
        assert_eq!(classify("you"), Some(Route::Human));
        assert_eq!(classify("lead"), Some(Route::Lead));
        assert_eq!(classify("10"), Some(Route::Worker(10)));
        assert_eq!(classify("1"), Some(Route::Worker(1)));
    }

    #[test]
    fn classify_ignores_unknown_identities() {
        // Non-numeric, non-sentinel targets (e.g. a typo or a future identity)
        // are dropped rather than misrouted to a worker.
        assert_eq!(classify(""), None);
        assert_eq!(classify("leader"), None);
        assert_eq!(classify("worker-3"), None);
    }

    // One delivery per key at a time; a wake arriving mid-delivery is coalesced
    // and re-delivered immediately when it ends, then the key releases.
    #[test]
    fn inflight_serializes_and_coalesces() {
        let mut f = Inflight::default();
        let k = "7/lead";
        assert!(f.begin(k)); // first wake → start a loop
        assert!(!f.begin(k)); // wake during the loop → coalesced, no new loop
        assert!(!f.begin(k)); // another → still coalesced
        assert!(f.next(k)); // loop end sees the coalesced wake → re-deliver
        assert!(!f.next(k)); // nothing pending → release the key
        assert!(f.begin(k)); // a later wake starts a fresh loop
    }
}
