//! Tauri command surface for the M2 workspace model. Thin wrappers; all logic
//! lives in store::repo and materialize.

use crate::store::{entities, repo, Db};
use crate::materialize;
use tauri::State;

type R<T> = Result<T, String>;
fn e<E: ToString>(x: E) -> String { x.to_string() }

#[tauri::command]
pub async fn create_workspace(db: State<'_, Db>, name: String) -> R<entities::workspace::Model> {
    repo::create_workspace(&db, &name).await.map_err(e)
}

#[tauri::command]
pub async fn list_workspaces(db: State<'_, Db>) -> R<Vec<entities::workspace::Model>> {
    repo::list_workspaces(&db).await.map_err(e)
}

/// Register an existing local git repo: validate, record, profile. Shared by
/// add (existing) / clone / create — they all converge on "a path weft refs".
async fn register_repo(
    db: &Db,
    workspace_id: i32,
    name: &str,
    path: &str,
) -> R<entities::repo_ref::Model> {
    if !crate::git::is_git_repo(std::path::Path::new(path)) {
        return Err("not a git repository".into());
    }
    // default base ref = current branch of the repo
    let base = crate::git::current_branch(std::path::Path::new(path)).unwrap_or_else(|_| "main".into());
    let r = repo::add_repo_ref(db, workspace_id, name, path, &base, "claude").await.map_err(e)?;
    // Eager, deterministic profiling (ARCHITECTURE §4.9): best-effort, never
    // blocks adding the repo if inference/git hiccups.
    let _ = crate::curator::profile_repo(db, &r).await;
    Ok(r)
}

#[tauri::command]
pub async fn add_repo_ref(
    db: State<'_, Db>,
    workspace_id: i32,
    name: String,
    local_git_path: String,
) -> R<entities::repo_ref::Model> {
    register_repo(&db, workspace_id, &name, &local_git_path).await
}

/// Clone a remote git URL into `<dest>/<name>`, then register it.
#[tauri::command]
pub async fn clone_repo(
    db: State<'_, Db>,
    workspace_id: i32,
    url: String,
    dest: String,
    name: String,
) -> R<entities::repo_ref::Model> {
    let path = std::path::Path::new(&dest).join(&name);
    let p = path.clone();
    tokio::task::spawn_blocking(move || crate::git::clone_repo(&url, &p))
        .await
        .map_err(|err| err.to_string())?
        .map_err(e)?;
    register_repo(&db, workspace_id, &name, &path.to_string_lossy()).await
}

/// Create a new git repo at `<dest>/<name>` (init + empty initial commit), then
/// register it.
#[tauri::command]
pub async fn create_repo(
    db: State<'_, Db>,
    workspace_id: i32,
    name: String,
    dest: String,
) -> R<entities::repo_ref::Model> {
    let path = std::path::Path::new(&dest).join(&name);
    let p = path.clone();
    tokio::task::spawn_blocking(move || crate::git::init_repo(&p))
        .await
        .map_err(|err| err.to_string())?
        .map_err(e)?;
    register_repo(&db, workspace_id, &name, &path.to_string_lossy()).await
}

#[tauri::command]
pub async fn list_repo_profiles(
    db: State<'_, Db>,
    workspace_id: i32,
) -> R<Vec<crate::curator::ProfileView>> {
    crate::curator::list(&db, workspace_id).await.map_err(e)
}

#[tauri::command]
pub async fn repo_graph(db: State<'_, Db>, workspace_id: i32) -> R<crate::curator::Graph> {
    crate::curator::graph(&db, workspace_id).await.map_err(e)
}

#[tauri::command]
pub async fn reprofile_repo(db: State<'_, Db>, repo_id: i32) -> R<()> {
    let r = repo::get_repo(&db, repo_id).await.map_err(e)?.ok_or("repo not found")?;
    crate::curator::profile_repo(&db, &r).await.map_err(e)?;
    Ok(())
}

#[tauri::command]
pub async fn update_repo_profile(
    db: State<'_, Db>,
    repo_id: i32,
    summary: String,
    role: String,
) -> R<()> {
    crate::curator::edit_profile(&db, repo_id, &summary, &role).await.map_err(e)?;
    Ok(())
}

#[tauri::command]
pub async fn create_thread(db: State<'_, Db>, workspace_id: i32, title: String, kind: String) -> R<entities::thread::Model> {
    repo::create_thread(&db, workspace_id, &title, &kind).await.map_err(e)
}

#[tauri::command]
pub async fn list_threads(db: State<'_, Db>, workspace_id: i32) -> R<Vec<entities::thread::Model>> {
    repo::list_threads(&db, workspace_id).await.map_err(e)
}

#[derive(serde::Serialize)]
pub struct RepoLite {
    pub id: i32,
    pub name: String,
}

/// A thread's roll-up for the workspace board (cards = threads). Live state
/// (sessions / needs / asks) is overlaid client-side; this is the structure.
#[derive(serde::Serialize)]
pub struct ThreadOverview {
    pub thread_id: i32,
    pub title: String,
    pub kind: String,
    pub direction_ids: Vec<i32>,
    /// Stored lifecycle status of each direction (same order as direction_ids),
    /// so the workspace board derives the thread's phase deterministically.
    pub statuses: Vec<String>,
    /// distinct repos this thread WRITES (across its directions).
    pub write_repos: Vec<RepoLite>,
}

/// Portfolio view of a workspace: every thread with its directions + write set,
/// so the board can show roll-ups and the repositories each task writes.
#[tauri::command]
pub async fn workspace_overview(db: State<'_, Db>, workspace_id: i32) -> R<Vec<ThreadOverview>> {
    let threads = repo::list_threads(&db, workspace_id).await.map_err(e)?;
    let mut out = Vec::new();
    for t in threads {
        let dirs = repo::list_directions(&db, t.id).await.map_err(e)?;
        let mut seen = std::collections::BTreeMap::<i32, String>::new();
        for d in &dirs {
            if let Some(r) = repo::direction_repo_of(&db, d.id).await.map_err(e)? {
                seen.entry(r.id).or_insert(r.name);
            }
        }
        out.push(ThreadOverview {
            thread_id: t.id,
            title: t.title,
            kind: t.kind,
            direction_ids: dirs.iter().map(|d| d.id).collect(),
            statuses: dirs.iter().map(|d| d.status.clone()).collect(),
            write_repos: seen.into_iter().map(|(id, name)| RepoLite { id, name }).collect(),
        });
    }
    Ok(out)
}

#[tauri::command]
pub async fn list_repos(db: State<'_, Db>, workspace_id: i32) -> R<Vec<entities::repo_ref::Model>> {
    repo::list_repos(&db, workspace_id).await.map_err(e)
}

#[tauri::command]
pub async fn list_directions(db: State<'_, Db>, thread_id: i32) -> R<Vec<entities::direction::Model>> {
    repo::list_directions(&db, thread_id).await.map_err(e)
}

/// The lead's proposed decomposition for a thread, resolved against the
/// workspace repos (ARCHITECTURE §4.10, §5.1). None if nothing proposed yet.
#[tauri::command]
pub async fn get_proposal(
    db: State<'_, Db>,
    thread_id: i32,
) -> R<Option<crate::planner::ResolvedProposal>> {
    crate::planner::get_resolved(&db, thread_id).await.map_err(e)
}

/// Save a (human-edited) proposal back, keeping it in "proposed" state.
#[tauri::command]
pub async fn save_proposal(
    db: State<'_, Db>,
    thread_id: i32,
    proposal: crate::planner::Proposal,
) -> R<()> {
    crate::planner::save_proposal(&db, thread_id, &proposal).await.map_err(e)
}

/// Confirm the stored proposal: create its directions + materialize worktrees.
#[tauri::command]
pub async fn confirm_proposal(db: State<'_, Db>, thread_id: i32) -> R<Vec<i32>> {
    crate::planner::confirm(&db, thread_id).await.map_err(e)
}

/// The brief a worker for this direction would be dispatched with (§4.10).
#[tauri::command]
pub async fn preview_brief(db: State<'_, Db>, direction_id: i32) -> R<String> {
    crate::brief::assemble(&db, direction_id).await.map_err(e)
}

/// Executable verification results per write repo of a direction (§4.13).
#[derive(serde::Serialize)]
pub struct RepoChecks {
    pub repo: String,
    pub worktree: String,
    pub checks: Vec<crate::check::CheckResult>,
}

/// Run the inferred check rungs in each of a direction's write worktrees.
/// "worker done = checks green, not self-report." Runs off the async runtime.
#[tauri::command]
pub async fn verify_direction(db: State<'_, Db>, direction_id: i32) -> R<Vec<RepoChecks>> {
    let wts = repo::list_worktrees(&db, Some(direction_id)).await.map_err(e)?;
    let mut targets: Vec<(String, String)> = Vec::new();
    for w in wts {
        let name = repo::get_repo(&db, w.repo_id)
            .await
            .map_err(e)?
            .map(|r| r.name)
            .unwrap_or_else(|| format!("repo {}", w.repo_id));
        targets.push((name, w.path));
    }
    tauri::async_runtime::spawn_blocking(move || {
        targets
            .into_iter()
            .map(|(repo, worktree)| {
                let checks = crate::check::run_checks(std::path::Path::new(&worktree));
                RepoChecks { repo, worktree, checks }
            })
            .collect::<Vec<_>>()
    })
    .await
    .map_err(e)
}

// The built-in review-agent rung is gone: review now runs as the user's global
// review skill INSIDE the worker's own conversation (frontend sends the slash
// command), and the repo's PR harness stays the authority (§7: 别重造 review/CI).

#[tauri::command]
pub async fn create_direction(
    db: State<'_, Db>,
    thread_id: i32,
    name: String,
    tool: String,
    repo_id: i32,
    reason: String,
    mandate: Option<String>,
) -> R<entities::direction::Model> {
    let dir = repo::create_direction(
        &db,
        thread_id,
        &name,
        &tool,
        repo_id,
        &reason,
        mandate.as_deref().unwrap_or("plan+impl"),
    )
    .await
    .map_err(e)?;
    materialize::materialize_direction(&db, dir.id).await.map_err(e)?;
    Ok(dir)
}

/// Set a task's lifecycle status (human override; the agent does this via the
/// bus tool). queued | working | review | done — freely reversible.
#[tauri::command]
pub async fn set_task_status(db: State<'_, Db>, direction_id: i32, status: String) -> R<()> {
    repo::set_direction_status(&db, direction_id, &status).await.map_err(e)
}

/// The worker's worktree diff (file stats + unified patch) for the Diff tab.
#[tauri::command]
pub fn worktree_diff(cwd: String) -> R<crate::git::WorktreeDiff> {
    let p = std::path::Path::new(&cwd);
    let files = crate::git::repo_diff(p).map_err(e)?.files;
    let patch = crate::git::repo_patch(p).unwrap_or_default();
    Ok(crate::git::WorktreeDiff { files, patch })
}

/// Observe-mode (§4.4): the agent's own transcript, normalized to app-native
/// events so the chat view never depends on rendering the live TUI.
#[tauri::command]
pub async fn read_transcript(cwd: String, tool: String) -> R<Vec<crate::sidecar::NormEvent>> {
    Ok(crate::sidecar::read_transcript(std::path::Path::new(&cwd), &tool).await)
}

#[tauri::command]
pub async fn list_worktrees(db: State<'_, Db>, direction_id: Option<i32>) -> R<Vec<entities::worktree::Model>> {
    repo::list_worktrees(&db, direction_id).await.map_err(e)
}

#[tauri::command]
pub async fn repo_diff(db: State<'_, Db>, worktree_id: i32) -> R<crate::git::DiffSummary> {
    use sea_orm::EntityTrait;
    let w = entities::worktree::Entity::find_by_id(worktree_id).one(&db.0).await.map_err(e)?
        .ok_or("worktree not found")?;
    crate::git::repo_diff(std::path::Path::new(&w.path)).map_err(e)
}

#[tauri::command]
pub async fn delete_thread(db: State<'_, Db>, thread_id: i32) -> R<()> {
    let removed = repo::delete_thread_cascade(&db, thread_id).await.map_err(e)?;
    materialize::cleanup_worktrees(&db, &removed).await.map_err(e)
}

#[tauri::command]
pub fn thread_messages(
    bus: tauri::State<'_, crate::bus::BusRegistry>,
    thread_id: i32,
) -> R<Vec<crate::bus::Msg>> {
    Ok(bus.log(thread_id))
}

/// One thing waiting on the human, with enough context to act on it cold.
#[derive(serde::Serialize)]
pub struct NeedItem {
    pub ask_id: u64,
    pub thread_id: i32,
    pub thread_title: String,
    pub direction_id: i32,
    pub direction_name: String,
    pub text: String,
    pub ts: u64,
}

/// Aggregate every open agent→human question across the workspace's threads.
/// This is the data behind the "Needs-you" surface — a pure bus + structure
/// projection, no TUI parsing.
#[tauri::command]
pub async fn needs_you(
    db: State<'_, Db>,
    bus: tauri::State<'_, crate::bus::BusRegistry>,
    workspace_id: i32,
) -> R<Vec<NeedItem>> {
    let threads = repo::list_threads(&db, workspace_id).await.map_err(e)?;
    let mut items: Vec<NeedItem> = Vec::new();
    for t in threads {
        let asks = bus.open_asks(t.id);
        if asks.is_empty() {
            continue;
        }
        let dirs = repo::list_directions(&db, t.id).await.map_err(e)?;
        for a in asks {
            let dir_id = a.from.parse::<i32>().unwrap_or(-1);
            let dir_name = dirs
                .iter()
                .find(|d| d.id == dir_id)
                .map(|d| d.name.clone())
                .unwrap_or_else(|| a.from.clone());
            items.push(NeedItem {
                ask_id: a.id,
                thread_id: t.id,
                thread_title: t.title.clone(),
                direction_id: dir_id,
                direction_name: dir_name,
                text: a.text,
                ts: a.ts,
            });
        }
    }
    items.sort_by_key(|i| i.ts);
    Ok(items)
}

/// One pending write declaration waiting on the human, with thread context.
#[derive(serde::Serialize)]
pub struct WriteTrigger {
    pub thread_id: i32,
    pub thread_title: String,
    pub index: usize,
    pub name: String,
    pub repo_name: String,
    pub reason: String,
}

/// Every pending write declaration across the workspace's threads — the
/// data behind the Needs-you "approve a write" cards.
#[tauri::command]
pub async fn write_triggers(
    db: State<'_, Db>,
    workspace_id: i32,
) -> R<Vec<WriteTrigger>> {
    let threads = repo::list_threads(&db, workspace_id).await.map_err(e)?;
    let mut out = Vec::new();
    for t in threads {
        for p in crate::planner::pending_writes(&db, t.id).await.map_err(e)? {
            out.push(WriteTrigger {
                thread_id: t.id,
                thread_title: t.title.clone(),
                index: p.index,
                name: p.name,
                repo_name: p.repo_name,
                reason: p.reason,
            });
        }
    }
    Ok(out)
}

/// Approve a write declaration: create its direction + materialize. Returns the
/// new direction id so the caller can dispatch a worker.
#[tauri::command]
pub async fn approve_write_trigger(
    db: State<'_, Db>,
    thread_id: i32,
    index: usize,
) -> R<i32> {
    crate::planner::approve_direction(&db, thread_id, index).await.map_err(e)
}

/// Deny a write declaration: mark denied + relay to the lead's bus inbox.
#[tauri::command]
pub async fn deny_write_trigger(
    db: State<'_, Db>,
    bus: tauri::State<'_, crate::bus::BusRegistry>,
    thread_id: i32,
    index: usize,
) -> R<()> {
    let (name, repo) = crate::planner::deny_direction(&db, thread_id, index).await.map_err(e)?;
    let msg = format!(
        "The human DENIED the write declaration \"{name}\" (repo {repo}). Do not create it; revise the plan or ask why.",
    );
    bus.post(thread_id, crate::bus::HUMAN, "lead", &msg, "message");
    Ok(())
}

/// Answer an open ask; the reply lands in the asking direction's bus inbox.
#[tauri::command]
pub fn answer_ask(
    bus: tauri::State<'_, crate::bus::BusRegistry>,
    thread_id: i32,
    ask_id: u64,
    text: String,
) -> R<()> {
    if bus.answer_ask(thread_id, ask_id, &text) {
        Ok(())
    } else {
        Err("that question was already answered or no longer exists".into())
    }
}

/// All pending permission Asks across the workspace (the Ask Bridge → Needs-you),
/// enriched with the owning thread's title and the asking task's name so the card
/// says which thread / which task is asking.
#[tauri::command]
pub async fn pending_asks(
    db: State<'_, Db>,
    asks: tauri::State<'_, crate::ask::AskRegistry>,
) -> R<Vec<crate::ask::Ask>> {
    let mut open = asks.open();
    for a in &mut open {
        if let Ok(Some(t)) = repo::get_thread(&db, a.thread).await {
            a.thread_title = t.title;
        }
        if let Ok(id) = a.dir.parse::<i32>() {
            if let Ok(Some(d)) = repo::get_direction(&db, id).await {
                a.dir_name = d.name;
            }
        }
    }
    Ok(open)
}

/// Dangerous mode (global): every agent's tool asks auto-allow, no prompts.
#[tauri::command]
pub fn set_dangerous_mode(asks: tauri::State<'_, crate::ask::AskRegistry>, on: bool) -> R<()> {
    asks.set_dangerous(on);
    Ok(())
}

/// Runaway-guardrail caps (§7 跑飞护栏), enforced per busy turn by the chat
/// engine's watchdog (lead_chat::engine::spawn_watchdog). Configurable at
/// runtime from Settings; seeded from the WEFT_* env defaults so an env
/// override still sets the initial value. 0 on either disables that cap.
pub struct GuardrailState {
    inner: std::sync::Mutex<(u64, u64)>, // (idle_secs, wall_secs)
}

impl Default for GuardrailState {
    fn default() -> Self {
        Self {
            inner: std::sync::Mutex::new((
                env_secs("WEFT_IDLE_WATCHDOG_SECS", 1800), // 30 min
                env_secs("WEFT_WALL_CAP_SECS", 7200),      // 2 h
            )),
        }
    }
}

impl GuardrailState {
    pub fn set(&self, idle_secs: u64, wall_secs: u64) {
        *self.inner.lock().unwrap_or_else(|e| e.into_inner()) = (idle_secs, wall_secs);
    }
    /// (idle_cap_secs, wall_cap_secs)
    pub fn get(&self) -> (u64, u64) {
        *self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

fn env_secs(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(default)
}

/// Runaway guardrails (§7): idle + wall-clock caps in seconds; 0 disables that
/// cap. See the GuardrailState note on enforcement.
#[tauri::command]
pub fn set_guardrails(
    guard: tauri::State<'_, GuardrailState>,
    idle_secs: u64,
    wall_secs: u64,
) -> R<()> {
    guard.set(idle_secs, wall_secs);
    Ok(())
}

/// Read-only snapshot backing the observe surface: the worktree to read
/// transcript/diff from, plus the latest session's identity/status if any.
/// `None` only when the (direction, repo) has no materialized worktree.
#[derive(serde::Serialize, Clone)]
pub struct ObserveRef {
    pub worktree: String,
    pub branch: String,
    pub tool: String,
    pub session_id: Option<i32>,
    pub native_id: Option<String>,
    pub status: Option<String>,
}

#[tauri::command]
pub async fn session_for(
    db: State<'_, Db>,
    direction_id: i32,
    repo_id: i32,
) -> R<Option<ObserveRef>> {
    let wt = match repo::worktree_for(&db, direction_id, repo_id).await.map_err(e)? {
        Some(w) => w,
        None => return Ok(None),
    };
    let dir = match repo::get_direction(&db, direction_id).await.map_err(e)? {
        Some(d) => d,
        None => return Ok(None),
    };
    let latest = repo::latest_session_for(&db, direction_id, repo_id).await.map_err(e)?;
    Ok(Some(ObserveRef {
        worktree: wt.path,
        branch: wt.branch,
        tool: dir.tool,
        session_id: latest.as_ref().map(|s| s.id),
        native_id: latest.as_ref().and_then(|s| s.native_session_id.clone()),
        status: latest.as_ref().map(|s| s.status.clone()),
    }))
}

/// Effective config for a repo (M6 有效配置预览): the skills + rules that apply,
/// each tagged with the layer it comes from (personal / repo) and whether a
/// higher layer shadows it.
#[tauri::command]
pub fn effective_config(repo_path: String) -> R<Vec<crate::config::ConfigItem>> {
    let home = dirs::home_dir().ok_or_else(|| "no home dir".to_string())?;
    Ok(crate::config::effective_for(
        std::path::Path::new(&repo_path),
        &home,
    ))
}

/// Pending "needs you" count per workspace (agent questions + tool asks), so the
/// workspace switcher can flag OTHER workspaces that want attention.
#[tauri::command]
pub async fn workspace_needs_counts(
    db: State<'_, Db>,
    bus: tauri::State<'_, crate::bus::BusRegistry>,
    asks: tauri::State<'_, crate::ask::AskRegistry>,
) -> R<Vec<(i32, u32)>> {
    use std::collections::HashSet;
    let open_asks = asks.open();
    let mut out = Vec::new();
    for w in repo::list_workspaces(&db).await.map_err(e)? {
        let threads = repo::list_threads(&db, w.id).await.map_err(e)?;
        let tids: HashSet<i32> = threads.iter().map(|t| t.id).collect();
        let mut count: u32 = 0;
        for t in &threads {
            count += bus.open_asks(t.id).len() as u32;
            count += crate::planner::pending_writes(&db, t.id).await.map_err(e)?.len() as u32;
        }
        count += open_asks.iter().filter(|a| tids.contains(&a.thread)).count() as u32;
        out.push((w.id, count));
    }
    Ok(out)
}

/// Answer a pending permission Ask. `answer` is allow | deny | always | full —
/// always remembers this action for the task, full grants it full access.
#[tauri::command]
pub fn answer_permission(
    asks: tauri::State<'_, crate::ask::AskRegistry>,
    ask_id: u64,
    answer: String,
) -> R<()> {
    let a = crate::ask::Answer::parse(&answer).ok_or("unknown answer")?;
    if asks.answer(ask_id, a) {
        Ok(())
    } else {
        Err("that request was already answered or has expired".into())
    }
}

#[tauri::command]
pub fn bus_post_human(
    bus: tauri::State<'_, crate::bus::BusRegistry>,
    thread_id: i32,
    to: Option<String>,
    text: String,
) -> R<()> {
    match to {
        Some(target) if !target.is_empty() && target != "*" => {
            bus.post(thread_id, "you", &target, &text, "message");
        }
        _ => {
            bus.broadcast(thread_id, "you", &text, "message");
        }
    }
    Ok(())
}
