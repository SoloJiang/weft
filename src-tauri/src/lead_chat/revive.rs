//! Boot-time recovery for turns that were durably recorded as `running` when
//! the app exited. This module deliberately has no timer, idle inference, stall
//! detector, or redrive loop: persisted `running` is the only recovery signal.

use std::collections::HashSet;
use std::sync::Arc;

use tauri::{AppHandle, Emitter, Manager};
use tokio::sync::Semaphore;

use crate::lead_chat::commands::{chat_open_worker_impl, lead_engine, lead_key};
use crate::lead_chat::engine::{self, EngineRef, LeadChatState};
use crate::store::{repo, Db};

const REVIVE_PROMPT: &str =
    "Your previous run was interrupted before it finished. Continue from where you left off.";
const MAX_CONCURRENT: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WorkerTarget {
    direction_id: i32,
    repo_id: i32,
    thread_id: i32,
    session_id: i32,
}

/// Run one boot-only recovery pass. A target is selected only from durable
/// `running` state and is excluded when the same engine key is already live.
pub fn spawn_revive(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        if let Err(err) = sweep(&app).await {
            eprintln!("[weft][revive] sweep error: {err}");
        }
    });
}

/// Select crash-interrupted sessions. Idle, stopped, exited, review, queued,
/// and historical `stalled` values are intentionally ignored.
async fn collect_targets(
    db: &Db,
    live: &HashSet<i64>,
) -> anyhow::Result<(Vec<i32>, Vec<WorkerTarget>)> {
    let mut leads = Vec::new();
    let mut workers = Vec::new();
    for workspace in repo::list_workspaces(db).await? {
        for thread in repo::list_threads(db, workspace.id).await? {
            if !live.contains(&lead_key(thread.id))
                && repo::lead_native_id(db, thread.id).await?.is_some()
                && repo::lead_status(db, thread.id).await?.as_deref() == Some("running")
            {
                leads.push(thread.id);
            }
            for direction in repo::list_directions(db, thread.id).await? {
                if direction.status == "done" {
                    continue;
                }
                let Some(session) = repo::latest_session_for_direction(db, direction.id).await?
                else {
                    continue;
                };
                if session.native_session_id.is_none() || session.status != "running" {
                    continue;
                }
                if live.contains(&(session.id as i64)) {
                    continue;
                }
                workers.push(WorkerTarget {
                    direction_id: direction.id,
                    repo_id: session.repo_id,
                    thread_id: thread.id,
                    session_id: session.id,
                });
            }
        }
    }
    Ok((leads, workers))
}

async fn sweep(app: &AppHandle) -> anyhow::Result<()> {
    let Some(db) = app.try_state::<Db>() else {
        return Ok(());
    };
    let db = Db(db.0.clone(), db.1);
    let live: HashSet<i64> = {
        let state = app.state::<LeadChatState>();
        state.0.iter().map(|entry| *entry.key()).collect()
    };
    let (leads, workers) = collect_targets(&db, &live).await?;
    if leads.is_empty() && workers.is_empty() {
        return Ok(());
    }

    eprintln!(
        "[weft][revive] reviving {} worker(s), {} lead(s)",
        workers.len(),
        leads.len()
    );
    let revived_workers = workers.len();
    let semaphore = Arc::new(Semaphore::new(MAX_CONCURRENT));
    let mut handles = Vec::new();
    for thread_id in leads {
        let app = app.clone();
        let semaphore = semaphore.clone();
        handles.push(tauri::async_runtime::spawn(async move {
            let Ok(_permit) = semaphore.acquire().await else {
                return;
            };
            revive_lead(&app, thread_id).await;
        }));
    }
    for worker in workers {
        let app = app.clone();
        let semaphore = semaphore.clone();
        handles.push(tauri::async_runtime::spawn(async move {
            let Ok(_permit) = semaphore.acquire().await else {
                return;
            };
            revive_worker(&app, worker).await;
        }));
    }
    for handle in handles {
        let _ = handle.await;
    }
    if revived_workers > 0 {
        let _ = app.emit("worker-revived", ());
    }
    Ok(())
}

async fn revive_worker(app: &AppHandle, worker: WorkerTarget) {
    let Some(db) = app.try_state::<Db>() else {
        return;
    };
    let db = Db(db.0.clone(), db.1);
    if let Err(err) = try_revive_worker(app, &db, worker).await {
        eprintln!(
            "[weft][revive] worker {}@{} failed: {err}",
            worker.direction_id, worker.thread_id
        );
    }
}

async fn try_revive_worker(
    app: &AppHandle,
    db: &Db,
    worker: WorkerTarget,
) -> anyhow::Result<()> {
    repo::mark_incomplete_turns_interrupted(db, worker.thread_id, Some(worker.session_id)).await?;
    let _ = repo::fail_queued(db, worker.thread_id, Some(worker.session_id)).await?;
    chat_open_worker_impl(app, db, worker.direction_id, worker.repo_id, "en").await?;
    if has_blocking_permission(app, &worker.direction_id.to_string(), worker.thread_id) {
        return Ok(());
    }
    nudge_if_idle(app, db, worker.session_id as i64).await
}

async fn revive_lead(app: &AppHandle, thread_id: i32) {
    let Some(db) = app.try_state::<Db>() else {
        return;
    };
    let db = Db(db.0.clone(), db.1);
    if let Err(err) = try_revive_lead(app, &db, thread_id).await {
        eprintln!("[weft][revive] lead@{thread_id} failed: {err}");
    }
}

async fn try_revive_lead(app: &AppHandle, db: &Db, thread_id: i32) -> anyhow::Result<()> {
    repo::mark_incomplete_turns_interrupted(db, thread_id, None).await?;
    let _ = repo::fail_queued(db, thread_id, None).await?;
    let engine_ref = lead_engine(app, db, thread_id, "en").await?;
    engine::ensure_running(app, db, &engine_ref).await?;
    if has_blocking_permission(app, "lead", thread_id) {
        return Ok(());
    }
    nudge_engine_if_idle(app, db, &engine_ref).await
}

/// Permission requests are ephemeral and cannot survive app restart. This
/// race guard only covers an ask created after boot recovery has started.
fn has_blocking_permission(app: &AppHandle, direction: &str, thread_id: i32) -> bool {
    app.try_state::<crate::ask::AskRegistry>()
        .map(|registry| {
            registry.open().iter().any(|ask| {
                ask.thread == thread_id
                    && (ask.dir == direction || (direction == "lead" && ask.dir.is_empty()))
            })
        })
        .unwrap_or(false)
}

async fn nudge_if_idle(app: &AppHandle, db: &Db, key: i64) -> anyhow::Result<()> {
    if let Some(engine_ref) = app.state::<LeadChatState>().get(key) {
        nudge_engine_if_idle(app, db, &engine_ref).await?;
    }
    Ok(())
}

async fn nudge_engine_if_idle(
    app: &AppHandle,
    db: &Db,
    engine_ref: &EngineRef,
) -> anyhow::Result<()> {
    if engine_ref.lock().await.turn.busy {
        return Ok(());
    }
    if let Err(err) = engine::nudge(app, db, engine_ref, REVIVE_PROMPT).await {
        engine::stop(app, engine_ref).await;
        return Err(err);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn fixture(db: &Db) -> (i32, i32, i32) {
        let workspace = repo::create_workspace(db, "ws").await.unwrap();
        let repo_ref = repo::add_repo_ref(
            db,
            workspace.id,
            "repo",
            "/tmp/weft-revive",
            "main",
            "",
            true,
        )
        .await
        .unwrap();
        let thread = repo::create_thread(db, workspace.id, "issue", "feature", "codex")
            .await
            .unwrap();
        let direction = repo::create_direction(
            db,
            thread.id,
            "task",
            "codex",
            repo_ref.id,
            "why",
            "impl-only",
            "",
        )
        .await
        .unwrap();
        let session = repo::create_session(
            db,
            direction.id,
            repo_ref.id,
            "codex",
            "/tmp/weft-revive-wt",
        )
        .await
        .unwrap();
        (thread.id, direction.id, session.id)
    }

    #[tokio::test]
    async fn only_persisted_running_targets_are_revived() {
        let db = Db::connect("sqlite::memory:").await.unwrap();
        let (thread_id, _direction_id, session_id) = fixture(&db).await;
        repo::set_lead_native_id(&db, thread_id, "lead-native")
            .await
            .unwrap();
        repo::set_lead_status(&db, thread_id, "running")
            .await
            .unwrap();
        repo::set_session_native_id(&db, session_id, "worker-native")
            .await
            .unwrap();
        repo::set_session_status(&db, session_id, "running")
            .await
            .unwrap();

        let (leads, workers) = collect_targets(&db, &HashSet::new()).await.unwrap();

        assert_eq!(leads, vec![thread_id]);
        assert_eq!(workers.len(), 1);
        assert_eq!(workers[0].session_id, session_id);
    }

    #[tokio::test]
    async fn idle_incomplete_work_is_not_redriven() {
        let db = Db::connect("sqlite::memory:").await.unwrap();
        let (thread_id, direction_id, session_id) = fixture(&db).await;
        repo::set_direction_status(&db, direction_id, "working")
            .await
            .unwrap();
        repo::set_lead_native_id(&db, thread_id, "lead-native")
            .await
            .unwrap();
        repo::set_lead_status(&db, thread_id, "idle").await.unwrap();
        repo::set_session_native_id(&db, session_id, "worker-native")
            .await
            .unwrap();
        repo::set_session_status(&db, session_id, "idle")
            .await
            .unwrap();

        let (leads, workers) = collect_targets(&db, &HashSet::new()).await.unwrap();

        assert!(leads.is_empty());
        assert!(workers.is_empty());
    }

    #[tokio::test]
    async fn legacy_stalled_status_is_ignored_without_error() {
        let db = Db::connect("sqlite::memory:").await.unwrap();
        let (thread_id, _direction_id, session_id) = fixture(&db).await;
        repo::set_lead_native_id(&db, thread_id, "lead-native")
            .await
            .unwrap();
        repo::set_lead_status(&db, thread_id, "stalled")
            .await
            .unwrap();
        repo::set_session_native_id(&db, session_id, "worker-native")
            .await
            .unwrap();
        repo::set_session_status(&db, session_id, "stalled")
            .await
            .unwrap();

        let (leads, workers) = collect_targets(&db, &HashSet::new()).await.unwrap();

        assert!(leads.is_empty());
        assert!(workers.is_empty());
    }

    #[tokio::test]
    async fn live_engine_keys_are_not_revived_twice() {
        let db = Db::connect("sqlite::memory:").await.unwrap();
        let (thread_id, _direction_id, session_id) = fixture(&db).await;
        repo::set_lead_native_id(&db, thread_id, "lead-native")
            .await
            .unwrap();
        repo::set_lead_status(&db, thread_id, "running")
            .await
            .unwrap();
        repo::set_session_native_id(&db, session_id, "worker-native")
            .await
            .unwrap();
        repo::set_session_status(&db, session_id, "running")
            .await
            .unwrap();
        let live = HashSet::from([lead_key(thread_id), session_id as i64]);

        let (leads, workers) = collect_targets(&db, &live).await.unwrap();

        assert!(leads.is_empty());
        assert!(workers.is_empty());
    }
}
