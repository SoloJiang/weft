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

use crate::bus::Wake;
use crate::lead_chat::engine::STATUS_STOPPED;
use crate::store::{repo, Db};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Emitter, Manager};
use weft_scheduler::{
    classify_party, enqueue, enqueue_promotes_status, EnqueueJob, Inflight, PartyRoute,
    WORKER_START_FAILED,
};

fn classify(dir: &str) -> Option<PartyRoute> {
    classify_party(dir)
}

/// Persist the shared enqueue after the human gate. Does not spawn sessions —
/// the command layer starts workers via SessionPort (`chat_open_worker_impl`).
pub(crate) async fn persist_enqueued(
    db: &Db,
    direction_ids: &[i32],
) -> anyhow::Result<Vec<EnqueueJob>> {
    let jobs = enqueue(direction_ids.iter().copied().map(i64::from));
    for job in &jobs {
        let direction_id = i32::try_from(job.direction_id)
            .map_err(|_| anyhow::anyhow!("direction id out of i32 range"))?;
        let Some(direction) = repo::get_direction(db, direction_id).await? else {
            continue;
        };
        if enqueue_promotes_status(&direction.status) {
            repo::set_direction_status(db, direction_id, job.status).await?;
        }
    }
    Ok(jobs)
}

/// Persist enqueue after the human gate. On failure, stamp
/// [`WORKER_START_FAILED`] so the UI can map it via attention-reason.
pub(crate) async fn persist_enqueued_or_flag(
    db: &Db,
    direction_ids: &[i32],
) -> anyhow::Result<Vec<EnqueueJob>> {
    match persist_enqueued(db, direction_ids).await {
        Ok(jobs) => Ok(jobs),
        Err(error) => {
            for &direction_id in direction_ids {
                if let Err(flag_error) =
                    repo::set_direction_attention(db, direction_id, Some(WORKER_START_FAILED)).await
                {
                    eprintln!(
                        "[weft] set enqueue attention {direction_id}: {flag_error}"
                    );
                }
            }
            Err(error)
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
            if route == PartyRoute::Human {
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
async fn deliver(app: &AppHandle, thread: i32, route: PartyRoute) -> anyhow::Result<()> {
    let Some(db) = app.try_state::<crate::store::Db>() else {
        return Ok(());
    };
    let db = crate::store::Db(db.0.clone(), db.1);
    match route {
        // Handled inline in run(); never reaches deliver().
        PartyRoute::Human => Ok(()),
        PartyRoute::Lead => {
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
        PartyRoute::Worker(dir) => {
            let Ok(dir) = i32::try_from(dir) else {
                return Ok(());
            };
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
        assert_eq!(classify("you"), Some(PartyRoute::Human));
        assert_eq!(classify("lead"), Some(PartyRoute::Lead));
        assert_eq!(classify("10"), Some(PartyRoute::Worker(10)));
        assert_eq!(classify("1"), Some(PartyRoute::Worker(1)));
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

    #[test]
    fn enqueue_after_gate_is_the_shared_scheduler() {
        let jobs = enqueue([4, 4, 0]);
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].direction_id, 4);
        assert_eq!(jobs[0].status, weft_scheduler::initial_status());
        assert!(enqueue_promotes_status("queued"));
        assert!(!enqueue_promotes_status("review"));
    }
}
