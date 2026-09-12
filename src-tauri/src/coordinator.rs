//! Consumes bus Wake events and drives the target's session to read its inbox.
//! This is the honest "push" half of bus + coordinator = near-realtime.
//!
//! Shape: **wake → `weft_scheduler::route` → [`SessionPort`]**. Skip / attach /
//! human-notify policy lives in the crate. This file observes store + engine
//! facts, then speaks to `lead_chat` (both [`DeliveryMode`]s map to
//! `TurnState::request_bus_read`). HumanPort is the Needs-you emit — not grown
//! here. `turn/steer` stays in the Codex adapter.
//!
//! Coalescing is busy-aware, not time-based: per (thread, dir) at most one
//! `deliver` runs at a time, and a wake arriving while one is in flight is
//! re-delivered the instant it finishes (no fixed delay).
//!
//! Single-writer safety is crate policy: taken-over + not live → skip.

use crate::bus::Wake;
use crate::lead_chat::engine::STATUS_STOPPED;
use crate::store::{repo, Db};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Emitter, Manager};
use weft_scheduler::{
    classify_party, delivery_mode, enqueue, enqueue_promotes_status, inflight_key, party_ref,
    route, DeliveryMode, EnqueueJob, Inflight, PartyRef, PartyRoute, SchedulerError, SessionPort,
    WakeAction, WakeFacts, STATUS_DONE, WORKER_START_FAILED,
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
                    eprintln!("[weft] set enqueue attention {direction_id}: {flag_error}");
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
            let Some(party) = classify(&w.dir) else {
                continue;
            };
            // HumanPort: crate says notify; emit stays in this adapter.
            if matches!(route(party, WakeFacts::default()), WakeAction::NotifyHuman) {
                let _ = app.emit("needs-you://changed", w.thread);
                continue;
            }
            let key = inflight_key(w.thread, &w.dir);
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
                    if let Err(e) = deliver(&app2, w.thread, party).await {
                        eprintln!(
                            "[weft][coordinator] wake {party:?}@{} failed: {e}",
                            w.thread
                        );
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

/// Observe adapter facts, then `route` → [`SessionPort`].
async fn deliver(app: &AppHandle, thread: i32, party: PartyRoute) -> anyhow::Result<()> {
    let Some(db) = app.try_state::<crate::store::Db>() else {
        return Ok(());
    };
    let db = crate::store::Db(db.0.clone(), db.1);
    let (facts, worker) = match party {
        PartyRoute::Human => (WakeFacts::default(), None),
        PartyRoute::Lead => lead_facts(app, &db, thread).await?,
        PartyRoute::Worker(dir) => worker_facts(app, &db, thread, dir).await?,
    };
    match route(party, facts) {
        WakeAction::NotifyHuman | WakeAction::Skip(_) => Ok(()),
        WakeAction::Deliver { attach } => {
            let port = WeftSessionPort {
                app: app.clone(),
                db,
                thread,
                attach,
                worker,
            };
            port.deliver(
                &party_ref(i64::from(thread), party),
                "",
                delivery_mode(facts.live),
            )
            .await
            .map(|_| ())
            .map_err(|error| anyhow::anyhow!(error))
        }
    }
}

async fn lead_facts(
    app: &AppHandle,
    db: &Db,
    thread: i32,
) -> anyhow::Result<(WakeFacts, Option<WorkerAttach>)> {
    let key = crate::lead_chat::commands::lead_key(thread);
    let taken_over = crate::store::repo::lead_status(db, thread)
        .await?
        .as_deref()
        == Some(STATUS_STOPPED);
    let live = live_resident(app, key).await;
    let resident = app
        .state::<crate::lead_chat::engine::LeadChatState>()
        .get(key)
        .is_some();
    Ok((
        WakeFacts {
            taken_over,
            live,
            resident,
            ..WakeFacts::default()
        },
        None,
    ))
}

async fn worker_facts(
    app: &AppHandle,
    db: &Db,
    thread: i32,
    dir: i64,
) -> anyhow::Result<(WakeFacts, Option<WorkerAttach>)> {
    let Ok(dir) = i32::try_from(dir) else {
        return Ok((
            WakeFacts {
                missing_target: true,
                ..WakeFacts::default()
            },
            None,
        ));
    };
    let Some(d) = crate::store::repo::get_direction(db, dir).await? else {
        return Ok((
            WakeFacts {
                missing_target: true,
                ..WakeFacts::default()
            },
            None,
        ));
    };
    if d.thread_id != thread {
        return Ok((
            WakeFacts {
                foreign_thread: true,
                ..WakeFacts::default()
            },
            None,
        ));
    }
    let Some(s) = crate::store::repo::latest_session_for_direction(db, dir).await? else {
        return Ok((
            WakeFacts {
                missing_target: true,
                ..WakeFacts::default()
            },
            None,
        ));
    };
    let live = live_resident(app, i64::from(s.id)).await;
    let resident = app
        .state::<crate::lead_chat::engine::LeadChatState>()
        .get(i64::from(s.id))
        .is_some();
    Ok((
        WakeFacts {
            taken_over: s.status == STATUS_STOPPED,
            live,
            resident,
            worker_done: d.status == STATUS_DONE,
            ..WakeFacts::default()
        },
        Some(WorkerAttach {
            dir,
            repo_id: s.repo_id,
            session_id: s.id,
        }),
    ))
}

#[derive(Clone, Copy)]
struct WorkerAttach {
    dir: i32,
    repo_id: i32,
    session_id: i32,
}

/// Weft SessionPort: both delivery modes become `nudge_bus_read`.
/// Attach (lazy `chat_open_worker_impl` / `lead_engine`) stays in this adapter.
struct WeftSessionPort {
    app: AppHandle,
    db: Db,
    thread: i32,
    attach: bool,
    worker: Option<WorkerAttach>,
}

impl SessionPort for WeftSessionPort {
    fn deliver(
        &self,
        party: &PartyRef,
        _text: &str,
        mode: DeliveryMode,
    ) -> impl std::future::Future<Output = Result<bool, SchedulerError>> + Send {
        let app = self.app.clone();
        let db = Db(self.db.0.clone(), self.db.1);
        let thread = self.thread;
        let attach = self.attach;
        let worker = self.worker;
        let party = party.clone();
        async move {
            weft_session_deliver(&app, &db, thread, attach, worker, &party, mode)
                .await
                .map_err(|error| SchedulerError::new(error.to_string()))
        }
    }
}

async fn weft_session_deliver(
    app: &AppHandle,
    db: &Db,
    thread: i32,
    attach: bool,
    worker: Option<WorkerAttach>,
    party: &PartyRef,
    mode: DeliveryMode,
) -> anyhow::Result<bool> {
    // Protocol names stay out of this adapter. Both modes are inbox-read.
    match mode {
        DeliveryMode::MergeActive | DeliveryMode::StartIdle => {}
    }
    let Some(target) = classify_party(&party.party) else {
        return Ok(false);
    };
    match target {
        PartyRoute::Human => Ok(true),
        PartyRoute::Lead => {
            let eng = crate::lead_chat::commands::lead_engine(app, db, thread, "en").await?;
            crate::lead_chat::engine::nudge_bus_read(app, db, &eng).await?;
            Ok(true)
        }
        PartyRoute::Worker(_) => {
            let Some(worker) = worker else {
                return Ok(false);
            };
            let state = app.state::<crate::lead_chat::engine::LeadChatState>();
            let eng = if attach {
                let info = crate::lead_chat::commands::chat_open_worker_impl(
                    app,
                    db,
                    worker.dir,
                    worker.repo_id,
                    "en",
                )
                .await?;
                state.get(i64::from(info.session_id))
            } else {
                state.get(i64::from(worker.session_id))
            };
            let Some(eng) = eng else {
                return Ok(false);
            };
            crate::lead_chat::engine::nudge_bus_read(app, db, &eng).await?;
            Ok(true)
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
        let k = inflight_key(7, "lead");
        assert!(f.begin(&k)); // first wake → start a loop
        assert!(!f.begin(&k)); // wake during the loop → coalesced, no new loop
        assert!(!f.begin(&k)); // another → still coalesced
        assert!(f.next(&k)); // loop end sees the coalesced wake → re-deliver
        assert!(!f.next(&k)); // nothing pending → release the key
        assert!(f.begin(&k)); // a later wake starts a fresh loop
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

    #[test]
    fn wake_policy_is_scheduler_route() {
        assert_eq!(
            route(PartyRoute::Human, WakeFacts::default()),
            WakeAction::NotifyHuman
        );
        assert_eq!(
            route(
                PartyRoute::Lead,
                WakeFacts {
                    taken_over: true,
                    live: false,
                    ..WakeFacts::default()
                }
            ),
            weft_scheduler::WakeAction::Skip(weft_scheduler::WakeSkip::TakenOver)
        );
    }
}
