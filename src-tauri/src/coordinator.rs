//! Consumes bus Wake events and drives the target's session to read its inbox.
//! A busy engine queues the nudge for after the current turn rather than fragile
//! idle detection — this is the honest "push" half of bus + coordinator =
//! near-realtime.
//!
//! Three wake targets: the human (`"you"` → refresh the Needs-you UI), the
//! thread lead (`"lead"` → drive the lead engine), and a worker (a numeric
//! direction id → drive that worker's engine, lazily attaching it if idle).
//!
//! Per (thread, dir) the delivery is debounced: a leading-edge nudge fires
//! immediately and any wake arriving inside the window is COALESCED into a
//! single trailing nudge, never dropped (one `bus_inbox` call drains the whole
//! inbox, so one trailing nudge suffices for any number of coalesced posts).
//!
//! Single-writer safety: a session the human has taken over in their terminal is
//! persisted `STATUS_STOPPED`; a wake never spawns a competing headless process
//! for it. A cleanly-idle session is still driven (its message goes through).

use crate::bus::{Wake, HUMAN, LEAD};
use crate::lead_chat::engine::STATUS_STOPPED;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager};

const WAKE_PROMPT: &str =
    "You have new messages on the thread bus. Call the bus_inbox tool to read them.";
const RATE_LIMIT: Duration = Duration::from_secs(8);

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

/// Leading+trailing debounce state, keyed by `"{thread}/{dir}"`.
#[derive(Default)]
struct Debounce {
    last: HashMap<String, Instant>,
    pending: HashSet<String>,
}

/// Run the coordinator loop on a dedicated OS thread (the mpsc Receiver is
/// blocking).
pub fn run(app: AppHandle, rx: Receiver<Wake>) {
    std::thread::spawn(move || {
        let dq: Arc<Mutex<Debounce>> = Arc::new(Mutex::new(Debounce::default()));
        while let Ok(w) = rx.recv() {
            let Some(route) = classify(&w.dir) else {
                continue;
            };
            // A wake addressed to the human means an agent asked a question:
            // nudge the UI to refresh its Needs-you surface, don't touch an
            // engine. Not debounced — it's a cheap UI event.
            if route == Route::Human {
                let _ = app.emit("needs-you://changed", w.thread);
                continue;
            }
            // The lead's dir ("lead") repeats across threads, so the thread must
            // be part of the key.
            let key = format!("{}/{}", w.thread, w.dir);
            // Leading-edge fire; a wake inside the window is remembered and
            // delivered once on the trailing edge instead of being dropped.
            let fire = {
                let mut g = dq.lock().unwrap_or_else(|e| e.into_inner());
                match g.last.get(&key) {
                    Some(t) if t.elapsed() < RATE_LIMIT => {
                        g.pending.insert(key.clone());
                        false
                    }
                    _ => {
                        g.last.insert(key.clone(), Instant::now());
                        true
                    }
                }
            };
            if !fire {
                continue;
            }
            let app2 = app.clone();
            let dq2 = dq.clone();
            tauri::async_runtime::spawn(async move {
                loop {
                    if let Err(e) = deliver(&app2, w.thread, route).await {
                        eprintln!("[weft][coordinator] wake {route:?}@{} failed: {e}", w.thread);
                    }
                    tokio::time::sleep(RATE_LIMIT).await;
                    // Trailing edge: deliver once more if a wake coalesced during
                    // the window; otherwise release the key so the next wake fires
                    // immediately.
                    let again = {
                        let mut g = dq2.lock().unwrap_or_else(|e| e.into_inner());
                        if g.pending.remove(&key) {
                            g.last.insert(key.clone(), Instant::now());
                            true
                        } else {
                            g.last.remove(&key);
                            false
                        }
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

/// Deliver an invisible bus-wake nudge to the routed engine. A busy engine
/// queues it for after the current turn; an idle/not-yet-resident worker is
/// lazily attached so a bus post still drives it. A session taken over in the
/// user's terminal (`STATUS_STOPPED`, not currently live under weft) is skipped
/// so we never spawn a competing headless process.
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
            // Get-or-create the lead engine, then nudge it to read its inbox.
            // nudge() spawns/resumes the process via ensure_running and starts a
            // turn if idle, or queues behind the current one.
            let eng = crate::lead_chat::commands::lead_engine(app, &db, thread, "en").await?;
            crate::lead_chat::engine::nudge(app, &db, &eng, WAKE_PROMPT).await
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
            crate::lead_chat::engine::nudge(app, &db, &eng, WAKE_PROMPT).await
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

    // The debounce coalesces a wake arriving inside the window into a single
    // trailing fire (never dropped) and releases the key when idle so the next
    // wake fires immediately. This exercises that state machine without a runtime.
    #[test]
    fn debounce_coalesces_then_releases() {
        let mut dq = Debounce::default();
        let key = "7/lead".to_string();

        // First wake: leading-edge fire.
        let fire1 = matches!(dq.last.get(&key), Some(t) if t.elapsed() < RATE_LIMIT);
        assert!(!fire1, "no prior timestamp → should fire");
        dq.last.insert(key.clone(), Instant::now());

        // Second wake inside the window: coalesced (remembered), not a new fire.
        let within = matches!(dq.last.get(&key), Some(t) if t.elapsed() < RATE_LIMIT);
        assert!(within, "still inside the window");
        dq.pending.insert(key.clone());

        // Trailing edge sees the pending wake → fires once more, keeps the key.
        assert!(dq.pending.remove(&key), "trailing fire consumes the pending wake");
        dq.last.insert(key.clone(), Instant::now());

        // Next trailing edge: nothing pending → release the key.
        assert!(!dq.pending.remove(&key));
        dq.last.remove(&key);
        assert!(dq.last.get(&key).is_none(), "key released when idle");
    }
}
