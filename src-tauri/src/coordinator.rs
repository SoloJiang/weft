//! Consumes bus Wake events and drives the target's live session to read its
//! inbox. Rate-limited per (thread, direction). A busy engine queues the nudge
//! for after the current turn rather than fragile idle detection — this is the
//! honest "push" half of bus + coordinator = near-realtime.
//!
//! Three wake targets: the human (`"you"` → refresh the Needs-you UI), the
//! thread lead (`"lead"` → drive the lead engine), and a worker (a numeric
//! direction id → drive that worker's engine, lazily attaching it if idle).

use crate::bus::{Wake, HUMAN, LEAD};
use std::collections::HashMap;
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

/// Run the coordinator loop on a dedicated OS thread (the mpsc Receiver is
/// blocking).
pub fn run(app: AppHandle, rx: Receiver<Wake>) {
    std::thread::spawn(move || {
        let mut last: HashMap<String, Instant> = HashMap::new();
        while let Ok(w) = rx.recv() {
            let Some(route) = classify(&w.dir) else {
                continue;
            };
            // A wake addressed to the human means an agent asked a question:
            // nudge the UI to refresh its Needs-you surface, don't touch an
            // engine. Not rate-limited — it's a cheap UI event.
            if route == Route::Human {
                let _ = app.emit("needs-you://changed", w.thread);
                continue;
            }
            // Rate-limit per (thread, dir): a burst of posts to the same target
            // shouldn't spam the agent, since one wake drains the whole inbox.
            // The lead's dir ("lead") repeats across threads, so the thread must
            // be part of the key.
            let key = format!("{}/{}", w.thread, w.dir);
            let now = Instant::now();
            if let Some(t) = last.get(&key) {
                if now.duration_since(*t) < RATE_LIMIT {
                    continue; // rate-limited: don't spam the agent
                }
            }
            last.insert(key, now);
            let app2 = app.clone();
            tauri::async_runtime::spawn(async move {
                if let Err(e) = deliver(&app2, w.thread, route).await {
                    eprintln!("[weft][coordinator] wake {route:?}@{} failed: {e}", w.thread);
                }
            });
        }
    });
}

/// Deliver an invisible bus-wake nudge to the routed engine. A busy engine
/// queues it for after the current turn; an idle/not-yet-resident worker is
/// lazily attached so it can still be driven (mirrors the boot revive path).
async fn deliver(app: &AppHandle, thread: i32, route: Route) -> anyhow::Result<()> {
    let Some(db) = app.try_state::<crate::store::Db>() else {
        return Ok(());
    };
    let db = crate::store::Db(db.0.clone(), db.1);
    match route {
        // Handled inline in run(); never reaches deliver().
        Route::Human => Ok(()),
        Route::Lead => {
            // Get-or-create the lead engine for this thread, then nudge it to read
            // its inbox. nudge() spawns/resumes the process via ensure_running and
            // starts a turn if idle, or queues behind the current one.
            let eng = crate::lead_chat::commands::lead_engine(app, &db, thread, "en").await?;
            crate::lead_chat::engine::nudge(app, &db, &eng, WAKE_PROMPT).await
        }
        Route::Worker(dir) => {
            let Some(s) = crate::store::repo::latest_session_for_direction(&db, dir).await? else {
                return Ok(());
            };
            let eng = match app
                .state::<crate::lead_chat::engine::LeadChatState>()
                .get(s.id as i64)
            {
                Some(e) => Some(e),
                None => {
                    // Not resident: lazily open the worker so an idle/closed
                    // worker can still be driven by a bus post. Never resurrect a
                    // finished direction — a stray message must not restart it.
                    let done = crate::store::repo::get_direction(&db, dir)
                        .await?
                        .map(|d| d.status == "done")
                        .unwrap_or(true);
                    if done {
                        None
                    } else {
                        let info = crate::lead_chat::commands::chat_open_worker_impl(
                            app, &db, dir, s.repo_id, "en",
                        )
                        .await?;
                        app.state::<crate::lead_chat::engine::LeadChatState>()
                            .get(info.session_id as i64)
                    }
                }
            };
            if let Some(eng) = eng {
                crate::lead_chat::engine::nudge(app, &db, &eng, WAKE_PROMPT).await?;
            }
            Ok(())
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
}
