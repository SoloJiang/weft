//! Boot-time recovery: re-attach and continue work interrupted by a hard app
//! exit — both orphaned `running` turns (cut off mid-flight) and silently-stalled
//! tasks (a lead that finished its turn cleanly yet still owns an in-progress task
//! whose worker drained to idle without delivering). Cleanly-idle work with no
//! in-progress task is left for lazy-attach on open. See the worker-restart spec.
//!
//! Runtime companion (issue #95): the same silent-stall shape can also develop
//! while the app keeps running — a coordination deadlock isn't only a boot-time
//! artifact. [`spawn_stall_watch`] re-runs [`collect_stalled_leads`] on a timer
//! and auto-redrives through the exact same path, cooldown-gated per thread so a
//! stall that doesn't actually resolve can't turn into a redrive storm (see
//! `cooldown_elapsed`). It also covers the shape boot revive — and the bus wake
//! path (`coordinator::deliver`) — can NEVER reach: a worker session left
//! `stopped` (see `engine::stop`). Silently auto-driving that would risk
//! spawning a second writer over a human's terminal takeover, so it is
//! deliberately never auto-redriven; instead it gets a visible, honest
//! Needs-you notice (`collect_stopped_worker_stalls`) so a lead that bus_post'd
//! it is never left believing the wake landed.

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

/// Default cadence of the runtime stall-watch sweep ([`spawn_stall_watch`],
/// issue #95). Much tighter than the boot-only, one-shot sweep: a coordination
/// deadlock that DEVELOPS while the app keeps running (unlike a crash-
/// interrupted turn) has no other trigger to catch it. `0` disables the loop
/// entirely. Override with `WEFT_STALL_REDRIVE_SWEEP_SECS`.
const STALL_REDRIVE_SWEEP_DEFAULT_SECS: u64 = 60;

/// Minimum gap between two auto-redrives of the SAME thread (防重复驱动风暴).
/// A redrive that doesn't actually resolve the stall — the lead nudges once,
/// then drains back to the exact same idle-worker shape — must not turn into a
/// redrive-every-sweep storm; this bounds it to at most one attempt per window
/// while still eventually retrying (a transient failure — CLI briefly missing —
/// shouldn't strand the thread forever). Override with
/// `WEFT_STALL_REDRIVE_COOLDOWN_SECS`.
const STALL_REDRIVE_COOLDOWN_DEFAULT_SECS: u64 = 300;

/// Names the exact worker bus ids (= direction ids) that went idle without
/// delivering, so the lead can address them: without these it has no worker
/// message to read a `from` id off (the stall means the worker never posted).
/// Appended to BOTH the idle-stall prompt and the interrupted-lead prompt — an
/// interrupted lead may be a stall-resume turn that was itself cut off, so the
/// ids must survive that crash rather than only riding the idle-only path.
fn stalled_ids_clause(stalled_dirs: &[i32]) -> String {
    let ids = stalled_dirs
        .iter()
        .map(|d| d.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    format!(" The workers that went idle are on the thread bus under these ids: {ids}. bus_post each id directly (a direct bus_post reaches an idle worker) to re-dispatch it.")
}

/// The idle-stall resume nudge: [`RESUME_STALLED_PROMPT`] + the stalled ids.
fn resume_stalled_prompt(stalled_dirs: &[i32]) -> String {
    format!("{RESUME_STALLED_PROMPT}{}", stalled_ids_clause(stalled_dirs))
}

/// The interrupted-lead nudge: the generic continue prompt, PLUS the stalled ids
/// when this interrupted lead also owns undelivered stalled tasks (e.g. its cut-off
/// turn was itself a stall-resume). Empty stalled set → unchanged `REVIVE_PROMPT`,
/// so a normal interrupted lead is a pure regression.
fn lead_revive_prompt(stalled_dirs: &[i32]) -> String {
    if stalled_dirs.is_empty() {
        REVIVE_PROMPT.to_string()
    } else {
        format!("{REVIVE_PROMPT}{}", stalled_ids_clause(stalled_dirs))
    }
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

/// Runtime companion to the boot-only sweep above (issue #95): a silent-stall
/// coordination deadlock isn't only a boot-time artifact — it can develop while
/// the app keeps running (e.g. a bus post to a worker races its lead going
/// idle), and nothing else re-checks for that once boot has passed. Re-runs
/// [`collect_stalled_leads`] on a timer and auto-redrives through the exact same
/// path as boot (`revive_stalled_lead`), cooldown-gated per thread so a stall
/// that doesn't actually resolve can't turn into a redrive storm. Also runs
/// [`collect_stopped_worker_stalls`] on the same timer — never auto-driven (see
/// that function's docs), but latched so its Needs-you notice is posted once per
/// episode and retracted on recovery, mirroring the engine watchdog's stall
/// notice.
pub fn spawn_stall_watch(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let sweep_secs =
            crate::commands::env_secs("WEFT_STALL_REDRIVE_SWEEP_SECS", STALL_REDRIVE_SWEEP_DEFAULT_SECS);
        if sweep_secs == 0 {
            return; // disabled
        }
        let cooldown_secs = crate::commands::env_secs(
            "WEFT_STALL_REDRIVE_COOLDOWN_SECS",
            STALL_REDRIVE_COOLDOWN_DEFAULT_SECS,
        );
        // Loop-owned state, mirroring the engine watchdog's `stall_notices`: no
        // DB/EngineInner field exists for either (stall markers never persist —
        // see the module doc), so the sweep itself is the sole owner for its
        // whole lifetime.
        let mut last_redrive: std::collections::HashMap<i32, u64> = std::collections::HashMap::new();
        let mut stopped_notices: std::collections::HashMap<i32, (i32, u64)> =
            std::collections::HashMap::new();
        let sem = Arc::new(Semaphore::new(MAX_CONCURRENT));
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(sweep_secs)).await;
            let Some(db) = app.try_state::<Db>() else {
                continue;
            };
            let db = Db(db.0.clone(), db.1);

            match collect_stalled_leads(&db).await {
                Ok(stalled) => {
                    let now = now_secs();
                    // Prune threads that recovered on their own so a stale
                    // cooldown from long ago can't shadow a genuinely fresh
                    // stall episode much later.
                    let live: HashSet<i32> = stalled.iter().map(|(t, _)| *t).collect();
                    last_redrive.retain(|t, _| live.contains(t));
                    for (tid, dirs) in stalled {
                        if !cooldown_elapsed(last_redrive.get(&tid).copied(), now, cooldown_secs) {
                            continue;
                        }
                        last_redrive.insert(tid, now);
                        let (app, sem) = (app.clone(), sem.clone());
                        tauri::async_runtime::spawn(async move {
                            let _permit = sem.acquire().await;
                            revive_stalled_lead(&app, tid, dirs).await;
                        });
                    }
                }
                Err(e) => eprintln!("[weft][revive] runtime stall sweep failed: {e}"),
            }

            match collect_stopped_worker_stalls(&db).await {
                Ok(current) => {
                    let known: Vec<i32> = stopped_notices.keys().copied().collect();
                    let (to_notify, to_retract) = diff_stopped_notices(&current, &known);
                    let Some(bus) = app.try_state::<crate::bus::BusRegistry>() else {
                        continue;
                    };
                    for dir_id in to_retract {
                        if let Some((thread_id, ask_id)) = stopped_notices.remove(&dir_id) {
                            if bus.cancel_open_asks_by_id(thread_id, ask_id) {
                                let _ = app.emit("needs-you://changed", thread_id);
                            }
                        }
                    }
                    for (thread_id, dir_id) in to_notify {
                        let id = bus.notify_human(
                            thread_id,
                            &dir_id.to_string(),
                            &stopped_worker_notice_text(dir_id),
                        );
                        stopped_notices.insert(dir_id, (thread_id, id));
                    }
                }
                Err(e) => eprintln!("[weft][revive] runtime stopped-worker sweep failed: {e}"),
            }
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

/// Threads' in-progress directions whose latest worker session is `stopped` —
/// the shape neither the boot revive above nor `coordinator::deliver` will EVER
/// wake: both explicitly skip `STATUS_STOPPED` so a bus post / restart can't
/// spawn a second writer over what might be a human's live terminal takeover
/// (see `engine::stop`). That safety rule means this can never be auto-driven
/// the way [`collect_stalled_leads`] is — so unlike that function, this one is
/// surfaced as an explicit, honest notice instead (see [`spawn_stall_watch`] and
/// the boot `sweep`), not redriven.
///
/// Deliberately independent of lead status (unlike [`collect_stalled_leads`]):
/// the stall lives in the WORKER, so it blocks progress whether the lead is
/// idle, busy, or itself stopped. Returns `(thread_id, direction_id)` pairs, one
/// per stalled direction, so each can carry its own notice — a human acting on
/// one shouldn't have to guess which of a thread's several tasks it was about.
/// Pure over DB → unit-testable.
async fn collect_stopped_worker_stalls(db: &Db) -> anyhow::Result<Vec<(i32, i32)>> {
    let mut out = Vec::new();
    for ws in repo::list_workspaces(db).await? {
        for th in repo::list_threads(db, ws.id).await? {
            for dir in repo::list_directions(db, th.id).await? {
                if dir.status != "planning" && dir.status != "working" {
                    continue;
                }
                let Some(sess) = repo::latest_session_for_direction(db, dir.id).await? else {
                    continue;
                };
                if sess.status == "stopped" {
                    out.push((th.id, dir.id));
                }
            }
        }
    }
    Ok(out)
}

/// The honest stopped-worker notice: names the exact task so the human isn't
/// left guessing, and says PLAINLY that no wake reached it — not "no response
/// yet" (which would read as "the agent is thinking"), but "the message never
/// arrived" (so a lead that bus_post'd this direction is never left believing
/// the wake landed). Points at the concrete fix (open it — sending resumes it,
/// same as the `chatStopped` composer hint) rather than just naming the problem.
fn stopped_worker_notice_text(direction_id: i32) -> String {
    format!(
        "⚠️ 任务 #{direction_id} 的 worker 处于 stopped 状态,总线唤醒无法触达它——不是「已唤醒但没反应」,而是消息根本没送到,可能已在终端被接管。请手动打开这个任务查看:如果没人在终端里,发送一条消息即可恢复;如果确实在终端接管中,忽略此提示即可。"
    )
}

/// Pure cooldown gate for the runtime redrive loop: is a redrive of a thread
/// allowed now, given the last redrive time (epoch seconds; `None` = never) and
/// `now` (epoch seconds)? Keeps a stall that doesn't actually resolve from being
/// redriven every sweep tick forever — see `STALL_REDRIVE_COOLDOWN_DEFAULT_SECS`.
/// `saturating_sub` makes a clock that moved backward (or a `now` a test passes
/// before `last`) fail safe as "cooldown not yet elapsed" rather than underflow.
/// Pure → unit-tested.
fn cooldown_elapsed(last: Option<u64>, now: u64, cooldown_secs: u64) -> bool {
    match last {
        None => true,
        Some(t) => now.saturating_sub(t) >= cooldown_secs,
    }
}

/// Pure reconciliation for the stopped-worker notice latch: given the latest
/// scan (`current`) and the direction ids already carrying a notice (`known`),
/// decide which directions need a FRESH notice (newly entered the stopped
/// shape) and which previously-notified ones should be RETRACTED (resumed /
/// moved on / worker no longer stopped). Mirrors the engine watchdog's
/// edge-triggered stall-notice latch (post once while the condition holds,
/// retract on the transition out) — the difference here is the trigger (DB scan
/// vs an in-memory busy/quiet clock). Pure over (current, known) →
/// unit-testable without a bus or `AppHandle`.
fn diff_stopped_notices(
    current: &[(i32, i32)],
    known: &[i32],
) -> (Vec<(i32, i32)>, Vec<i32>) {
    let current_dirs: HashSet<i32> = current.iter().map(|(_, d)| *d).collect();
    let known_set: HashSet<i32> = known.iter().copied().collect();
    let to_notify = current
        .iter()
        .filter(|(_, d)| !known_set.contains(d))
        .copied()
        .collect();
    let to_retract = known
        .iter()
        .filter(|d| !current_dirs.contains(d))
        .copied()
        .collect();
    (to_notify, to_retract)
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
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
    // Stopped-worker coverage (issue #95) is the same best-effort ENHANCEMENT
    // shape as the stall scan above: never gates the primary revive.
    let stopped = collect_stopped_worker_stalls(&db).await.unwrap_or_else(|e| {
        eprintln!("[weft][revive] stopped-worker scan failed: {e}");
        Vec::new()
    });
    if leads.is_empty() && workers.is_empty() && stalled.is_empty() && stopped.is_empty() {
        return Ok(());
    }
    eprintln!(
        "[weft][revive] reviving {} worker(s), {} lead(s), {} stalled thread(s), {} stopped-worker notice(s)",
        workers.len(),
        leads.len(),
        stalled.len(),
        stopped.len(),
    );
    // Never auto-driven (see `collect_stopped_worker_stalls`): a fresh, honest
    // notice per boot is enough — no latch to manage, unlike the runtime loop's
    // retraction dance, since a one-shot boot sweep has no "later" to retract in.
    if let Some(bus) = app.try_state::<crate::bus::BusRegistry>() {
        for (thread_id, dir_id) in &stopped {
            bus.notify_human(*thread_id, &dir_id.to_string(), &stopped_worker_notice_text(*dir_id));
        }
    }
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
    // An interrupted lead that ALSO owns stalled tasks — its cut-off turn may have
    // BEEN a stall-resume, and the idle-only stall scan skips a lead persisted
    // "running" — must carry the worker ids too, delivered on the SAME reliable
    // terms as the idle stall path: QUEUE if transiently busy (don't drop the ids)
    // and stay retryable on failure. A normal interrupted lead (no stalled tasks)
    // keeps the exact previous behavior: plain REVIVE_PROMPT, skip-if-busy,
    // stop-on-fail — a pure regression.
    let stalled_dirs = stalled_direction_ids(db, thread_id).await?;
    if stalled_dirs.is_empty() {
        nudge_eng_if_idle(app, db, &eng, REVIVE_PROMPT).await?;
    } else {
        deliver_stalled_resume(app, db, &eng, &lead_revive_prompt(&stalled_dirs)).await?;
    }
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

/// Deliver `prompt`, resetting the engine to idle on failure. nudge can fail
/// AFTER marking the turn busy + running (a per-turn CLI that fails to spawn), so
/// resetting first stops later user sends queuing behind a turn that never emits
/// TurnEnd; propagating the error lets the caller's report_failure surface it in
/// Needs-you instead of a silent "revived". A busy engine QUEUES the prompt
/// (single-writer: it runs AFTER the current turn, never in parallel) — never a
/// second concurrent turn, so this cannot double-drive.
async fn nudge_or_reset(
    app: &AppHandle,
    db: &Db,
    eng: &EngineRef,
    prompt: &str,
) -> anyhow::Result<()> {
    if let Err(e) = engine::nudge(app, db, eng, prompt).await {
        engine::stop(app, eng).await;
        return Err(e);
    }
    Ok(())
}

/// Running-revive nudge: deliver ONLY when the (freshly-resumed) engine is idle.
/// A racing concurrent send that already made it busy is driving it — don't queue
/// a redundant "continue where you left off" behind that. The stall paths instead
/// use [`deliver_stalled_resume`]: their prompt carries the re-dispatch ids and
/// must QUEUE (not drop) if the lead is transiently busy, and stay retryable.
async fn nudge_eng_if_idle(
    app: &AppHandle,
    db: &Db,
    eng: &EngineRef,
    prompt: &str,
) -> anyhow::Result<()> {
    let busy = { eng.lock().await.turn.busy };
    if !busy {
        nudge_or_reset(app, db, eng, prompt).await?;
    }
    Ok(())
}

/// Deliver a stalled-resume prompt (it names the worker ids the lead must
/// re-dispatch). Differs from [`nudge_eng_if_idle`] on two axes that both matter
/// for stall recovery:
/// - QUEUES rather than skips when the lead is transiently busy (a frontend/IM
///   send racing the sweep). `engine::nudge` → `hidden_delivery(busy) → Queue`
///   runs the resume after the current turn (single-writer, never parallel), so
///   the ids are delivered, not dropped until another restart.
/// - Does NOT `engine::stop` on failure. `engine::nudge` already rolls a failed
///   hidden turn back to idle (`rollback_failed_turn`), so a `stop` here would only
///   ADD a `STATUS_STOPPED` persist — which excludes the thread from every future
///   revive scan (`collect_stalled_leads` is idle-only), turning a TRANSIENT boot
///   failure (CLI briefly missing) into a permanent stall. Staying idle keeps the
///   next boot retryable; the caller's `report_failure` still surfaces the failure.
async fn deliver_stalled_resume(
    app: &AppHandle,
    db: &Db,
    eng: &EngineRef,
    prompt: &str,
) -> anyhow::Result<()> {
    engine::nudge(app, db, eng, prompt).await
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
    deliver_stalled_resume(app, db, &eng, &resume_stalled_prompt(stalled_dirs)).await?;
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

    // ---- Stopped-worker coverage (collect_stopped_worker_stalls, issue #95) ----

    /// A direction whose worker is `stopped` (not idle, not running) while still
    /// in-progress: the exact shape `collect_stalled_leads` deliberately
    /// excludes (`excludes_stalled_when_worker_stopped` above), because bus wake
    /// can never reach it. This is the sibling scan that surfaces it instead of
    /// silently dropping it.
    async fn stopped_worker_direction(db: &Db, thread_id: i32, repo_id: i32, status: &str) -> i32 {
        let dir = mk_direction(db, thread_id, repo_id, "alpha").await;
        repo::set_direction_status(db, dir, status).await.unwrap();
        let sess = running_session(db, dir, repo_id).await; // native id + running
        repo::set_session_status(db, sess, "stopped").await.unwrap();
        dir
    }

    /// THE shape: an in-progress direction whose latest session is `stopped`.
    /// Detected with NO lead state set up at all (no native id, no status row)
    /// — proves the scan doesn't key off the lead.
    #[tokio::test]
    async fn selects_stopped_worker_on_working_direction_without_any_lead_state() {
        let db = mem().await;
        let (th, repo_id) = fixture(&db).await;
        let dir = stopped_worker_direction(&db, th, repo_id, "working").await;

        let stopped = collect_stopped_worker_stalls(&db).await.unwrap();
        assert_eq!(stopped, vec![(th, dir)]);
    }

    /// "planning" is equally in-progress.
    #[tokio::test]
    async fn selects_stopped_worker_on_planning_direction() {
        let db = mem().await;
        let (th, repo_id) = fixture(&db).await;
        let dir = stopped_worker_direction(&db, th, repo_id, "planning").await;

        let stopped = collect_stopped_worker_stalls(&db).await.unwrap();
        assert_eq!(stopped, vec![(th, dir)]);
    }

    /// Independent of lead status (unlike `collect_stalled_leads`): a RUNNING
    /// lead doesn't hide a stopped worker — the stall lives in the worker, not
    /// the lead.
    #[tokio::test]
    async fn selects_stopped_worker_even_when_lead_is_running() {
        let db = mem().await;
        let (th, repo_id) = fixture(&db).await;
        repo::set_lead_native_id(&db, th, "lead-nat").await.unwrap();
        repo::set_lead_status(&db, th, "running").await.unwrap();
        let dir = stopped_worker_direction(&db, th, repo_id, "working").await;

        let stopped = collect_stopped_worker_stalls(&db).await.unwrap();
        assert_eq!(stopped, vec![(th, dir)]);
    }

    /// Independent of lead status: a STOPPED lead (its own terminal takeover)
    /// doesn't hide a stopped worker either — each direction's stall is tracked
    /// on its own.
    #[tokio::test]
    async fn selects_stopped_worker_even_when_lead_is_stopped() {
        let db = mem().await;
        let (th, repo_id) = fixture(&db).await;
        repo::set_lead_native_id(&db, th, "lead-nat").await.unwrap();
        repo::set_lead_status(&db, th, "stopped").await.unwrap();
        let dir = stopped_worker_direction(&db, th, repo_id, "working").await;

        let stopped = collect_stopped_worker_stalls(&db).await.unwrap();
        assert_eq!(stopped, vec![(th, dir)]);
    }

    /// Multiple stopped-worker directions under one thread each get their own
    /// entry — a human acting on a notice must be able to tell which task it
    /// was about, unlike the per-THREAD collapse `collect_stalled_leads` does.
    #[tokio::test]
    async fn selects_one_entry_per_stopped_direction() {
        let db = mem().await;
        let (th, repo_id) = fixture(&db).await;
        let a = stopped_worker_direction(&db, th, repo_id, "working").await;
        let b = stopped_worker_direction(&db, th, repo_id, "planning").await;

        let mut stopped = collect_stopped_worker_stalls(&db).await.unwrap();
        stopped.sort();
        let mut want = vec![(th, a), (th, b)];
        want.sort();
        assert_eq!(stopped, want);
    }

    /// An idle (cleanly drained) worker is `collect_stalled_leads`'s shape, not
    /// this one.
    #[tokio::test]
    async fn excludes_stopped_scan_when_worker_idle() {
        let db = mem().await;
        let (th, repo_id) = fixture(&db).await;
        let dir = mk_direction(&db, th, repo_id, "alpha").await;
        repo::set_direction_status(&db, dir, "working").await.unwrap();
        let sess = running_session(&db, dir, repo_id).await;
        repo::set_session_status(&db, sess, "idle").await.unwrap();

        let stopped = collect_stopped_worker_stalls(&db).await.unwrap();
        assert!(stopped.is_empty());
    }

    /// A "running" (interrupted, not stopped) worker isn't this shape either.
    #[tokio::test]
    async fn excludes_stopped_scan_when_worker_running() {
        let db = mem().await;
        let (th, repo_id) = fixture(&db).await;
        let dir = mk_direction(&db, th, repo_id, "alpha").await;
        repo::set_direction_status(&db, dir, "working").await.unwrap();
        running_session(&db, dir, repo_id).await; // native id + RUNNING, not stopped

        let stopped = collect_stopped_worker_stalls(&db).await.unwrap();
        assert!(stopped.is_empty());
    }

    /// A "done" task's stopped worker isn't a stall to surface — the work is
    /// already delivered.
    #[tokio::test]
    async fn excludes_stopped_scan_when_direction_done() {
        let db = mem().await;
        let (th, repo_id) = fixture(&db).await;
        stopped_worker_direction(&db, th, repo_id, "done").await;

        let stopped = collect_stopped_worker_stalls(&db).await.unwrap();
        assert!(stopped.is_empty());
    }

    /// "review" is legitimately awaiting the human, not a stall.
    #[tokio::test]
    async fn excludes_stopped_scan_when_direction_review() {
        let db = mem().await;
        let (th, repo_id) = fixture(&db).await;
        stopped_worker_direction(&db, th, repo_id, "review").await;

        let stopped = collect_stopped_worker_stalls(&db).await.unwrap();
        assert!(stopped.is_empty());
    }

    /// A direction with no session at all hasn't reached a worker — nothing to
    /// report as stopped.
    #[tokio::test]
    async fn excludes_stopped_scan_when_no_session() {
        let db = mem().await;
        let (th, repo_id) = fixture(&db).await;
        let dir = mk_direction(&db, th, repo_id, "alpha").await;
        repo::set_direction_status(&db, dir, "working").await.unwrap();

        let stopped = collect_stopped_worker_stalls(&db).await.unwrap();
        assert!(stopped.is_empty());
    }

    /// The notice names the exact task and says PLAINLY the wake never arrived
    /// — not "no response yet" (which would read as "still thinking").
    #[test]
    fn stopped_worker_notice_text_names_the_direction_and_says_undelivered() {
        let text = stopped_worker_notice_text(42);
        assert!(text.contains("42"));
        assert!(text.contains("stopped"));
        assert!(text.contains("没送到"));
    }

    // ---- Runtime redrive cooldown (cooldown_elapsed) ----

    #[test]
    fn cooldown_elapsed_allows_a_first_ever_redrive() {
        assert!(cooldown_elapsed(None, 1_000, 300));
    }

    #[test]
    fn cooldown_elapsed_blocks_within_the_window_then_allows_at_the_boundary() {
        assert!(!cooldown_elapsed(Some(1_000), 1_299, 300));
        assert!(cooldown_elapsed(Some(1_000), 1_300, 300)); // inclusive boundary
    }

    #[test]
    fn cooldown_elapsed_never_underflows_on_a_clock_that_moved_backward() {
        // `now` before `last` must fail safe as "not yet due", never panic/wrap.
        assert!(!cooldown_elapsed(Some(1_000), 500, 300));
    }

    // ---- Runtime stopped-notice latch (diff_stopped_notices) ----

    #[test]
    fn diff_stopped_notices_flags_a_newly_stopped_direction_for_notice() {
        let (to_notify, to_retract) = diff_stopped_notices(&[(1, 10)], &[]);
        assert_eq!(to_notify, vec![(1, 10)]);
        assert!(to_retract.is_empty());
    }

    #[test]
    fn diff_stopped_notices_is_silent_while_the_condition_holds() {
        // Already-known direction, still in the current scan → neither notify
        // nor retract (the notice posted on the FIRST sweep is still accurate).
        let (to_notify, to_retract) = diff_stopped_notices(&[(1, 10)], &[10]);
        assert!(to_notify.is_empty());
        assert!(to_retract.is_empty());
    }

    #[test]
    fn diff_stopped_notices_retracts_a_recovered_direction() {
        let (to_notify, to_retract) = diff_stopped_notices(&[], &[10]);
        assert!(to_notify.is_empty());
        assert_eq!(to_retract, vec![10]);
    }

    #[test]
    fn diff_stopped_notices_handles_a_mixed_sweep_in_one_pass() {
        // 10 recovered (drop from known), 20 still stopped (no-op), 30 newly
        // stopped (notify).
        let (to_notify, to_retract) = diff_stopped_notices(&[(1, 20), (1, 30)], &[10, 20]);
        assert_eq!(to_notify, vec![(1, 30)]);
        assert_eq!(to_retract, vec![10]);
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

    /// The interrupted-lead prompt is the plain continue prompt for a normal
    /// interrupted lead, but folds in the stalled worker ids when the lead still
    /// owns them — so a crash during a stall-resume turn (which lands on the
    /// interrupted path next boot) doesn't strand those workers without their ids.
    #[test]
    fn lead_revive_prompt_folds_in_stalled_ids_only_when_present() {
        assert_eq!(lead_revive_prompt(&[]), REVIVE_PROMPT); // regression: unchanged
        let p = lead_revive_prompt(&[5, 7]);
        assert!(p.starts_with(REVIVE_PROMPT));
        assert!(p.contains("5, 7"));
        assert!(p.contains("bus_post"));
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
