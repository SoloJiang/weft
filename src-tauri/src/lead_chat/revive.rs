//! Boot-time recovery: re-attach and continue work interrupted by a hard app
//! exit — both orphaned `running` turns (cut off mid-flight) and silently-stalled
//! tasks (a lead that finished its turn cleanly yet still owns an in-progress task
//! whose worker drained to idle without delivering). Cleanly-idle work with no
//! in-progress task is left for lazy-attach on open. See the worker-restart spec.

use std::collections::HashSet;
use std::sync::Arc;

use tauri::{AppHandle, Emitter, Manager};
use tokio::sync::Semaphore;

use crate::lead_chat::commands::{chat_open_worker_impl, lead_engine, lead_key};
use crate::lead_chat::engine::{self, EngineRef, LeadChatState};
use crate::store::{repo, Db};

const REVIVE_PROMPT: &str =
    "Your previous run was interrupted before it finished. Continue from where you left off.";
/// Base of the stalled-resume nudge for an idle lead whose in-progress task
/// stalled after a clean restart (its worker drained to idle without delivering).
/// Unlike [`REVIVE_PROMPT`] the lead's OWN turn wasn't cut off — so this asks it
/// to re-examine task state and RESUME DRIVING, not "continue where it left off".
/// The "keep driving … not just nudged once" framing matters: a single poke would
/// move the task one step before it stalls again; the lead must re-enter its
/// normal orchestration loop (worker reports back on the bus → lead dispatches the
/// next step) and run it to delivery. [`resume_stalled_prompt`] appends the exact
/// stalled worker bus ids the lead must re-dispatch.
const RESUME_STALLED_PROMPT: &str = "The app just restarted. One or more of your in-progress tasks has a worker that went idle without finishing — its in-flight instruction was likely lost on restart. Resume driving these tasks the way you normally would: check each one's current state and latest output, then re-dispatch its worker with the next concrete step, and keep driving the normal loop — read the worker's replies on the bus as they arrive and dispatch the next step — until the task is actually delivered, not just nudged once. Don't repeat work that's already done.";
const MAX_CONCURRENT: usize = 4;

/// The stalled-resume instruction for a lead, naming the exact worker bus ids
/// (= direction ids) that went idle without delivering. Without these the lead —
/// which has no worker message to read a `from` id off (the stall means the worker
/// never posted) — cannot reliably address the idle workers to re-dispatch them
/// (see the lead's bus instructions in commands.rs).
fn resume_stalled_prompt(stalled_dirs: &[i32]) -> String {
    let ids = stalled_dirs
        .iter()
        .map(|d| d.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    format!("{RESUME_STALLED_PROMPT} The workers that went idle are on the thread bus under these ids: {ids}. bus_post each id directly (a direct bus_post reaches an idle worker) to re-dispatch it.")
}

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

/// Silent-stall selection: threads whose lead finished its own turn cleanly
/// (idle, so NOT caught by the `running`-only [`collect_targets`]) yet still own
/// an in-progress task whose worker has gone idle without delivering. Nothing is
/// driving such a thread forward, so on boot we re-drive its lead to re-dispatch.
/// Returns `(thread_id, stalled direction ids)` per stalled thread — the ids are
/// carried into the resume prompt so the lead can address the idle workers
/// directly (it has no worker bus message to read a `from` id off). Pure over DB
/// → unit-testable. Not filtered by a live-set: a resident-but-idle lead (e.g.
/// the frontend opened it for slash discovery) still needs the prompt — idempotency
/// is handled at drive time by [`nudge_eng_if_idle`]'s busy check, not by exclusion.
///
/// Authorization guard ("don't restart a task waiting on a human"): a BLOCKING
/// permission ask holds the tool call open, keeping the turn busy → persisted
/// "running", so the idle predicate excludes it by construction (the interrupted
/// path owns it). A NON-blocking `ask_human` that already ended its turn idle IS
/// re-driven — intended, not a leak: that question lived only in the in-memory
/// AskRegistry (cleared on restart), so re-driving the lead re-surfaces it
/// instead of stalling forever on an answer that can no longer arrive.
async fn collect_stalled_leads(db: &Db) -> anyhow::Result<Vec<(i32, Vec<i32>)>> {
    let mut stalled = Vec::new();
    for ws in repo::list_workspaces(db).await? {
        for th in repo::list_threads(db, ws.id).await? {
            // The lead must have orchestration context to resume from (a native
            // id) and be legitimately idle — NOT "running" (an interrupted turn,
            // already handled by collect_targets) and NOT "stopped" (taken over
            // in the user's terminal).
            if repo::lead_native_id(db, th.id).await?.is_none() {
                continue;
            }
            if repo::lead_status(db, th.id).await?.as_deref() != Some("idle") {
                continue;
            }
            let dirs = stalled_direction_ids(db, th.id).await?;
            if !dirs.is_empty() {
                stalled.push((th.id, dirs));
            }
        }
    }
    Ok(stalled)
}

/// The in-progress ("planning"/"working") directions of a thread whose latest
/// session finished its turn cleanly — a captured native id and status=="idle" —
/// yet the task never reached "review"/"done". "queued" (never started) and
/// "review" (awaiting the human) are legitimate rest states, not stalls; a
/// "running" session is an interrupted turn (collect_targets owns it) and
/// "stopped" is a worker taken over in the user's terminal.
async fn stalled_direction_ids(db: &Db, thread_id: i32) -> anyhow::Result<Vec<i32>> {
    let mut ids = Vec::new();
    for dir in repo::list_directions(db, thread_id).await? {
        if dir.status != "planning" && dir.status != "working" {
            continue;
        }
        let Some(sess) = repo::latest_session_for_direction(db, dir.id).await? else {
            continue;
        };
        if sess.native_session_id.is_some() && sess.status == "idle" {
            ids.push(dir.id);
        }
    }
    Ok(ids)
}

async fn sweep(app: &AppHandle) -> anyhow::Result<()> {
    let Some(db) = app.try_state::<Db>() else {
        return Ok(());
    };
    let db = Db(db.0.clone(), db.1);
    let live: HashSet<i64> = {
        let st = app.state::<LeadChatState>();
        st.0.iter().map(|r| *r.key()).collect()
    };
    let (leads, workers) = collect_targets(&db, &live).await?;
    // The silent-stall scan is a best-effort ENHANCEMENT — it must never gate the
    // primary interrupted-turn revive below. A transient failure here (e.g. a WAL
    // BUSY under the boot storm of concurrent services) degrades to "no stall
    // recovery this boot" + a log, rather than propagating and skipping the
    // running-revive dispatch entirely.
    let stalled = collect_stalled_leads(&db).await.unwrap_or_else(|e| {
        eprintln!("[weft][revive] stalled scan failed: {e}");
        Vec::new()
    });
    if leads.is_empty() && workers.is_empty() && stalled.is_empty() {
        return Ok(());
    }
    eprintln!(
        "[weft][revive] reviving {} worker(s), {} lead(s), {} stalled thread(s)",
        workers.len(),
        leads.len(),
        stalled.len(),
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
    // Silent-stall recovery: idle leads whose in-progress task's worker drained to
    // idle without delivering. Re-drive the lead (get-or-create engine → resume →
    // nudge with the stalled worker ids) exactly like the interrupted paths, so a
    // failure surfaces in Needs-you instead of silently staying stalled. Shares the
    // spawn budget; disjoint from the running-turn revive by status (idle vs running).
    for (tid, dirs) in stalled {
        let (app, sem) = (app.clone(), sem.clone());
        handles.push(tauri::async_runtime::spawn(async move {
            let _permit = sem.acquire().await;
            revive_stalled_lead(&app, tid, dirs).await;
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
    // Un-sent queued messages must not stay stuck; surface them as errors so the
    // user can see and resend them.
    let _ = repo::fail_queued(db, w.thread_id, Some(w.session_id)).await?;
    // Re-open the EXACT repo the selected running session belongs to (the session
    // row carries `repo_id`). Resolving via the direction's worktree could pick a
    // different repo if a direction ever has multiple worktree rows, opening the
    // wrong session and leaving the interrupted one unrecovered.
    chat_open_worker_impl(app, db, w.direction_id, w.repo_id, "en").await?;
    if has_open_ask(app, &w.direction_id.to_string(), w.thread_id) {
        return Ok(());
    }
    nudge_if_idle(app, db, w.session_id as i64, REVIVE_PROMPT).await?;
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
    // Un-sent queued messages must not stay stuck; surface them as errors so the
    // user can see and resend them.
    let _ = repo::fail_queued(db, thread_id, None).await?;
    let eng = lead_engine(app, db, thread_id, "en").await?;
    engine::ensure_running(app, db, &eng).await?;
    if has_open_ask(app, "lead", thread_id) {
        return Ok(());
    }
    nudge_eng_if_idle(app, db, &eng, REVIVE_PROMPT).await?;
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

async fn nudge_if_idle(app: &AppHandle, db: &Db, key: i64, prompt: &str) -> anyhow::Result<()> {
    if let Some(eng) = app.state::<LeadChatState>().get(key) {
        nudge_eng_if_idle(app, db, &eng, prompt).await?;
    }
    Ok(())
}

async fn nudge_eng_if_idle(
    app: &AppHandle,
    db: &Db,
    eng: &EngineRef,
    prompt: &str,
) -> anyhow::Result<()> {
    let busy = { eng.lock().await.turn.busy };
    if !busy {
        // Propagate failure (e.g. the CLI/app-server can't start at boot) so the
        // caller's report_failure surfaces it in Needs-you instead of marking the
        // session silently revived while its interrupted work stays stuck. nudge can
        // fail AFTER already marking the turn busy + running (a per-turn CLI that
        // fails to spawn), so reset the engine to idle first — otherwise later user
        // sends queue behind a turn that never emits TurnEnd.
        if let Err(e) = engine::nudge(app, db, eng, prompt).await {
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

async fn revive_stalled_lead(app: &AppHandle, thread_id: i32, stalled_dirs: Vec<i32>) {
    let Some(db) = app.try_state::<Db>() else {
        return;
    };
    let db = Db(db.0.clone(), db.1);
    if let Err(e) = try_revive_stalled_lead(app, &db, thread_id, &stalled_dirs).await {
        report_failure(app, thread_id, "lead", &e);
    }
}

/// Re-drive an idle lead whose task stalled: get-or-create its engine, resume the
/// native session, and nudge it with the stalled worker ids so it re-dispatches.
/// Structurally a sibling of [`try_revive_lead`] — the difference is the trigger
/// (a cleanly-idle lead with an undelivered in-progress task, not an interrupted
/// turn) and the prompt. No `mark_incomplete`/`fail_queued`: a cleanly-idle lead
/// has no half-streamed row and no orphaned queue (idle ⟺ empty queue). Failures
/// (missing CLI, read-only workspace, native resume fails) propagate so the
/// caller's `report_failure` surfaces them in Needs-you rather than leaving the
/// task silently stalled. `lead_engine` get-or-create also covers a resident-but-
/// idle lead (opened by the frontend for slash discovery) — never excluded.
async fn try_revive_stalled_lead(
    app: &AppHandle,
    db: &Db,
    thread_id: i32,
    stalled_dirs: &[i32],
) -> anyhow::Result<()> {
    let eng = lead_engine(app, db, thread_id, "en").await?;
    engine::ensure_running(app, db, &eng).await?;
    if has_open_ask(app, "lead", thread_id) {
        return Ok(());
    }
    nudge_eng_if_idle(app, db, &eng, &resume_stalled_prompt(stalled_dirs)).await?;
    Ok(())
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
        let repo_ref = repo::add_repo_ref(db, ws.id, "r", "/tmp/weft-revive-fake", "main", "", true)
            .await
            .unwrap();
        let th = repo::create_thread(db, ws.id, "issue", "feature", "codex")
            .await
            .unwrap();
        (th.id, repo_ref.id)
    }

    async fn mk_direction(db: &Db, thread_id: i32, repo_id: i32, name: &str) -> i32 {
        repo::create_direction(db, thread_id, name, "codex", repo_id, "why", "impl-only", "")
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

    // ---- Silent-stall selection (collect_stalled_leads) ----

    /// A direction whose worker drained cleanly to idle: an in-progress ("working"
    /// / "planning") task with a latest session that has a native id and
    /// status=="idle". This is the shape the running-only collect_targets misses.
    async fn stalled_direction(db: &Db, thread_id: i32, repo_id: i32, status: &str) -> i32 {
        let dir = mk_direction(db, thread_id, repo_id, "alpha").await;
        repo::set_direction_status(db, dir, status).await.unwrap();
        let sess = running_session(db, dir, repo_id).await; // native id + running
        repo::set_session_status(db, sess, "idle").await.unwrap(); // drained to idle
        dir
    }

    /// An idle lead with orchestration context (native id) that finished its turn.
    async fn idle_lead(db: &Db, thread_id: i32) {
        repo::set_lead_native_id(db, thread_id, "lead-nat")
            .await
            .unwrap();
        repo::set_lead_status(db, thread_id, "idle").await.unwrap();
    }

    /// THE silent-stall shape: an idle lead whose in-progress task has a worker
    /// that drained to idle without delivering. Nothing drives it → select the
    /// lead's thread to wake. Proves selection spans the all-idle shape that the
    /// running-only path misses.
    #[tokio::test]
    async fn selects_stalled_lead_with_idle_worker_on_working_direction() {
        let db = mem().await;
        let (th, repo_id) = fixture(&db).await;
        idle_lead(&db, th).await;
        let dir = stalled_direction(&db, th, repo_id, "working").await;

        let stalled = collect_stalled_leads(&db).await.unwrap();
        // Thread + its stalled worker id, so the resume prompt can address it.
        assert_eq!(stalled, vec![(th, vec![dir])]);
    }

    /// "planning" is equally in-progress — the stall path spans both live states.
    #[tokio::test]
    async fn selects_stalled_lead_on_planning_direction() {
        let db = mem().await;
        let (th, repo_id) = fixture(&db).await;
        idle_lead(&db, th).await;
        let dir = stalled_direction(&db, th, repo_id, "planning").await;

        let stalled = collect_stalled_leads(&db).await.unwrap();
        assert_eq!(stalled, vec![(th, vec![dir])]);
    }

    /// Two stalled directions under one thread wake the lead EXACTLY once — the
    /// per-thread (not per-direction) idempotency the plan requires, so a
    /// multi-task thread can't post duplicate resume wakes.
    #[tokio::test]
    async fn selects_stalled_lead_once_for_multiple_stalled_directions() {
        let db = mem().await;
        let (th, repo_id) = fixture(&db).await;
        idle_lead(&db, th).await;
        let mut want: Vec<i32> = Vec::new();
        for (name, status) in [("alpha", "working"), ("beta", "planning")] {
            let dir = mk_direction(&db, th, repo_id, name).await;
            repo::set_direction_status(&db, dir, status).await.unwrap();
            let sess = running_session(&db, dir, repo_id).await;
            repo::set_session_status(&db, sess, "idle").await.unwrap();
            want.push(dir);
        }

        let stalled = collect_stalled_leads(&db).await.unwrap();
        // ONE thread entry despite two stalled directions, carrying BOTH worker ids.
        assert_eq!(stalled.len(), 1);
        assert_eq!(stalled[0].0, th);
        let mut got = stalled[0].1.clone();
        got.sort();
        want.sort();
        assert_eq!(got, want);
    }

    /// A "running" lead is an interrupted turn owned by collect_targets; the stall
    /// path must not double-drive it.
    #[tokio::test]
    async fn excludes_stalled_when_lead_running() {
        let db = mem().await;
        let (th, repo_id) = fixture(&db).await;
        repo::set_lead_native_id(&db, th, "lead-nat").await.unwrap();
        repo::set_lead_status(&db, th, "running").await.unwrap();
        stalled_direction(&db, th, repo_id, "working").await;

        let stalled = collect_stalled_leads(&db).await.unwrap();
        assert!(stalled.is_empty());
    }

    /// A "stopped" lead was taken over in the user's terminal — never wake a
    /// competing headless process for it.
    #[tokio::test]
    async fn excludes_stalled_when_lead_stopped() {
        let db = mem().await;
        let (th, repo_id) = fixture(&db).await;
        repo::set_lead_native_id(&db, th, "lead-nat").await.unwrap();
        repo::set_lead_status(&db, th, "stopped").await.unwrap();
        stalled_direction(&db, th, repo_id, "working").await;

        let stalled = collect_stalled_leads(&db).await.unwrap();
        assert!(stalled.is_empty());
    }

    /// A lead with no native id has no orchestration context to resume from.
    #[tokio::test]
    async fn excludes_stalled_lead_without_native_id() {
        let db = mem().await;
        let (th, repo_id) = fixture(&db).await;
        repo::set_lead_status(&db, th, "idle").await.unwrap(); // idle, but no native id
        stalled_direction(&db, th, repo_id, "working").await;

        let stalled = collect_stalled_leads(&db).await.unwrap();
        assert!(stalled.is_empty());
    }

    /// A "done" task is delivered — not a stall.
    #[tokio::test]
    async fn excludes_stalled_when_direction_done() {
        let db = mem().await;
        let (th, repo_id) = fixture(&db).await;
        idle_lead(&db, th).await;
        stalled_direction(&db, th, repo_id, "done").await;

        let stalled = collect_stalled_leads(&db).await.unwrap();
        assert!(stalled.is_empty());
    }

    /// A "queued" task never started — nothing has stalled.
    #[tokio::test]
    async fn excludes_stalled_when_direction_queued() {
        let db = mem().await;
        let (th, repo_id) = fixture(&db).await;
        idle_lead(&db, th).await;
        stalled_direction(&db, th, repo_id, "queued").await;

        let stalled = collect_stalled_leads(&db).await.unwrap();
        assert!(stalled.is_empty());
    }

    /// A "review" task is legitimately awaiting the human — not a stall to drive.
    #[tokio::test]
    async fn excludes_stalled_when_direction_review() {
        let db = mem().await;
        let (th, repo_id) = fixture(&db).await;
        idle_lead(&db, th).await;
        stalled_direction(&db, th, repo_id, "review").await;

        let stalled = collect_stalled_leads(&db).await.unwrap();
        assert!(stalled.is_empty());
    }

    /// A "running" worker session is an interrupted turn (collect_targets owns
    /// it), not a silent stall — the stall shape is a worker DRAINED to idle.
    #[tokio::test]
    async fn excludes_stalled_when_worker_running() {
        let db = mem().await;
        let (th, repo_id) = fixture(&db).await;
        idle_lead(&db, th).await;
        let dir = mk_direction(&db, th, repo_id, "alpha").await;
        repo::set_direction_status(&db, dir, "working").await.unwrap();
        running_session(&db, dir, repo_id).await; // native id + RUNNING (not drained)

        let stalled = collect_stalled_leads(&db).await.unwrap();
        assert!(stalled.is_empty());
    }

    /// A "stopped" worker session was taken over in the user's terminal.
    #[tokio::test]
    async fn excludes_stalled_when_worker_stopped() {
        let db = mem().await;
        let (th, repo_id) = fixture(&db).await;
        idle_lead(&db, th).await;
        let dir = mk_direction(&db, th, repo_id, "alpha").await;
        repo::set_direction_status(&db, dir, "working").await.unwrap();
        let sess = running_session(&db, dir, repo_id).await;
        repo::set_session_status(&db, sess, "stopped").await.unwrap();

        let stalled = collect_stalled_leads(&db).await.unwrap();
        assert!(stalled.is_empty());
    }

    /// An idle worker session that never captured a native id can't be resumed.
    #[tokio::test]
    async fn excludes_stalled_when_worker_without_native_id() {
        let db = mem().await;
        let (th, repo_id) = fixture(&db).await;
        idle_lead(&db, th).await;
        let dir = mk_direction(&db, th, repo_id, "alpha").await;
        repo::set_direction_status(&db, dir, "working").await.unwrap();
        let s = repo::create_session(&db, dir, repo_id, "codex", "/tmp/wt")
            .await
            .unwrap();
        repo::set_session_status(&db, s.id, "idle").await.unwrap(); // idle, no native id

        let stalled = collect_stalled_leads(&db).await.unwrap();
        assert!(stalled.is_empty());
    }

    /// An in-progress task with no session at all hasn't reached a worker → not
    /// the "worker drained to idle" stall this path recovers.
    #[tokio::test]
    async fn excludes_stalled_when_no_session() {
        let db = mem().await;
        let (th, repo_id) = fixture(&db).await;
        idle_lead(&db, th).await;
        let dir = mk_direction(&db, th, repo_id, "alpha").await;
        repo::set_direction_status(&db, dir, "working").await.unwrap();

        let stalled = collect_stalled_leads(&db).await.unwrap();
        assert!(stalled.is_empty());
    }

    /// The resume prompt names the exact stalled worker bus ids so the lead can
    /// bus_post each idle worker directly (the stall means no worker message it
    /// could read a `from` id off), and keeps the "not just nudged once" framing.
    #[test]
    fn resume_stalled_prompt_names_the_stalled_worker_ids() {
        let p = resume_stalled_prompt(&[5, 7]);
        assert!(p.contains("restarted"));
        assert!(p.contains("5, 7"));
        assert!(p.contains("bus_post"));
        assert!(p.contains("not just nudged once"));
    }

    /// Leftover queued user messages must surface as "error" (un-sent, resendable)
    /// after revive, not stay stuck as "queued" with no live processor to deliver them.
    #[tokio::test]
    async fn fail_queued_flips_worker_queued_rows_to_error() {
        let db = mem().await;
        let (th, repo_id) = fixture(&db).await;
        let dir = mk_direction(&db, th, repo_id, "alpha").await;
        let sess = running_session(&db, dir, repo_id).await;
        // A user message queued before the crash, now orphaned (no live FIFO).
        let queued = repo::insert_lead_message(
            &db,
            th,
            Some(sess),
            1,
            "user",
            "text",
            r#"{"text":"pending send"}"#,
            "queued",
        )
        .await
        .unwrap();

        // Simulate the revive path: mark_incomplete first, then fail_queued.
        repo::mark_incomplete_turns_interrupted(&db, th, Some(sess))
            .await
            .unwrap();
        repo::fail_queued(&db, th, Some(sess)).await.unwrap();

        let all = repo::list_lead_messages(&db, th).await.unwrap();
        assert_eq!(
            all.iter().find(|m| m.id == queued.id).unwrap().status,
            "error"
        );
    }

    /// Same for lead threads (session_id == None).
    #[tokio::test]
    async fn fail_queued_flips_lead_queued_rows_to_error() {
        let db = mem().await;
        let (th, _) = fixture(&db).await;
        // Lead has a native id and running status so revive would select it.
        repo::set_lead_native_id(&db, th, "lead-nat").await.unwrap();
        repo::set_lead_status(&db, th, "running").await.unwrap();
        // A user message queued on the lead (no session_id).
        let queued = repo::insert_lead_message(
            &db,
            th,
            None,
            1,
            "user",
            "text",
            r#"{"text":"lead queued"}"#,
            "queued",
        )
        .await
        .unwrap();

        repo::mark_incomplete_turns_interrupted(&db, th, None)
            .await
            .unwrap();
        repo::fail_queued(&db, th, None).await.unwrap();

        let all = repo::list_lead_messages(&db, th).await.unwrap();
        assert_eq!(
            all.iter().find(|m| m.id == queued.id).unwrap().status,
            "error"
        );
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
