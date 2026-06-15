//! Tauri command surface for the M2 workspace model. Thin wrappers; all logic
//! lives in store::repo and materialize.

use crate::materialize;
use crate::store::{entities, repo, Db};
use tauri::Manager;
use tauri::State;

type R<T> = Result<T, String>;
fn e<E: ToString>(x: E) -> String {
    x.to_string()
}

#[tauri::command]
pub async fn create_workspace(db: State<'_, Db>, name: String) -> R<entities::workspace::Model> {
    repo::create_workspace(&db, &name).await.map_err(e)
}

#[tauri::command]
pub async fn rename_workspace(
    db: State<'_, Db>,
    workspace_id: i32,
    name: String,
) -> R<entities::workspace::Model> {
    repo::rename_workspace(&db, workspace_id, &name)
        .await
        .map_err(e)
}

#[tauri::command]
pub async fn list_workspaces(db: State<'_, Db>) -> R<Vec<entities::workspace::Model>> {
    let hidden = repo::get_setting(&db, repo::K_CONCIERGE_WORKSPACE)
        .await
        .map_err(e)?
        .and_then(|s| s.parse::<i32>().ok());
    let workspaces = repo::list_workspaces(&db).await.map_err(e)?;
    Ok(workspaces
        .into_iter()
        .filter(|w| Some(w.id) != hidden)
        .collect())
}

/// Return the id of the most-recently created workspace. This never creates a
/// synthetic "Default" workspace; callers that need one must ask the user for
/// an explicit name. Kept as a free function so integration tests can drive it
/// without a Tauri runtime.
pub async fn ensure_default_workspace_inner(db: &Db) -> R<i32> {
    if let Some(w) = repo::latest_workspace(db).await.map_err(e)? {
        return Ok(w.id);
    }
    Err("workspace required".into())
}

#[tauri::command]
pub async fn ensure_default_workspace(db: State<'_, Db>) -> R<i32> {
    ensure_default_workspace_inner(&db).await
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
    let base =
        crate::git::current_branch(std::path::Path::new(path)).unwrap_or_else(|_| "main".into());
    let r = repo::add_repo_ref(db, workspace_id, name, path, &base)
        .await
        .map_err(e)?;
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

/// Cheap pre-check used by first-run onboarding to validate every picked folder
/// *before* a workspace is created — so a non-git folder can't leave an orphan
/// workspace behind. Mirrors the guard inside `register_repo`.
#[tauri::command]
pub fn check_git_repo(path: String) -> bool {
    crate::git::is_git_repo(std::path::Path::new(&path))
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
    let r = repo::get_repo(&db, repo_id)
        .await
        .map_err(e)?
        .ok_or("repo not found")?;
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
    crate::curator::edit_profile(&db, repo_id, &summary, &role)
        .await
        .map_err(e)?;
    Ok(())
}

#[tauri::command]
pub async fn create_thread(
    db: State<'_, Db>,
    workspace_id: i32,
    title: String,
    kind: String,
) -> R<entities::thread::Model> {
    let tool = crate::tools::default_tool(&db).await;
    repo::create_thread(&db, workspace_id, &title, &kind, &tool)
        .await
        .map_err(e)
}

#[tauri::command]
pub async fn rename_thread(
    db: State<'_, Db>,
    thread_id: i32,
    title: String,
) -> R<entities::thread::Model> {
    repo::rename_thread(&db, thread_id, &title).await.map_err(e)
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
            write_repos: seen
                .into_iter()
                .map(|(id, name)| RepoLite { id, name })
                .collect(),
        });
    }
    Ok(out)
}

#[tauri::command]
pub async fn list_repos(db: State<'_, Db>, workspace_id: i32) -> R<Vec<entities::repo_ref::Model>> {
    repo::list_repos(&db, workspace_id).await.map_err(e)
}

#[tauri::command]
pub async fn list_directions(
    db: State<'_, Db>,
    thread_id: i32,
) -> R<Vec<entities::direction::Model>> {
    repo::list_directions(&db, thread_id).await.map_err(e)
}

/// The lead's proposed decomposition for a thread, resolved against the
/// workspace repos (ARCHITECTURE §4.10, §5.1). None if nothing proposed yet.
#[tauri::command]
pub async fn get_proposal(
    db: State<'_, Db>,
    thread_id: i32,
) -> R<Option<crate::planner::ResolvedProposal>> {
    crate::planner::get_resolved(&db, thread_id)
        .await
        .map_err(e)
}

/// Save a (human-edited) proposal back, keeping it in "proposed" state.
#[tauri::command]
pub async fn save_proposal(
    db: State<'_, Db>,
    thread_id: i32,
    proposal: crate::planner::Proposal,
) -> R<()> {
    crate::planner::save_proposal(&db, thread_id, &proposal)
        .await
        .map_err(e)
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
    let wts = repo::list_worktrees(&db, Some(direction_id))
        .await
        .map_err(e)?;
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
                RepoChecks {
                    repo,
                    worktree,
                    checks,
                }
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
    materialize::materialize_direction(&db, dir.id)
        .await
        .map_err(e)?;
    Ok(dir)
}

/// Set a task's lifecycle status (human override; the agent does this via the
/// bus tool). queued | working | review | done — freely reversible.
#[tauri::command]
pub async fn set_task_status(db: State<'_, Db>, direction_id: i32, status: String) -> R<()> {
    repo::set_direction_status(&db, direction_id, &status)
        .await
        .map_err(e)
}

#[tauri::command]
pub async fn rename_direction(
    db: State<'_, Db>,
    direction_id: i32,
    name: String,
) -> R<entities::direction::Model> {
    repo::rename_direction(&db, direction_id, &name)
        .await
        .map_err(e)
}

/// The worker's worktree diff (file stats + unified patch) for the Diff tab.
#[tauri::command]
pub fn worktree_diff(cwd: String) -> R<crate::git::WorktreeDiff> {
    let p = std::path::Path::new(&cwd);
    let files = crate::git::repo_diff(p).map_err(e)?.files;
    let patch = crate::git::repo_patch(p).unwrap_or_default();
    Ok(crate::git::WorktreeDiff { files, patch })
}

/// Diff panel "vs target" response: PR-style diff against the task's target
/// branch, plus the resolved comparison ref and the editor's current/default
/// values so the frontend has everything in one round-trip.
#[derive(serde::Serialize)]
pub struct TargetDiffResp {
    pub files: Vec<crate::git::FileDiff>,
    pub patch: String,
    /// The ref actually compared against, e.g. `origin/main`.
    pub resolved: String,
    /// The direction's stored target branch ("" = using the default).
    pub target: String,
    /// The effective default target branch (repo base) shown as the placeholder.
    pub default_branch: String,
}

/// The worker's diff against its task's target branch (PR-style: the task's
/// changes since the merge-base with the target's latest remote, incl.
/// uncommitted edits). `fetch` refreshes `origin/<target>` first — the frontend
/// passes true on mode-enter / manual refresh / after a target edit, false on
/// the live poll. Runs the git work off the async runtime.
#[tauri::command]
pub async fn worktree_diff_target(
    db: State<'_, Db>,
    cwd: String,
    direction_id: i32,
    fetch: bool,
) -> R<TargetDiffResp> {
    let (stored, base_ref) = repo::direction_target_branch(&db, direction_id)
        .await
        .map_err(e)?;
    let cwd2 = cwd.clone();
    let (default_branch, effective) = {
        let p = std::path::Path::new(&cwd);
        let default_branch = crate::git::default_target_branch(p, &base_ref);
        let effective = if stored.trim().is_empty() {
            default_branch.clone()
        } else {
            stored.trim().to_string()
        };
        (default_branch, effective)
    };
    // git (subprocess; fetch may hit the network) off the async worker threads.
    let td = tokio::task::spawn_blocking(move || {
        crate::git::target_diff(std::path::Path::new(&cwd2), &effective, fetch)
    })
    .await
    .map_err(|err| err.to_string())?
    .map_err(e)?;
    Ok(TargetDiffResp {
        files: td.files,
        patch: td.patch,
        resolved: td.resolved,
        target: stored,
        default_branch,
    })
}

/// Persist a task's diff target branch ("" = use the repo default).
#[tauri::command]
pub async fn set_direction_target_branch(
    db: State<'_, Db>,
    direction_id: i32,
    target: String,
) -> R<()> {
    repo::set_direction_target_branch(&db, direction_id, &target)
        .await
        .map_err(e)
}

/// Observe-mode (§4.4): the agent's own transcript, normalized to app-native
/// events so the chat view never depends on rendering the live TUI.
#[tauri::command]
pub async fn read_transcript(cwd: String, tool: String) -> R<Vec<crate::sidecar::NormEvent>> {
    Ok(crate::sidecar::read_transcript(std::path::Path::new(&cwd), &tool).await)
}

#[tauri::command]
pub async fn list_worktrees(
    db: State<'_, Db>,
    direction_id: Option<i32>,
) -> R<Vec<entities::worktree::Model>> {
    repo::list_worktrees(&db, direction_id).await.map_err(e)
}

#[tauri::command]
pub async fn repo_diff(db: State<'_, Db>, worktree_id: i32) -> R<crate::git::DiffSummary> {
    use sea_orm::EntityTrait;
    let w = entities::worktree::Entity::find_by_id(worktree_id)
        .one(&db.0)
        .await
        .map_err(e)?
        .ok_or("worktree not found")?;
    crate::git::repo_diff(std::path::Path::new(&w.path)).map_err(e)
}

#[tauri::command]
pub async fn delete_thread(db: State<'_, Db>, thread_id: i32) -> R<()> {
    let removed = repo::delete_thread_cascade(&db, thread_id)
        .await
        .map_err(e)?;
    materialize::cleanup_worktrees(&db, &removed)
        .await
        .map_err(e)
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

/// The resolved default coding tool plus the user's explicit choice (if any).
/// `tool` is what new threads/directions get; `configured != tool` means the
/// configured CLI is missing and we fell back.
#[derive(serde::Serialize)]
pub struct DefaultTool {
    pub tool: String,
    pub configured: Option<String>,
}

#[tauri::command]
pub async fn get_default_tool(db: State<'_, Db>) -> R<DefaultTool> {
    let configured = repo::get_setting(&db, "default_tool").await.map_err(e)?;
    let tool = crate::detect::resolve_default_tool(configured.as_deref());
    Ok(DefaultTool { tool, configured })
}

#[tauri::command]
pub async fn set_default_tool(db: State<'_, Db>, tool: String) -> R<()> {
    if !crate::detect::TOOL_PRIORITY.contains(&tool.as_str()) {
        return Err(format!(
            "unknown tool {tool:?}; expected one of {:?}",
            crate::detect::TOOL_PRIORITY
        ));
    }
    repo::set_setting(&db, "default_tool", &tool)
        .await
        .map_err(e)
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
pub async fn write_triggers(db: State<'_, Db>, workspace_id: i32) -> R<Vec<WriteTrigger>> {
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
    tool: String,
) -> R<i32> {
    crate::planner::approve_direction(&db, thread_id, index, &tool)
        .await
        .map_err(e)
}

/// Deny a write declaration: mark denied + relay to the lead's bus inbox.
#[tauri::command]
pub async fn deny_write_trigger(
    db: State<'_, Db>,
    bus: tauri::State<'_, crate::bus::BusRegistry>,
    thread_id: i32,
    index: usize,
) -> R<()> {
    let (name, repo) = crate::planner::deny_direction(&db, thread_id, index)
        .await
        .map_err(e)?;
    let msg = format!(
        "The human DENIED the write declaration \"{name}\" (repo {repo}). Do not create it; revise the plan or ask why.",
    );
    bus.post(thread_id, crate::bus::HUMAN, "lead", &msg, "message");
    Ok(())
}

/// Mark a lead action_card as resolved once its repo flow succeeded, persisting
/// the settled state into the row so it survives reload (no re-click double-add).
#[tauri::command]
pub async fn resolve_action_card(db: State<'_, Db>, message_id: i32, name: String) -> R<()> {
    if let Some(m) = repo::resolve_action_card(&db, message_id, &name)
        .await
        .map_err(e)?
    {
        if let Some(app) = crate::APP_HANDLE.get() {
            use tauri::Emitter;
            let _ = app.emit(
                crate::lead_chat::engine::EVENT,
                crate::lead_chat::engine::Push::ToolResult {
                    thread_id: m.thread_id,
                    message_id: m.id,
                    content: m.content,
                    status: m.status,
                },
            );
        }
    }
    Ok(())
}

/// Answer an open ask; the reply lands in the asking direction's bus inbox. The
/// durable settled-trail row is written by the `trail` consumer off the bus's
/// resolution event, so every answer path (desktop / remote / IM) is covered.
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

/// Keep-awake (global): hold a "prevent idle sleep" OS assertion while any
/// session is busy (display may still sleep). Re-pushed from the frontend on
/// every launch — the backend state is in-memory, default ON.
#[tauri::command]
pub fn set_keep_awake(power: tauri::State<'_, crate::power::PowerGuard>, on: bool) -> R<()> {
    power.set_enabled(on);
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
    // —— 会话信息面板回填(worker 重挂不空白)——
    pub context_tokens: Option<u64>,
    pub window: Option<u64>,
    pub model: Option<String>,
    pub mcp_servers: Vec<crate::lead_chat::proto::McpServer>,
    /// claude `mcp__<server>__<tool>` 名(分组成每个 server 的 tool 列表);重挂后
    /// 即便 init 已不再重放也能展开 tool。
    pub tools: Vec<String>,
}

#[tauri::command]
pub async fn session_for(
    app: tauri::AppHandle,
    db: State<'_, Db>,
    direction_id: i32,
    repo_id: i32,
) -> R<Option<ObserveRef>> {
    let wt = match repo::worktree_for(&db, direction_id, repo_id)
        .await
        .map_err(e)?
    {
        Some(w) => w,
        None => return Ok(None),
    };
    let dir = match repo::get_direction(&db, direction_id).await.map_err(e)? {
        Some(d) => d,
        None => return Ok(None),
    };
    let latest = repo::latest_session_for(&db, direction_id, repo_id)
        .await
        .map_err(e)?;
    // 有活引擎(claude worker)就读它缓存的会话信息快照;否则给空(由 init/usage
    // event 在首条消息后补全)。
    let (context_tokens, window, model, mcp_servers, tools) = match latest
        .as_ref()
        .map(|s| s.id)
        .and_then(|sid| {
            app.state::<crate::lead_chat::engine::LeadChatState>()
                .get(sid as i64)
        }) {
        Some(eng) => {
            let g = eng.lock().await;
            (
                g.last_context_tokens,
                g.last_window,
                g.last_model.clone(),
                g.last_mcp_servers.clone(),
                g.last_tools.clone(),
            )
        }
        None => (None, None, None, vec![], vec![]),
    };
    Ok(Some(ObserveRef {
        worktree: wt.path,
        branch: wt.branch,
        tool: dir.tool,
        session_id: latest.as_ref().map(|s| s.id),
        native_id: latest.as_ref().and_then(|s| s.native_session_id.clone()),
        status: latest.as_ref().map(|s| s.status.clone()),
        context_tokens,
        window,
        model,
        mcp_servers,
        tools,
    }))
}

/// 会话信息面板(M2):codex/opencode 的带外 meta(Context / model / window / MCP
/// server,**不含 tool**)。claude 不走这里——其 meta 全在事件流 + 引擎缓存。
#[tauri::command]
pub async fn session_meta(
    db: State<'_, Db>,
    direction_id: i32,
    repo_id: i32,
) -> R<crate::session_meta::SessionMetaSnapshot> {
    let wt = repo::worktree_for(&db, direction_id, repo_id)
        .await
        .map_err(e)?;
    let dir = repo::get_direction(&db, direction_id).await.map_err(e)?;
    let (Some(wt), Some(dir)) = (wt, dir) else {
        return Ok(Default::default());
    };
    let native = repo::latest_session_for(&db, direction_id, repo_id)
        .await
        .map_err(e)?
        .and_then(|s| s.native_session_id.clone());
    Ok(crate::session_meta::gather(&dir.tool, &wt.path, native.as_deref()).await)
}

/// Effective config for a repo (M6 有效配置预览): the skills + rules that apply,
/// each tagged with the layer it comes from (personal / weft-global /
/// weft-workspace / repo) and whether a higher layer shadows it. `ws_id`
/// is optional — when absent, weft-managed layers are omitted (personal + repo
/// only), keeping backward-compat with existing frontend calls that don't pass it.
#[tauri::command]
pub async fn effective_config(
    db: State<'_, Db>,
    repo_path: String,
    ws_id: Option<i32>,
) -> R<Vec<crate::config::ConfigItem>> {
    let home = dirs::home_dir().ok_or_else(|| "no home".to_string())?;
    let weft: Vec<(String, String, String)> = match ws_id {
        Some(w) => crate::skills::enabled_for_workspace(&db, w)
            .await
            .map_err(e)?
            .into_iter()
            .filter(|s| !s.overridden)
            .map(|s| {
                let layer = if s.global {
                    "weft-global"
                } else {
                    "weft-workspace"
                };
                (s.name, layer.to_string(), s.dir)
            })
            .collect(),
        None => Vec::new(),
    };
    Ok(crate::config::effective_for_with_weft(
        std::path::Path::new(&repo_path),
        &home,
        &weft,
    ))
}

// --- Skills (git-hosted skill sources): source CRUD, sync, parse preview, enable ---

#[tauri::command]
pub async fn list_skill_sources(db: State<'_, Db>) -> R<Vec<entities::skill_source::Model>> {
    repo::list_skill_sources(&db).await.map_err(e)
}

#[tauri::command]
pub async fn add_skill_source(
    db: State<'_, Db>,
    git_url: String,
    git_ref: Option<String>,
) -> R<entities::skill_source::Model> {
    let src = repo::add_skill_source(&db, &git_url, git_ref.as_deref())
        .await
        .map_err(e)?;
    let _ = crate::skills::sync_source(&db, src.id).await;
    repo::get_skill_source(&db, src.id)
        .await
        .map_err(e)?
        .ok_or_else(|| "source vanished".to_string())
}

#[tauri::command]
pub async fn remove_skill_source(db: State<'_, Db>, id: i32) -> R<()> {
    // best-effort cache removal, then DB
    if let Ok(home) = crate::paths::skills_home() {
        let _ = std::fs::remove_dir_all(home.join(id.to_string()));
    }
    repo::remove_skill_source(&db, id).await.map_err(e)
}

#[tauri::command]
pub async fn sync_skill_source(db: State<'_, Db>, id: i32) -> R<entities::skill_source::Model> {
    crate::skills::sync_source(&db, id).await.map_err(e)?;
    repo::get_skill_source(&db, id)
        .await
        .map_err(e)?
        .ok_or_else(|| "source not found".to_string())
}

#[tauri::command]
pub async fn sync_all_skill_sources(db: State<'_, Db>) -> R<Vec<entities::skill_source::Model>> {
    for s in repo::list_skill_sources(&db).await.map_err(e)? {
        let _ = crate::skills::sync_source(&db, s.id).await;
    }
    repo::list_skill_sources(&db).await.map_err(e)
}

#[tauri::command]
pub async fn list_parsed_skills(id: i32) -> R<Vec<crate::skills::parse::ParsedSkill>> {
    let home = crate::paths::skills_home().map_err(e)?;
    Ok(crate::skills::parse::parse_source(
        &home.join(id.to_string()),
    ))
}

#[tauri::command]
pub async fn set_skill_enabled(
    db: State<'_, Db>,
    source_id: i32,
    name: String,
    scope: String,
    on: bool,
) -> R<()> {
    repo::set_skill_enable(&db, source_id, &name, &scope, on)
        .await
        .map_err(e)
}

#[tauri::command]
pub async fn workspace_skills(
    db: State<'_, Db>,
    ws_id: i32,
) -> R<Vec<crate::skills::EnabledSkill>> {
    crate::skills::enabled_for_workspace(&db, ws_id)
        .await
        .map_err(e)
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
            count += crate::planner::pending_writes(&db, t.id)
                .await
                .map_err(e)?
                .len() as u32;
        }
        count += open_asks
            .iter()
            .filter(|a| tids.contains(&a.thread))
            .count() as u32;
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

// ───────────────────────── IM · 飞书设置（Task 10）─────────────────────────

/// IM 设置视图：secret 只回是否已设置，不回明文（与 ImSettings::Debug 同纪律）。
#[derive(serde::Serialize)]
pub struct ImSettingsView {
    pub app_id: String,
    pub has_secret: bool,
    pub bound: bool,
    pub enabled: bool,
    /// 远程待命（im.remote_standby）：桥启用期间保持系统唤醒。
    pub remote_standby: bool,
}

#[tauri::command]
pub async fn im_get_settings(db: State<'_, Db>) -> R<ImSettingsView> {
    let s = crate::im::ImSettings::load(&db).await.map_err(e)?;
    Ok(ImSettingsView {
        app_id: s.app_id,
        has_secret: !s.app_secret.is_empty(),
        bound: !s.allow_open_ids.is_empty(),
        enabled: s.enabled,
        remote_standby: s.remote_standby,
    })
}

/// 保存凭证并重启桥。secret 传空字符串 = 保持原值（不覆盖已存的密钥）。
/// 是否真正连接由 `im.feishu.enabled` 和双凭证共同决定。
#[tauri::command]
pub async fn im_set_settings(
    app: tauri::AppHandle,
    db: State<'_, Db>,
    app_id: String,
    app_secret: String,
) -> R<()> {
    repo::set_setting(&db, crate::im::K_APP_ID, app_id.trim())
        .await
        .map_err(e)?;
    if !app_secret.is_empty() {
        repo::set_setting(&db, crate::im::K_APP_SECRET, app_secret.trim())
            .await
            .map_err(e)?;
    }
    crate::im::spawn(app);
    Ok(())
}

/// 开关桥：写 enabled 标志并重启。off = 断开但保留凭证；on = 凭证齐全则连接
/// （缺凭证时置 disabled，等用户在已展开的表单里补齐再保存）。
#[tauri::command]
pub async fn im_set_enabled(app: tauri::AppHandle, db: State<'_, Db>, enabled: bool) -> R<()> {
    repo::set_setting(&db, crate::im::K_ENABLED, if enabled { "1" } else { "0" })
        .await
        .map_err(e)?;
    crate::im::spawn(app);
    Ok(())
}

/// 远程待命：桥启用期间持有「防空闲休眠」断言，保证飞书指令随时可达。
/// 纯电源层开关——不重启桥、不断 WS；写库后立即收敛 PowerGuard。
#[tauri::command]
pub async fn im_set_remote_standby(
    app: tauri::AppHandle,
    db: State<'_, Db>,
    enabled: bool,
) -> R<()> {
    repo::set_setting(
        &db,
        crate::im::K_REMOTE_STANDBY,
        if enabled { "1" } else { "0" },
    )
    .await
    .map_err(e)?;
    let s = crate::im::ImSettings::load(&db).await.map_err(e)?;
    crate::power::set_standby(&app, enabled && s.enabled && s.ready());
    Ok(())
}

#[tauri::command]
pub fn im_status(bridge: State<'_, crate::im::ImBridge>) -> R<String> {
    Ok(bridge.status())
}

// ───────────────────────── IM · 话题绑定（M2-5）─────────────────────────
//
// 把 issue（lead 的 thread_id）绑到一个飞书话题：之后该话题里的群消息会被
// 路由进 lead engine，lead 的回流文本也会反向贴回这条话题（M2-4）。绑定关系
// 是 1:1（同一 thread 重 bind 覆盖旧目标，同一目标只能映射一个 thread——表上
// 双唯一约束保证）。前端用 chat_id + 话题根 message_id 当 im_thread_ref 调本组。

#[derive(serde::Serialize)]
pub struct ImRouteView {
    pub thread_id: i32,
    pub channel: String,
    pub chat_id: String,
    pub im_thread_ref: String,
    pub created_at: String,
}

fn route_view(m: entities::im_route::Model) -> ImRouteView {
    ImRouteView {
        thread_id: m.thread_id,
        channel: m.channel,
        chat_id: m.chat_id,
        im_thread_ref: m.im_thread_ref,
        created_at: m.created_at,
    }
}

#[tauri::command]
pub async fn im_bind_thread(
    db: State<'_, Db>,
    thread_id: i32,
    channel: String,
    chat_id: String,
    im_thread_ref: String,
) -> R<ImRouteView> {
    let ch = channel.trim();
    let chat = chat_id.trim();
    let r = im_thread_ref.trim();
    if ch.is_empty() || chat.is_empty() || r.is_empty() {
        return Err("channel/chat_id/im_thread_ref must be non-empty".into());
    }
    let m = repo::bind_im_route(&db, thread_id, ch, chat, r)
        .await
        .map_err(e)?;
    Ok(route_view(m))
}

#[tauri::command]
pub async fn im_unbind_thread(db: State<'_, Db>, thread_id: i32) -> R<()> {
    repo::unbind_im_route(&db, thread_id).await.map_err(e)
}

#[tauri::command]
pub async fn im_route_for_thread(db: State<'_, Db>, thread_id: i32) -> R<Option<ImRouteView>> {
    let m = repo::im_route_of_thread(&db, thread_id).await.map_err(e)?;
    Ok(m.map(route_view))
}

#[tauri::command]
pub async fn im_list_routes(db: State<'_, Db>) -> R<Vec<ImRouteView>> {
    let rows = repo::list_im_routes(&db).await.map_err(e)?;
    Ok(rows.into_iter().map(route_view).collect())
}

// --- Encryption ---

#[derive(serde::Serialize)]
pub struct DbEncryptionStatus {
    pub encrypted: bool,
}

#[tauri::command]
pub fn db_encryption_status(db: State<'_, Db>) -> R<DbEncryptionStatus> {
    Ok(DbEncryptionStatus {
        encrypted: db.encrypted(),
    })
}

#[derive(serde::Serialize)]
pub struct DbEncryptionMutationResult {
    pub restart_required: bool,
}

#[tauri::command]
pub async fn db_enable_encryption(password: String) -> R<DbEncryptionMutationResult> {
    let path = crate::paths::db_path().map_err(e)?;
    crate::store::encryption::enable(&path, &password)
        .await
        .map_err(e)?;
    Ok(DbEncryptionMutationResult {
        restart_required: true,
    })
}

#[tauri::command]
pub async fn db_disable_encryption(password: String) -> R<DbEncryptionMutationResult> {
    let path = crate::paths::db_path().map_err(e)?;
    crate::store::encryption::disable(&path, &password)
        .await
        .map_err(e)?;
    Ok(DbEncryptionMutationResult {
        restart_required: true,
    })
}

#[tauri::command]
pub async fn db_change_password(
    old_password: String,
    new_password: String,
) -> R<DbEncryptionMutationResult> {
    let path = crate::paths::db_path().map_err(e)?;
    crate::store::encryption::change_password(&path, &old_password, &new_password)
        .await
        .map_err(e)?;
    Ok(DbEncryptionMutationResult {
        restart_required: true,
    })
}
