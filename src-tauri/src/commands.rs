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

#[tauri::command]
pub async fn add_repo_ref(
    db: State<'_, Db>,
    workspace_id: i32,
    name: String,
    local_git_path: String,
) -> R<entities::repo_ref::Model> {
    if !crate::git::is_git_repo(std::path::Path::new(&local_git_path)) {
        return Err("not a git repository".into());
    }
    // default base ref = current branch of the repo
    let base = crate::git::current_branch(std::path::Path::new(&local_git_path)).unwrap_or_else(|_| "main".into());
    let r = repo::add_repo_ref(&db, workspace_id, &name, &local_git_path, &base, "claude").await.map_err(e)?;
    // Eager, deterministic profiling (ARCHITECTURE §4.9): best-effort, never
    // blocks adding the repo if inference/git hiccups.
    let _ = crate::curator::profile_repo(&db, &r).await;
    Ok(r)
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
    pub status: String,
    pub direction_ids: Vec<i32>,
    /// distinct repos this thread WRITES (across its directions).
    pub write_repos: Vec<RepoLite>,
}

/// Portfolio view of a workspace: every thread with its directions + write set,
/// so the board can show roll-ups and compute cross-thread repo contention
/// (a repo written by 2+ threads is a "hot repo").
#[tauri::command]
pub async fn workspace_overview(db: State<'_, Db>, workspace_id: i32) -> R<Vec<ThreadOverview>> {
    let threads = repo::list_threads(&db, workspace_id).await.map_err(e)?;
    let mut out = Vec::new();
    for t in threads {
        let dirs = repo::list_directions(&db, t.id).await.map_err(e)?;
        let mut seen = std::collections::BTreeMap::<i32, String>::new();
        for d in &dirs {
            for r in repo::direction_write_repos(&db, d.id).await.map_err(e)? {
                seen.entry(r.id).or_insert(r.name);
            }
        }
        out.push(ThreadOverview {
            thread_id: t.id,
            title: t.title,
            kind: t.kind,
            status: t.status,
            direction_ids: dirs.iter().map(|d| d.id).collect(),
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

#[tauri::command]
pub async fn list_direction_repos(
    db: State<'_, Db>,
    direction_id: i32,
) -> R<Vec<entities::direction_repo::Model>> {
    repo::list_direction_repos(&db, direction_id).await.map_err(e)
}

/// scope: list of { repoId, role } from the frontend.
#[derive(serde::Deserialize)]
pub struct ScopeItem { pub repo_id: i32, pub role: String }

#[tauri::command]
pub async fn create_direction(
    db: State<'_, Db>,
    thread_id: i32,
    name: String,
    tool: String,
    scope: Vec<ScopeItem>,
) -> R<entities::direction::Model> {
    let scope: Vec<(i32, String)> = scope.into_iter().map(|s| (s.repo_id, s.role)).collect();
    let dir = repo::create_direction(&db, thread_id, &name, &tool, &scope).await.map_err(e)?;
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

/// All pending permission Asks across the workspace (the Ask Bridge → Needs-you).
#[tauri::command]
pub fn pending_asks(asks: tauri::State<'_, crate::ask::AskRegistry>) -> R<Vec<crate::ask::Ask>> {
    Ok(asks.open())
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
