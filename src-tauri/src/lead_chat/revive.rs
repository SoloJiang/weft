//! Boot-time recovery for turns that were durably recorded as `running` when
//! the app exited, plus durable hidden lead batches accepted before dispatch.
//! This module deliberately has no timer, idle inference, stall detector, or
//! redrive loop: persisted `running` and pending hidden rows are the only
//! recovery signals. A pending plan decision is explicit authorization to
//! revive an idle/stopped lead; repo-only feedback never wakes a stopped lead.

use std::collections::{HashMap, HashSet};
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PendingLeadTarget {
    thread_id: i32,
    has_plan_decision: bool,
}

/// Run one boot-only recovery pass. Running sessions are selected from durable
/// `running` state; hidden lead batches are selected from durable pending rows.
/// Both are excluded when the same engine key is already live.
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

/// Select durable hidden lead batches that were accepted before a process
/// exit but never reached the engine. A live engine, or a lead already selected
/// for running-turn recovery, will hydrate the same batch through
/// `lead_engine`; excluding those targets here prevents a second startup task
/// from constructing or dispatching the same engine concurrently.
async fn collect_pending_lead_targets(
    db: &Db,
    live: &HashSet<i64>,
    revived_leads: &HashSet<i32>,
) -> anyhow::Result<Vec<PendingLeadTarget>> {
    let mut targets: Vec<PendingLeadTarget> = Vec::new();
    let mut indexes: HashMap<i32, usize> = HashMap::new();
    for row in repo::list_pending_lead_hidden_deliveries(db, None).await? {
        if live.contains(&lead_key(row.thread_id)) || revived_leads.contains(&row.thread_id) {
            continue;
        }
        if let Some(index) = indexes.get(&row.thread_id).copied() {
            targets[index].has_plan_decision |= row.source_kind == "plan_decision";
            continue;
        }
        indexes.insert(row.thread_id, targets.len());
        targets.push(PendingLeadTarget {
            thread_id: row.thread_id,
            has_plan_decision: row.source_kind == "plan_decision",
        });
    }
    Ok(targets)
}

async fn sweep(app: &AppHandle) -> anyhow::Result<()> {
    let Some(db) = app.try_state::<Db>() else {
        return Ok(());
    };
    let db = Db(db.0.clone(), db.1);
    // Materialize any journal-backed repo feedback before taking the live
    // engine snapshot. Otherwise a later plan row could be recovered first,
    // while the older repo row is still waiting for the separate startup
    // feedback task to create its durable hidden delivery.
    if let Err(error) = crate::commands::restore_pending_repo_action_feedback_once(&db, None).await
    {
        eprintln!("[weft][revive] repository feedback startup pass failed: {error}");
    }
    let live: HashSet<i64> = {
        let state = app.state::<LeadChatState>();
        state.0.iter().map(|entry| *entry.key()).collect()
    };
    let (leads, workers) = collect_targets(&db, &live).await?;
    let revived_leads = leads.iter().copied().collect::<HashSet<_>>();
    let pending_leads = collect_pending_lead_targets(&db, &live, &revived_leads).await?;
    if leads.is_empty() && workers.is_empty() && pending_leads.is_empty() {
        return Ok(());
    }

    eprintln!(
        "[weft][revive] reviving {} worker(s), {} lead(s), {} pending hidden lead batch(es)",
        workers.len(),
        leads.len(),
        pending_leads.len()
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
    for target in pending_leads {
        let app = app.clone();
        let semaphore = semaphore.clone();
        handles.push(tauri::async_runtime::spawn(async move {
            let Ok(_permit) = semaphore.acquire().await else {
                return;
            };
            revive_pending_lead(&app, target).await;
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

async fn revive_pending_lead(app: &AppHandle, target: PendingLeadTarget) {
    let Some(db) = app.try_state::<Db>() else {
        return;
    };
    let db = Db(db.0.clone(), db.1);
    if let Err(error) = try_revive_pending_lead(app, &db, target).await {
        eprintln!(
            "[weft][revive] pending hidden lead batch @{} failed: {error}",
            target.thread_id
        );
    }
}

async fn try_revive_pending_lead(
    app: &AppHandle,
    db: &Db,
    target: PendingLeadTarget,
) -> anyhow::Result<bool> {
    // A plan decision is explicit persisted user authorization and may revive
    // an idle/stopped lead through the existing ordered batch admission. A
    // repo-only batch has no such authorization: never wake a stopped lead in
    // the background merely because feedback is pending.
    let status = repo::lead_status(db, target.thread_id).await?;
    if !pending_lead_can_start(status.as_deref(), target.has_plan_decision) {
        return Ok(false);
    }

    // `lead_engine` performs the one ordered durable-batch admission before it
    // returns, including the plan-authorized stopped-lead revive. Keeping this
    // call as the sole startup path avoids constructing another engine or
    // dispatching the same rows a second time.
    let _ = lead_engine(app, db, target.thread_id, "en").await?;
    Ok(true)
}

fn pending_lead_can_start(status: Option<&str>, has_plan_decision: bool) -> bool {
    has_plan_decision || status != Some("stopped")
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
    use sea_orm::{ActiveModelTrait, Set};

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

    #[tokio::test]
    async fn disk_restart_recovers_one_plan_batch_in_row_id_order() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("revive-plan.sqlite");
        let url = format!("sqlite://{}?mode=rwc", path.display());
        let db = Db::connect(&url).await.unwrap();
        let workspace = repo::create_workspace(&db, "restart-plan").await.unwrap();
        let thread = repo::create_thread(&db, workspace.id, "issue", "feature", "claude")
            .await
            .unwrap();
        repo::set_lead_status(&db, thread.id, "stopped")
            .await
            .unwrap();
        let repo_ref = repo::add_repo_ref(
            &db,
            workspace.id,
            "repo",
            "/tmp/revive-repo",
            "main",
            "",
            true,
        )
        .await
        .unwrap();
        let execution = crate::store::entities::repo_action_execution::ActiveModel {
            id: Set(41),
            workspace_id: Set(workspace.id),
            thread_id: Set(thread.id),
            message_id: Set(0),
            action_id: Set("repo-action-41".to_string()),
            action_kind: Set("add".to_string()),
            invocation_fingerprint: Set("restart-fingerprint-41".to_string()),
            execution_token: Set("restart-token-41".to_string()),
            status: Set(repo::REPO_ACTION_COMPLETED.to_string()),
            target_path: Set(repo_ref.local_git_path.clone()),
            staging_path: Set(String::new()),
            repo_id: Set(repo_ref.id),
            repo_name: Set(repo_ref.name.clone()),
            feedback_state: Set(repo::REPO_ACTION_FEEDBACK_PENDING.to_string()),
            feedback_payload: Set(
                r#"{"tool":"repo_action","execution_id":41,"status":"ok"}"#
                    .to_string(),
            ),
            cleanup_preserve_target: Set(false),
            created_at: Set(String::new()),
            updated_at: Set(String::new()),
        }
        .insert(&db.0)
        .await
        .unwrap();
        let older = repo::enqueue_lead_hidden_delivery(
            &db,
            thread.id,
            "repo_action",
            execution.id,
            "repo_action:41",
            r#"{"tool":"repo_action","execution_id":41,"status":"ok"}"#,
        )
        .await
        .unwrap();
        let card = repo::insert_lead_message(
            &db,
            thread.id,
            None,
            1,
            "assistant",
            "plan_card",
            r#"{"title":"Ship it","requirements":[],"approach":"","split":[],"risks":[]}"#,
            "complete",
        )
        .await
        .unwrap();
        let (resolved_card, newer) = repo::enqueue_plan_decision_and_resolve(
            &db,
            thread.id,
            card.id,
            false,
        )
        .await
        .unwrap()
        .unwrap();
        assert!(resolved_card.content.contains("resolved"));
        drop(db);

        // Reopen the same file to model a fresh process. The transaction that
        // accepted the plan already committed, while no engine receipt exists.
        let db = Db::connect(&url).await.unwrap();
        let targets = collect_pending_lead_targets(&db, &HashSet::new(), &HashSet::new())
            .await
            .unwrap();

        assert_eq!(
            targets,
            vec![PendingLeadTarget {
                thread_id: thread.id,
                has_plan_decision: true,
            }]
        );
        let rows = repo::list_pending_lead_hidden_deliveries(&db, Some(thread.id))
            .await
            .unwrap();
        assert_eq!(
            rows.iter().map(|row| row.id).collect::<Vec<_>>(),
            vec![older.id, newer.id],
            "startup recovery must hand the whole batch to ordered admission"
        );
        let reloaded_card = repo::get_lead_message(&db, card.id).await.unwrap().unwrap();
        assert!(
            reloaded_card.content.contains("resolved"),
            "the committed plan transaction must leave no actionable card to replay"
        );
        assert!(
            pending_lead_can_start(Some("stopped"), true),
            "the persisted plan decision is explicit authorization to revive"
        );

        // Model the engine's first-activity receipt after startup admission:
        // both rows settle durably, and a later restart no longer selects the
        // batch again.
        repo::consume_lead_hidden_delivery(&db, older.id)
            .await
            .unwrap();
        repo::consume_lead_hidden_delivery(&db, newer.id)
            .await
            .unwrap();
        assert!(
            repo::list_pending_lead_hidden_deliveries(&db, Some(thread.id))
                .await
                .unwrap()
                .is_empty(),
            "receipt completion must clear the recovered batch"
        );
        assert!(
            collect_pending_lead_targets(&db, &HashSet::new(), &HashSet::new())
                .await
                .unwrap()
                .is_empty(),
            "consumed rows must not be dispatched again on a later restart"
        );
    }

    #[tokio::test]
    async fn disk_restart_defers_repo_only_batch_for_stopped_lead() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("revive-repo-only.sqlite");
        let url = format!("sqlite://{}?mode=rwc", path.display());
        let db = Db::connect(&url).await.unwrap();
        let workspace = repo::create_workspace(&db, "restart-repo-only").await.unwrap();
        let thread = repo::create_thread(&db, workspace.id, "issue", "feature", "claude")
            .await
            .unwrap();
        repo::set_lead_status(&db, thread.id, "stopped")
            .await
            .unwrap();
        repo::enqueue_lead_hidden_delivery(
            &db,
            thread.id,
            "repo_action",
            51,
            "repo_action:51",
            r#"{"tool":"repo_action","execution_id":51,"status":"ok"}"#,
        )
        .await
        .unwrap();
        drop(db);

        let db = Db::connect(&url).await.unwrap();
        let targets = collect_pending_lead_targets(&db, &HashSet::new(), &HashSet::new())
            .await
            .unwrap();
        assert_eq!(targets.len(), 1);
        assert!(!targets[0].has_plan_decision);
        assert!(
            !pending_lead_can_start(Some("stopped"), targets[0].has_plan_decision),
            "repo feedback alone must not wake a stopped lead during startup"
        );
        assert_eq!(
            repo::list_pending_lead_hidden_deliveries(&db, Some(thread.id))
                .await
                .unwrap()
                .len(),
            1,
            "defer must retain the durable row for a later explicit recovery"
        );
    }
}
