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
pub async fn delete_workspace(
    app: tauri::AppHandle,
    db: State<'_, Db>,
    workspace_id: i32,
) -> R<()> {
    repo::mark_workspace_deleting(&db, workspace_id)
        .await
        .map_err(e)?;
    let result = delete_workspace_after_fence(app, &db, workspace_id).await;
    if result.is_err() {
        let _ = repo::clear_workspace_deleting(&db, workspace_id).await;
    }
    result
}

async fn delete_workspace_after_fence(
    app: tauri::AppHandle,
    db: &Db,
    workspace_id: i32,
) -> R<()> {
    stop_workspace_engines(&app, db, workspace_id).await?;
    cancel_workspace_asks(db, app.state::<crate::ask::AskRegistry>().inner(), workspace_id)
        .await?;
    cancel_workspace_human_asks(
        db,
        app.state::<crate::bus::BusRegistry>().inner(),
        workspace_id,
    )
    .await?;
    let mut repo_paths: std::collections::HashMap<i32, String> = repo::list_repos(db, workspace_id)
        .await
        .map_err(e)?
        .into_iter()
        .map(|repo| (repo.id, repo.local_git_path))
        .collect();
    for repo_id in repo_paths.keys() {
        crate::curator::run_forget(*repo_id);
    }
    let removed = repo::delete_workspace_cascade(db, workspace_id)
        .await
        .map_err(e)?;
    extend_removed_repo_paths(db, &mut repo_paths, &removed).await?;
    for (repo_id, path, branch, created_branch, created_checkout) in &removed {
        let Some(repo_path) = repo_paths.get(repo_id) else {
            continue;
        };
        let repo_path = std::path::Path::new(repo_path);
        if *created_checkout {
            if let Err(err) = crate::git::remove_worktree(repo_path, std::path::Path::new(path)) {
                eprintln!("[weft] worktree remove failed for {path}: {err}");
            }
        } else {
            let _ = crate::git::lock_worktree(repo_path, std::path::Path::new(path));
        }
        if *created_branch {
            if let Err(err) = crate::git::delete_branch(repo_path, branch) {
                eprintln!("[weft] branch delete failed for {branch}: {err}");
            }
        }
    }
    Ok(())
}

async fn extend_removed_repo_paths(
    db: &Db,
    repo_paths: &mut std::collections::HashMap<i32, String>,
    removed: &[(i32, String, String, bool, bool)],
) -> R<()> {
    for (repo_id, _, _, _, _) in removed {
        if repo_paths.contains_key(repo_id) {
            continue;
        }
        if let Some(repo_ref) = repo::get_repo(db, *repo_id).await.map_err(e)? {
            repo_paths.insert(*repo_id, repo_ref.local_git_path);
        }
    }
    Ok(())
}

async fn cancel_workspace_human_asks(
    db: &Db,
    bus: &crate::bus::BusRegistry,
    workspace_id: i32,
) -> R<()> {
    let scope = workspace_ask_scope(db, workspace_id).await?;
    for thread_id in &scope.thread_ids {
        bus.cancel_open_asks(*thread_id);
    }
    for (thread_id, from) in &scope.direction_routes {
        if !scope.thread_ids.contains(thread_id) {
            bus.cancel_open_asks_from(*thread_id, from);
        }
    }
    Ok(())
}

async fn cancel_workspace_asks(
    db: &Db,
    asks: &crate::ask::AskRegistry,
    workspace_id: i32,
) -> R<()> {
    let scope = workspace_ask_scope(db, workspace_id).await?;
    for ask in asks.open() {
        if scope.thread_ids.contains(&ask.thread)
            || scope.direction_routes.contains(&(ask.thread, ask.dir.clone()))
        {
            asks.cancel(ask.id);
        }
    }
    Ok(())
}

#[derive(Default)]
struct WorkspaceAskScope {
    thread_ids: std::collections::BTreeSet<i32>,
    direction_routes: std::collections::BTreeSet<(i32, String)>,
}

async fn workspace_ask_scope(db: &Db, workspace_id: i32) -> R<WorkspaceAskScope> {
    let threads = repo::list_threads(db, workspace_id).await.map_err(e)?;
    let repos = repo::list_repos(db, workspace_id).await.map_err(e)?;
    let mut scope = WorkspaceAskScope::default();
    for thread in threads {
        scope.thread_ids.insert(thread.id);
    }
    for repo_ref in repos {
        for session in repo::sessions_for_repo(db, repo_ref.id).await.map_err(e)? {
            if let Some(direction) = repo::get_direction(db, session.direction_id)
                .await
                .map_err(e)?
            {
                scope
                    .direction_routes
                    .insert((direction.thread_id, direction.id.to_string()));
            }
        }
    }
    Ok(scope)
}

async fn stop_workspace_engines(app: &tauri::AppHandle, db: &Db, workspace_id: i32) -> R<()> {
    let state = app.state::<crate::lead_chat::engine::LeadChatState>();
    let keys = workspace_engine_keys(db, workspace_id).await?;
    for key in keys {
        if let Some(eng) = state.get(key) {
            crate::lead_chat::engine::stop(app, &eng).await;
        }
    }
    Ok(())
}

async fn workspace_engine_keys(
    db: &Db,
    workspace_id: i32,
) -> R<std::collections::BTreeSet<i64>> {
    let threads = repo::list_threads(db, workspace_id).await.map_err(e)?;
    let repos = repo::list_repos(db, workspace_id).await.map_err(e)?;
    let mut keys = std::collections::BTreeSet::<i64>::new();

    for thread in &threads {
        keys.insert(crate::lead_chat::commands::lead_key(thread.id));
        for session in repo::sessions_for_thread(db, thread.id).await.map_err(e)? {
            keys.insert(session.id as i64);
        }
    }
    for repo in &repos {
        for session in repo::sessions_for_repo(db, repo.id).await.map_err(e)? {
            keys.insert(session.id as i64);
        }
    }
    Ok(keys)
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
    let p = std::path::Path::new(path);
    if !crate::git::is_git_repo(p) {
        return Err("not a git repository".into());
    }
    // Canonicalize so the same repo reached via a trailing slash, redundant
    // slashes, or a symlink dedups to one row; fall back to the raw path if
    // canonicalization fails. (The git probes above use the original path.)
    let canonical = std::fs::canonicalize(p)
        .map(|c| c.to_string_lossy().into_owned())
        .unwrap_or_else(|_| path.to_string());
    // Default base ref = the repo's real default branch: the remote's default
    // (origin/HEAD), else the conventional integration branch (main/master), else
    // the locally checked-out branch. So a repo added while on a feature branch
    // records the integration branch as base, not that feature branch.
    // Use the LOCAL default only — never a blocking network lookup (ls-remote) at
    // registration time, which would hang Add Repo on a slow/VPN/SSH remote for a
    // best-effort base hint. Materialization does the authoritative live-default
    // resolution (and fetch) later, where a brief network call is acceptable.
    // `vetted` is true ONLY when `base` came from origin/HEAD or a real main/master
    // branch — NOT the current-branch / "main"-last-resort fallback (R47-2).
    let (base, base_is_vetted_default) = crate::git::default_base_branch_vetted(
        p,
        &crate::git::current_branch(p).unwrap_or_default(),
    );
    // Captured for workspace-level dedup; empty for a local repo with no origin.
    // Credentials embedded in an HTTPS remote are redacted so a PAT/password from
    // .git/config never lands in Weft's DB/backups.
    let remote = crate::git::remote_url(p)
        .map(|r| crate::git::redact_remote(&r))
        .unwrap_or_default();
    // Backfill for repos added before this change, so dedup below can match them
    // on upgraded databases. Best-effort, cheap (a handful of repos per workspace):
    //   - remote: catch a second clone of an already-present origin;
    //   - path: canonicalize a non-canonical (symlink/`..`) stored path so a
    //     local-only repo with no remote still dedups by path.
    for existing in repo::list_repos(db, workspace_id).await.map_err(e)? {
        if existing.remote_url.is_empty() {
            if let Some(rem) = crate::git::remote_url(std::path::Path::new(&existing.local_git_path))
            {
                let _ = repo::set_repo_remote(db, existing.id, &crate::git::redact_remote(&rem)).await;
            }
        }
        if let Ok(canon) = std::fs::canonicalize(&existing.local_git_path) {
            let canon = canon.to_string_lossy();
            if canon != existing.local_git_path {
                let _ = repo::set_repo_path(db, existing.id, &canon).await;
            }
        }
    }
    // Mark `base` as the captured default ONLY when it is a genuinely-vetted default
    // (origin/HEAD or a real main/master branch — `base_is_vetted_default`). For a
    // single-branch / nonstandard checkout with NO origin/HEAD and NO main/master, the
    // base is whatever happened to be checked out / the "main" last-resort, which is NOT
    // vetted; marking it is_default=true would make the offline fallback
    // (`recorded_base_or_default`) trust it over the main/master chain (R47-2).
    let mut r = repo::add_repo_ref(
        db,
        workspace_id,
        name,
        &canonical,
        &base,
        &remote,
        base_is_vetted_default,
    )
    .await
    .map_err(e)?;
    // If dedup resolved to an EXISTING row (by remote) at a different path whose
    // checkout is gone, repoint it to the path the user just gave us — a local add
    // OR a clone — so we don't keep pointing at a dead checkout and report success.
    // (A live duplicate keeps the existing row; clone_repo removes the redundant
    // clone dir in that case.) Done before profiling so facts come from the live path.
    if r.local_git_path != canonical
        && !crate::git::is_git_repo(std::path::Path::new(&r.local_git_path))
    {
        if let Ok(Some(updated)) = repo::set_repo_path(db, r.id, &canonical).await {
            r = updated;
            // Repointed from a DEAD checkout to this live one: forget the stale
            // "checkout not found" failure so the auto pass below reclassifies the
            // fresh path (its new HEAD ≠ the old profiled_commit → needs_classification
            // re-runs it). We DON'T clear on a live-duplicate re-add: that pass isn't
            // forced and would skip an unchanged classified repo, dropping the failure
            // with no retry.
            crate::curator::clear_failure(r.id);
            // The auto pass reads the PERSISTED analysis_state (not just the
            // in-memory map), so clearing only memory would leave the DB column at
            // "failed" — the non-forced add-pass would skip the now-valid repo and
            // it would render as failed indefinitely. Persist "idle" too.
            let _ = repo::set_analysis_state(db, r.id, "idle", None).await;
        }
    }
    // The curator is agent-only now (ARCHITECTURE §4.9): there is no deterministic
    // profiling on add. Eagerly create an empty placeholder profile row (tier "",
    // so it renders as "analyzing" until classified) ONLY when one doesn't already
    // exist — `register_repo` is also reached by a duplicate add/clone where `r` is
    // an existing row, and clobbering its tier/summary/stack would lose data.
    // Creating it here, before analysis is spawned, also means every later write
    // (agent pass, user edit, calibration) is an UPDATE, so two can't race on the
    // unique `repo_id` insert. Best-effort: a hiccup never blocks the add.
    if matches!(repo::get_repo_profile(db, r.id).await, Ok(None)) {
        let _ = repo::upsert_repo_profile(db, r.id, "", "[]", "", "[]", "agent", "").await;
    }
    // Fire-and-forget the agent curator over the whole workspace so the new repo
    // gets a deep per-repo classification and cross-repo relations refresh.
    // Read-only, coalesced (a batch add runs one pass), and best-effort — it
    // never blocks the add. Not forced: an add shouldn't retry OTHER repos' failures.
    let db_bg = db.clone();
    let ws = r.workspace_id;
    tauri::async_runtime::spawn(async move {
        crate::curator::analyze_workspace_coalesced(&db_bg, ws, false).await;
    });
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
    let r = register_repo(&db, workspace_id, &name, &path.to_string_lossy()).await?;
    // If the row points somewhere other than the dir we just cloned, dedup
    // resolved to a DIFFERENT live repo (a dead-checkout match was already
    // repointed to this clone inside register_repo, so paths would match here) —
    // the fresh clone is a redundant duplicate, so remove it rather than leaving
    // an orphan dir on disk.
    let cloned = std::fs::canonicalize(&path).ok();
    let registered = std::fs::canonicalize(&r.local_git_path).ok();
    if cloned.is_some() && cloned != registered {
        let _ = std::fs::remove_dir_all(&path);
    }
    Ok(r)
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
    // `curator::graph` itself schedules the one-shot legacy backfill for upgraded
    // workspaces, so every read path (this command and the planner's MCP
    // `get_repo_map`) is covered.
    crate::curator::graph(&db, workspace_id).await.map_err(e)
}

/// Re-run the deep, read-only agent classification for a single repo (tier +
/// summary + components), then refresh the workspace's cross-repo relations so
/// the stored edges reflect the repo's changed dependencies. Slow (spawns the
/// agent); the caller refreshes the map after it resolves.
#[tauri::command]
pub async fn reprofile_repo(db: State<'_, Db>, repo_id: i32) -> R<()> {
    let r = repo::get_repo(&db, repo_id)
        .await
        .map_err(e)?
        .ok_or("repo not found")?;
    crate::curator::reprofile_repo(&db, &r).await.map_err(e)?;
    Ok(())
}

/// Manually re-run the agent dependency curator over a workspace (the same
/// read-only pass that fires after each add). Returns when it completes so the
/// caller can refresh the graph; coalesced with any in-flight pass.
#[tauri::command]
pub async fn analyze_workspace_deps(db: State<'_, Db>, workspace_id: i32) -> R<()> {
    // An EXPLICIT user re-run: `force` so repos whose last classify failed are
    // retried even if unchanged (the auto anti-storm skip otherwise ignores them).
    // A failed repo whose checkout is gone is still filtered out by the pass and
    // keeps its failed state — we no longer clear failures up front (that dropped
    // such repos to a silent idle).
    crate::curator::analyze_workspace_coalesced(&db, workspace_id, true).await;
    // If every tracked repo's checkout is gone, the pass analyzes nothing yet the
    // stale profiles still read as classified — surface that as an error so the UI
    // reports a failed run instead of a misleading "map updated" (the frontend's
    // catch maps a rejection to the failed status; the message itself isn't shown).
    let repos = repo::list_repos(&db, workspace_id).await.map_err(e)?;
    if !repos.is_empty()
        && !repos
            .iter()
            .any(|r| std::path::Path::new(&r.local_git_path).exists())
    {
        return Err("all repository checkouts are missing on disk".to_string());
    }
    Ok(())
}

/// Get-or-create this workspace's hidden curator-chat thread and return its id,
/// so the frontend can open its lead-chat surface for dependency calibration.
#[tauri::command]
pub async fn open_curator_chat(db: State<'_, Db>, workspace_id: i32) -> R<i32> {
    // Stamp the curator thread with the user's configured default tool so the
    // calibration chat is usable for codex/opencode users (not hard-coded claude).
    let tool = crate::tools::default_tool(&db).await;
    repo::ensure_curator_thread(&db, workspace_id, &tool)
        .await
        .map_err(e)
}

/// Return the analyst-synthesized markdown repo-map for a workspace, or `None`
/// when no analysis has produced one yet.
#[tauri::command]
pub async fn get_repo_map_doc(db: State<'_, Db>, workspace_id: i32) -> R<Option<String>> {
    repo::get_repo_map_doc(&db, workspace_id).await.map_err(e)
}

/// Remove a repo from its workspace: delete Weft's reference, the repo's
/// profile, the directions bound to it (with their sessions), and its worktrees
/// (physically removed from git). The user's actual repository at its local path
/// is NEVER deleted — only Weft's tracking of it.
#[tauri::command]
pub async fn delete_repo(db: State<'_, Db>, repo_id: i32) -> R<()> {
    // Capture the repo path before the cascade — the repo_ref row is gone after
    // delete_repo_cascade, so we must resolve the git path first.
    let repo = repo::get_repo(&db, repo_id)
        .await
        .map_err(e)?
        .ok_or("repo not found")?;
    // Forget before the cascade so any in-flight curator pass sees the deletion
    // as early as possible and does not publish stale running/done state.
    crate::curator::run_forget(repo_id);
    let removed = repo::delete_repo_cascade(&db, repo_id).await.map_err(e)?;
    // Gate worktree removal on created_checkout (a reused pre-existing path must
    // survive) and branch deletion on created_branch (a pre-existing branch reused
    // by the -b fallback survives repo deletion). cleanup_worktrees cannot be used
    // here because delete_repo_cascade already deleted the repo_ref row (which
    // cleanup_worktrees needs for the git path lookup); instead we use the
    // pre-fetched path directly.
    let repo_path = std::path::Path::new(&repo.local_git_path);
    for (_repo_id, path, branch, created_branch, created_checkout) in &removed {
        if *created_checkout {
            if let Err(err) = crate::git::remove_worktree(repo_path, std::path::Path::new(path)) {
                eprintln!("[weft] worktree remove failed for {path}: {err}");
            }
        } else {
            // Reused (non-weft) checkout: keep it as a usable worktree, but LOCK it so the
            // orphan GC skips it now that its DB row is gone (mirrors cleanup_worktrees).
            // Locking via git metadata also means a later re-add of this repo can't
            // re-orphan the checkout.
            let _ = crate::git::lock_worktree(repo_path, std::path::Path::new(path));
        }
        if *created_branch {
            if let Err(err) = crate::git::delete_branch(repo_path, branch) {
                eprintln!("[weft] branch delete failed for {branch}: {err}");
            }
        }
    }
    Ok(())
}

#[tauri::command]
pub async fn update_repo_profile(
    db: State<'_, Db>,
    repo_id: i32,
    summary: Option<String>,
    tier: Option<String>,
) -> R<()> {
    // Only the field(s) the user actually changed are `Some`, so editing the
    // summary doesn't pin the tier and vice versa.
    crate::curator::edit_profile(&db, repo_id, summary.as_deref(), tier.as_deref())
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
    // NOTE: the hidden curator-chat thread IS included here — the frontend needs
    // it in `threads` to render its chat surface (ThreadBoard/LeadTab look the
    // active thread up in this list). It's filtered from the board cards
    // (`workspace_overview`) and from the nav/palette thread lists instead.
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
    let threads: Vec<_> = repo::list_threads(&db, workspace_id)
        .await
        .map_err(e)?
        .into_iter()
        .filter(|t| t.kind != "curator") // hidden curator-chat thread is not a board issue
        .collect();
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

/// Set one proposed direction's base branch in the stored proposal (targeted; keeps status).
/// `name` and `repo` are the lane identity the frontend edited — rejected if the
/// proposal was replaced under the index (re-propose while a blur-save was in flight).
/// `expected_base` is the base the field was editing FROM — rejected if a same-identity
/// re-propose changed the lane's base in the meantime (optimistic concurrency).
/// `expected_version` is the proposal version (created_at) the edit was composed against —
/// rejected if a re-propose bumped it even with the lane's base unchanged (R54-2). Empty is
/// tolerated; the guard only applies when non-empty.
#[tauri::command]
pub async fn set_proposal_direction_base(
    db: State<'_, Db>,
    thread_id: i32,
    index: usize,
    name: String,
    repo: String,
    expected_base: String,
    expected_version: String,
    base: String,
) -> R<()> {
    crate::planner::set_direction_base(
        &db,
        thread_id,
        index,
        &name,
        &repo,
        &expected_base,
        &expected_version,
        &base,
    )
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
    base_branch: Option<String>,
) -> R<entities::direction::Model> {
    let dir = repo::create_direction(
        &db,
        thread_id,
        &name,
        &tool,
        repo_id,
        &reason,
        mandate.as_deref().unwrap_or("plan+impl"),
        base_branch.as_deref().unwrap_or(""),
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

/// A worktree row plus whether its directory is still present on disk. The board
/// uses `exists` to offer "delete worktree" only when there's actually a directory
/// to reclaim — a row can outlive its directory if it was removed out-of-band.
#[derive(serde::Serialize)]
pub struct WorktreeView {
    #[serde(flatten)]
    inner: entities::worktree::Model,
    exists: bool,
}

#[tauri::command]
pub async fn list_worktrees(
    db: State<'_, Db>,
    direction_id: Option<i32>,
) -> R<Vec<WorktreeView>> {
    let rows = repo::list_worktrees(&db, direction_id).await.map_err(e)?;
    Ok(rows
        .into_iter()
        .map(|w| {
            let exists = std::path::Path::new(&w.path).exists();
            WorktreeView { inner: w, exists }
        })
        .collect())
}

/// Delete a single finished task's worktree (directory + DB row), keeping the
/// branch and the task. See `materialize::remove_direction_worktree`.
#[tauri::command]
pub async fn delete_worktree(db: State<'_, Db>, worktree_id: i32) -> R<()> {
    materialize::remove_direction_worktree(&db, worktree_id)
        .await
        .map_err(e)
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
/// Worktree file tree response, including a truncation flag when the directory
/// is too large to render efficiently.
#[derive(serde::Serialize)]
pub struct FileTree {
    pub nodes: Vec<FileNode>,
    pub truncated: bool,
    pub total: usize,
}

#[derive(serde::Serialize)]
pub struct FileNode {
    pub path: String,
    pub name: String,
    pub kind: FileNodeKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub children: Option<Vec<FileNode>>,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum FileNodeKind {
    File,
    Directory,
}

const FILE_TREE_MAX_DEPTH: usize = 8;
const FILE_TREE_MAX_NODES: usize = 5000;

/// Directories that are usually large and uninteresting for code review.
fn is_skipped_dir(name: &str) -> bool {
    matches!(
        name,
        ".git"
            | "node_modules"
            | "target"
            | "dist"
            | "build"
            | ".next"
            | ".turbo"
            | "coverage"
            | ".coverage"
            | "__pycache__"
            | ".venv"
            | "venv"
    )
}

fn read_dir_tree(
    path: &std::path::Path,
    depth: usize,
    counter: &mut usize,
) -> R<(Vec<FileNode>, bool)> {
    if *counter >= FILE_TREE_MAX_NODES {
        return Ok((Vec::new(), true));
    }
    if depth == 0 {
        // Reached the depth limit. If this directory has any entries, report
        // truncation so the UI doesn't show a non-empty folder as empty.
        let has_entries = std::fs::read_dir(path)
            .map_err(e)?
            .next()
            .is_some();
        return Ok((Vec::new(), has_entries));
    }

    // Collect up to the remaining budget so we never sort an unbounded list.
    let mut entries = Vec::with_capacity(256);
    let mut truncated = false;
    for entry in std::fs::read_dir(path).map_err(e)? {
        let entry = entry.map_err(e)?;
        if *counter + entries.len() >= FILE_TREE_MAX_NODES {
            truncated = true;
            break;
        }
        entries.push(entry);
    }
    entries.sort_by(|a, b| a.file_name().cmp(&b.file_name()));

    let mut nodes = Vec::with_capacity(entries.len());
    for entry in entries {
        if *counter >= FILE_TREE_MAX_NODES {
            truncated = true;
            break;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let entry_path = entry.path();
        let path_str = entry_path.to_string_lossy().into_owned();
        // Use symlink_metadata so we don't follow symlinks into directories
        // outside the worktree. Symlinks are shown as files and not recursed.
        let metadata = match std::fs::symlink_metadata(&entry_path) {
            Ok(m) => m,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(err) => return Err(err.to_string()),
        };
        if metadata.is_dir() {
            if is_skipped_dir(&name) {
                continue;
            }
            *counter += 1;
            let (children, child_truncated) =
                read_dir_tree(&entry_path, depth - 1, counter)?;
            truncated = truncated || child_truncated;
            nodes.push(FileNode {
                path: path_str,
                name,
                kind: FileNodeKind::Directory,
                children: Some(children),
            });
        } else {
            *counter += 1;
            nodes.push(FileNode {
                path: path_str,
                name,
                kind: FileNodeKind::File,
                children: None,
            });
        }
    }
    Ok((nodes, truncated))
}

/// The worktree file tree for the Files panel: a recursive snapshot of the
/// worktree's directory structure, excluding build/output dirs and `.git`.
#[tauri::command]
pub fn list_worktree_files(cwd: String) -> R<FileTree> {
    let mut counter = 0;
    let (nodes, truncated) = read_dir_tree(std::path::Path::new(&cwd), FILE_TREE_MAX_DEPTH, &mut counter)?;
    Ok(FileTree {
        nodes,
        truncated,
        total: counter,
    })
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
    let threads: Vec<_> = repo::list_threads(&db, workspace_id)
        .await
        .map_err(e)?
        .into_iter()
        .filter(|t| t.kind != "curator") // hidden curator-chat thread is not a board issue
        .collect();
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

/// The user-configured coding-agent command overrides ("aliases"): identity →
/// command (e.g. `claude` → `cc-claude`). Empty map when none are set.
#[tauri::command]
pub async fn get_tool_commands(
    db: State<'_, Db>,
) -> R<std::collections::HashMap<String, String>> {
    repo::get_tool_commands(&db).await.map_err(e)
}

/// Set or clear (blank/identity command) the alias for one tool. `applyToExisting`
/// = false pins existing sessions of that tool to their prior command so only new
/// sessions adopt the alias; = true lets existing sessions pick it up on next run.
/// Refreshes the process-global override map so spawns see the change immediately.
#[tauri::command]
pub async fn set_tool_command(
    app: tauri::AppHandle,
    db: State<'_, Db>,
    tool: String,
    command: String,
    apply_to_existing: bool,
) -> R<()> {
    if !crate::detect::TOOL_PRIORITY.contains(&tool.as_str()) {
        return Err(format!(
            "unknown tool {tool:?}; expected one of {:?}",
            crate::detect::TOOL_PRIORITY
        ));
    }
    let (map, prev) = repo::set_tool_command(&db, &tool, &command, apply_to_existing)
        .await
        .map_err(e)?;
    let new_cmd = map.get(&tool).cloned().unwrap_or_else(|| tool.clone());
    crate::tool_command::set_overrides(map);
    let changed = new_cmd != prev;

    // Sync live engines so the reconcile applies WITHOUT closing/reopening the
    // session (their `command` was captured when the engine was built). Mirror the
    // DB: apply-to-existing clears pins (follow new global) and, when the command
    // actually changed, flags a silent resident bounce so the open session's next
    // send respawns from the new binary (a Claude child / codex client spawned
    // from the old command would otherwise outlive the change). Opt-out freezes
    // currently un-pinned engines to their prior command — which is what their
    // resident process is already running, so no bounce is needed.
    use tauri::Manager;
    let engines: Vec<crate::lead_chat::engine::EngineRef> = app
        .state::<crate::lead_chat::engine::LeadChatState>()
        .0
        .iter()
        .map(|r| r.value().clone())
        .collect();
    for eng in engines {
        let mut inner = eng.lock().await;
        if inner.tool != tool {
            continue;
        }
        if apply_to_existing {
            // Bounce when THIS engine's effective command changes — covers a
            // global change (pin already None) AND clearing a stale pin while the
            // global stayed the same (pin Some(old) → global), which the coarse
            // `changed` flag misses.
            let old_eff = crate::tool_command::effective(inner.command.as_deref(), &tool);
            inner.command = None;
            if old_eff != new_cmd {
                inner.pending_command_refresh = true;
            }
        } else if inner.command.is_none() {
            inner.command = Some(prev.clone());
        }
    }

    // Recycle the GLOBAL discovery helpers spawned from the old binary so the
    // command palette / session metadata reconnect with the new command. These
    // are app-scoped (not per-session), so recycle whenever the effective command
    // changed, regardless of apply-to-existing.
    if changed {
        match tool.as_str() {
            "opencode" => crate::opencode::shutdown().await,
            "codex" => crate::codex_app_server::shutdown_global().await,
            _ => {}
        }
    }
    Ok(())
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
    pub base_branch: String,
}

/// Every pending write declaration across the workspace's threads — the
/// data behind the Needs-you "approve a write" cards.
#[tauri::command]
pub async fn write_triggers(db: State<'_, Db>, workspace_id: i32) -> R<Vec<WriteTrigger>> {
    let threads: Vec<_> = repo::list_threads(&db, workspace_id)
        .await
        .map_err(e)?
        .into_iter()
        .filter(|t| t.kind != "curator") // hidden curator-chat thread is not a board issue
        .collect();
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
                base_branch: p.base_branch,
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
    /// Effective binary for the resume command (configured alias / per-session
    /// pin, else the tool identity).
    pub command: String,
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
    let command = crate::tool_command::effective(
        latest.as_ref().and_then(|s| s.command.as_deref()),
        &dir.tool,
    );
    Ok(Some(ObserveRef {
        worktree: wt.path,
        branch: wt.branch,
        tool: dir.tool,
        command,
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
    let latest = repo::latest_session_for(&db, direction_id, repo_id)
        .await
        .map_err(e)?;
    let native = latest.as_ref().and_then(|s| s.native_session_id.clone());
    // Probe the binary this session actually runs (per-session pin, else alias).
    let command = crate::tool_command::effective(
        latest.as_ref().and_then(|s| s.command.as_deref()),
        &dir.tool,
    );
    Ok(crate::session_meta::gather(&dir.tool, &wt.path, native.as_deref(), &command).await)
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
        let threads: Vec<_> = repo::list_threads(&db, w.id)
            .await
            .map_err(e)?
            .into_iter()
            .filter(|t| t.kind != "curator") // hidden curator chat isn't a board issue
            .collect();
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

/// 写飞书凭证并重启桥。secret 空 = 保持原值(不覆盖已存密钥)。`enable=true` 时同时置
/// `im.feishu.enabled`(扫码接入即启用);手填保存走 `enable=false`,enabled 仍由开关 /
/// 默认决定。`owner_open_id=Some` 时把该 open_id 设为白名单——扫码创建的新机器人用它
/// 让授权者立即可对话,并覆盖旧应用遗留的白名单(那些 open_id 属于旧 app、对新 bot 无效,
/// 留着反而会让新 owner 被 `inbound::route` 忽略);手填路径传 `None`,不动既有白名单。
/// 扫码注册成功路径与本函数共用,保证「落库 + 重连」单一实现。
async fn apply_feishu_credentials(
    app: &tauri::AppHandle,
    db: &Db,
    app_id: &str,
    app_secret: &str,
    enable: bool,
    owner_open_id: Option<&str>,
) -> anyhow::Result<()> {
    repo::set_setting(db, crate::im::K_APP_ID, app_id.trim()).await?;
    if !app_secret.is_empty() {
        repo::set_setting(db, crate::im::K_APP_SECRET, app_secret.trim()).await?;
    }
    if let Some(oid) = owner_open_id {
        let oid = oid.trim();
        if !oid.is_empty() {
            repo::set_setting(db, crate::im::K_ALLOW, oid).await?;
        }
    }
    if enable {
        repo::set_setting(db, crate::im::K_ENABLED, "1").await?;
    }
    crate::im::spawn(app.clone());
    Ok(())
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
    apply_feishu_credentials(&app, &db, &app_id, &app_secret, false, None)
        .await
        .map_err(e)
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

// ───────────────────────── 飞书扫码接入(device-flow）─────────────────────────

#[derive(serde::Serialize)]
pub struct ScanBeginView {
    pub qr_data_uri: String,
    pub expire_secs: u64,
    pub poll_interval_ms: u64,
}

#[derive(serde::Serialize)]
pub struct ScanStatusView {
    pub status: String,
    pub error_reason: Option<String>,
}

/// 发起扫码:begin device-flow → 返回二维码 data URI 供前端 `<img>` 渲染。后台轮询在
/// RegistrationService 内进行;成功时用拿到的 client_id/secret 落库 + 重连(enable）。
#[tauri::command]
pub async fn feishu_scan_begin(
    app: tauri::AppHandle,
    svc: State<'_, crate::im::feishu::registration::RegistrationService>,
) -> R<ScanBeginView> {
    use crate::im::feishu::registration::{OnSuccess, ReqwestTransport};
    let app_cb = app.clone();
    let on_success: OnSuccess = std::sync::Arc::new(move |client_id, client_secret, open_id| {
        let app = app_cb.clone();
        Box::pin(async move {
            let db = app.state::<Db>().inner().clone();
            apply_feishu_credentials(&app, &db, &client_id, &client_secret, true, Some(&open_id))
                .await
        }) as futures::future::BoxFuture<'static, anyhow::Result<()>>
    });
    let transport = std::sync::Arc::new(ReqwestTransport::default());
    let begin = svc.begin(transport, on_success).await.map_err(e)?;
    Ok(ScanBeginView {
        qr_data_uri: begin.qr_data_uri,
        expire_secs: begin.expire_secs,
        poll_interval_ms: begin.interval_secs.saturating_mul(1000),
    })
}

/// 查询扫码状态(前端按 poll_interval_ms 轮询)。
#[tauri::command]
pub fn feishu_scan_status(
    svc: State<'_, crate::im::feishu::registration::RegistrationService>,
) -> R<ScanStatusView> {
    use crate::im::feishu::registration::ScanStatus;
    let (status, error_reason) = match svc.status() {
        ScanStatus::Idle => ("idle", None),
        ScanStatus::Pending => ("pending", None),
        ScanStatus::Success => ("success", None),
        ScanStatus::Expired => ("expired", None),
        ScanStatus::Error(r) => ("error", Some(r)),
    };
    Ok(ScanStatusView {
        status: status.to_string(),
        error_reason,
    })
}

/// 取消扫码(关闭 dialog / 卸载时调用),停止后台轮询。
#[tauri::command]
pub fn feishu_scan_cancel(
    svc: State<'_, crate::im::feishu::registration::RegistrationService>,
) -> R<()> {
    svc.cancel();
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(dir: &std::path::Path, args: &[&str]) {
        let status = std::process::Command::new(args[0])
            .args(&args[1..])
            .current_dir(dir)
            .status()
            .expect("spawn git");
        assert!(status.success(), "command failed: {args:?}");
    }

    /// Init a repo with a real integration branch (main/master) present.
    fn init_main_repo(root: &std::path::Path, name: &str) -> std::path::PathBuf {
        let p = root.join(name);
        std::fs::create_dir_all(&p).unwrap();
        sh(&p, &["git", "init", "-q"]);
        sh(&p, &["git", "config", "user.email", "t@t.t"]);
        sh(&p, &["git", "config", "user.name", "t"]);
        std::fs::write(p.join("README.md"), "# x\n").unwrap();
        sh(&p, &["git", "add", "-A"]);
        sh(&p, &["git", "commit", "-q", "-m", "init"]);
        p
    }

    /// R47-2: `register_repo` must capture `base_ref_is_default` HONESTLY.
    /// - A standard repo (real main/master, the vetted default) → is_default=true (unchanged).
    /// - A nonstandard single-branch repo (only `trunk`, no main/master, no origin/HEAD) →
    ///   the base is the current-branch / "main"-last-resort fallback, NOT a vetted default →
    ///   is_default=false, so the offline fallback won't trust it over the main/master chain.
    #[tokio::test]
    async fn register_repo_marks_only_vetted_default_as_default() {
        let tag = format!("weft-regrepo-{}", std::process::id());
        let root = std::env::temp_dir().join(tag);
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();

        let db = Db::connect("sqlite::memory:").await.unwrap();
        let ws = repo::create_workspace(&db, "ws").await.unwrap();

        // (a) Standard repo with a real main/master integration branch → vetted default.
        let std_repo = init_main_repo(&root, "api");
        let def = crate::git::current_branch(&std_repo).unwrap();
        assert!(def == "main" || def == "master", "precondition: init produced main/master");
        let r_std = register_repo(&db, ws.id, "api", std_repo.to_str().unwrap())
            .await
            .unwrap();
        assert!(
            r_std.base_ref_is_default,
            "R47-2: a real main/master default must be captured as is_default=true (unchanged)"
        );
        assert_eq!(r_std.base_ref, def, "captured base is the vetted default branch");

        // (b) Nonstandard repo: rename the only branch to `trunk` (no main/master, no remote).
        let nonstd = init_main_repo(&root, "weird");
        sh(&nonstd, &["git", "branch", "-m", "trunk"]);
        assert!(
            crate::git::ref_resolves(&nonstd, "refs/heads/trunk"),
            "precondition: trunk exists"
        );
        assert!(
            !crate::git::ref_resolves(&nonstd, "refs/heads/main")
                && !crate::git::ref_resolves(&nonstd, "refs/heads/master"),
            "precondition: no main/master branch"
        );
        let r_nonstd = register_repo(&db, ws.id, "weird", nonstd.to_str().unwrap())
            .await
            .unwrap();
        assert!(
            !r_nonstd.base_ref_is_default,
            "R47-2: a nonstandard fallback base (trunk; no main/master/origin-HEAD) must NOT be \
             captured as a vetted default"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn workspace_engine_keys_cover_workspace_leads_workers_and_repo_sessions() {
        let db = Db::connect("sqlite::memory:").await.unwrap();
        let ws = repo::create_workspace(&db, "delete me").await.unwrap();
        let keep_ws = repo::create_workspace(&db, "keep me").await.unwrap();
        let repo_ref = repo::add_repo_ref(&db, ws.id, "web", "/tmp/web", "main", "", true)
            .await
            .unwrap();
        let keep_repo = repo::add_repo_ref(&db, keep_ws.id, "api", "/tmp/api", "main", "", true)
            .await
            .unwrap();
        let thread = repo::create_thread(&db, ws.id, "remove", "feature", "claude")
            .await
            .unwrap();
        let keep_thread = repo::create_thread(&db, keep_ws.id, "keep", "feature", "claude")
            .await
            .unwrap();
        let direction = repo::create_direction(
            &db,
            thread.id,
            "web task",
            "claude",
            repo_ref.id,
            "change",
            "plan+impl",
            "",
        )
        .await
        .unwrap();
        let keep_direction = repo::create_direction(
            &db,
            keep_thread.id,
            "api task",
            "claude",
            keep_repo.id,
            "change",
            "plan+impl",
            "",
        )
        .await
        .unwrap();
        let worker = repo::create_session(&db, direction.id, repo_ref.id, "claude", "/tmp/wt")
            .await
            .unwrap();
        let keep_worker =
            repo::create_session(&db, keep_direction.id, keep_repo.id, "claude", "/tmp/keep")
                .await
                .unwrap();
        let repo_scoped_worker =
            repo::create_session(&db, keep_direction.id, repo_ref.id, "claude", "/tmp/orphan")
                .await
                .unwrap();

        let keys = workspace_engine_keys(&db, ws.id).await.unwrap();

        let expected = std::collections::BTreeSet::from([
            crate::lead_chat::commands::lead_key(thread.id),
            worker.id as i64,
            repo_scoped_worker.id as i64,
        ]);
        assert_eq!(keys, expected);
        assert!(!keys.contains(&crate::lead_chat::commands::lead_key(keep_thread.id)));
        assert!(!keys.contains(&(keep_worker.id as i64)));
    }

    #[tokio::test]
    async fn extend_removed_repo_paths_loads_external_repo_refs() {
        let db = Db::connect("sqlite::memory:").await.unwrap();
        let ws = repo::create_workspace(&db, "delete me").await.unwrap();
        let keep_ws = repo::create_workspace(&db, "keep me").await.unwrap();
        let repo_ref = repo::add_repo_ref(&db, ws.id, "web", "/tmp/web", "main", "", true)
            .await
            .unwrap();
        let external_repo =
            repo::add_repo_ref(&db, keep_ws.id, "api", "/tmp/api", "main", "", true)
                .await
                .unwrap();
        let mut repo_paths = std::collections::HashMap::from([(
            repo_ref.id,
            repo_ref.local_git_path.clone(),
        )]);
        let removed = vec![(
            external_repo.id,
            "/tmp/api-wt".to_string(),
            "feature/api".to_string(),
            true,
            true,
        )];

        extend_removed_repo_paths(&db, &mut repo_paths, &removed)
            .await
            .unwrap();

        assert_eq!(repo_paths.get(&repo_ref.id).map(String::as_str), Some("/tmp/web"));
        assert_eq!(
            repo_paths.get(&external_repo.id).map(String::as_str),
            Some("/tmp/api")
        );
    }

    #[tokio::test]
    async fn cancel_workspace_asks_only_clears_deleted_workspace_threads() {
        let db = Db::connect("sqlite::memory:").await.unwrap();
        let ws = repo::create_workspace(&db, "delete me").await.unwrap();
        let keep_ws = repo::create_workspace(&db, "keep me").await.unwrap();
        let repo_ref = repo::add_repo_ref(&db, ws.id, "web", "/tmp/web", "main", "", true)
            .await
            .unwrap();
        let keep_repo = repo::add_repo_ref(&db, keep_ws.id, "api", "/tmp/api", "main", "", true)
            .await
            .unwrap();
        let thread = repo::create_thread(&db, ws.id, "remove", "feature", "claude")
            .await
            .unwrap();
        let keep_thread = repo::create_thread(&db, keep_ws.id, "keep", "feature", "claude")
            .await
            .unwrap();
        let keep_direction = repo::create_direction(
            &db,
            keep_thread.id,
            "api task",
            "claude",
            keep_repo.id,
            "change",
            "plan+impl",
            "",
        )
        .await
        .unwrap();
        repo::create_session(
            &db,
            keep_direction.id,
            repo_ref.id,
            "claude",
            "/tmp/orphan",
        )
        .await
        .unwrap();
        let asks = crate::ask::AskRegistry::new();
        let (remove_id, remove_rx) =
            asks.request(thread.id, "", "claude", "Run: rm", "rm -rf tmp");
        let (repo_scoped_id, repo_scoped_rx) = asks.request(
            keep_thread.id,
            &keep_direction.id.to_string(),
            "claude",
            "Run: clean",
            "rm -rf tmp",
        );
        let (keep_id, _keep_rx) =
            asks.request(keep_thread.id, "20", "claude", "Run: test", "pnpm test");

        cancel_workspace_asks(&db, &asks, ws.id).await.unwrap();

        assert!(remove_rx.await.is_err());
        assert!(repo_scoped_rx.await.is_err());
        assert_eq!(
            asks.open().iter().map(|ask| ask.id).collect::<Vec<_>>(),
            vec![keep_id]
        );
        assert!(!asks.open().iter().any(|ask| ask.id == remove_id));
        assert!(!asks.open().iter().any(|ask| ask.id == repo_scoped_id));
    }

    #[tokio::test]
    async fn cancel_workspace_human_asks_only_clears_deleted_workspace_threads() {
        let db = Db::connect("sqlite::memory:").await.unwrap();
        let ws = repo::create_workspace(&db, "delete me").await.unwrap();
        let keep_ws = repo::create_workspace(&db, "keep me").await.unwrap();
        let repo_ref = repo::add_repo_ref(&db, ws.id, "web", "/tmp/web", "main", "", true)
            .await
            .unwrap();
        let keep_repo = repo::add_repo_ref(&db, keep_ws.id, "api", "/tmp/api", "main", "", true)
            .await
            .unwrap();
        let thread = repo::create_thread(&db, ws.id, "remove", "feature", "claude")
            .await
            .unwrap();
        let keep_thread = repo::create_thread(&db, keep_ws.id, "keep", "feature", "claude")
            .await
            .unwrap();
        let keep_direction = repo::create_direction(
            &db,
            keep_thread.id,
            "api task",
            "claude",
            keep_repo.id,
            "change",
            "plan+impl",
            "",
        )
        .await
        .unwrap();
        repo::create_session(
            &db,
            keep_direction.id,
            repo_ref.id,
            "claude",
            "/tmp/orphan",
        )
        .await
        .unwrap();
        let bus = crate::bus::BusRegistry::new();
        let remove_id = bus.ask_human(thread.id, "lead", "delete?");
        let repo_scoped_id =
            bus.ask_human(keep_thread.id, &keep_direction.id.to_string(), "delete repo?");
        let keep_id = bus.ask_human(keep_thread.id, "lead", "keep?");

        cancel_workspace_human_asks(&db, &bus, ws.id).await.unwrap();

        assert!(bus.open_asks(thread.id).is_empty());
        assert_eq!(bus.open_asks(keep_thread.id)[0].id, keep_id);
        assert!(!bus
            .open_asks(keep_thread.id)
            .iter()
            .any(|ask| ask.id == repo_scoped_id));
        assert_ne!(remove_id, keep_id);
        assert_ne!(repo_scoped_id, keep_id);
    }
}
