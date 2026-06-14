//! Boot-time recovery: re-attach and continue the worker/lead turns that were
//! interrupted by a hard app exit (orphaned `running` activity status). Idle work
//! is left for lazy-attach on open. See the worker-restart-recovery spec.

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

/// One-shot boot sweep on the async runtime.
pub fn spawn_revive(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        if let Err(e) = sweep(&app).await {
            eprintln!("[weft][revive] sweep error: {e}");
        }
    });
}

/// DB-driven selection of interrupted sessions (orphaned `running`), excluding any
/// key already live in `live`. Pure over (DB, live-set) → unit-testable.
async fn collect_targets(
    db: &Db,
    live: &HashSet<i64>,
) -> anyhow::Result<(Vec<i32>, Vec<WorkerTarget>)> {
    let mut leads = Vec::new();
    let mut workers = Vec::new();
    for ws in repo::list_workspaces(db).await? {
        for th in repo::list_threads(db, ws.id).await? {
            if !live.contains(&lead_key(th.id))
                && repo::lead_native_id(db, th.id).await?.is_some()
                && repo::lead_status(db, th.id).await?.as_deref() == Some("running")
            {
                leads.push(th.id);
            }
            for dir in repo::list_directions(db, th.id).await? {
                if dir.status == "done" {
                    continue;
                }
                let Some(sess) = repo::latest_session_for_direction(db, dir.id).await? else {
                    continue;
                };
                if sess.native_session_id.is_none() || sess.status != "running" {
                    continue;
                }
                if live.contains(&(sess.id as i64)) {
                    continue;
                }
                workers.push(WorkerTarget {
                    direction_id: dir.id,
                    repo_id: sess.repo_id,
                    thread_id: th.id,
                    session_id: sess.id,
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
        let st = app.state::<LeadChatState>();
        let g = st.0.lock().unwrap_or_else(|e| e.into_inner());
        g.keys().copied().collect()
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
    let sem = Arc::new(Semaphore::new(MAX_CONCURRENT));
    let mut handles = Vec::new();
    for tid in leads {
        let (app, sem) = (app.clone(), sem.clone());
        handles.push(tauri::async_runtime::spawn(async move {
            let _permit = sem.acquire().await;
            revive_lead(&app, tid).await;
        }));
    }
    for w in workers {
        let (app, sem) = (app.clone(), sem.clone());
        handles.push(tauri::async_runtime::spawn(async move {
            let _permit = sem.acquire().await;
            revive_worker(&app, w).await;
        }));
    }
    for h in handles {
        let _ = h.await;
    }
    // A worker revived AFTER the frontend store mounted isn't caught by its
    // mount-time pull, and a nudge-driven turn emits no busy push the listener
    // could react to. Tell the frontend to re-pull live workers. Payload-less.
    if revived_workers > 0 {
        let _ = app.emit("worker-revived", ());
    }
    Ok(())
}

async fn revive_worker(app: &AppHandle, w: WorkerTarget) {
    let Some(db) = app.try_state::<Db>() else {
        return;
    };
    let db = Db(db.0.clone(), db.1);
    if let Err(e) = try_revive_worker(app, &db, w).await {
        report_failure(app, w.thread_id, &w.direction_id.to_string(), &e);
    }
}

async fn try_revive_worker(app: &AppHandle, db: &Db, w: WorkerTarget) -> anyhow::Result<()> {
    repo::mark_incomplete_turns_interrupted(db, w.thread_id, Some(w.session_id)).await?;
    // Re-open the EXACT repo the selected running session belongs to (the session
    // row carries `repo_id`). Resolving via the direction's worktree could pick a
    // different repo if a direction ever has multiple worktree rows, opening the
    // wrong session and leaving the interrupted one unrecovered.
    chat_open_worker_impl(app, db, w.direction_id, w.repo_id, "en").await?;
    if has_open_ask(app, &w.direction_id.to_string(), w.thread_id) {
        return Ok(());
    }
    nudge_if_idle(app, db, w.session_id as i64).await?;
    Ok(())
}

async fn revive_lead(app: &AppHandle, thread_id: i32) {
    let Some(db) = app.try_state::<Db>() else {
        return;
    };
    let db = Db(db.0.clone(), db.1);
    if let Err(e) = try_revive_lead(app, &db, thread_id).await {
        report_failure(app, thread_id, "lead", &e);
    }
}

async fn try_revive_lead(app: &AppHandle, db: &Db, thread_id: i32) -> anyhow::Result<()> {
    repo::mark_incomplete_turns_interrupted(db, thread_id, None).await?;
    let eng = lead_engine(app, db, thread_id, "en").await?;
    engine::ensure_running(app, db, &eng).await?;
    if has_open_ask(app, "lead", thread_id) {
        return Ok(());
    }
    nudge_eng_if_idle(app, db, &eng).await?;
    Ok(())
}

/// Mirror of the watchdog's gate (engine.rs): an ask owned by this direction
/// (workers key by direction id; the lead's asks carry an empty dir) means the
/// turn is legitimately blocked on a human — don't nudge it forward.
fn has_open_ask(app: &AppHandle, dir: &str, thread_id: i32) -> bool {
    app.try_state::<crate::ask::AskRegistry>()
        .map(|a| {
            a.open().iter().any(|k| {
                // Worker asks carry the direction id (globally unique). Lead asks
                // use an empty dir, so scope them to this thread or one lead's ask
                // would suppress the continue nudge for every other lead.
                k.dir == dir || (dir == "lead" && k.dir.is_empty() && k.thread == thread_id)
            })
        })
        .unwrap_or(false)
}

async fn nudge_if_idle(app: &AppHandle, db: &Db, key: i64) -> anyhow::Result<()> {
    if let Some(eng) = app.state::<LeadChatState>().get(key) {
        nudge_eng_if_idle(app, db, &eng).await?;
    }
    Ok(())
}

async fn nudge_eng_if_idle(app: &AppHandle, db: &Db, eng: &EngineRef) -> anyhow::Result<()> {
    let busy = { eng.lock().await.turn.busy };
    if !busy {
        // Propagate failure (e.g. the CLI/app-server can't start at boot) so the
        // caller's report_failure surfaces it in Needs-you instead of marking the
        // session silently revived while its interrupted work stays stuck. nudge can
        // fail AFTER already marking the turn busy + running (a per-turn CLI that
        // fails to spawn), so reset the engine to idle first — otherwise later user
        // sends queue behind a turn that never emits TurnEnd.
        if let Err(e) = engine::nudge(app, db, eng, REVIVE_PROMPT).await {
            engine::stop(app, eng).await;
            return Err(e);
        }
    }
    Ok(())
}

fn report_failure(app: &AppHandle, thread_id: i32, dir: &str, e: &anyhow::Error) {
    if let Some(bus) = app.try_state::<crate::bus::BusRegistry>() {
        bus.ask_human(thread_id, dir, &format!("未能恢复：{e}"));
    }
    eprintln!("[weft][revive] {dir}@{thread_id} failed: {e}");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{repo, Db};

    // In-memory DB with migrations, matching repo.rs's own test harness.
    async fn mem() -> Db {
        Db::connect("sqlite::memory:").await.unwrap()
    }

    // A workspace + a repo_ref (needed by create_direction) + an issue thread.
    // The repo's git path is fake; create_direction's branch picker shells out to
    // git and falls back gracefully on a missing repo, so no real worktree needed.
    async fn fixture(db: &Db) -> (i32, i32) {
        let ws = repo::create_workspace(db, "ws").await.unwrap();
        let repo_ref = repo::add_repo_ref(db, ws.id, "r", "/tmp/weft-revive-fake", "main")
            .await
            .unwrap();
        let th = repo::create_thread(db, ws.id, "issue", "feature", "codex")
            .await
            .unwrap();
        (th.id, repo_ref.id)
    }

    async fn mk_direction(db: &Db, thread_id: i32, repo_id: i32, name: &str) -> i32 {
        repo::create_direction(db, thread_id, name, "codex", repo_id, "why", "impl-only")
            .await
            .unwrap()
            .id
    }

    // A worker session shaped like a crash-interrupted one: a captured native id
    // and status == "running".
    async fn running_session(db: &Db, direction_id: i32, repo_id: i32) -> i32 {
        let s = repo::create_session(db, direction_id, repo_id, "codex", "/tmp/wt")
            .await
            .unwrap();
        // A live, mid-turn session: native id captured + status running (the
        // honest turn-start state we crash out of).
        repo::set_session_native_id(db, s.id, "nat-123")
            .await
            .unwrap();
        repo::set_session_status(db, s.id, "running").await.unwrap();
        s.id
    }

    fn empty() -> HashSet<i64> {
        HashSet::new()
    }

    /// THE leftover-session shape: a non-done direction whose latest session has a
    /// native id and status=="running", even though every message row is complete.
    /// Proves selection keys on session.status, not on message rows.
    #[tokio::test]
    async fn selects_running_worker_with_only_complete_messages() {
        let db = mem().await;
        let (th, repo_id) = fixture(&db).await;
        let dir = mk_direction(&db, th, repo_id, "alpha").await;
        let sess = running_session(&db, dir, repo_id).await;
        // A complete (NOT streaming) assistant row — mark_incomplete would skip it.
        repo::insert_lead_message(
            &db,
            th,
            Some(sess),
            1,
            "assistant",
            "text",
            r#"{"text":"done"}"#,
            "complete",
        )
        .await
        .unwrap();

        let (leads, workers) = collect_targets(&db, &empty()).await.unwrap();
        assert!(leads.is_empty());
        assert_eq!(
            workers,
            vec![WorkerTarget {
                direction_id: dir,
                repo_id,
                thread_id: th,
                session_id: sess,
            }]
        );
    }

    /// A done direction is excluded even with a running session under it.
    #[tokio::test]
    async fn excludes_done_direction() {
        let db = mem().await;
        let (th, repo_id) = fixture(&db).await;
        let dir = mk_direction(&db, th, repo_id, "alpha").await;
        running_session(&db, dir, repo_id).await;
        repo::set_direction_status(&db, dir, "done").await.unwrap();

        let (_, workers) = collect_targets(&db, &empty()).await.unwrap();
        assert!(workers.is_empty());
    }

    /// status=="idle" (drained cleanly) is excluded.
    #[tokio::test]
    async fn excludes_idle_session() {
        let db = mem().await;
        let (th, repo_id) = fixture(&db).await;
        let dir = mk_direction(&db, th, repo_id, "alpha").await;
        let sess = running_session(&db, dir, repo_id).await;
        repo::set_session_status(&db, sess, "idle").await.unwrap();

        let (_, workers) = collect_targets(&db, &empty()).await.unwrap();
        assert!(workers.is_empty());
    }

    /// No native id (never started a turn) is excluded even if status says running.
    #[tokio::test]
    async fn excludes_session_without_native_id() {
        let db = mem().await;
        let (th, repo_id) = fixture(&db).await;
        let dir = mk_direction(&db, th, repo_id, "alpha").await;
        let s = repo::create_session(&db, dir, repo_id, "codex", "/tmp/wt")
            .await
            .unwrap();
        // running status, but no native id captured.
        repo::set_session_status(&db, s.id, "running").await.unwrap();

        let (_, workers) = collect_targets(&db, &empty()).await.unwrap();
        assert!(workers.is_empty());
    }

    /// A direction with no session at all is excluded.
    #[tokio::test]
    async fn excludes_direction_without_session() {
        let db = mem().await;
        let (th, repo_id) = fixture(&db).await;
        mk_direction(&db, th, repo_id, "alpha").await;

        let (_, workers) = collect_targets(&db, &empty()).await.unwrap();
        assert!(workers.is_empty());
    }

    /// A live worker (session id already in `live`) is excluded → idempotent reboot.
    #[tokio::test]
    async fn excludes_live_worker() {
        let db = mem().await;
        let (th, repo_id) = fixture(&db).await;
        let dir = mk_direction(&db, th, repo_id, "alpha").await;
        let sess = running_session(&db, dir, repo_id).await;

        let mut live = HashSet::new();
        live.insert(sess as i64);
        let (_, workers) = collect_targets(&db, &live).await.unwrap();
        assert!(workers.is_empty());
    }

    /// The lead is selected only with a native id AND lead_status=="running".
    #[tokio::test]
    async fn selects_running_lead() {
        let db = mem().await;
        let (th, _repo_id) = fixture(&db).await;
        repo::set_lead_native_id(&db, th, "lead-nat").await.unwrap();
        repo::set_lead_status(&db, th, "running").await.unwrap();

        let (leads, workers) = collect_targets(&db, &empty()).await.unwrap();
        assert_eq!(leads, vec![th]);
        assert!(workers.is_empty());
    }

    /// An idle lead (status != running) is excluded even with a native id.
    #[tokio::test]
    async fn excludes_idle_lead() {
        let db = mem().await;
        let (th, _repo_id) = fixture(&db).await;
        repo::set_lead_native_id(&db, th, "lead-nat").await.unwrap();
        repo::set_lead_status(&db, th, "idle").await.unwrap();

        let (leads, _) = collect_targets(&db, &empty()).await.unwrap();
        assert!(leads.is_empty());
    }

    /// A running lead with no native id (never streamed) is excluded.
    #[tokio::test]
    async fn excludes_lead_without_native_id() {
        let db = mem().await;
        let (th, _repo_id) = fixture(&db).await;
        repo::set_lead_status(&db, th, "running").await.unwrap();

        let (leads, _) = collect_targets(&db, &empty()).await.unwrap();
        assert!(leads.is_empty());
    }

    /// A live lead (lead_key already in `live`) is excluded → idempotent reboot.
    #[tokio::test]
    async fn excludes_live_lead() {
        let db = mem().await;
        let (th, _repo_id) = fixture(&db).await;
        repo::set_lead_native_id(&db, th, "lead-nat").await.unwrap();
        repo::set_lead_status(&db, th, "running").await.unwrap();

        let mut live = HashSet::new();
        live.insert(lead_key(th));
        let (leads, _) = collect_targets(&db, &live).await.unwrap();
        assert!(leads.is_empty());
    }

    /// Failure reporting surfaces in 「待你处理」: posting via the bus's ask_human
    /// projects an open ask carrying the dir (as `from`) and the message.
    #[tokio::test]
    async fn report_failure_surfaces_open_ask() {
        let bus = crate::bus::BusRegistry::new();
        bus.ask_human(7, "lead", "未能恢复：boom");
        let open = bus.open_asks(7);
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].from, "lead");
        assert!(open[0].text.contains("未能恢复"));
    }
}
