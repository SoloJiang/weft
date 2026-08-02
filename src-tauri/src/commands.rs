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

/// Pre-flight for a new repo target `<dest>/<name>` (clone + create): reject a
/// symlink (even to an empty directory — git would write the checkout through it,
/// landing outside `dest`) and any existing non-empty path, but allow a REAL
/// empty directory: both `git clone` and `git init` support initializing into
/// one, and rejecting it would regress a previously valid flow.
fn reject_occupied_repo_target(path: &std::path::Path) -> R<()> {
    let Ok(meta) = path.symlink_metadata() else {
        return Ok(()); // does not exist — free to create
    };
    if meta.file_type().is_symlink() {
        return Err(format!("repo path is a symlink: {}", path.display()));
    }
    if !meta.is_dir() {
        return Err(format!("repo path already exists: {}", path.display()));
    }
    let mut entries =
        std::fs::read_dir(path).map_err(|e| format!("cannot inspect {}: {e}", path.display()))?;
    if entries.next().is_some() {
        return Err(format!(
            "repo path already exists and is not empty: {}",
            path.display()
        ));
    }
    Ok(())
}

/// Ensure `path` resolves to a location inside `dest`. Prevents symlink or
/// non-canonical `dest` from causing the actual repo to land elsewhere.
fn ensure_path_under_dest(path: &std::path::Path, dest: &std::path::Path) -> R<()> {
    let dest_canon = std::fs::canonicalize(dest)
        .map_err(|_| "destination directory does not exist or is not accessible")?;
    let path_canon =
        std::fs::canonicalize(path).map_err(|_| "created repo path could not be resolved")?;
    if !path_canon.starts_with(&dest_canon) {
        return Err("repo path resolved outside the destination directory".into());
    }
    Ok(())
}

/// Validate a repo name before it is used as a directory segment under `dest`.
/// Rejects empty names, path separators, `..`, and any character that would be
/// unsafe or surprising on a filesystem.
fn validate_repo_name(name: &str) -> R<()> {
    if name.is_empty() {
        return Err("repo name cannot be empty".into());
    }
    if name == "." || name == ".." {
        return Err("repo name cannot be '.' or '..'".into());
    }
    if name.contains('/') || name.contains('\\') || name.contains("..") {
        return Err("repo name cannot contain path separators or '..'".into());
    }
    // Conservative allow-list: alphanumeric, dot, dash, underscore, space.
    if !name
        .chars()
        .all(|c| c.is_alphanumeric() || c == '.' || c == '-' || c == '_' || c == ' ')
    {
        return Err("repo name may only contain letters, digits, spaces, '.', '-', '_'".into());
    }
    Ok(())
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

async fn delete_workspace_after_fence(app: tauri::AppHandle, db: &Db, workspace_id: i32) -> R<()> {
    let action_cleanups = lock_repo_action_cleanups(
        db,
        repo::repo_action_executions_requiring_lock_for_workspace(db, workspace_id)
            .await
            .map_err(e)?,
    )
    .await?;
    let action_cleanup_plans = repo_action_cleanup_plans(&action_cleanups);
    let engine_state = app.state::<crate::lead_chat::engine::LeadChatState>();
    let engine_admission = engine_state.engine_admission_write().await;
    // Resolve the complete stop set while rows still exist, but do not mutate
    // a running engine until the DB cascade commits. The workspace deletion
    // marker already fences new engine admission; a failed cascade can now
    // clear that marker and leave every live turn untouched.
    let engine_keys = workspace_engine_keys(db, workspace_id).await?;
    let repo_ids = repo::list_repos(db, workspace_id)
        .await
        .map_err(e)?
        .into_iter()
        .map(|repo| repo.id)
        .collect::<Vec<_>>();
    let bus = app.state::<crate::bus::BusRegistry>();
    let scope = workspace_ask_scope(db, workspace_id).await?;
    let affected_threads = scope.affected_thread_ids();
    let lifecycle_guards = lock_thread_lifecycles(&bus, &affected_threads).await;
    let mut closing_asks = std::collections::BTreeMap::new();
    for thread_id in &scope.thread_ids {
        let (_, ask_ids) = bus.begin_thread_close(*thread_id);
        closing_asks.insert(*thread_id, ask_ids);
    }
    let effects = match repo::delete_workspace_cascade_with_human_cancellations_and_action_cleanups(
        db,
        workspace_id,
        &action_cleanup_plans,
    )
    .await
    {
        Ok(effects) => effects,
        Err(error) => {
            for thread_id in &scope.thread_ids {
                bus.rollback_thread_close(*thread_id);
            }
            return Err(e(error));
        }
    };
    stop_engines_by_key(&app, &engine_keys).await;
    drop(engine_admission);
    apply_committed_bus_delete_effects(
        &bus,
        &effects.cancelled_requests,
        &effects.removed_threads,
        &effects.removed_directions,
        &closing_asks,
    );
    purge_committed_permission_effects(
        app.state::<crate::ask::AskRegistry>().inner(),
        &effects.removed_threads,
        &effects.removed_directions,
    )
    .await?;
    cleanup_locked_repo_actions(db, &action_cleanups).await;
    drop(lifecycle_guards);
    for repo_id in repo_ids {
        crate::curator::run_forget(repo_id);
    }
    materialize::cleanup_removed_worktrees(&effects.removed_worktrees)
        .await
        .map_err(e)
}

fn human_cancel_event_ids(in_memory: Vec<u64>, durable: &[i32]) -> Vec<u64> {
    let mut ids = in_memory
        .into_iter()
        .collect::<std::collections::BTreeSet<_>>();
    ids.extend(
        durable
            .iter()
            .filter_map(|request_id| u64::try_from(*request_id).ok()),
    );
    ids.into_iter().collect()
}

async fn lock_thread_lifecycles(
    bus: &crate::bus::BusRegistry,
    thread_ids: &std::collections::BTreeSet<i32>,
) -> Vec<tokio::sync::OwnedMutexGuard<()>> {
    let mut guards = Vec::with_capacity(thread_ids.len());
    for thread_id in thread_ids {
        guards.push(bus.thread_lifecycle_gate(*thread_id).lock_owned().await);
    }
    guards
}

fn apply_committed_bus_delete_effects(
    bus: &crate::bus::BusRegistry,
    cancelled_requests: &[repo::CancelledHumanRequest],
    removed_threads: &[i32],
    removed_directions: &[(i32, i32)],
    closing_asks: &std::collections::BTreeMap<i32, Vec<u64>>,
) {
    use std::collections::{BTreeMap, BTreeSet};

    let removed_thread_ids = removed_threads.iter().copied().collect::<BTreeSet<_>>();
    let mut events = BTreeMap::<i32, BTreeSet<u64>>::new();
    for request in cancelled_requests {
        if let Ok(request_id) = u64::try_from(request.request_id) {
            events
                .entry(request.thread_id)
                .or_default()
                .insert(request_id);
        }
    }

    for thread_id in removed_threads {
        bus.apply_thread_human_cancellation(*thread_id);
        events
            .entry(*thread_id)
            .or_default()
            .extend(closing_asks.get(thread_id).into_iter().flatten().copied());
    }
    for (thread_id, request_ids) in &events {
        if removed_thread_ids.contains(thread_id) {
            continue;
        }
        let ids = request_ids.iter().copied().collect::<Vec<_>>();
        bus.apply_human_cancellations_by_id(*thread_id, &ids);
    }
    for (thread_id, direction_id) in removed_directions {
        if removed_thread_ids.contains(thread_id) {
            continue;
        }
        events
            .entry(*thread_id)
            .or_default()
            .extend(bus.apply_direction_human_cancellation(*thread_id, &direction_id.to_string()));
    }
    for (thread_id, ask_ids) in events {
        let ask_ids = ask_ids.into_iter().collect::<Vec<_>>();
        bus.notify_cancelled_asks(thread_id, &ask_ids);
    }
    for thread_id in removed_threads {
        bus.commit_thread_close(*thread_id);
    }
}

async fn purge_committed_permission_effects(
    asks: &crate::ask::AskRegistry,
    removed_threads: &[i32],
    removed_directions: &[(i32, i32)],
) -> R<()> {
    let removed_thread_ids = removed_threads
        .iter()
        .copied()
        .collect::<std::collections::BTreeSet<_>>();
    for ask in asks.open() {
        if removed_thread_ids.contains(&ask.thread) {
            asks.cancel(ask.id);
        }
    }
    for thread_id in removed_threads {
        asks.revoke_thread(*thread_id);
    }
    for (thread_id, direction_id) in removed_directions {
        if !removed_thread_ids.contains(thread_id) {
            asks.purge_dir(*thread_id, &direction_id.to_string());
        }
    }
    let _ = crate::auth_persist::flush(asks).await;
    Ok(())
}

#[derive(Default)]
struct WorkspaceAskScope {
    thread_ids: std::collections::BTreeSet<i32>,
    direction_routes: std::collections::BTreeSet<(i32, String)>,
}

impl WorkspaceAskScope {
    fn affected_thread_ids(&self) -> std::collections::BTreeSet<i32> {
        let mut thread_ids = self.thread_ids.clone();
        thread_ids.extend(
            self.direction_routes
                .iter()
                .map(|(thread_id, _)| *thread_id),
        );
        thread_ids
    }
}

async fn workspace_ask_scope(db: &Db, workspace_id: i32) -> R<WorkspaceAskScope> {
    let threads = repo::list_threads(db, workspace_id).await.map_err(e)?;
    let repos = repo::list_repos(db, workspace_id).await.map_err(e)?;
    let mut scope = WorkspaceAskScope::default();
    for thread in threads {
        scope.thread_ids.insert(thread.id);
    }
    for repo_ref in repos {
        for direction in repo::directions_for_repo(db, repo_ref.id)
            .await
            .map_err(e)?
        {
            scope
                .direction_routes
                .insert((direction.thread_id, direction.id.to_string()));
        }
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

async fn stop_engines_by_key(app: &tauri::AppHandle, keys: &std::collections::BTreeSet<i64>) {
    let state = app.state::<crate::lead_chat::engine::LeadChatState>();
    for key in keys {
        if let Some(eng) = state.remove(*key) {
            crate::lead_chat::engine::stop(app, &eng).await;
        }
    }
}

async fn workspace_engine_keys(db: &Db, workspace_id: i32) -> R<std::collections::BTreeSet<i64>> {
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
        for direction in repo::directions_for_repo(db, repo.id).await.map_err(e)? {
            for session in repo::sessions_for_direction(db, direction.id)
                .await
                .map_err(e)?
            {
                keys.insert(session.id as i64);
            }
        }
    }
    Ok(keys)
}

async fn repo_engine_keys(db: &Db, repo_id: i32) -> R<std::collections::BTreeSet<i64>> {
    let mut session_ids = std::collections::BTreeSet::new();
    for session in repo::sessions_for_repo(db, repo_id).await.map_err(e)? {
        session_ids.insert(session.id);
    }
    for direction in repo::directions_for_repo(db, repo_id).await.map_err(e)? {
        for session in repo::sessions_for_direction(db, direction.id)
            .await
            .map_err(e)?
        {
            session_ids.insert(session.id);
        }
    }
    Ok(session_ids.into_iter().map(i64::from).collect())
}

/// Engine registry keys for ONE thread: the lead (`-thread_id`) plus every
/// chat-mode worker keyed by `session_id`. Collected from live session rows, so
/// a caller that also deletes those rows (delete_thread) MUST call this BEFORE
/// the cascade — afterwards the sessions are gone and their engines become
/// unreachable, leaking child processes.
async fn thread_engine_keys(db: &Db, thread_id: i32) -> R<std::collections::BTreeSet<i64>> {
    let mut keys = std::collections::BTreeSet::<i64>::new();
    keys.insert(crate::lead_chat::commands::lead_key(thread_id));
    for session in repo::sessions_for_thread(db, thread_id).await.map_err(e)? {
        keys.insert(session.id as i64);
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

fn repo_action_gate(message_id: i32) -> std::sync::Arc<tokio::sync::Mutex<()>> {
    use std::sync::{Arc, Mutex, OnceLock, Weak};

    static GATES: OnceLock<Mutex<std::collections::HashMap<i32, Weak<tokio::sync::Mutex<()>>>>> =
        OnceLock::new();
    let gates = GATES.get_or_init(|| Mutex::new(std::collections::HashMap::new()));
    let mut gates = gates.lock().unwrap_or_else(|error| error.into_inner());
    if let Some(gate) = gates.get(&message_id).and_then(Weak::upgrade) {
        return gate;
    }
    let gate = Arc::new(tokio::sync::Mutex::new(()));
    gates.insert(message_id, Arc::downgrade(&gate));
    gate
}

fn repo_action_guard_is_present(
    thread_id: Option<i32>,
    message_id: Option<i32>,
    action_id: Option<&str>,
    action_kind: Option<&str>,
) -> R<bool> {
    match (thread_id, message_id, action_id, action_kind) {
        (None, None, None, None) => Ok(false),
        (Some(_), Some(_), Some(_), Some(_)) => Ok(true),
        _ => Err("action_card_stale".to_string()),
    }
}

fn normalized_existing_repo_target(path: &str) -> R<std::path::PathBuf> {
    let path = std::path::Path::new(path);
    if let Ok(canonical) = std::fs::canonicalize(path) {
        return Ok(canonical);
    }
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().map_err(e)?.join(path)
    };
    let mut normalized = std::path::PathBuf::new();
    for component in absolute.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    Ok(normalized)
}

fn normalized_repo_destination(dest: &str) -> R<std::path::PathBuf> {
    let canonical = std::fs::canonicalize(dest)
        .map_err(|_| "destination directory does not exist or is not accessible".to_string())?;
    if !canonical.is_dir() {
        return Err("destination is not a directory".to_string());
    }
    Ok(canonical)
}

fn repo_action_fingerprint(parts: &[&str]) -> String {
    use sha1::{Digest, Sha1};

    let mut digest = Sha1::new();
    for part in parts {
        digest.update((part.len() as u64).to_be_bytes());
        digest.update(part.as_bytes());
    }
    hex::encode(digest.finalize())
}

fn new_repo_action_token() -> String {
    use rand::RngCore;

    let mut token = [0_u8; 20];
    rand::thread_rng().fill_bytes(&mut token);
    hex::encode(token)
}

struct RepoActionAdmission {
    execution: entities::repo_action_execution::Model,
    _os_lock: repo::RepoActionOsLock,
    _lifecycle: Option<tokio::sync::OwnedMutexGuard<()>>,
    _gate: tokio::sync::OwnedMutexGuard<()>,
}

fn acquire_repo_action_os_lock(execution_token: &str) -> R<repo::RepoActionOsLock> {
    repo::acquire_repo_action_os_lock(execution_token).map_err(e)
}

async fn admit_repo_action(
    db: &Db,
    workspace_id: i32,
    thread_id: Option<i32>,
    message_id: Option<i32>,
    action_id: Option<&str>,
    action_kind: Option<&str>,
    expected_action_kind: &str,
    invocation_fingerprint: &str,
    target_path: &std::path::Path,
    staging_parent: Option<&std::path::Path>,
) -> R<Option<RepoActionAdmission>> {
    if !repo_action_guard_is_present(thread_id, message_id, action_id, action_kind)? {
        return Ok(None);
    }
    let Some(thread_id) = thread_id else {
        return Err("action_card_stale".to_string());
    };
    let Some(message_id) = message_id else {
        return Err("action_card_stale".to_string());
    };
    let Some(action_id) = action_id else {
        return Err("action_card_stale".to_string());
    };
    let Some(action_kind) = action_kind else {
        return Err("action_card_stale".to_string());
    };
    let lifecycle_gate = crate::APP_HANDLE
        .get()
        .and_then(|app| app.try_state::<crate::bus::BusRegistry>())
        .map(|bus| bus.thread_lifecycle_gate(thread_id));
    let lifecycle = if let Some(gate) = lifecycle_gate {
        Some(gate.lock_owned().await)
    } else {
        None
    };
    let gate = repo_action_gate(message_id).lock_owned().await;
    let candidate_token = new_repo_action_token();
    let candidate_staging = staging_parent
        .map(|parent| parent.join(format!(".weft-repo-action-{candidate_token}.staging")))
        .unwrap_or_default();
    let request = repo::RepoActionClaimRequest {
        workspace_id,
        thread_id,
        message_id,
        action_id,
        action_kind,
        expected_action_kind,
        invocation_fingerprint,
        execution_token: &candidate_token,
        target_path: &target_path.to_string_lossy(),
        staging_path: &candidate_staging.to_string_lossy(),
    };
    let execution = repo::claim_repo_action_execution(db, &request)
        .await
        .map_err(e)?;
    let os_lock = acquire_repo_action_os_lock(&execution.execution_token)?;
    Ok(Some(RepoActionAdmission {
        execution,
        _os_lock: os_lock,
        _lifecycle: lifecycle,
        _gate: gate,
    }))
}

async fn completed_repo_for_admission(
    db: &Db,
    admission: &RepoActionAdmission,
) -> R<Option<entities::repo_ref::Model>> {
    if admission.execution.status != repo::REPO_ACTION_COMPLETED {
        return Ok(None);
    }
    let Some(repo_ref) = repo::get_repo(db, admission.execution.repo_id)
        .await
        .map_err(e)?
    else {
        return Err("action_card_stale".to_string());
    };
    if repo_ref.workspace_id != admission.execution.workspace_id
        || repo_ref.name != admission.execution.repo_name
    {
        return Err("action_card_stale".to_string());
    }
    Ok(Some(repo_ref))
}

async fn complete_admitted_repo_action(
    db: &Db,
    admission: &mut RepoActionAdmission,
    repo_ref: &entities::repo_ref::Model,
) -> R<()> {
    admission.execution = repo::complete_repo_action_execution(db, &admission.execution, repo_ref)
        .await
        .map_err(e)?;
    Ok(())
}

const REPO_ACTION_TOKEN_MARKER: &str = "weft-action-token";

fn repo_action_target_has_token(path: &std::path::Path, token: &str) -> bool {
    let git_dir = path.join(".git");
    let marker = git_dir.join(REPO_ACTION_TOKEN_MARKER);
    let Ok(git_meta) = std::fs::symlink_metadata(&git_dir) else {
        return false;
    };
    let Ok(marker_meta) = std::fs::symlink_metadata(&marker) else {
        return false;
    };
    if !git_meta.file_type().is_dir() || !marker_meta.file_type().is_file() {
        return false;
    }
    std::fs::read_to_string(marker).is_ok_and(|stored| stored == token)
}

fn repo_action_owner_path(staging: &std::path::Path, token: &str) -> R<std::path::PathBuf> {
    let Some(parent) = staging.parent() else {
        return Err("action_card_stale".to_string());
    };
    Ok(parent.join(format!(".weft-repo-action-{token}.owner")))
}

fn token_file_matches(path: &std::path::Path, token: &str) -> bool {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return false;
    };
    meta.file_type().is_file() && std::fs::read_to_string(path).is_ok_and(|stored| stored == token)
}

fn write_token_file(path: &std::path::Path, token: &str, create_new: bool) -> R<()> {
    use std::io::Write;

    let mut options = std::fs::OpenOptions::new();
    options.write(true);
    if create_new {
        options.create_new(true);
    } else {
        options.create(true).truncate(true);
    }
    let mut file = options
        .open(path)
        .map_err(|error| format!("cannot write repository action marker: {error}"))?;
    file.write_all(token.as_bytes())
        .map_err(|error| format!("cannot write repository action marker: {error}"))?;
    file.sync_all()
        .map_err(|error| format!("cannot persist repository action marker: {error}"))
}

fn write_repo_action_target_marker(path: &std::path::Path, token: &str) -> R<()> {
    let git_dir = path.join(".git");
    let meta = std::fs::symlink_metadata(&git_dir)
        .map_err(|_| "created repository has no .git directory".to_string())?;
    if !meta.file_type().is_dir() {
        return Err("created repository has an unsafe .git path".to_string());
    }
    let marker = git_dir.join(REPO_ACTION_TOKEN_MARKER);
    if marker.exists() {
        if token_file_matches(&marker, token) {
            return Ok(());
        }
        return Err("created repository has a foreign action marker".to_string());
    }
    write_token_file(&marker, token, true)
}

fn cleanup_owned_staging_checked(execution: &entities::repo_action_execution::Model) -> R<()> {
    if execution.staging_path.is_empty() {
        return Ok(());
    }
    let staging = std::path::Path::new(&execution.staging_path);
    let Ok(owner) = repo_action_owner_path(staging, &execution.execution_token) else {
        return Ok(());
    };
    let owns_staging = token_file_matches(&owner, &execution.execution_token)
        || repo_action_target_has_token(staging, &execution.execution_token);
    if owns_staging && staging.exists() {
        std::fs::remove_dir_all(staging)
            .map_err(|error| format!("cannot remove repository action staging path: {error}"))?;
    }
    if token_file_matches(&owner, &execution.execution_token) {
        std::fs::remove_file(owner)
            .map_err(|error| format!("cannot remove repository action owner marker: {error}"))?;
    }
    Ok(())
}

fn cleanup_owned_target_checked(execution: &entities::repo_action_execution::Model) -> R<()> {
    let target = std::path::Path::new(&execution.target_path);
    if repo_action_target_has_token(target, &execution.execution_token) {
        std::fs::remove_dir_all(target)
            .map_err(|error| format!("cannot remove repository action target: {error}"))?;
    }
    Ok(())
}

fn cleanup_owned_target_marker_checked(
    execution: &entities::repo_action_execution::Model,
) -> R<()> {
    let marker = std::path::Path::new(&execution.target_path)
        .join(".git")
        .join(REPO_ACTION_TOKEN_MARKER);
    if token_file_matches(&marker, &execution.execution_token) {
        std::fs::remove_file(marker)
            .map_err(|error| format!("cannot remove repository action target marker: {error}"))?;
    }
    Ok(())
}

fn repo_paths_match(left: &str, right: &str) -> bool {
    let left_path = std::path::Path::new(left);
    let right_path = std::path::Path::new(right);
    match (
        std::fs::canonicalize(left_path),
        std::fs::canonicalize(right_path),
    ) {
        (Ok(left), Ok(right)) => left == right,
        _ => left_path == right_path,
    }
}

pub(crate) struct LockedRepoActionCleanup {
    execution: entities::repo_action_execution::Model,
    target_is_registered: bool,
    _lock: repo::RepoActionOsLock,
}

pub(crate) async fn lock_repo_action_cleanups(
    db: &Db,
    mut executions: Vec<entities::repo_action_execution::Model>,
) -> R<Vec<LockedRepoActionCleanup>> {
    executions.sort_by(|left, right| left.execution_token.cmp(&right.execution_token));
    let mut locked = Vec::with_capacity(executions.len());
    for observed in executions {
        let lock = acquire_repo_action_os_lock(&observed.execution_token).map_err(|error| {
            if error == "action_card_in_progress" {
                "repository action is still in progress".to_string()
            } else {
                error
            }
        })?;
        let Some(execution) = repo::get_repo_action_execution_by_id(db, observed.id)
            .await
            .map_err(e)?
        else {
            continue;
        };
        if execution.execution_token != observed.execution_token {
            return Err("repository action cleanup ownership changed".to_string());
        }
        let target_is_registered = if execution.status == repo::REPO_ACTION_CLEANUP_PENDING {
            execution.cleanup_preserve_target
        } else {
            let repos = repo::list_repos(db, execution.workspace_id)
                .await
                .map_err(e)?;
            repos
                .iter()
                .any(|repo_ref| repo_paths_match(&execution.target_path, &repo_ref.local_git_path))
        };
        locked.push(LockedRepoActionCleanup {
            execution,
            target_is_registered,
            _lock: lock,
        });
    }
    Ok(locked)
}

pub(crate) fn repo_action_cleanup_plans(
    locked: &[LockedRepoActionCleanup],
) -> Vec<repo::RepoActionCleanupPlan> {
    locked
        .iter()
        .filter(|item| {
            matches!(
                item.execution.status.as_str(),
                repo::REPO_ACTION_PENDING | repo::REPO_ACTION_MATERIALIZED
            )
        })
        .map(|item| repo::RepoActionCleanupPlan {
            execution_id: item.execution.id,
            execution_token: item.execution.execution_token.clone(),
            expected_status: item.execution.status.clone(),
            preserve_target: item.target_is_registered,
        })
        .collect()
}

pub(crate) fn repo_action_rewind_plans(
    locked: &[LockedRepoActionCleanup],
) -> Vec<repo::RepoActionRewindPlan> {
    locked
        .iter()
        .map(|item| repo::RepoActionRewindPlan {
            execution_id: item.execution.id,
            execution_token: item.execution.execution_token.clone(),
            thread_id: item.execution.thread_id,
            message_id: item.execution.message_id,
            expected_status: item.execution.status.clone(),
            expected_feedback_state: item.execution.feedback_state.clone(),
        })
        .collect()
}

async fn cleanup_locked_repo_action_with<F>(
    db: &Db,
    item: &LockedRepoActionCleanup,
    cleanup: F,
) -> R<()>
where
    F: FnOnce(&entities::repo_action_execution::Model, bool) -> R<()>,
{
    if item.execution.status == repo::REPO_ACTION_COMPLETED {
        return Ok(());
    }
    let Some(current) = repo::get_repo_action_execution_by_id(db, item.execution.id)
        .await
        .map_err(e)?
    else {
        return Ok(());
    };
    if current.execution_token != item.execution.execution_token
        || current.status != repo::REPO_ACTION_CLEANUP_PENDING
    {
        return Err("repository action cleanup record changed before cleanup".to_string());
    }
    cleanup(&current, current.cleanup_preserve_target)?;
    if !repo::delete_repo_action_cleanup_record(db, current.id, &current.execution_token)
        .await
        .map_err(e)?
    {
        return Err("repository action cleanup record changed before acknowledgement".to_string());
    }
    Ok(())
}

pub(crate) async fn cleanup_locked_repo_actions(db: &Db, locked: &[LockedRepoActionCleanup]) {
    for item in locked {
        let cleanup =
            cleanup_locked_repo_action_with(db, item, |execution, target_is_registered| {
                cleanup_owned_staging_checked(execution)?;
                if target_is_registered {
                    cleanup_owned_target_marker_checked(execution)
                } else {
                    cleanup_owned_target_checked(execution)
                }
            })
            .await;
        if let Err(error) = cleanup {
            eprintln!(
                "[weft] repository action cleanup {} retained for retry: {error}",
                item.execution.id
            );
        }
    }
}

pub(crate) fn spawn_pending_repo_action_cleanups(db: Db) {
    tauri::async_runtime::spawn(async move {
        let executions = match repo::pending_repo_action_cleanups(&db).await {
            Ok(executions) => executions,
            Err(error) => {
                eprintln!("[weft] list pending repository action cleanups failed: {error}");
                return;
            }
        };
        for execution in executions {
            match lock_repo_action_cleanups(&db, vec![execution]).await {
                Ok(locked) => cleanup_locked_repo_actions(&db, &locked).await,
                Err(error) => eprintln!(
                    "[weft] repository action cleanup retry could not acquire ownership: {error}"
                ),
            }
        }
    });
}

async fn abandon_pending_repo_action(
    db: &Db,
    admission: &RepoActionAdmission,
    cleanup_target: bool,
    error: String,
) -> String {
    let cleanup = match repo::stage_pending_repo_action_cleanup(
        db,
        admission.execution.id,
        &admission.execution.execution_token,
        !cleanup_target,
    )
    .await
    {
        Ok(cleanup) => cleanup,
        Err(stage_error) => {
            return format!("{error}; failed to retain repository action cleanup: {stage_error}");
        }
    };
    let cleanup_result = cleanup_owned_staging_checked(&cleanup).and_then(|()| {
        if cleanup.cleanup_preserve_target {
            cleanup_owned_target_marker_checked(&cleanup)
        } else {
            cleanup_owned_target_checked(&cleanup)
        }
    });
    if let Err(cleanup_error) = cleanup_result {
        return format!("{error}; cleanup retained for retry: {cleanup_error}");
    }
    match repo::delete_repo_action_cleanup_record(db, cleanup.id, &cleanup.execution_token).await {
        Ok(true) => error,
        Ok(false) => format!("{error}; cleanup acknowledgement changed before commit"),
        Err(cleanup_error) => {
            format!("{error}; cleanup acknowledgement retained for retry: {cleanup_error}")
        }
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn repo_action_c_path(path: &std::path::Path) -> std::io::Result<std::ffi::CString> {
    use std::os::unix::ffi::OsStrExt;

    std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "repository action path contains NUL",
        )
    })
}

/// Promote a completed sibling staging directory without ever replacing a
/// target that appeared after the preflight. Staging and target share a parent,
/// so the platform primitive is also the single-filesystem atomic commit.
fn promote_repo_action_noreplace(
    staging: &std::path::Path,
    target: &std::path::Path,
) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        let staging = repo_action_c_path(staging)?;
        let target = repo_action_c_path(target)?;
        let result = unsafe {
            libc::renameatx_np(
                libc::AT_FDCWD,
                staging.as_ptr(),
                libc::AT_FDCWD,
                target.as_ptr(),
                libc::RENAME_EXCL,
            )
        };
        if result == 0 {
            return Ok(());
        }
        return Err(std::io::Error::last_os_error());
    }

    #[cfg(target_os = "linux")]
    {
        let staging = repo_action_c_path(staging)?;
        let target = repo_action_c_path(target)?;
        let result = unsafe {
            libc::renameat2(
                libc::AT_FDCWD,
                staging.as_ptr(),
                libc::AT_FDCWD,
                target.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        if result == 0 {
            return Ok(());
        }
        return Err(std::io::Error::last_os_error());
    }

    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;

        #[link(name = "Kernel32")]
        extern "system" {
            fn MoveFileExW(
                existing_file_name: *const u16,
                new_file_name: *const u16,
                flags: u32,
            ) -> i32;
        }

        fn wide_path(path: &std::path::Path) -> std::io::Result<Vec<u16>> {
            let mut encoded = path.as_os_str().encode_wide().collect::<Vec<_>>();
            if encoded.contains(&0) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "repository action path contains NUL",
                ));
            }
            encoded.push(0);
            Ok(encoded)
        }

        let staging = wide_path(staging)?;
        let target = wide_path(target)?;
        // flags=0 deliberately omits MOVEFILE_REPLACE_EXISTING.
        let result = unsafe { MoveFileExW(staging.as_ptr(), target.as_ptr(), 0) };
        if result != 0 {
            return Ok(());
        }
        return Err(std::io::Error::last_os_error());
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
    {
        let _ = (staging, target);
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "atomic no-replace repository promotion is unsupported on this platform",
        ))
    }
}

/// Materialize clone/init work through a token-owned sibling staging path, then
/// atomically rename it into place. A target marker is the only evidence that
/// an occupied final directory belongs to this execution.
async fn materialize_repo_action<F>(
    db: &Db,
    admission: &mut RepoActionAdmission,
    mutation: F,
) -> R<std::path::PathBuf>
where
    F: FnOnce(&std::path::Path) -> anyhow::Result<()> + Send + 'static,
{
    let target = std::path::PathBuf::from(&admission.execution.target_path);
    let staging = std::path::PathBuf::from(&admission.execution.staging_path);
    let token = admission.execution.execution_token.clone();
    let Some(parent) = target.parent() else {
        return Err("action_card_stale".to_string());
    };
    let expected_staging = parent.join(format!(".weft-repo-action-{token}.staging"));
    if staging != expected_staging || staging == target {
        return Err("action_card_stale".to_string());
    }

    if admission.execution.status == repo::REPO_ACTION_COMPLETED {
        return Ok(target);
    }
    if admission.execution.status == repo::REPO_ACTION_MATERIALIZED {
        if repo_action_target_has_token(&target, &token) {
            return Ok(target);
        }
        return Err("action_card_stale".to_string());
    }
    if admission.execution.status != repo::REPO_ACTION_PENDING {
        return Err("action_card_stale".to_string());
    }

    if std::fs::symlink_metadata(&target).is_ok() {
        if repo_action_target_has_token(&target, &token) {
            cleanup_owned_staging_checked(&admission.execution)?;
            admission.execution =
                repo::mark_repo_action_materialized(db, admission.execution.id, &token)
                    .await
                    .map_err(e)?;
            return Ok(target);
        }
        let error = format!(
            "repo path already exists and is not owned by this action: {}",
            target.display()
        );
        return Err(abandon_pending_repo_action(db, admission, false, error).await);
    }

    let owner = repo_action_owner_path(&staging, &token)?;
    if staging.exists() {
        if repo_action_target_has_token(&staging, &token) {
            // Git completed before a crash; continue with the atomic promotion.
        } else if token_file_matches(&owner, &token) {
            std::fs::remove_dir_all(&staging).map_err(|error| {
                format!("cannot clean interrupted repository staging path: {error}")
            })?;
        } else {
            let error = format!(
                "repository staging path is occupied by another owner: {}",
                staging.display()
            );
            return Err(abandon_pending_repo_action(db, admission, false, error).await);
        }
    }

    if !staging.exists() {
        if owner.exists() && !token_file_matches(&owner, &token) {
            let error = format!(
                "repository staging marker is occupied by another owner: {}",
                owner.display()
            );
            return Err(abandon_pending_repo_action(db, admission, false, error).await);
        }
        if token_file_matches(&owner, &token) {
            std::fs::remove_file(&owner)
                .map_err(|error| format!("cannot reset repository staging marker: {error}"))?;
        }
        if let Err(error) = write_token_file(&owner, &token, true) {
            let error = abandon_pending_repo_action(db, admission, false, error).await;
            return Err(error);
        }
        let staging_for_mutation = staging.clone();
        let mutation_result = tokio::task::spawn_blocking(move || mutation(&staging_for_mutation))
            .await
            .map_err(|error| error.to_string())
            .and_then(|result| result.map_err(e));
        if let Err(error) = mutation_result {
            let error = abandon_pending_repo_action(db, admission, false, error).await;
            return Err(error);
        }
        if let Err(error) = write_repo_action_target_marker(&staging, &token) {
            let error = abandon_pending_repo_action(db, admission, false, error).await;
            return Err(error);
        }
    }

    if let Err(error) = promote_repo_action_noreplace(&staging, &target) {
        let message = format!("cannot atomically install repository: {error}");
        let message = abandon_pending_repo_action(db, admission, false, message).await;
        return Err(message);
    }
    if token_file_matches(&owner, &token) {
        let _ = std::fs::remove_file(&owner);
    }
    if let Err(error) = ensure_path_under_dest(&target, parent) {
        let error = abandon_pending_repo_action(db, admission, true, error).await;
        return Err(error);
    }

    // From this point onward, failures retain both claim and target marker.
    // The exact retry recognizes the marker and cannot re-run clone/init.
    admission.execution = repo::mark_repo_action_materialized(db, admission.execution.id, &token)
        .await
        .map_err(e)?;
    Ok(target)
}

/// Register an existing local git repo: validate, record, profile. Shared by
/// add (existing) / clone / create — they all converge on "a path weft refs".
async fn register_repo_without_schedule(
    db: &Db,
    workspace_id: i32,
    name: &str,
    path: &str,
    held_action_lock: Option<&repo::RepoActionOsLock>,
) -> R<entities::repo_ref::Model> {
    let p = std::path::Path::new(path);
    // A cleanup owns the same token lock from planning through post-commit
    // filesystem removal. Join that protocol before validating or persisting a
    // marker-bearing target, and keep the guard through every registration
    // write so cleanup and adoption have one linearization order.
    let registration_lock =
        repo::acquire_repo_action_target_registration_lock(p, held_action_lock).map_err(e)?;
    let held_action_lock = registration_lock.as_ref().or(held_action_lock);
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
            if let Some(rem) =
                crate::git::remote_url(std::path::Path::new(&existing.local_git_path))
            {
                let _ =
                    repo::set_repo_remote(db, existing.id, &crate::git::redact_remote(&rem)).await;
            }
        }
        if let Ok(canon) = std::fs::canonicalize(&existing.local_git_path) {
            let canon = canon.to_string_lossy();
            if canon != existing.local_git_path {
                let _ = match held_action_lock {
                    Some(action_lock) => {
                        repo::set_repo_path_with_action_lock(db, existing.id, &canon, action_lock)
                            .await
                    }
                    None => repo::set_repo_path(db, existing.id, &canon).await,
                };
            }
        }
    }
    // Mark `base` as the captured default ONLY when it is a genuinely-vetted default
    // (origin/HEAD or a real main/master branch — `base_is_vetted_default`). For a
    // single-branch / nonstandard checkout with NO origin/HEAD and NO main/master, the
    // base is whatever happened to be checked out / the "main" last-resort, which is NOT
    // vetted; marking it is_default=true would make the offline fallback
    // (`recorded_base_or_default`) trust it over the main/master chain (R47-2).
    let mut r = match held_action_lock {
        Some(action_lock) => {
            repo::add_repo_ref_with_action_lock(
                db,
                workspace_id,
                name,
                &canonical,
                &base,
                &remote,
                base_is_vetted_default,
                action_lock,
            )
            .await
        }
        None => {
            repo::add_repo_ref(
                db,
                workspace_id,
                name,
                &canonical,
                &base,
                &remote,
                base_is_vetted_default,
            )
            .await
        }
    }
    .map_err(e)?;
    // If dedup resolved to an EXISTING row (by remote) at a different path whose
    // checkout is gone, repoint it to the path the user just gave us — a local add
    // OR a clone — so we don't keep pointing at a dead checkout and report success.
    // (A live duplicate keeps the existing row; clone_repo removes the redundant
    // clone dir in that case.) Done before profiling so facts come from the live path.
    if r.local_git_path != canonical
        && !crate::git::is_git_repo(std::path::Path::new(&r.local_git_path))
    {
        let update = match held_action_lock {
            Some(action_lock) => {
                repo::set_repo_path_with_action_lock(db, r.id, &canonical, action_lock).await
            }
            None => repo::set_repo_path(db, r.id, &canonical).await,
        };
        if let Ok(Some(updated)) = update {
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
    Ok(r)
}

/// Schedule only after a guarded execution's completion commit. Exact completed
/// replays schedule again as a crash-safe nudge; the curator coalesces duplicates.
fn schedule_repo_curator(db: &Db, repo_ref: &entities::repo_ref::Model) {
    if crate::APP_HANDLE.get().is_some() {
        let db_bg = db.clone();
        let ws = repo_ref.workspace_id;
        tauri::async_runtime::spawn(async move {
            crate::curator::analyze_workspace_coalesced(&db_bg, ws, false).await;
        });
    }
}

/// Legacy/guardless registration keeps its existing eager scheduling behavior.
async fn register_repo(
    db: &Db,
    workspace_id: i32,
    name: &str,
    path: &str,
) -> R<entities::repo_ref::Model> {
    let r = register_repo_without_schedule(db, workspace_id, name, path, None).await?;
    schedule_repo_curator(db, &r);
    Ok(r)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RepoActionExecutionOutcome {
    FreshlyCompleted,
    Replayed,
}

#[derive(Debug)]
struct RepoActionInvocationResult {
    repo: entities::repo_ref::Model,
    outcome: RepoActionExecutionOutcome,
    execution_id: Option<i32>,
}

#[derive(serde::Serialize)]
pub struct RepoActionCommandResult {
    pub execution_outcome: &'static str,
    pub repo: Option<entities::repo_ref::Model>,
}

impl From<RepoActionInvocationResult> for RepoActionCommandResult {
    fn from(result: RepoActionInvocationResult) -> Self {
        let execution_outcome = match result.outcome {
            RepoActionExecutionOutcome::FreshlyCompleted => "freshly_completed",
            RepoActionExecutionOutcome::Replayed => "replayed",
        };
        Self {
            execution_outcome,
            repo: Some(result.repo),
        }
    }
}

fn repo_action_in_progress_result() -> RepoActionCommandResult {
    RepoActionCommandResult {
        execution_outcome: "in_progress",
        repo: None,
    }
}

async fn drain_repo_action_feedback_with<F, Fut>(db: &Db, execution_id: i32, deliver: F) -> R<bool>
where
    F: FnOnce(i32, serde_json::Value) -> Fut,
    Fut: std::future::Future<Output = R<bool>>,
{
    let Some(snapshot) = repo::get_repo_action_execution_by_id(db, execution_id)
        .await
        .map_err(e)?
    else {
        return Ok(true);
    };
    if snapshot.status != repo::REPO_ACTION_COMPLETED
        || snapshot.feedback_state != repo::REPO_ACTION_FEEDBACK_PENDING
    {
        return Ok(true);
    }
    let _lock = match acquire_repo_action_os_lock(&snapshot.execution_token) {
        Ok(lock) => lock,
        Err(error) if error == "action_card_in_progress" => return Ok(false),
        Err(error) => return Err(error),
    };
    let Some(current) = repo::get_repo_action_execution_by_id(db, execution_id)
        .await
        .map_err(e)?
    else {
        return Ok(true);
    };
    if current.execution_token != snapshot.execution_token
        || current.status != repo::REPO_ACTION_COMPLETED
        || current.feedback_state != repo::REPO_ACTION_FEEDBACK_PENDING
    {
        return Ok(true);
    }
    cleanup_completed_execution_target(&current)?;
    let payload: serde_json::Value = serde_json::from_str(&current.feedback_payload)
        .map_err(|error| format!("invalid repository action feedback payload: {error}"))?;
    if !deliver(current.thread_id, payload).await? {
        return Ok(false);
    }
    repo::mark_repo_action_feedback_delivered(db, current.id, &current.execution_token)
        .await
        .map_err(e)?;
    repo::delete_delivered_repo_action_feedback_if_repo_missing(
        db,
        current.id,
        &current.execution_token,
        current.repo_id,
    )
    .await
    .map_err(e)?;
    Ok(true)
}

async fn drain_repo_action_feedback_once(db: &Db, execution_id: i32) -> R<bool> {
    let Some(app) = crate::APP_HANDLE.get().cloned() else {
        return Ok(false);
    };
    let db_for_delivery = db.clone();
    drain_repo_action_feedback_with(db, execution_id, move |thread_id, payload| {
        let app = app.clone();
        let db = db_for_delivery.clone();
        async move {
            crate::lead_chat::commands::post_lead_tool_result_inner(
                &app, &db, thread_id, payload, "en",
            )
            .await
        }
    })
    .await
}

fn spawn_repo_action_feedback_drain(db: Db, execution_id: i32) {
    tauri::async_runtime::spawn(async move {
        for delay in [0_u64, 1_000, 5_000, 30_000] {
            if delay > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
            }
            match drain_repo_action_feedback_once(&db, execution_id).await {
                Ok(true) => return,
                Ok(false) => {}
                Err(error) => {
                    eprintln!(
                        "[weft] repository action feedback {execution_id} delivery failed: {error}"
                    );
                }
            }
        }
    });
}

pub(crate) fn spawn_pending_repo_action_feedback(db: Db, thread_id: Option<i32>) {
    tauri::async_runtime::spawn(async move {
        match repo::pending_repo_action_feedback(&db, thread_id).await {
            Ok(executions) => {
                for execution in executions {
                    spawn_repo_action_feedback_drain(db.clone(), execution.id);
                }
            }
            Err(error) => {
                eprintln!("[weft] list pending repository action feedback failed: {error}");
            }
        }
    });
}

async fn add_repo_ref_inner(
    db: &Db,
    workspace_id: i32,
    name: String,
    local_git_path: String,
    thread_id: Option<i32>,
    message_id: Option<i32>,
    action_id: Option<String>,
    action_kind: Option<String>,
) -> R<RepoActionInvocationResult> {
    let guarded = repo_action_guard_is_present(
        thread_id,
        message_id,
        action_id.as_deref(),
        action_kind.as_deref(),
    )?;
    if !guarded {
        let repo = register_repo(db, workspace_id, &name, &local_git_path).await?;
        return Ok(RepoActionInvocationResult {
            repo,
            outcome: RepoActionExecutionOutcome::FreshlyCompleted,
            execution_id: None,
        });
    }

    let target = normalized_existing_repo_target(&local_git_path)?;
    let target_text = target.to_string_lossy().into_owned();
    let fingerprint = repo_action_fingerprint(&["add", &target_text, &name]);
    let admission = admit_repo_action(
        db,
        workspace_id,
        thread_id,
        message_id,
        action_id.as_deref(),
        action_kind.as_deref(),
        "add",
        &fingerprint,
        &target,
        None,
    )
    .await?;
    let Some(mut admission) = admission else {
        return Err("action_card_stale".to_string());
    };
    if let Some(repo_ref) = completed_repo_for_admission(db, &admission).await? {
        cleanup_completed_action_target(&admission, &repo_ref)?;
        schedule_repo_curator(db, &repo_ref);
        return Ok(RepoActionInvocationResult {
            repo: repo_ref,
            outcome: RepoActionExecutionOutcome::Replayed,
            execution_id: Some(admission.execution.id),
        });
    }
    if admission.execution.status == repo::REPO_ACTION_PENDING {
        if !crate::git::is_git_repo(&target) {
            let error = abandon_pending_repo_action(
                db,
                &admission,
                false,
                "not a git repository".to_string(),
            )
            .await;
            return Err(error);
        }
        admission.execution = repo::mark_repo_action_materialized(
            db,
            admission.execution.id,
            &admission.execution.execution_token,
        )
        .await
        .map_err(e)?;
    }
    if let Some(repo_ref) = completed_repo_for_admission(db, &admission).await? {
        cleanup_completed_action_target(&admission, &repo_ref)?;
        schedule_repo_curator(db, &repo_ref);
        return Ok(RepoActionInvocationResult {
            repo: repo_ref,
            outcome: RepoActionExecutionOutcome::Replayed,
            execution_id: Some(admission.execution.id),
        });
    }
    let repo_ref = register_repo_without_schedule(
        db,
        workspace_id,
        &name,
        &target_text,
        Some(&admission._os_lock),
    )
    .await?;
    complete_admitted_repo_action(db, &mut admission, &repo_ref).await?;
    cleanup_completed_action_target(&admission, &repo_ref)?;
    schedule_repo_curator(db, &repo_ref);
    Ok(RepoActionInvocationResult {
        repo: repo_ref,
        outcome: RepoActionExecutionOutcome::FreshlyCompleted,
        execution_id: Some(admission.execution.id),
    })
}

#[tauri::command]
pub async fn add_repo_ref(
    db: State<'_, Db>,
    workspace_id: i32,
    name: String,
    local_git_path: String,
    thread_id: Option<i32>,
    message_id: Option<i32>,
    action_id: Option<String>,
    action_kind: Option<String>,
) -> R<RepoActionCommandResult> {
    match add_repo_ref_inner(
        &db,
        workspace_id,
        name,
        local_git_path,
        thread_id,
        message_id,
        action_id,
        action_kind,
    )
    .await
    {
        Ok(result) => {
            if let Some(execution_id) = result.execution_id {
                spawn_repo_action_feedback_drain(db.inner().clone(), execution_id);
            }
            Ok(result.into())
        }
        Err(error) if error == "action_card_in_progress" => Ok(repo_action_in_progress_result()),
        Err(error) => Err(error),
    }
}

/// Cheap pre-check used by first-run onboarding to validate every picked folder
/// *before* a workspace is created — so a non-git folder can't leave an orphan
/// workspace behind. Mirrors the guard inside `register_repo`.
#[tauri::command]
pub fn check_git_repo(path: String) -> bool {
    crate::git::is_git_repo(std::path::Path::new(&path))
}

/// Clone a remote git URL into `<dest>/<name>`, then register it.
fn cleanup_completed_action_target(
    admission: &RepoActionAdmission,
    _repo_ref: &entities::repo_ref::Model,
) -> R<()> {
    cleanup_completed_execution_target(&admission.execution)
}

fn cleanup_completed_execution_target(execution: &entities::repo_action_execution::Model) -> R<()> {
    if execution.cleanup_preserve_target {
        cleanup_owned_target_marker_checked(execution)?;
    } else {
        cleanup_owned_target_checked(execution)?;
    }
    Ok(())
}

async fn clone_repo_inner(
    db: &Db,
    workspace_id: i32,
    url: String,
    dest: String,
    name: String,
    thread_id: Option<i32>,
    message_id: Option<i32>,
    action_id: Option<String>,
    action_kind: Option<String>,
) -> R<RepoActionInvocationResult> {
    validate_repo_name(&name)?;
    let guarded = repo_action_guard_is_present(
        thread_id,
        message_id,
        action_id.as_deref(),
        action_kind.as_deref(),
    )?;
    if !guarded {
        let path = std::path::Path::new(&dest).join(&name);
        reject_occupied_repo_target(&path)?;
        let p = path.clone();
        tokio::task::spawn_blocking(move || crate::git::clone_repo(&url, &p))
            .await
            .map_err(|err| err.to_string())?
            .map_err(e)?;
        ensure_path_under_dest(&path, std::path::Path::new(&dest))?;
        let r = register_repo(db, workspace_id, &name, &path.to_string_lossy()).await?;
        let cloned = std::fs::canonicalize(&path).ok();
        let registered = std::fs::canonicalize(&r.local_git_path).ok();
        if cloned.is_some() && cloned != registered {
            let _ = std::fs::remove_dir_all(&path);
        }
        return Ok(RepoActionInvocationResult {
            repo: r,
            outcome: RepoActionExecutionOutcome::FreshlyCompleted,
            execution_id: None,
        });
    }

    let destination = normalized_repo_destination(&dest)?;
    let path = destination.join(&name);
    let path_text = path.to_string_lossy().into_owned();
    let normalized_remote = crate::git::redact_remote(url.trim());
    let fingerprint = repo_action_fingerprint(&["clone", &normalized_remote, &path_text, &name]);
    let admission = admit_repo_action(
        db,
        workspace_id,
        thread_id,
        message_id,
        action_id.as_deref(),
        action_kind.as_deref(),
        "clone",
        &fingerprint,
        &path,
        Some(&destination),
    )
    .await?;
    let Some(mut admission) = admission else {
        return Err("action_card_stale".to_string());
    };
    if let Some(repo_ref) = completed_repo_for_admission(db, &admission).await? {
        cleanup_completed_action_target(&admission, &repo_ref)?;
        schedule_repo_curator(db, &repo_ref);
        return Ok(RepoActionInvocationResult {
            repo: repo_ref,
            outcome: RepoActionExecutionOutcome::Replayed,
            execution_id: Some(admission.execution.id),
        });
    }
    let clone_url = url;
    let materialized = materialize_repo_action(db, &mut admission, move |staging| {
        crate::git::clone_repo(&clone_url, staging)
    })
    .await?;
    if let Some(repo_ref) = completed_repo_for_admission(db, &admission).await? {
        cleanup_completed_action_target(&admission, &repo_ref)?;
        schedule_repo_curator(db, &repo_ref);
        return Ok(RepoActionInvocationResult {
            repo: repo_ref,
            outcome: RepoActionExecutionOutcome::Replayed,
            execution_id: Some(admission.execution.id),
        });
    }
    let repo_ref = register_repo_without_schedule(
        db,
        workspace_id,
        &name,
        &materialized.to_string_lossy(),
        Some(&admission._os_lock),
    )
    .await?;
    complete_admitted_repo_action(db, &mut admission, &repo_ref).await?;
    cleanup_completed_action_target(&admission, &repo_ref)?;
    schedule_repo_curator(db, &repo_ref);
    Ok(RepoActionInvocationResult {
        repo: repo_ref,
        outcome: RepoActionExecutionOutcome::FreshlyCompleted,
        execution_id: Some(admission.execution.id),
    })
}

#[tauri::command]
pub async fn clone_repo(
    db: State<'_, Db>,
    workspace_id: i32,
    url: String,
    dest: String,
    name: String,
    thread_id: Option<i32>,
    message_id: Option<i32>,
    action_id: Option<String>,
    action_kind: Option<String>,
) -> R<RepoActionCommandResult> {
    match clone_repo_inner(
        &db,
        workspace_id,
        url,
        dest,
        name,
        thread_id,
        message_id,
        action_id,
        action_kind,
    )
    .await
    {
        Ok(result) => {
            if let Some(execution_id) = result.execution_id {
                spawn_repo_action_feedback_drain(db.inner().clone(), execution_id);
            }
            Ok(result.into())
        }
        Err(error) if error == "action_card_in_progress" => Ok(repo_action_in_progress_result()),
        Err(error) => Err(error),
    }
}

/// Create a new git repo at `<dest>/<name>` (init + empty initial commit), then
/// register it.
async fn create_repo_inner(
    db: &Db,
    workspace_id: i32,
    name: String,
    dest: String,
    thread_id: Option<i32>,
    message_id: Option<i32>,
    action_id: Option<String>,
    action_kind: Option<String>,
) -> R<RepoActionInvocationResult> {
    validate_repo_name(&name)?;
    let guarded = repo_action_guard_is_present(
        thread_id,
        message_id,
        action_id.as_deref(),
        action_kind.as_deref(),
    )?;
    if !guarded {
        let path = std::path::Path::new(&dest).join(&name);
        reject_occupied_repo_target(&path)?;
        let p = path.clone();
        tokio::task::spawn_blocking(move || crate::git::init_repo(&p))
            .await
            .map_err(|err| err.to_string())?
            .map_err(e)?;
        ensure_path_under_dest(&path, std::path::Path::new(&dest))?;
        let repo = register_repo(db, workspace_id, &name, &path.to_string_lossy()).await?;
        return Ok(RepoActionInvocationResult {
            repo,
            outcome: RepoActionExecutionOutcome::FreshlyCompleted,
            execution_id: None,
        });
    }

    let destination = normalized_repo_destination(&dest)?;
    let path = destination.join(&name);
    let path_text = path.to_string_lossy().into_owned();
    let fingerprint = repo_action_fingerprint(&["new", &path_text, &name]);
    let admission = admit_repo_action(
        db,
        workspace_id,
        thread_id,
        message_id,
        action_id.as_deref(),
        action_kind.as_deref(),
        "new",
        &fingerprint,
        &path,
        Some(&destination),
    )
    .await?;
    let Some(mut admission) = admission else {
        return Err("action_card_stale".to_string());
    };
    if let Some(repo_ref) = completed_repo_for_admission(db, &admission).await? {
        cleanup_completed_action_target(&admission, &repo_ref)?;
        schedule_repo_curator(db, &repo_ref);
        return Ok(RepoActionInvocationResult {
            repo: repo_ref,
            outcome: RepoActionExecutionOutcome::Replayed,
            execution_id: Some(admission.execution.id),
        });
    }
    let materialized = materialize_repo_action(db, &mut admission, crate::git::init_repo).await?;
    if let Some(repo_ref) = completed_repo_for_admission(db, &admission).await? {
        cleanup_completed_action_target(&admission, &repo_ref)?;
        schedule_repo_curator(db, &repo_ref);
        return Ok(RepoActionInvocationResult {
            repo: repo_ref,
            outcome: RepoActionExecutionOutcome::Replayed,
            execution_id: Some(admission.execution.id),
        });
    }
    let repo_ref = register_repo_without_schedule(
        db,
        workspace_id,
        &name,
        &materialized.to_string_lossy(),
        Some(&admission._os_lock),
    )
    .await?;
    complete_admitted_repo_action(db, &mut admission, &repo_ref).await?;
    cleanup_completed_action_target(&admission, &repo_ref)?;
    schedule_repo_curator(db, &repo_ref);
    Ok(RepoActionInvocationResult {
        repo: repo_ref,
        outcome: RepoActionExecutionOutcome::FreshlyCompleted,
        execution_id: Some(admission.execution.id),
    })
}

#[tauri::command]
pub async fn create_repo(
    db: State<'_, Db>,
    workspace_id: i32,
    name: String,
    dest: String,
    thread_id: Option<i32>,
    message_id: Option<i32>,
    action_id: Option<String>,
    action_kind: Option<String>,
) -> R<RepoActionCommandResult> {
    match create_repo_inner(
        &db,
        workspace_id,
        name,
        dest,
        thread_id,
        message_id,
        action_id,
        action_kind,
    )
    .await
    {
        Ok(result) => {
            if let Some(execution_id) = result.execution_id {
                spawn_repo_action_feedback_drain(db.inner().clone(), execution_id);
            }
            Ok(result.into())
        }
        Err(error) if error == "action_card_in_progress" => Ok(repo_action_in_progress_result()),
        Err(error) => Err(error),
    }
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

/// User-initiated "Analyze deps": run a FORCED analysis pass for the workspace.
///
/// A forced pass re-classifies EVERY repo, including ones stuck in `failed` —
/// unlike the auto/background pass, which skips failed repos so a persistently
/// broken one can't storm (`should_analyze`). That skip is correct for auto passes
/// but used to leave a repo whose FIRST analysis hit a transient error (e.g. the
/// agent CLI briefly off PATH) failed forever, because the button only sent a chat
/// message and depended on the curator agent to invoke its reanalyze tool — a no-op
/// whenever the agent backend was down. Triggering the forced pass directly makes
/// the retry deterministic. Fire-and-forget + coalesced: rapid clicks and concurrent
/// background passes collapse into one run; the UI follows `repo-graph-updated`.
/// Outcome of a direct "Analyze deps" pass, so the frontend can surface the same
/// feedback the curator chat tool gives (the button bypasses that chat round-trip).
#[derive(serde::Serialize)]
pub struct ReanalyzeReport {
    /// Every tracked repo's checkout is missing on disk — the pass was skipped (it
    /// would have filtered them all out and "succeeded" against a stale graph).
    pub all_missing: bool,
    /// The pass was cancelled (Stop) before completing.
    pub cancelled: bool,
    /// Repos the pass left unanalyzed (classifier error or missing checkout).
    pub unanalyzed: Vec<String>,
}

#[tauri::command]
pub async fn reanalyze_workspace_deps(db: State<'_, Db>, workspace_id: i32) -> R<ReanalyzeReport> {
    // Register the cancel token BEFORE the (multi-repo, disk-stat) checkout preflight, so
    // a Stop landing during it trips THIS call's token instead of being dropped (matches
    // the curator reanalyze path). The inner body runs everything after registration;
    // always unregister afterwards.
    let token = crate::curator::register_ws_analysis_cancel(workspace_id);
    let report = reanalyze_deps_inner(&db, workspace_id, &token).await;
    crate::curator::unregister_ws_analysis_cancel(workspace_id, &token);
    report
}

async fn reanalyze_deps_inner(
    db: &Db,
    workspace_id: i32,
    token: &std::sync::atomic::AtomicBool,
) -> R<ReanalyzeReport> {
    // Same all-checkouts-missing preflight as the curator reanalyze tool: without it the
    // pass filters every repo out and reports a clean "success" against a stale graph,
    // with no feedback. Skip the pass and tell the frontend to surface it instead.
    let repos = repo::list_repos(db, workspace_id).await.map_err(e)?;
    if !repos.is_empty()
        && !repos
            .iter()
            .any(|r| std::path::Path::new(&r.local_git_path).exists())
    {
        return Ok(ReanalyzeReport {
            all_missing: true,
            cancelled: false,
            unanalyzed: Vec::new(),
        });
    }
    // A Stop during the (possibly slow) preflight already tripped the token — honor it
    // before starting the pass rather than running a pass the user just cancelled.
    if token.load(std::sync::atomic::Ordering::SeqCst) {
        return Ok(ReanalyzeReport {
            all_missing: false,
            cancelled: true,
            unanalyzed: Vec::new(),
        });
    }
    // Run the CANCELLABLE forced pass (`reanalyze_workspace`) under the workspace-keyed
    // token so the toolbar's Stop (`cancel_reanalyze_workspace_deps`) can interrupt it —
    // the button has no curator lead turn to 中止. It AWAITs the pass (so the button stays
    // busy for the real work) and is serialized via the gate lock.
    let cancelled = crate::curator::reanalyze_workspace(db, workspace_id, token).await;
    // Surface repos left unanalyzed so the user sees WHICH/why (the map renders them as a
    // plain "未分析" card). Skip on cancel — a half-run's failed set isn't meaningful.
    let unanalyzed = if cancelled {
        Vec::new()
    } else {
        crate::curator::unanalyzed_repo_names(db, workspace_id).await
    };
    Ok(ReanalyzeReport {
        all_missing: false,
        cancelled,
        unanalyzed,
    })
}

/// Stop an in-flight "Analyze deps" forced pass for a workspace (trips its cancel
/// token; the pass bails at the next safe point — between repos / before the relation
/// pass). No-op if none is running.
#[tauri::command]
pub async fn cancel_reanalyze_workspace_deps(workspace_id: i32) -> R<()> {
    crate::curator::cancel_ws_analysis(workspace_id);
    Ok(())
}

/// Get-or-create this workspace's hidden curator-chat thread and return its id,
/// so the frontend can open its lead-chat surface for dependency calibration.
#[tauri::command]
pub async fn open_curator_chat(db: State<'_, Db>, workspace_id: i32) -> R<i32> {
    if let Some(existing) = repo::curator_thread_for_workspace(&db, workspace_id)
        .await
        .map_err(e)?
    {
        return Ok(existing);
    }
    // The curator is one more Weft-owned surface: it shares the global policy,
    // with a deep hint because repository calibration benefits from deeper
    // reasoning. OpenCode remains a legacy/manual fallback only.
    let legacy_tool = crate::tools::default_tool(&db).await;
    let route = crate::engine_routing::resolve_for_db(
        &db,
        None,
        &legacy_tool,
        crate::engine_routing::RoutingHint::Deep,
    )
    .await;
    let tool = route
        .selected()
        .map(|selected| selected.as_str().to_string())
        .unwrap_or(legacy_tool);
    let thread_id = repo::ensure_curator_thread(&db, workspace_id, &tool)
        .await
        .map_err(e)?;
    crate::engine_routing::record_decision(&db, thread_id, None, None, "curator_start", &route)
        .await;
    Ok(thread_id)
}

/// Return the analyst-synthesized markdown repo-map for a workspace, or `None`
/// when no analysis has produced one yet.
#[tauri::command]
pub async fn get_repo_map_doc(db: State<'_, Db>, workspace_id: i32) -> R<Option<String>> {
    repo::get_repo_map_doc(&db, workspace_id).await.map_err(e)
}

async fn repo_ask_scope(db: &Db, repo_id: i32) -> R<WorkspaceAskScope> {
    let mut scope = WorkspaceAskScope::default();
    for direction in repo::directions_for_repo(db, repo_id).await.map_err(e)? {
        scope
            .direction_routes
            .insert((direction.thread_id, direction.id.to_string()));
    }
    for session in repo::sessions_for_repo(db, repo_id).await.map_err(e)? {
        if let Some(direction) = repo::get_direction(db, session.direction_id)
            .await
            .map_err(e)?
        {
            scope
                .direction_routes
                .insert((direction.thread_id, direction.id.to_string()));
        }
    }
    Ok(scope)
}

/// Remove a repo from its workspace: delete Weft's reference, the repo's
/// profile, the directions bound to it (with their sessions), and its worktrees
/// (physically removed from git). The user's actual repository at its local path
/// is NEVER deleted — only Weft's tracking of it.
#[tauri::command]
pub async fn delete_repo(
    app: tauri::AppHandle,
    db: State<'_, Db>,
    asks: State<'_, crate::ask::AskRegistry>,
    repo_id: i32,
) -> R<()> {
    repo::mark_repo_deleting(&db, repo_id).await.map_err(e)?;
    let result = delete_repo_after_fence(app, &db, &asks, repo_id).await;
    if result.is_err() {
        let _ = repo::clear_repo_deleting(&db, repo_id).await;
    }
    result
}

async fn delete_repo_after_fence(
    app: tauri::AppHandle,
    db: &Db,
    asks: &crate::ask::AskRegistry,
    repo_id: i32,
) -> R<()> {
    let feedback_locks = lock_repo_action_cleanups(
        db,
        repo::pending_repo_action_feedback_for_repo(db, repo_id)
            .await
            .map_err(e)?,
    )
    .await?;
    let engine_state = app.state::<crate::lead_chat::engine::LeadChatState>();
    let engine_admission = engine_state.engine_admission_write().await;
    // Snapshot engines while their session rows still exist. The repo deletion
    // marker blocks reconstruction, and the snapshot is stopped only after the
    // atomic cascade succeeds so rollback never interrupts a surviving turn.
    let engine_keys = repo_engine_keys(db, repo_id).await?;
    let bus = app.state::<crate::bus::BusRegistry>();
    let scope = repo_ask_scope(db, repo_id).await?;
    let lifecycle_guards = lock_thread_lifecycles(&bus, &scope.affected_thread_ids()).await;
    let effects = repo::delete_repo_cascade_with_human_cancellations(db, repo_id)
        .await
        .map_err(e)?;
    stop_engines_by_key(&app, &engine_keys).await;
    drop(engine_admission);
    apply_committed_bus_delete_effects(
        &bus,
        &effects.cancelled_requests,
        &[],
        &effects.removed_directions,
        &std::collections::BTreeMap::new(),
    );
    purge_committed_permission_effects(asks, &[], &effects.removed_directions).await?;
    drop(lifecycle_guards);
    drop(feedback_locks);
    crate::curator::run_forget(repo_id);
    materialize::cleanup_removed_worktrees(&effects.removed_worktrees)
        .await
        .map_err(e)
}

#[tauri::command]
pub async fn update_repo_profile(
    db: State<'_, Db>,
    repo_id: i32,
    summary: Option<String>,
    tier: Option<String>,
) -> R<()> {
    // Only the field(s) the user actually changed are `Some`, so editing the
    // summary doesn't pin the tier and vice versa. The detail-pane editor doesn't
    // change the role/category, so pass None for it (the curator's set_classification
    // tool is the path that pins a role).
    crate::curator::edit_profile(&db, repo_id, summary.as_deref(), tier.as_deref(), None)
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
    let legacy_tool = crate::tools::default_tool(&db).await;
    let route = crate::engine_routing::resolve_for_db(
        &db,
        None,
        &legacy_tool,
        crate::engine_routing::RoutingHint::Normal,
    )
    .await;
    let tool = route
        .selected()
        .map(|selected| selected.as_str().to_string())
        .unwrap_or(legacy_tool);
    let thread = repo::create_thread(&db, workspace_id, &title, &kind, &tool)
        .await
        .map_err(e)?;
    crate::engine_routing::record_decision(&db, thread.id, None, None, "new_thread", &route).await;
    Ok(thread)
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
    app: tauri::AppHandle,
    db: State<'_, Db>,
    thread_id: i32,
) -> R<Option<crate::planner::ResolvedProposal>> {
    let live_sessions = app.state::<crate::lead_chat::engine::LeadChatState>();
    let is_session_live = |session_id| live_sessions.worker_is_running(session_id);
    crate::planner::get_resolved_with_live_sessions(&db, thread_id, &is_session_live)
        .await
        .map_err(e)
}

/// Save a (human-edited) proposal back, keeping it in "proposed" state.
#[tauri::command]
pub async fn save_proposal(
    db: State<'_, Db>,
    thread_id: i32,
    proposal: serde_json::Value,
) -> R<()> {
    crate::planner::save_proposal_value(&db, thread_id, &proposal)
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

/// Confirm the stored proposal + propagate issue #103's read-only auto-allow
/// to the whole issue. Extracted from the `#[tauri::command]` wrapper (mirrors
/// `revoke_grant_durable`) so the propagation itself is directly testable
/// without constructing a `tauri::State`.
///
/// Approving dispatch here IS the human's "I already trust this issue's
/// worktree reads" decision (the issue's own motivating pain point — a worker
/// started right after approval still asking `pwd`), so every dir under this
/// thread — this call's new directions AND any spawned later (for example a
/// re-dispatch) — auto-allows a `RiskLevel::ReadOnly` ask from
/// here on. Never widens beyond ReadOnly (see `AskRegistry::grant_read_only_issue`);
/// in-memory only, so it does NOT survive a restart (contrast Full/Always) and
/// is separately revocable (`revoke_read_only_grant`). Granted unconditionally
/// on a successful confirm (including the idempotent re-dispatch retry path) —
/// `HashSet::insert` is a no-op when already granted, so this is safe to call
/// every time.
async fn confirm_proposal_and_propagate_read_only(
    db: &Db,
    asks: &crate::ask::AskRegistry,
    thread_id: i32,
) -> anyhow::Result<Vec<i32>> {
    confirm_proposal_and_propagate_read_only_with_manual_tool(db, asks, thread_id, None).await
}

async fn confirm_proposal_and_propagate_read_only_with_manual_tool(
    db: &Db,
    asks: &crate::ask::AskRegistry,
    thread_id: i32,
    manual_tool: Option<&str>,
) -> anyhow::Result<Vec<i32>> {
    let ids = crate::planner::confirm_with_manual_tool(db, thread_id, manual_tool).await?;
    asks.grant_read_only_issue(thread_id);
    Ok(ids)
}

async fn confirm_proposal_and_propagate_read_only_with_manual_tool_and_live_sessions(
    db: &Db,
    asks: &crate::ask::AskRegistry,
    thread_id: i32,
    manual_tool: Option<&str>,
    is_session_live: &(dyn Fn(i32) -> bool + Send + Sync),
) -> anyhow::Result<Vec<i32>> {
    let ids = crate::planner::confirm_with_manual_tool_and_live_sessions(
        db,
        thread_id,
        manual_tool,
        is_session_live,
    )
    .await?;
    asks.grant_read_only_issue(thread_id);
    Ok(ids)
}

/// Confirm the stored proposal: create its directions + materialize worktrees.
/// See `confirm_proposal_and_propagate_read_only` for the read-only propagation.
#[tauri::command]
pub async fn confirm_proposal(
    app: tauri::AppHandle,
    db: State<'_, Db>,
    asks: tauri::State<'_, crate::ask::AskRegistry>,
    thread_id: i32,
    manual_tool: Option<String>,
) -> R<Vec<i32>> {
    let live_sessions = app.state::<crate::lead_chat::engine::LeadChatState>();
    let is_session_live = |session_id| live_sessions.worker_is_running(session_id);
    let ids = confirm_proposal_and_propagate_read_only_with_manual_tool_and_live_sessions(
        &db,
        &asks,
        thread_id,
        manual_tool.as_deref(),
        &is_session_live,
    )
    .await
    .map_err(e)?;
    use tauri::Emitter;
    let _ = app.emit("needs-you://changed", thread_id);
    Ok(ids)
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

async fn create_direction_for_explicit_tool(
    db: &Db,
    thread_id: i32,
    name: &str,
    tool: &str,
    repo_id: i32,
    reason: &str,
    mandate: &str,
    base_branch: &str,
) -> anyhow::Result<entities::direction::Model> {
    let dir = repo::create_direction_with_engine_pin(
        db,
        thread_id,
        name,
        tool,
        repo_id,
        reason,
        mandate,
        base_branch,
        true,
    )
    .await?;
    Ok(dir)
}

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
    let dir = create_direction_for_explicit_tool(
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
pub async fn list_worktrees(db: State<'_, Db>, direction_id: Option<i32>) -> R<Vec<WorktreeView>> {
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
        let has_entries = std::fs::read_dir(path).map_err(e)?.next().is_some();
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
            let (children, child_truncated) = read_dir_tree(&entry_path, depth - 1, counter)?;
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
    let (nodes, truncated) = read_dir_tree(
        std::path::Path::new(&cwd),
        FILE_TREE_MAX_DEPTH,
        &mut counter,
    )?;
    Ok(FileTree {
        nodes,
        truncated,
        total: counter,
    })
}

async fn delete_thread_cascade_after_bus_fence(
    db: &Db,
    bus: &crate::bus::BusRegistry,
    asks: &crate::ask::AskRegistry,
    thread_id: i32,
    action_cleanups: Vec<LockedRepoActionCleanup>,
) -> R<Vec<repo::RemovedWorktree>> {
    let lifecycle_gate = bus.thread_lifecycle_gate(thread_id);
    let _lifecycle = lifecycle_gate.lock().await;
    let action_cleanup_plans = repo_action_cleanup_plans(&action_cleanups);
    // Install the reversible process-local fence before the DB await. A
    // concurrently answered command can win OCC before durable cancellation,
    // but its later delivery sees `closing` and cannot recreate the bus.
    let (_, cancelled_asks) = bus.begin_thread_close(thread_id);
    let (removed, durable_cancelled) =
        match repo::delete_thread_cascade_with_human_cancellations_and_action_cleanups(
            db,
            thread_id,
            &action_cleanup_plans,
        )
        .await
        {
            Ok(result) => result,
            Err(error) => {
                bus.rollback_thread_close(thread_id);
                return Err(e(error));
            }
        };
    // Only mutate the retained live bus and publish terminal card events after
    // the atomic cancellation+cascade commit. A failed delete rolls back to an
    // answerable request and an untouched bus.
    bus.apply_thread_human_cancellation(thread_id);
    let durable_ids = durable_cancelled
        .iter()
        .map(|request| request.request_id)
        .collect::<Vec<_>>();
    let event_ids = human_cancel_event_ids(cancelled_asks, &durable_ids);
    bus.notify_cancelled_asks(thread_id, &event_ids);
    bus.commit_thread_close(thread_id);
    // The Ask Bridge registers under this SAME lifecycle gate. Purge before
    // releasing it so every hook that linearized first is cancelled/revoked,
    // while every hook that arrives later observes the durable delete marker
    // or missing identity and cannot recreate an ask or grant after cleanup.
    asks.purge_thread(thread_id);
    cleanup_locked_repo_actions(db, &action_cleanups).await;
    Ok(removed)
}

#[tauri::command]
pub async fn delete_thread(app: tauri::AppHandle, db: State<'_, Db>, thread_id: i32) -> R<()> {
    // Fence session/engine admission before snapshotting keys. Without this, a
    // worker could open after the snapshot but before the cascade, leaving a
    // live engine whose newly-created session row is then deleted.
    repo::mark_thread_deleting(&db, thread_id)
        .await
        .map_err(e)?;
    let result = delete_thread_after_fence(app, &db, thread_id).await;
    if result.is_err() {
        let _ = repo::clear_thread_deleting(&db, thread_id).await;
    }
    result
}

async fn delete_thread_after_fence(app: tauri::AppHandle, db: &Db, thread_id: i32) -> R<()> {
    let action_cleanups = lock_repo_action_cleanups(
        db,
        repo::repo_action_executions_requiring_lock_for_thread(db, thread_id)
            .await
            .map_err(e)?,
    )
    .await?;
    let engine_state = app.state::<crate::lead_chat::engine::LeadChatState>();
    let engine_admission = engine_state.engine_admission_write().await;
    // Collect keys while the durable marker blocks every worker reconstruction,
    // then stop only after the cascade commits. Failed deletion therefore keeps
    // both rows and running turns intact.
    let keys = thread_engine_keys(&db, thread_id).await?;
    let bus = app.state::<crate::bus::BusRegistry>();
    let asks = app.state::<crate::ask::AskRegistry>();
    let removed = delete_thread_cascade_after_bus_fence(
        db,
        &bus,
        asks.inner(),
        thread_id,
        action_cleanups,
    )
    .await?;
    // Purge already happened under the lifecycle gate. The revoke emit reaches
    // the writer; this best-effort awaited flush makes it durable without
    // widening the handler's gate across its one-hour human wait.
    let _ = crate::auth_persist::flush(&asks).await;
    let state = app.state::<crate::lead_chat::engine::LeadChatState>();
    for key in keys {
        if let Some(eng) = state.remove(key) {
            crate::lead_chat::engine::stop(&app, &eng).await;
        }
    }
    drop(engine_admission);
    materialize::cleanup_removed_worktrees(&removed)
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

/// Whether the one global automatic engine-routing policy is enabled. Unset is
/// deliberately false so existing default-tool/manual behavior is unchanged
/// for upgraded databases.
#[tauri::command]
pub async fn get_automatic_engine_routing_enabled(db: State<'_, Db>) -> R<bool> {
    Ok(matches!(
        repo::get_setting(&db, crate::engine_routing::K_AUTOMATIC_ROUTING_ENABLED)
            .await
            .map_err(e)?
            .as_deref(),
        Some("1") | Some("true") | Some("on") | Some("yes")
    ))
}

#[tauri::command]
pub async fn set_automatic_engine_routing_enabled(db: State<'_, Db>, enabled: bool) -> R<()> {
    repo::set_setting(
        &db,
        crate::engine_routing::K_AUTOMATIC_ROUTING_ENABLED,
        if enabled { "1" } else { "0" },
    )
    .await
    .map_err(e)
}

/// issue #97: whether Weft should auto-switch a thread/session to its
/// fallback engine when the current one reports its usage limit as exceeded
/// (`crate::lead_chat::commands::maybe_failover_on_quota`). Opt-in, default
/// off — see `K_QUOTA_FAILOVER_ENABLED`'s doc for why.
#[tauri::command]
pub async fn get_quota_failover_enabled(db: State<'_, Db>) -> R<bool> {
    Ok(matches!(
        repo::get_setting(&db, crate::engine_routing::K_QUOTA_FAILOVER_ENABLED)
            .await
            .map_err(e)?
            .as_deref(),
        Some("1") | Some("true")
    ))
}

#[tauri::command]
pub async fn set_quota_failover_enabled(db: State<'_, Db>, enabled: bool) -> R<()> {
    repo::set_setting(
        &db,
        crate::engine_routing::K_QUOTA_FAILOVER_ENABLED,
        if enabled { "1" } else { "0" },
    )
    .await
    .map_err(e)
}

/// Issue #110 T3: whether Weft should squash-merge a tracked PR/MR on its own
/// once it reaches this repo's truly-mergeable bar
/// (`crate::host::automerge::spawn_pr_automerge_watch`). Opt-in, default OFF
/// — see `crate::host::automerge::K_AUTO_MERGE_ENABLED`'s doc for why: this
/// performs an irreversible action with no human confirming the specific
/// merge.
#[tauri::command]
pub async fn get_pr_auto_merge_enabled(db: State<'_, Db>) -> R<bool> {
    crate::host::automerge::try_auto_merge_enabled(&db)
        .await
        .map_err(e)
}

#[tauri::command]
pub async fn set_pr_auto_merge_enabled(db: State<'_, Db>, enabled: bool) -> R<()> {
    repo::set_setting(
        &db,
        crate::host::automerge::K_AUTO_MERGE_ENABLED,
        if enabled { "1" } else { "0" },
    )
    .await
    .map_err(e)
}

/// The user-configured coding-agent command overrides ("aliases"): identity →
/// command (e.g. `claude` → `cc-claude`). Empty map when none are set.
#[tauri::command]
pub async fn get_tool_commands(db: State<'_, Db>) -> R<std::collections::HashMap<String, String>> {
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
    // Validate BEFORE repo::set_tool_command mutates anything: it reconciles the
    // per-session pins and persists the raw value first, and parse_overrides
    // would then drop an invalid value on reload — with apply_to_existing=true
    // the pins are already cleared by then, silently retargeting existing
    // sessions to the default binary even though the override never took effect.
    // Blank / identity values mean "clear the override" and skip validation.
    let trimmed = command.trim();
    if !trimmed.is_empty() && trimmed != tool {
        crate::tool_command::validate_override_value(trimmed)
            .map_err(|err| format!("invalid command for {tool}: {err}"))?;
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
            t if crate::acp::backend_for(t).is_some() => {
                // Shared ACP child serves all sessions for this backend. Command
                // changes already set pending_command_refresh on engines so the
                // bounce happens on the next idle send — never reap here or an
                // in-flight prompt dies mid-turn.
                let _ = t;
            }
            _ => {}
        }
    }
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
            let _ = app.emit("needs-you://changed", m.thread_id);
        }
    }
    Ok(())
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

pub(crate) fn env_secs(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(default)
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
    /// This worker's `--model` override (issue #98), if one was set via
    /// `switch_worker_tool` — distinct from `model` above (the LIVE probed/
    /// reported model): the override is what the user asked for, `model` is
    /// what the engine actually reported running. Prefills the switch dialog.
    pub model_override: Option<String>,
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
    let (context_tokens, window, model, mcp_servers, tools) =
        match latest.as_ref().map(|s| s.id).and_then(|sid| {
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
            None => {
                // No live engine (e.g. after an app relaunch): serve the persisted
                // snapshot (session.meta) so the panel isn't blank until the next turn.
                let snap = latest
                    .as_ref()
                    .filter(|s| !s.meta.is_empty())
                    .and_then(|s| {
                        serde_json::from_str::<crate::lead_chat::engine::PersistedMeta>(&s.meta)
                            .ok()
                    })
                    .unwrap_or_default();
                (
                    snap.context_tokens,
                    snap.window,
                    snap.model,
                    snap.mcp_servers,
                    snap.tools,
                )
            }
        };
    let command = crate::tool_command::effective(
        latest.as_ref().and_then(|s| s.command.as_deref()),
        &dir.tool,
    );
    let model_override = latest.as_ref().and_then(|s| s.model.clone());
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
        model_override,
    }))
}

/// 会话信息面板(M2):codex/opencode 的带外 meta(Context / model / window / MCP
/// server,**不含 tool**)。claude 不走这里——其 meta 全在事件流 + 引擎缓存。
#[tauri::command]
pub async fn session_meta(
    app: tauri::AppHandle,
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
    // Ticket BEFORE gathering: a slow probe overlapping a fresher one must not
    // roll usage back when it finally lands (see absorb_probe_meta).
    let sid = latest.as_ref().map(|s| s.id);
    let ticket = match sid {
        Some(sid) => {
            crate::lead_chat::engine::take_probe_ticket(&app, dir.thread_id, Some(sid)).await
        }
        None => None,
    };
    let mut snap =
        crate::session_meta::gather(&dir.tool, &wt.path, native.as_deref(), &command).await;
    // ACP workers: overlay live engine cache (MCP inject + reasoning/model).
    if crate::lead_chat::engine::is_acp_tool(&dir.tool) {
        if let Some(sid) = sid {
            if let Some(states) = app.try_state::<crate::lead_chat::engine::LeadChatState>() {
                if let Some(eng) = states.get(sid as i64) {
                    let g = eng.lock().await;
                    snap.mcp_servers = Some(g.last_mcp_servers.clone());
                    if snap.model.is_none() {
                        snap.model = g.last_model.clone();
                    }
                    if snap.reasoning_effort.is_none() {
                        snap.reasoning_effort = g.last_reasoning.clone();
                    }
                    if snap.context_tokens.is_none() {
                        snap.context_tokens = g.last_context_tokens;
                    }
                    if snap.window.is_none() {
                        snap.window = g.last_window;
                    }
                }
            }
        }
    }
    // Probe results feed the engine cache + persisted snapshot: codex/opencode
    // model/window/MCP only exist here, never in engine events.
    if let Some(sid) = sid {
        crate::lead_chat::engine::absorb_probe_meta(
            &app,
            &db,
            dir.thread_id,
            Some(sid),
            ticket,
            &snap,
        )
        .await;
    }
    Ok(snap)
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

/// Answer a pending permission Ask. `answer` is allow | deny | always | full —
/// always remembers this action for the task, full grants it full access.
#[tauri::command]
pub async fn answer_permission(
    asks: tauri::State<'_, crate::ask::AskRegistry>,
    ask_id: u64,
    answer: String,
) -> R<()> {
    let a = crate::ask::Answer::parse(&answer).ok_or("unknown answer")?;
    if !asks.answer(ask_id, a) {
        return Err("that request was already answered or has expired".into());
    }
    // Both Full and Always now create a durable standing grant (issue #89: Always
    // is keyed by the exact action_key, not the lossy display summary, so it's
    // safe to persist too). Persist it durably — routed through the single
    // ordered writer, awaited — before reporting success, so an immediate
    // quit/crash can't drop it and a write failure surfaces.
    if matches!(a, crate::ask::Answer::Full | crate::ask::Answer::Always) {
        crate::auth_persist::flush(&asks).await?;
    }
    Ok(())
}

/// The current standing authorization grants (full / always), so the board can
/// mark tasks whose Full/Always access was inherited across a restart and offer
/// a one-click revoke.
#[tauri::command]
pub fn list_auth_grants(
    asks: tauri::State<'_, crate::ask::AskRegistry>,
) -> R<crate::ask::GrantSnapshot> {
    Ok(asks.snapshot_grants())
}

/// Revoke a standing grant (the human's one-click undo), at the granularity the
/// caller passes:
/// - `dir == None`                    → clear the whole issue's grants (every
///   task's full/always under this thread) — the board card's one-click "revoke
///   all".
/// - `dir == Some`, `action_key == None` → clear that one task's `(thread, dir)`
///   grant (its full access + every always-rule).
/// - `dir == Some`, `action_key == Some` → drop only that one always-rule.
/// Revoke a standing grant and durably persist it, ATOMICALLY: the revoke is the
/// safety net for persisted Full/Always access, so it must never leave memory
/// ahead of the store (chip gone in the UI but the grant still on disk, ready to
/// resurrect on restart) or report success on a failed write. Uses
/// `revoke_no_emit` so the ONLY store write is the acked `flush` — a
/// fire-and-forget emit here plus the acked flush could diverge if one landed
/// and the other failed (memory rolled back while disk is already revoked → the
/// session keeps auto-approving). `revoke_no_emit` returns EXACTLY what it
/// removed under one lock, so on a failed write the rollback re-adds only this
/// call's removals. Extracted from the command so the rollback is testable.
async fn revoke_grant_durable(
    asks: &crate::ask::AskRegistry,
    thread: i32,
    dir: Option<&str>,
    action_key: Option<&str>,
) -> Result<(), String> {
    // Serialize the whole mutate → flush → rollback: two overlapping revokes of the
    // same grant would otherwise race, an earlier failed revoke's rollback resurrecting
    // a grant a later, already-succeeded revoke removed.
    let _guard = asks.lock_revoke().await;
    let removed = asks.revoke_no_emit(thread, dir, action_key);
    // Nothing matched → memory is unchanged, so disk is already consistent. Skip the
    // write entirely (mirrors the emit path's guard) so a "revoke nothing" can't
    // surface a spurious failure on an unrelated write error.
    if removed.is_empty() {
        return Ok(());
    }
    if let Err(err) = crate::auth_persist::flush(asks).await {
        asks.seed_grants(removed);
        return Err(err);
    }
    Ok(())
}

#[tauri::command]
pub async fn revoke_auth_grant(
    asks: tauri::State<'_, crate::ask::AskRegistry>,
    thread: i32,
    dir: Option<String>,
    action_key: Option<String>,
) -> R<()> {
    revoke_grant_durable(&asks, thread, dir.as_deref(), action_key.as_deref()).await
}

/// Current read-only auto-allow scopes (issue #103) — in-memory only, NEVER
/// persisted (see `ask::Inner::read_only_session`'s doc), so this is a live
/// snapshot, not something restored at boot. Backs the frontend's "read-only
/// trusted" indicators (session + issue-wide) and their revoke entry points.
#[tauri::command]
pub fn read_only_grants(
    asks: tauri::State<'_, crate::ask::AskRegistry>,
) -> R<crate::ask::ReadOnlyGrants> {
    Ok(asks.read_only_grants())
}

/// "Release all read-only for this session" (issue #103's core batch action):
/// resolves every currently open `RiskLevel::ReadOnly` ask in (thread, dir) to
/// Allow and installs a forward-looking session-scoped rule so a later
/// ReadOnly ask in the same session doesn't re-prompt either. A Write/
/// NetworkOrCredential/Unknown ask in this session is left untouched — still
/// open, still needs a real human answer (`AskRegistry::grant_read_only_session`
/// is the actual enforcement; this command is a thin wrapper). In-memory only,
/// never persisted. Returns how many open asks were just resolved, so the
/// frontend can toast "released N".
#[tauri::command]
pub fn release_session_read_only(
    asks: tauri::State<'_, crate::ask::AskRegistry>,
    thread: i32,
    dir: String,
) -> R<usize> {
    Ok(asks.grant_read_only_session(thread, &dir))
}

/// Revoke a read-only auto-allow grant (issue #103), at the granularity the
/// caller passes: `dir == None` revokes the WHOLE issue's propagation
/// (`grant_read_only_issue`'s counterpart); `dir == Some` revokes just that one
/// session's batch grant. In-memory only — there is no durable write to roll
/// back here (contrast `revoke_auth_grant`'s acked flush for Full/Always), so
/// this can't fail short of the ask registry itself being gone.
#[tauri::command]
pub fn revoke_read_only_grant(
    asks: tauri::State<'_, crate::ask::AskRegistry>,
    thread: i32,
    dir: Option<String>,
) -> R<()> {
    match dir {
        Some(dir) => {
            asks.revoke_read_only_session(thread, &dir);
        }
        None => {
            asks.revoke_read_only_issue(thread);
        }
    }
    Ok(())
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
    pub provider: String,
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
        provider: s.provider.as_str().to_string(),
        app_id: s.app_id,
        has_secret: !s.app_secret.is_empty(),
        bound: !s.allow_open_ids.is_empty(),
        enabled: s.enabled,
        remote_standby: s.remote_standby,
    })
}

async fn persist_im_credentials(
    db: &Db,
    provider: crate::im::ImProvider,
    app_id: &str,
    app_secret: &str,
    enable: bool,
    owner_id: Option<&str>,
    select_provider: bool,
) -> anyhow::Result<()> {
    let app_id = app_id.trim();
    let app_secret = app_secret.trim();
    let owner_id = owner_id.map(str::trim).filter(|owner| !owner.is_empty());
    let mut settings = Vec::with_capacity(5);
    if select_provider {
        settings.push((crate::im::K_PROVIDER, provider.as_str()));
    }
    settings.push((provider.app_id_key(), app_id));
    if !app_secret.is_empty() {
        settings.push((provider.app_secret_key(), app_secret));
    }
    if let Some(owner_id) = owner_id {
        settings.push((provider.allow_key(), owner_id));
    }
    if enable {
        settings.push((provider.enabled_key(), "1"));
    }
    repo::set_settings_atomic(db, &settings).await
}

async fn apply_im_credentials(
    app: &tauri::AppHandle,
    db: &Db,
    provider: crate::im::ImProvider,
    app_id: &str,
    app_secret: &str,
    enable: bool,
    owner_id: Option<&str>,
) -> anyhow::Result<()> {
    let bridge = app.state::<crate::im::ImBridge>();
    let _authority = bridge.authority_write_lease().await;
    persist_im_credentials(
        db,
        provider,
        app_id,
        app_secret,
        enable,
        owner_id,
        true,
    )
    .await?;
    crate::im::spawn(app.clone());
    Ok(())
}

/// Persist a completed Feishu scan without selecting Feishu. A callback can
/// finish after the user has switched to DingTalk; keeping `K_PROVIDER`
/// untouched makes that newer choice authoritative while retaining the scanned
/// credentials for a later switch back.
async fn persist_feishu_scan_credentials(
    db: &Db,
    app_id: &str,
    app_secret: &str,
    owner_open_id: &str,
) -> anyhow::Result<()> {
    persist_im_credentials(
        db,
        crate::im::ImProvider::Feishu,
        app_id,
        app_secret,
        true,
        Some(owner_open_id),
        false,
    )
    .await
}

async fn persist_im_enabled(
    db: &Db,
    provider: crate::im::ImProvider,
    enabled: bool,
) -> anyhow::Result<()> {
    repo::set_setting(db, provider.enabled_key(), if enabled { "1" } else { "0" }).await
}

async fn apply_im_enabled(
    app: &tauri::AppHandle,
    db: &Db,
    provider: crate::im::ImProvider,
    enabled: bool,
) -> anyhow::Result<()> {
    let bridge = app.state::<crate::im::ImBridge>();
    // A disable is an authority retirement. Keep the write lease through both
    // persistence and generation bump so an in-flight lead enqueue is wholly
    // before the disable or validates against the disabled state afterward.
    let _authority = bridge.authority_write_lease().await;
    persist_im_enabled(db, provider, enabled).await?;
    crate::im::spawn(app.clone());
    Ok(())
}

async fn reset_im_owner(
    app: &tauri::AppHandle,
    db: &Db,
    provider: crate::im::ImProvider,
) -> anyhow::Result<()> {
    let bridge = app.state::<crate::im::ImBridge>();
    let _authority = bridge.authority_write_lease().await;
    let active_provider = crate::im::ImSettings::active_provider(db).await?;
    crate::im::reset_owner(db, provider).await?;
    if active_provider == provider {
        crate::im::spawn(app.clone());
    }
    Ok(())
}

#[tauri::command]
pub async fn im_set_provider(
    app: tauri::AppHandle,
    db: State<'_, Db>,
    provider: String,
) -> R<()> {
    let provider = crate::im::ImProvider::parse(&provider).map_err(e)?;
    let bridge = app.state::<crate::im::ImBridge>();
    let _authority = bridge.authority_write_lease().await;
    repo::set_setting(&db, crate::im::K_PROVIDER, provider.as_str())
        .await
        .map_err(e)?;
    crate::im::spawn(app.clone());
    Ok(())
}

/// 保存凭证并重启桥。secret 传空字符串 = 保持原值（不覆盖已存的密钥）。
/// 是否真正连接由 `im.feishu.enabled` 和双凭证共同决定。
#[tauri::command]
pub async fn im_set_settings(
    app: tauri::AppHandle,
    db: State<'_, Db>,
    registration: State<'_, crate::im::feishu::registration::RegistrationService>,
    provider: String,
    app_id: String,
    app_secret: String,
) -> R<()> {
    let provider = crate::im::ImProvider::parse(&provider).map_err(e)?;
    let apply = apply_im_credentials(&app, &db, provider, &app_id, &app_secret, false, None);
    let result = if provider == crate::im::ImProvider::Feishu {
        registration.supersede_with(apply).await
    } else {
        apply.await
    };
    result.map_err(e)
}

/// 开关桥：写 enabled 标志并重启。off = 断开但保留凭证；on = 凭证齐全则连接
/// （缺凭证时置 disabled，等用户在已展开的表单里补齐再保存）。
#[tauri::command]
pub async fn im_set_enabled(
    app: tauri::AppHandle,
    db: State<'_, Db>,
    registration: State<'_, crate::im::feishu::registration::RegistrationService>,
    provider: String,
    enabled: bool,
) -> R<()> {
    let provider = crate::im::ImProvider::parse(&provider).map_err(e)?;
    // Toggling a named provider must not select it. A delayed toggle can race a
    // newer provider choice; keeping K_PROVIDER untouched makes that newer
    // choice authoritative while still preserving this provider's enabled bit.
    // Feishu's registration apply gate stays outermost, matching credentials,
    // scan completion, and owner reset; this prevents lock-order inversion.
    let apply = apply_im_enabled(&app, &db, provider, enabled);
    if provider == crate::im::ImProvider::Feishu {
        registration.supersede_with(apply).await
    } else {
        apply.await
    }
    .map_err(e)
}

/// Clear a provider's locally bound owner without deleting credentials. The
/// active bridge is restarted so in-memory prompt-recipient/card mappings for
/// the retired owner cannot suppress or leak status updates after rebind.
#[tauri::command]
pub async fn im_reset_owner(
    app: tauri::AppHandle,
    db: State<'_, Db>,
    registration: State<'_, crate::im::feishu::registration::RegistrationService>,
    provider: String,
) -> R<()> {
    let provider = crate::im::ImProvider::parse(&provider).map_err(e)?;
    // Feishu keeps the registration apply gate outermost, matching manual and
    // scan credential paths. The authority gate is acquired inside `reset` so
    // concurrent registration callbacks cannot deadlock on inverted lock order.
    let reset = reset_im_owner(&app, &db, provider);
    if provider == crate::im::ImProvider::Feishu {
        registration.supersede_with(reset).await
    } else {
        reset.await
    }
    .map_err(e)
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

fn dingtalk_copy_should_start_bridge(
    update: crate::im::DingTalkCopyUpdate,
    bridge_status: &str,
) -> bool {
    update == crate::im::DingTalkCopyUpdate::Initialized || bridge_status == "waiting_locale"
}

/// Synchronize DingTalk's fixed user-facing copy from the frontend i18n
/// catalogs. Active channels share this memory-only bundle, so locale changes
/// update rendering in place without retiring output subscribers. The first
/// bundle still starts a selected DingTalk bridge that was waiting for copy.
#[tauri::command]
pub async fn im_set_dingtalk_copy(
    app: tauri::AppHandle,
    db: State<'_, Db>,
    bridge: State<'_, crate::im::ImBridge>,
    copy: crate::im::outbound::DingTalkCopy,
) -> R<()> {
    copy.validate().map_err(e)?;
    let update = bridge.set_dingtalk_copy(copy);
    if !dingtalk_copy_should_start_bridge(update, &bridge.status()) {
        return Ok(());
    }
    let settings = crate::im::ImSettings::load(&db).await.map_err(e)?;
    if settings.provider == crate::im::ImProvider::DingTalk {
        crate::im::spawn(app);
    }
    Ok(())
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
    let registration_cb = svc.inner().clone();
    let on_success: OnSuccess = std::sync::Arc::new(
        move |generation, client_id, client_secret, open_id| {
            let app = app_cb.clone();
            let registration = registration_cb.clone();
            Box::pin(async move {
                registration
                    .apply_if_live(generation, async move {
                        let db = app.state::<Db>().inner().clone();
                        let bridge = app.state::<crate::im::ImBridge>();
                        let _authority = bridge.authority_write_lease().await;
                        persist_feishu_scan_credentials(
                            &db,
                            &client_id,
                            &client_secret,
                            &open_id,
                        )
                        .await?;
                        let selected = crate::im::ImSettings::active_provider(&db).await?;
                        if selected == crate::im::ImProvider::Feishu {
                            crate::im::spawn(app.clone());
                        }
                        Ok(())
                    })
                    .await?;
                Ok(())
            }) as futures::future::BoxFuture<'static, anyhow::Result<()>>
        },
    );
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
pub async fn feishu_scan_cancel(
    svc: State<'_, crate::im::feishu::registration::RegistrationService>,
) -> R<()> {
    svc.cancel().await;
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

    #[test]
    fn waiting_dingtalk_bridge_retries_after_copy_was_already_initialized() {
        assert!(dingtalk_copy_should_start_bridge(
            crate::im::DingTalkCopyUpdate::Unchanged,
            "waiting_locale"
        ));
        assert!(dingtalk_copy_should_start_bridge(
            crate::im::DingTalkCopyUpdate::Updated,
            "waiting_locale"
        ));
        assert!(!dingtalk_copy_should_start_bridge(
            crate::im::DingTalkCopyUpdate::Updated,
            "online"
        ));
    }

    #[tokio::test]
    async fn toggling_named_im_provider_does_not_select_it() {
        let db = Db::connect("sqlite::memory:").await.unwrap();
        repo::set_setting(&db, crate::im::K_PROVIDER, "dingtalk")
            .await
            .unwrap();

        persist_im_enabled(&db, crate::im::ImProvider::Feishu, true)
            .await
            .unwrap();

        assert_eq!(
            repo::get_setting(&db, crate::im::K_PROVIDER)
                .await
                .unwrap()
                .as_deref(),
            Some("dingtalk")
        );
        assert_eq!(
            repo::get_setting(&db, crate::im::K_ENABLED)
                .await
                .unwrap()
                .as_deref(),
            Some("1")
        );
    }

    #[tokio::test]
    async fn late_feishu_scan_credentials_keep_newer_dingtalk_selection() {
        let db = Db::connect("sqlite::memory:").await.unwrap();
        repo::set_setting(&db, crate::im::K_PROVIDER, "dingtalk")
            .await
            .unwrap();

        persist_feishu_scan_credentials(&db, "cli_feishu", "sec_feishu", "ou_owner")
            .await
            .unwrap();

        assert_eq!(
            repo::get_setting(&db, crate::im::K_PROVIDER)
                .await
                .unwrap()
                .as_deref(),
            Some("dingtalk")
        );
        assert_eq!(
            repo::get_setting(&db, crate::im::K_APP_ID)
                .await
                .unwrap()
                .as_deref(),
            Some("cli_feishu")
        );
        assert_eq!(
            repo::get_setting(&db, crate::im::K_ALLOW)
                .await
                .unwrap()
                .as_deref(),
            Some("ou_owner")
        );
        assert_eq!(
            repo::get_setting(&db, crate::im::K_ENABLED)
                .await
                .unwrap()
                .as_deref(),
            Some("1")
        );
    }

    #[tokio::test]
    async fn late_scan_generation_cannot_overwrite_newer_manual_feishu_credentials() {
        let db = Db::connect("sqlite::memory:").await.unwrap();
        let registration =
            crate::im::feishu::registration::RegistrationService::default();

        registration
            .supersede_with(persist_im_credentials(
                &db,
                crate::im::ImProvider::Feishu,
                "cli_manual",
                "sec_manual",
                false,
                None,
                true,
            ))
            .await
            .unwrap();
        let applied = registration
            .apply_if_live(
                0,
                persist_feishu_scan_credentials(&db, "cli_scan", "sec_scan", "ou_scan"),
            )
            .await
            .unwrap();

        assert!(!applied, "superseded scan callback must not write");
        assert_eq!(
            repo::get_setting(&db, crate::im::K_APP_ID)
                .await
                .unwrap()
                .as_deref(),
            Some("cli_manual")
        );
        assert_eq!(
            repo::get_setting(&db, crate::im::K_APP_SECRET)
                .await
                .unwrap()
                .as_deref(),
            Some("sec_manual")
        );
        assert!(repo::get_setting(&db, crate::im::K_ALLOW)
            .await
            .unwrap()
            .is_none());
        assert!(repo::get_setting(&db, crate::im::K_ENABLED)
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn fresh_profile_scan_uses_default_feishu_provider() {
        let db = Db::connect("sqlite::memory:").await.unwrap();

        persist_feishu_scan_credentials(&db, "cli_feishu", "sec_feishu", "ou_owner")
            .await
            .unwrap();

        assert!(repo::get_setting(&db, crate::im::K_PROVIDER)
            .await
            .unwrap()
            .is_none());
        assert_eq!(
            crate::im::ImSettings::active_provider(&db).await.unwrap(),
            crate::im::ImProvider::Feishu
        );
    }

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

    async fn repo_action_card(
        db: &Db,
        action_id: &str,
        action_kind: &str,
    ) -> (
        entities::workspace::Model,
        entities::thread::Model,
        entities::lead_message::Model,
    ) {
        let workspace = repo::create_workspace(db, &format!("ws-{action_id}"))
            .await
            .unwrap();
        let thread = repo::create_thread(db, workspace.id, "issue", "feature", "codex")
            .await
            .unwrap();
        let card = repo::insert_lead_message(
            db,
            thread.id,
            None,
            1,
            "assistant",
            "action_card",
            &serde_json::json!({
                "title": "Add the repository",
                "actions": [{"id": action_id, "kind": action_kind, "label": "Run"}],
            })
            .to_string(),
            "complete",
        )
        .await
        .unwrap();
        (workspace, thread, card)
    }

    async fn materialize_unregistered_repo_action(
        db: &Db,
        root: &std::path::Path,
        action_id: &str,
    ) -> (
        entities::workspace::Model,
        entities::thread::Model,
        entities::lead_message::Model,
        entities::repo_action_execution::Model,
        std::path::PathBuf,
    ) {
        let destination = root.join(format!("{action_id}-dest"));
        std::fs::create_dir_all(&destination).unwrap();
        let destination = std::fs::canonicalize(destination).unwrap();
        let target = destination.join("checkout");
        let (workspace, thread, card) = repo_action_card(db, action_id, "new").await;
        let fingerprint = repo_action_fingerprint(&["new", &target.to_string_lossy(), "checkout"]);
        let mut admission = admit_repo_action(
            db,
            workspace.id,
            Some(thread.id),
            Some(card.id),
            Some(action_id),
            Some("new"),
            "new",
            &fingerprint,
            &target,
            Some(&destination),
        )
        .await
        .unwrap()
        .unwrap();
        materialize_repo_action(db, &mut admission, crate::git::init_repo)
            .await
            .unwrap();
        let execution = admission.execution.clone();
        drop(admission);
        (workspace, thread, card, execution, target)
    }

    #[tokio::test]
    async fn repo_action_wrong_identity_and_stale_card_are_rejected_before_mutation() {
        let db = Db::connect("sqlite::memory:").await.unwrap();
        let root = tempfile::tempdir().unwrap();
        let local = init_main_repo(root.path(), "local");
        let (workspace, thread, card) = repo_action_card(&db, "add", "add").await;

        let wrong = add_repo_ref_inner(
            &db,
            workspace.id,
            "local".to_string(),
            local.to_string_lossy().into_owned(),
            Some(thread.id),
            Some(card.id),
            Some("other-action".to_string()),
            Some("add".to_string()),
        )
        .await
        .unwrap_err();
        assert_eq!(wrong, "action_card_stale");
        assert!(repo::get_repo_action_execution(&db, card.id)
            .await
            .unwrap()
            .is_none());

        repo::insert_lead_message(
            &db,
            thread.id,
            None,
            2,
            "assistant",
            "text",
            "Use a different repository instead",
            "complete",
        )
        .await
        .unwrap();
        let stale = add_repo_ref_inner(
            &db,
            workspace.id,
            "local".to_string(),
            local.to_string_lossy().into_owned(),
            Some(thread.id),
            Some(card.id),
            Some("add".to_string()),
            Some("add".to_string()),
        )
        .await
        .unwrap_err();
        assert_eq!(stale, "action_card_stale");
        assert!(
            repo::list_repos(&db, workspace.id)
                .await
                .unwrap()
                .is_empty(),
            "the stale action is rejected before any workspace mutation"
        );
    }

    #[tokio::test]
    async fn stale_new_and_clone_cards_leave_no_target_or_staging_side_effect() {
        let db = Db::connect("sqlite::memory:").await.unwrap();
        let root = tempfile::tempdir().unwrap();

        let new_dest = root.path().join("new-dest");
        std::fs::create_dir_all(&new_dest).unwrap();
        let (new_workspace, new_thread, new_card) =
            repo_action_card(&db, "stale-new", "new").await;
        repo::insert_lead_message(
            &db,
            new_thread.id,
            None,
            2,
            "assistant",
            "text",
            "A newer turn superseded the repository card",
            "complete",
        )
        .await
        .unwrap();
        let new_error = create_repo_inner(
            &db,
            new_workspace.id,
            "checkout".to_string(),
            new_dest.to_string_lossy().into_owned(),
            Some(new_thread.id),
            Some(new_card.id),
            Some("stale-new".to_string()),
            Some("new".to_string()),
        )
        .await
        .unwrap_err();
        assert_eq!(new_error, "action_card_stale");
        assert!(repo::get_repo_action_execution(&db, new_card.id)
            .await
            .unwrap()
            .is_none());
        assert!(repo::list_repos(&db, new_workspace.id)
            .await
            .unwrap()
            .is_empty());
        assert!(!new_dest.join("checkout").exists());
        assert!(std::fs::read_dir(&new_dest)
            .unwrap()
            .all(|entry| !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains("weft-repo-action")));

        let source = init_main_repo(root.path(), "stale-clone-source");
        let clone_dest = root.path().join("clone-dest");
        std::fs::create_dir_all(&clone_dest).unwrap();
        let (clone_workspace, clone_thread, clone_card) =
            repo_action_card(&db, "stale-clone", "clone").await;
        repo::insert_lead_message(
            &db,
            clone_thread.id,
            None,
            2,
            "assistant",
            "text",
            "A newer turn superseded the clone card",
            "complete",
        )
        .await
        .unwrap();
        let clone_error = clone_repo_inner(
            &db,
            clone_workspace.id,
            source.to_string_lossy().into_owned(),
            clone_dest.to_string_lossy().into_owned(),
            "checkout".to_string(),
            Some(clone_thread.id),
            Some(clone_card.id),
            Some("stale-clone".to_string()),
            Some("clone".to_string()),
        )
        .await
        .unwrap_err();
        assert_eq!(clone_error, "action_card_stale");
        assert!(repo::get_repo_action_execution(&db, clone_card.id)
            .await
            .unwrap()
            .is_none());
        assert!(repo::list_repos(&db, clone_workspace.id)
            .await
            .unwrap()
            .is_empty());
        assert!(!clone_dest.join("checkout").exists());
        assert!(std::fs::read_dir(&clone_dest)
            .unwrap()
            .all(|entry| !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains("weft-repo-action")));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_exact_repo_action_is_one_execution_mutation_resolution_and_feedback() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let db = Db::connect("sqlite::memory:").await.unwrap();
        let root = tempfile::tempdir().unwrap();
        let local = init_main_repo(root.path(), "concurrent-local");
        let (workspace, thread, card) = repo_action_card(&db, "concurrent-add", "add").await;
        let args = || {
            (
                workspace.id,
                "concurrent-local".to_string(),
                local.to_string_lossy().into_owned(),
                Some(thread.id),
                Some(card.id),
                Some("concurrent-add".to_string()),
                Some("add".to_string()),
            )
        };

        let first_db = db.clone();
        let (first_workspace, first_name, first_path, first_thread, first_message, first_id, first_kind) =
            args();
        let first = tokio::spawn(async move {
            add_repo_ref_inner(
                &first_db,
                first_workspace,
                first_name,
                first_path,
                first_thread,
                first_message,
                first_id,
                first_kind,
            )
            .await
        });
        let second_db = db.clone();
        let (second_workspace, second_name, second_path, second_thread, second_message, second_id, second_kind) =
            args();
        let second = tokio::spawn(async move {
            add_repo_ref_inner(
                &second_db,
                second_workspace,
                second_name,
                second_path,
                second_thread,
                second_message,
                second_id,
                second_kind,
            )
            .await
        });
        let first = first.await.unwrap().unwrap();
        let second = second.await.unwrap().unwrap();
        let outcomes = [first.outcome, second.outcome];
        assert!(outcomes.contains(&RepoActionExecutionOutcome::FreshlyCompleted));
        assert!(outcomes.contains(&RepoActionExecutionOutcome::Replayed));
        assert_eq!(first.repo.id, second.repo.id);
        assert_eq!(repo::list_repos(&db, workspace.id).await.unwrap().len(), 1);

        let message = repo::get_lead_message(&db, card.id)
            .await
            .unwrap()
            .unwrap();
        let content: serde_json::Value = serde_json::from_str(&message.content).unwrap();
        assert_eq!(content["resolved"], "concurrent-local");
        let execution = repo::get_repo_action_execution(&db, card.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(execution.status, repo::REPO_ACTION_COMPLETED);
        assert_eq!(execution.repo_id, first.repo.id);
        assert_eq!(execution.feedback_state, repo::REPO_ACTION_FEEDBACK_PENDING);

        let deliveries = std::sync::Arc::new(AtomicUsize::new(0));
        let first_deliveries = deliveries.clone();
        let second_deliveries = deliveries.clone();
        let execution_id = execution.id;
        let expected_thread_id = thread.id;
        let first_drain = drain_repo_action_feedback_with(
            &db,
            execution_id,
            move |delivered_thread_id, payload| async move {
                assert_eq!(delivered_thread_id, expected_thread_id);
                assert_eq!(payload["execution_id"], execution_id);
                first_deliveries.fetch_add(1, Ordering::SeqCst);
                Ok(true)
            },
        );
        let second_drain = drain_repo_action_feedback_with(
            &db,
            execution_id,
            move |_, _| async move {
                second_deliveries.fetch_add(1, Ordering::SeqCst);
                Ok(true)
            },
        );
        let (first_done, second_done) = tokio::join!(first_drain, second_drain);
        assert!(first_done.unwrap() || second_done.unwrap());
        assert_eq!(deliveries.load(Ordering::SeqCst), 1);
        assert_eq!(
            repo::get_repo_action_execution(&db, card.id)
                .await
                .unwrap()
                .unwrap()
                .feedback_state,
            repo::REPO_ACTION_FEEDBACK_DELIVERED
        );
    }

    #[tokio::test]
    async fn claimed_repo_action_owner_completes_after_newer_assistant_activity() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let db = Db::connect("sqlite::memory:").await.unwrap();
        let root = tempfile::tempdir().unwrap();
        let local = init_main_repo(root.path(), "barrier-local");
        let (workspace, thread, card) = repo_action_card(&db, "barrier-add", "add").await;
        let local_text = local.to_string_lossy().into_owned();
        let target = normalized_existing_repo_target(&local_text).unwrap();
        let target_text = target.to_string_lossy().into_owned();
        let fingerprint = repo_action_fingerprint(&["add", &target_text, "barrier-local"]);
        let mut admission = admit_repo_action(
            &db,
            workspace.id,
            Some(thread.id),
            Some(card.id),
            Some("barrier-add"),
            Some("add"),
            "add",
            &fingerprint,
            &target,
            None,
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(admission.execution.status, repo::REPO_ACTION_PENDING);

        // The claim is the write-first actionability linearization point. A
        // newer assistant row after it must not revoke this owner completion;
        // otherwise the already-authorized mutation could strand its repo.
        repo::insert_lead_message(
            &db,
            thread.id,
            None,
            2,
            "assistant",
            "text",
            "A newer turn arrived after the action claim",
            "complete",
        )
        .await
        .unwrap();
        admission.execution = repo::mark_repo_action_materialized(
            &db,
            admission.execution.id,
            &admission.execution.execution_token,
        )
        .await
        .unwrap();
        let repo_ref = register_repo_without_schedule(
            &db,
            workspace.id,
            "barrier-local",
            &target_text,
            Some(&admission._os_lock),
        )
        .await
        .unwrap();
        complete_admitted_repo_action(&db, &mut admission, &repo_ref)
        .await
        .unwrap();
        cleanup_completed_action_target(&admission, &repo_ref).unwrap();
        drop(admission);

        assert_eq!(repo::list_repos(&db, workspace.id).await.unwrap().len(), 1);
        let message = repo::get_lead_message(&db, card.id)
            .await
            .unwrap()
            .unwrap();
        let content: serde_json::Value = serde_json::from_str(&message.content).unwrap();
        assert_eq!(content["resolved"], "barrier-local");
        let execution = repo::get_repo_action_execution(&db, card.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(execution.status, repo::REPO_ACTION_COMPLETED);
        assert_eq!(execution.feedback_state, repo::REPO_ACTION_FEEDBACK_PENDING);

        let deliveries = std::sync::Arc::new(AtomicUsize::new(0));
        let first_deliveries = deliveries.clone();
        let second_deliveries = deliveries.clone();
        let first = drain_repo_action_feedback_with(
            &db,
            execution.id,
            move |_, payload| async move {
                assert_eq!(payload["execution_id"], execution.id);
                first_deliveries.fetch_add(1, Ordering::SeqCst);
                Ok(true)
            },
        );
        let second = drain_repo_action_feedback_with(
            &db,
            execution.id,
            move |_, _| async move {
                second_deliveries.fetch_add(1, Ordering::SeqCst);
                Ok(true)
            },
        );
        let (first, second) = tokio::join!(first, second);
        assert!(first.unwrap() || second.unwrap());
        assert_eq!(deliveries.load(Ordering::SeqCst), 1);
        assert_eq!(
            repo::get_repo_action_execution(&db, card.id)
                .await
                .unwrap()
                .unwrap()
                .feedback_state,
            repo::REPO_ACTION_FEEDBACK_DELIVERED
        );
    }

    #[test]
    fn repo_action_os_lock_allows_only_one_execution_holder() {
        let token = new_repo_action_token();
        let first = acquire_repo_action_os_lock(&token).unwrap();
        let second = acquire_repo_action_os_lock(&token).unwrap_err();
        assert_eq!(second, "action_card_in_progress");
        drop(first);
        assert!(acquire_repo_action_os_lock(&token).is_ok());
    }

    #[tokio::test]
    async fn materialize_promotion_never_replaces_a_foreign_empty_target() {
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;

        let db = Db::connect("sqlite::memory:").await.unwrap();
        let root = tempfile::tempdir().unwrap();
        let destination = std::fs::canonicalize(root.path()).unwrap();
        let target = destination.join("foreign-empty");
        let (workspace, thread, card) = repo_action_card(&db, "noreplace", "new").await;
        let fingerprint =
            repo_action_fingerprint(&["new", &target.to_string_lossy(), "foreign-empty"]);
        let mut admission = admit_repo_action(
            &db,
            workspace.id,
            Some(thread.id),
            Some(card.id),
            Some("noreplace"),
            Some("new"),
            "new",
            &fingerprint,
            &target,
            Some(&destination),
        )
        .await
        .unwrap()
        .unwrap();
        let staging = std::path::PathBuf::from(&admission.execution.staging_path);
        #[cfg(unix)]
        let foreign_inode = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        #[cfg(unix)]
        let mutation_inode = foreign_inode.clone();
        let foreign_target = target.clone();
        let error = materialize_repo_action(&db, &mut admission, move |staging| {
            crate::git::init_repo(staging)?;
            // Deterministic promotion seam: this lands after the initial target
            // preflight and before the no-replace rename.
            std::fs::create_dir(&foreign_target)?;
            #[cfg(unix)]
            mutation_inode.store(
                std::fs::symlink_metadata(&foreign_target)?.ino(),
                std::sync::atomic::Ordering::SeqCst,
            );
            Ok(())
        })
        .await
        .unwrap_err();

        assert!(error.contains("cannot atomically install repository"));
        assert!(target.is_dir());
        assert!(std::fs::read_dir(&target).unwrap().next().is_none());
        #[cfg(unix)]
        assert_eq!(
            std::fs::symlink_metadata(&target).unwrap().ino(),
            foreign_inode.load(std::sync::atomic::Ordering::SeqCst)
        );
        assert!(!target.join(".git").join(REPO_ACTION_TOKEN_MARKER).exists());
        assert!(
            !staging.exists(),
            "owned staging is journaled and reclaimed"
        );
        assert!(repo::get_repo_action_execution(&db, card.id)
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn add_repo_action_completed_replay_is_idempotent_and_rejects_new_args() {
        let db = Db::connect("sqlite::memory:").await.unwrap();
        let root = tempfile::tempdir().unwrap();
        let local = init_main_repo(root.path(), "local");
        let (workspace, thread, card) = repo_action_card(&db, "add", "add").await;
        let first = add_repo_ref_inner(
            &db,
            workspace.id,
            "local".to_string(),
            local.to_string_lossy().into_owned(),
            Some(thread.id),
            Some(card.id),
            Some("add".to_string()),
            Some("add".to_string()),
        )
        .await
        .unwrap();
        let replay = add_repo_ref_inner(
            &db,
            workspace.id,
            "local".to_string(),
            local.to_string_lossy().into_owned(),
            Some(thread.id),
            Some(card.id),
            Some("add".to_string()),
            Some("add".to_string()),
        )
        .await
        .unwrap();
        assert_eq!(first.outcome, RepoActionExecutionOutcome::FreshlyCompleted);
        assert_eq!(replay.outcome, RepoActionExecutionOutcome::Replayed);
        assert_eq!(replay.repo.id, first.repo.id);
        assert_eq!(repo::list_repos(&db, workspace.id).await.unwrap().len(), 1);
        let execution = repo::get_repo_action_execution(&db, card.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(execution.status, repo::REPO_ACTION_COMPLETED);
        assert_eq!(execution.repo_id, first.repo.id);
        assert_eq!(execution.feedback_state, repo::REPO_ACTION_FEEDBACK_PENDING);
        let payload: serde_json::Value = serde_json::from_str(&execution.feedback_payload).unwrap();
        assert_eq!(payload["execution_id"], execution.id);
        assert_eq!(payload["action_id"], "add");
        assert_eq!(payload["status"], "ok");

        let deliveries = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let execution_id = execution.id;
        let expected_thread_id = thread.id;
        let first_deliveries = deliveries.clone();
        let second_deliveries = deliveries.clone();
        let first_drain = drain_repo_action_feedback_with(
            &db,
            execution_id,
            move |delivered_thread_id, payload| async move {
                assert_eq!(delivered_thread_id, expected_thread_id);
                assert_eq!(payload["execution_id"], execution_id);
                first_deliveries.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
                Ok(true)
            },
        );
        let second_drain =
            drain_repo_action_feedback_with(&db, execution_id, move |_, _| async move {
                second_deliveries.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(true)
            });
        let (first_done, second_done) = tokio::join!(first_drain, second_drain);
        assert!(first_done.unwrap() || second_done.unwrap());
        assert_eq!(deliveries.load(std::sync::atomic::Ordering::SeqCst), 1);
        let delivered = repo::get_repo_action_execution(&db, card.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            delivered.feedback_state,
            repo::REPO_ACTION_FEEDBACK_DELIVERED
        );

        let wrong_args = add_repo_ref_inner(
            &db,
            workspace.id,
            "renamed".to_string(),
            local.to_string_lossy().into_owned(),
            Some(thread.id),
            Some(card.id),
            Some("add".to_string()),
            Some("add".to_string()),
        )
        .await
        .unwrap_err();
        assert_eq!(wrong_args, "action_card_stale");
    }

    #[tokio::test]
    async fn completed_feedback_survives_repo_delete_until_it_is_delivered_once() {
        let db = Db::connect("sqlite::memory:").await.unwrap();
        let root = tempfile::tempdir().unwrap();
        let local = init_main_repo(root.path(), "delete-before-feedback");
        let (workspace, thread, card) = repo_action_card(&db, "add-delete", "add").await;
        let completed = add_repo_ref_inner(
            &db,
            workspace.id,
            "delete-before-feedback".to_string(),
            local.to_string_lossy().into_owned(),
            Some(thread.id),
            Some(card.id),
            Some("add-delete".to_string()),
            Some("add".to_string()),
        )
        .await
        .unwrap();
        let execution_id = completed.execution_id.unwrap();
        let feedback_locks = lock_repo_action_cleanups(
            &db,
            repo::pending_repo_action_feedback_for_repo(&db, completed.repo.id)
                .await
                .unwrap(),
        )
        .await
        .unwrap();
        repo::delete_repo_cascade_with_human_cancellations(&db, completed.repo.id)
            .await
            .unwrap();
        let retained = repo::get_repo_action_execution_by_id(&db, execution_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(retained.feedback_state, repo::REPO_ACTION_FEEDBACK_PENDING);
        assert!(repo::get_repo(&db, completed.repo.id)
            .await
            .unwrap()
            .is_none());
        drop(feedback_locks);

        let deliveries = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let delivered = deliveries.clone();
        assert!(drain_repo_action_feedback_with(
            &db,
            execution_id,
            move |delivered_thread, payload| async move {
                assert_eq!(delivered_thread, thread.id);
                assert_eq!(payload["execution_id"], execution_id);
                delivered.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(true)
            },
        )
        .await
        .unwrap());
        assert_eq!(deliveries.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(repo::get_repo_action_execution_by_id(&db, execution_id)
            .await
            .unwrap()
            .is_none());
        assert!(local.exists(), "repo delete only removes Weft tracking");
    }

    #[tokio::test]
    async fn clone_mutation_failure_releases_owned_claim_and_same_args_can_retry() {
        let db = Db::connect("sqlite::memory:").await.unwrap();
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        std::fs::create_dir_all(&source).unwrap();
        let dest = root.path().join("dest");
        std::fs::create_dir_all(&dest).unwrap();
        let (workspace, thread, card) = repo_action_card(&db, "clone", "clone").await;
        let args = || {
            (
                source.to_string_lossy().into_owned(),
                dest.to_string_lossy().into_owned(),
                "checkout".to_string(),
                Some(thread.id),
                Some(card.id),
                Some("clone".to_string()),
                Some("clone".to_string()),
            )
        };
        let (url, destination, name, thread_id, message_id, action_id, action_kind) = args();
        assert!(clone_repo_inner(
            &db,
            workspace.id,
            url,
            destination,
            name,
            thread_id,
            message_id,
            action_id,
            action_kind,
        )
        .await
        .is_err());
        assert!(repo::get_repo_action_execution(&db, card.id)
            .await
            .unwrap()
            .is_none());
        assert!(!dest.join("checkout").exists());
        assert!(std::fs::read_dir(&dest).unwrap().all(|entry| !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .contains("weft-repo-action")));

        sh(&source, &["git", "init", "-q"]);
        sh(&source, &["git", "config", "user.email", "t@t.t"]);
        sh(&source, &["git", "config", "user.name", "t"]);
        std::fs::write(source.join("README.md"), "# source\n").unwrap();
        sh(&source, &["git", "add", "-A"]);
        sh(&source, &["git", "commit", "-q", "-m", "init"]);
        let (url, destination, name, thread_id, message_id, action_id, action_kind) = args();
        let repo_ref = clone_repo_inner(
            &db,
            workspace.id,
            url,
            destination,
            name,
            thread_id,
            message_id,
            action_id,
            action_kind,
        )
        .await
        .unwrap();
        assert_eq!(
            std::fs::canonicalize(&repo_ref.repo.local_git_path).unwrap(),
            std::fs::canonicalize(dest.join("checkout")).unwrap()
        );
        assert!(dest.join("checkout").join(".git").is_dir());
        assert!(!dest
            .join("checkout")
            .join(".git")
            .join(REPO_ACTION_TOKEN_MARKER)
            .exists());
    }

    #[tokio::test]
    async fn guarded_create_success_preserves_checkout_and_removes_only_token_marker() {
        let db = Db::connect("sqlite::memory:").await.unwrap();
        let root = tempfile::tempdir().unwrap();
        let dest = root.path().join("new-dest");
        std::fs::create_dir_all(&dest).unwrap();
        let (workspace, thread, card) = repo_action_card(&db, "new", "new").await;

        let created = create_repo_inner(
            &db,
            workspace.id,
            "created".to_string(),
            dest.to_string_lossy().into_owned(),
            Some(thread.id),
            Some(card.id),
            Some("new".to_string()),
            Some("new".to_string()),
        )
        .await
        .unwrap();
        let target = dest.join("created");
        assert_eq!(
            std::fs::canonicalize(&created.repo.local_git_path).unwrap(),
            std::fs::canonicalize(&target).unwrap()
        );
        assert!(target.join(".git").is_dir());
        assert!(!target.join(".git").join(REPO_ACTION_TOKEN_MARKER).exists());
        let execution = repo::get_repo_action_execution(&db, card.id)
            .await
            .unwrap()
            .unwrap();
        assert!(execution.cleanup_preserve_target);
        assert_eq!(execution.feedback_state, repo::REPO_ACTION_FEEDBACK_PENDING);
    }

    #[tokio::test]
    async fn guarded_remote_duplicate_clone_still_removes_only_the_redundant_checkout() {
        let db = Db::connect("sqlite::memory:").await.unwrap();
        let root = tempfile::tempdir().unwrap();
        let source = init_main_repo(root.path(), "dedup-source");
        let existing = root.path().join("existing");
        sh(
            root.path(),
            &[
                "git",
                "clone",
                "-q",
                source.to_str().unwrap(),
                existing.to_str().unwrap(),
            ],
        );
        let dest = root.path().join("dedup-dest");
        std::fs::create_dir_all(&dest).unwrap();
        let (workspace, thread, card) = repo_action_card(&db, "dedup-clone", "clone").await;
        let existing_repo =
            register_repo(&db, workspace.id, "existing", &existing.to_string_lossy())
                .await
                .unwrap();

        let completed = clone_repo_inner(
            &db,
            workspace.id,
            source.to_string_lossy().into_owned(),
            dest.to_string_lossy().into_owned(),
            "duplicate".to_string(),
            Some(thread.id),
            Some(card.id),
            Some("dedup-clone".to_string()),
            Some("clone".to_string()),
        )
        .await
        .unwrap();

        let duplicate = dest.join("duplicate");
        assert_eq!(completed.repo.id, existing_repo.id);
        assert!(existing.join(".git").is_dir());
        assert!(!duplicate.exists());
        assert_eq!(repo::list_repos(&db, workspace.id).await.unwrap().len(), 1);
        let execution = repo::get_repo_action_execution(&db, card.id)
            .await
            .unwrap()
            .unwrap();
        assert!(!execution.cleanup_preserve_target);
    }

    #[tokio::test]
    async fn clone_finalize_failure_resumes_marker_without_recloning() {
        use sea_orm::ConnectionTrait;

        let db = Db::connect("sqlite::memory:").await.unwrap();
        let root = tempfile::tempdir().unwrap();
        let source = init_main_repo(root.path(), "source");
        let dest = root.path().join("dest");
        std::fs::create_dir_all(&dest).unwrap();
        let (workspace, thread, card) = repo_action_card(&db, "clone", "clone").await;
        db.0.execute_unprepared(
            "CREATE TRIGGER fail_repo_action_complete BEFORE UPDATE OF status \
                 ON repo_action_execution WHEN NEW.status = 'completed' BEGIN \
                 SELECT RAISE(ABORT, 'forced finalize failure'); END;",
        )
        .await
        .unwrap();
        let call = |url: String| {
            clone_repo_inner(
                &db,
                workspace.id,
                url,
                dest.to_string_lossy().into_owned(),
                "checkout".to_string(),
                Some(thread.id),
                Some(card.id),
                Some("clone".to_string()),
                Some("clone".to_string()),
            )
        };
        let first_error = call(source.to_string_lossy().into_owned())
            .await
            .unwrap_err();
        assert!(first_error.contains("forced finalize failure"));
        let pending = repo::get_repo_action_execution(&db, card.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(pending.status, repo::REPO_ACTION_MATERIALIZED);
        let target = dest.join("checkout");
        assert!(repo_action_target_has_token(
            &target,
            &pending.execution_token
        ));
        let first_repo = repo::list_repos(&db, workspace.id).await.unwrap();
        assert_eq!(first_repo.len(), 1);

        db.0.execute_unprepared("DROP TRIGGER fail_repo_action_complete")
            .await
            .unwrap();
        std::fs::remove_dir_all(&source).unwrap();
        let replay = call(source.to_string_lossy().into_owned()).await.unwrap();
        assert_eq!(replay.repo.id, first_repo[0].id);
        assert_eq!(repo::list_repos(&db, workspace.id).await.unwrap().len(), 1);
        assert!(!target.join(".git").join(REPO_ACTION_TOKEN_MARKER).exists());
    }

    #[tokio::test]
    async fn failed_thread_delete_rolls_back_action_cleanup_and_action_retries() {
        use sea_orm::{ConnectionTrait, Statement};

        let db = Db::connect("sqlite::memory:").await.unwrap();
        let root = tempfile::tempdir().unwrap();
        let source = init_main_repo(root.path(), "rollback-source");
        let dest = root.path().join("rollback-dest");
        std::fs::create_dir_all(&dest).unwrap();
        let (workspace, thread, card) = repo_action_card(&db, "rollback-clone", "clone").await;
        db.0.execute_unprepared(
            "CREATE TRIGGER fail_action_finalize_for_delete BEFORE UPDATE OF status \
                 ON repo_action_execution WHEN NEW.status = 'completed' BEGIN \
                 SELECT RAISE(ABORT, 'forced finalize failure'); END;",
        )
        .await
        .unwrap();
        let call = || {
            clone_repo_inner(
                &db,
                workspace.id,
                source.to_string_lossy().into_owned(),
                dest.to_string_lossy().into_owned(),
                "checkout".to_string(),
                Some(thread.id),
                Some(card.id),
                Some("rollback-clone".to_string()),
                Some("clone".to_string()),
            )
        };
        assert!(call()
            .await
            .unwrap_err()
            .contains("forced finalize failure"));
        db.0.execute_unprepared("DROP TRIGGER fail_action_finalize_for_delete")
            .await
            .unwrap();

        let locked = lock_repo_action_cleanups(
            &db,
            repo::repo_action_executions_requiring_lock_for_thread(&db, thread.id)
                .await
                .unwrap(),
        )
        .await
        .unwrap();
        let plans = repo_action_cleanup_plans(&locked);
        db.0.execute(Statement::from_string(
            sea_orm::DatabaseBackend::Sqlite,
            format!(
                "CREATE TRIGGER fail_thread_delete_with_action BEFORE DELETE ON thread \
                     WHEN OLD.id = {} BEGIN SELECT RAISE(ABORT, 'forced delete failure'); END",
                thread.id
            ),
        ))
        .await
        .unwrap();
        assert!(
            repo::delete_thread_cascade_with_human_cancellations_and_action_cleanups(
                &db, thread.id, &plans,
            )
            .await
            .is_err()
        );
        let execution = repo::get_repo_action_execution(&db, card.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(execution.status, repo::REPO_ACTION_MATERIALIZED);
        assert!(!execution.cleanup_preserve_target);
        let target = dest.join("checkout");
        assert!(repo_action_target_has_token(
            &target,
            &execution.execution_token
        ));
        assert!(repo::get_thread(&db, thread.id).await.unwrap().is_some());

        db.0.execute_unprepared("DROP TRIGGER fail_thread_delete_with_action")
            .await
            .unwrap();
        drop(locked);
        let completed = call().await.unwrap();
        assert_eq!(
            completed.outcome,
            RepoActionExecutionOutcome::FreshlyCompleted
        );
        assert!(target.exists());
        assert!(!target.join(".git").join(REPO_ACTION_TOKEN_MARKER).exists());
    }

    #[tokio::test]
    async fn cleanup_plan_false_is_upgraded_when_exact_target_is_registered_in_cascade_txn() {
        let db = Db::connect("sqlite::memory:").await.unwrap();
        let root = tempfile::tempdir().unwrap();
        let (workspace, thread, card, execution, target) =
            materialize_unregistered_repo_action(&db, root.path(), "cleanup-adopt").await;

        let locked = lock_repo_action_cleanups(&db, vec![execution])
            .await
            .unwrap();
        let plans = repo_action_cleanup_plans(&locked);
        assert_eq!(plans.len(), 1);
        assert!(!plans[0].preserve_target);

        // Deterministic plan→cascade seam. The typed held-lock path models the
        // exact DB state a writer that linearized before cleanup would leave,
        // without dropping cleanup ownership between plan and transaction.
        let registered = register_repo_without_schedule(
            &db,
            workspace.id,
            "adopted",
            &target.to_string_lossy(),
            Some(&locked[0]._lock),
        )
        .await
        .unwrap();
        assert_eq!(
            std::fs::canonicalize(&registered.local_git_path).unwrap(),
            std::fs::canonicalize(&target).unwrap()
        );

        repo::delete_thread_cascade_with_human_cancellations_and_action_cleanups(
            &db, thread.id, &plans,
        )
        .await
        .unwrap();
        let journal = repo::get_repo_action_execution(&db, card.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(journal.status, repo::REPO_ACTION_CLEANUP_PENDING);
        assert!(journal.cleanup_preserve_target);

        cleanup_locked_repo_actions(&db, &locked).await;
        assert!(repo::get_repo(&db, registered.id).await.unwrap().is_some());
        assert!(target.join(".git").is_dir());
        assert!(!target.join(".git").join(REPO_ACTION_TOKEN_MARKER).exists());
        assert!(repo::get_repo_action_execution(&db, card.id)
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn cleanup_lock_rejects_guardless_registration_between_commit_and_target_removal() {
        let db = Db::connect("sqlite::memory:").await.unwrap();
        let root = tempfile::tempdir().unwrap();
        let (workspace, thread, card, execution, target) =
            materialize_unregistered_repo_action(&db, root.path(), "cleanup-linearize").await;
        let existing = repo::add_repo_ref(
            &db,
            workspace.id,
            "existing",
            &root.path().join("existing").to_string_lossy(),
            "main",
            "",
            true,
        )
        .await
        .unwrap();

        let locked = lock_repo_action_cleanups(&db, vec![execution])
            .await
            .unwrap();
        let plans = repo_action_cleanup_plans(&locked);
        assert!(!plans[0].preserve_target);
        repo::delete_thread_cascade_with_human_cancellations_and_action_cleanups(
            &db, thread.id, &plans,
        )
        .await
        .unwrap();
        let journal = repo::get_repo_action_execution(&db, card.id)
            .await
            .unwrap()
            .unwrap();
        assert!(!journal.cleanup_preserve_target);

        let registration_error =
            register_repo(&db, workspace.id, "too-late", &target.to_string_lossy())
                .await
                .unwrap_err();
        assert_eq!(registration_error, "action_card_in_progress");
        let repoint_error = repo::set_repo_path(&db, existing.id, &target.to_string_lossy())
            .await
            .unwrap_err();
        assert_eq!(repoint_error.to_string(), "action_card_in_progress");
        assert_ne!(
            repo::get_repo(&db, existing.id)
                .await
                .unwrap()
                .unwrap()
                .local_git_path,
            target.to_string_lossy()
        );

        cleanup_locked_repo_actions(&db, &locked).await;
        assert!(!target.exists());
        assert!(repo::get_repo_action_execution(&db, card.id)
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn action_cleanup_failure_retains_journal_until_checked_retry_succeeds() {
        use sea_orm::ConnectionTrait;

        let db = Db::connect("sqlite::memory:").await.unwrap();
        let root = tempfile::tempdir().unwrap();
        let source = init_main_repo(root.path(), "cleanup-source");
        let dest = root.path().join("cleanup-dest");
        std::fs::create_dir_all(&dest).unwrap();
        let (workspace, thread, card) = repo_action_card(&db, "cleanup-clone", "clone").await;
        db.0.execute_unprepared(
            "CREATE TRIGGER fail_action_finalize_for_cleanup BEFORE UPDATE OF status \
                 ON repo_action_execution WHEN NEW.status = 'completed' BEGIN \
                 SELECT RAISE(ABORT, 'forced finalize failure'); END;",
        )
        .await
        .unwrap();
        assert!(clone_repo_inner(
            &db,
            workspace.id,
            source.to_string_lossy().into_owned(),
            dest.to_string_lossy().into_owned(),
            "checkout".to_string(),
            Some(thread.id),
            Some(card.id),
            Some("cleanup-clone".to_string()),
            Some("clone".to_string()),
        )
        .await
        .unwrap_err()
        .contains("forced finalize failure"));
        db.0.execute_unprepared("DROP TRIGGER fail_action_finalize_for_cleanup")
            .await
            .unwrap();

        let locked = lock_repo_action_cleanups(
            &db,
            repo::repo_action_executions_requiring_lock_for_thread(&db, thread.id)
                .await
                .unwrap(),
        )
        .await
        .unwrap();
        let plans = repo_action_cleanup_plans(&locked);
        repo::delete_thread_cascade_with_human_cancellations_and_action_cleanups(
            &db, thread.id, &plans,
        )
        .await
        .unwrap();
        let journal = repo::get_repo_action_execution(&db, card.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(journal.status, repo::REPO_ACTION_CLEANUP_PENDING);
        assert!(journal.cleanup_preserve_target);
        let target = dest.join("checkout");
        assert!(target.exists());
        assert!(repo_action_target_has_token(
            &target,
            &journal.execution_token
        ));

        let forced = cleanup_locked_repo_action_with(&db, &locked[0], |_, _| {
            Err("forced remove failure".to_string())
        })
        .await;
        assert_eq!(forced.unwrap_err(), "forced remove failure");
        assert!(repo::get_repo_action_execution(&db, card.id)
            .await
            .unwrap()
            .is_some());
        cleanup_locked_repo_actions(&db, &locked).await;
        assert!(repo::get_repo_action_execution(&db, card.id)
            .await
            .unwrap()
            .is_none());
        assert!(
            target.exists(),
            "a registered user repository is never deleted"
        );
        assert!(!target.join(".git").join(REPO_ACTION_TOKEN_MARKER).exists());
    }

    #[tokio::test]
    async fn guarded_clone_preserves_foreign_occupied_target() {
        let db = Db::connect("sqlite::memory:").await.unwrap();
        let root = tempfile::tempdir().unwrap();
        let source = init_main_repo(root.path(), "source");
        let dest = root.path().join("dest");
        let target = dest.join("checkout");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("keep.txt"), "mine").unwrap();
        let (workspace, thread, card) = repo_action_card(&db, "clone", "clone").await;
        let error = clone_repo_inner(
            &db,
            workspace.id,
            source.to_string_lossy().into_owned(),
            dest.to_string_lossy().into_owned(),
            "checkout".to_string(),
            Some(thread.id),
            Some(card.id),
            Some("clone".to_string()),
            Some("clone".to_string()),
        )
        .await
        .unwrap_err();
        assert!(error.contains("not owned by this action"));
        assert_eq!(
            std::fs::read_to_string(target.join("keep.txt")).unwrap(),
            "mine"
        );
        assert!(repo::get_repo_action_execution(&db, card.id)
            .await
            .unwrap()
            .is_none());
        assert!(repo::list_repos(&db, workspace.id)
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn direct_direction_creation_pins_the_explicit_tool() {
        let db = Db::connect("sqlite::memory:").await.unwrap();
        let workspace = repo::create_workspace(&db, "ws").await.unwrap();
        let repo_ref = repo::add_repo_ref(&db, workspace.id, "api", "/tmp/api", "main", "", true)
            .await
            .unwrap();
        let thread = repo::create_thread(&db, workspace.id, "issue", "feature", "codex")
            .await
            .unwrap();

        let direction = create_direction_for_explicit_tool(
            &db,
            thread.id,
            "manual task",
            "opencode",
            repo_ref.id,
            "r",
            "plan+impl",
            "",
        )
        .await
        .unwrap();

        assert_eq!(direction.tool, "opencode");
        assert!(direction.engine_pinned);
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
        assert!(
            def == "main" || def == "master",
            "precondition: init produced main/master"
        );
        let r_std = register_repo(&db, ws.id, "api", std_repo.to_str().unwrap())
            .await
            .unwrap();
        assert!(
            r_std.base_ref_is_default,
            "R47-2: a real main/master default must be captured as is_default=true (unchanged)"
        );
        assert_eq!(
            r_std.base_ref, def,
            "captured base is the vetted default branch"
        );

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

    /// `register_repo` must NOT fire background curator analysis outside a
    /// running app (`maybe_schedule_backfill`'s rule): `tauri::async_runtime::spawn`
    /// falls back to its own runtime even when no App was built, so in unit
    /// tests the leaked pass outlives the test body and mutates the
    /// process-global pass-gate/run-state registries — keyed by the tiny ids
    /// every fresh in-memory DB restarts at, i.e. shared across concurrent
    /// tests — and can even launch a real curator agent on the repo.
    #[tokio::test]
    async fn register_repo_without_app_fires_no_background_analysis() {
        const WS: i32 = 2_000_000_777;
        let tag = format!("weft-regrepo-noapp-{}", std::process::id());
        let root = std::env::temp_dir().join(tag);
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let db = Db::connect("sqlite::memory:").await.unwrap();
        // A real workspace row at a SENTINEL id (`add_repo_ref` checks the
        // workspace exists): the pass-gate registry is process-global and keyed
        // by workspace id, so asserting quiescence on a small fresh-DB id would
        // race any concurrent test legitimately driving that workspace's gate.
        {
            use sea_orm::{ActiveModelTrait, Set};
            entities::workspace::ActiveModel {
                id: Set(WS),
                name: Set("noapp".into()),
                slug: Set("noapp".into()),
                created_at: Set(String::new()),
            }
            .insert(&db.0)
            .await
            .unwrap();
        }

        // Freeze the gate FIRST — and leak the hold on purpose: a leaked spawn
        // marks the gate dirty before blocking on the lock, so the leak shows up
        // as a stuck flag, and a regression's parked task stays parked even
        // after this test's assert unwinds (it must never reach run-state/DB
        // writes or a real agent launch). Sentinel gate, so nothing else in the
        // process ever wants this lock.
        std::mem::forget(crate::curator::test_hold_pass_gate(WS).await);

        let repo_dir = init_main_repo(&root, "svc");
        let r = register_repo(&db, WS, "svc", repo_dir.to_str().unwrap())
            .await
            .unwrap();
        assert_eq!(
            r.workspace_id, WS,
            "precondition: row landed on the sentinel workspace"
        );

        // Absence assert over a generous window — the buggy spawn reaches the
        // gate within milliseconds of register_repo returning.
        for _ in 0..20 {
            assert!(
                !crate::curator::test_pass_gate_dirty(WS),
                "register_repo queued a background analysis pass with no app running"
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }

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
    async fn thread_deletion_fence_blocks_session_creation_after_engine_snapshot() {
        let db = Db::connect("sqlite::memory:").await.unwrap();
        let workspace = repo::create_workspace(&db, "ws").await.unwrap();
        let repo_ref = repo::add_repo_ref(&db, workspace.id, "api", "/tmp/api", "main", "", true)
            .await
            .unwrap();
        let thread = repo::create_thread(&db, workspace.id, "issue", "feature", "codex")
            .await
            .unwrap();
        let direction = repo::create_direction(
            &db,
            thread.id,
            "implementation",
            "codex",
            repo_ref.id,
            "why",
            "impl-only",
            "",
        )
        .await
        .unwrap();

        repo::mark_thread_deleting(&db, thread.id).await.unwrap();
        let keys = thread_engine_keys(&db, thread.id).await.unwrap();
        assert_eq!(
            keys,
            std::collections::BTreeSet::from([crate::lead_chat::commands::lead_key(thread.id)])
        );
        let error = repo::create_session(&db, direction.id, repo_ref.id, "codex", "/tmp/api-wt")
            .await
            .unwrap_err();
        assert!(error
            .to_string()
            .contains(&format!("thread {} is being deleted", thread.id)));

        repo::clear_thread_deleting(&db, thread.id).await.unwrap();
        let session = repo::create_session(&db, direction.id, repo_ref.id, "codex", "/tmp/api-wt")
            .await
            .unwrap();
        assert!(!keys.contains(&i64::from(session.id)));
    }

    #[test]
    fn occupied_repo_target_allows_only_real_empty_dirs() {
        let root = std::env::temp_dir().join(format!("weft-target-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();

        // Nonexistent → free to create.
        assert!(reject_occupied_repo_target(&root.join("fresh")).is_ok());
        // A REAL empty directory is a valid target (git init/clone support it).
        let empty = root.join("empty");
        std::fs::create_dir(&empty).unwrap();
        assert!(reject_occupied_repo_target(&empty).is_ok());
        // Non-empty directory → rejected.
        let full = root.join("full");
        std::fs::create_dir(&full).unwrap();
        std::fs::write(full.join("x"), "x").unwrap();
        assert!(reject_occupied_repo_target(&full).is_err());
        // Plain file → rejected.
        let file = root.join("file");
        std::fs::write(&file, "x").unwrap();
        assert!(reject_occupied_repo_target(&file).is_err());
        // Symlink — even to an empty directory — rejected: git would write the
        // checkout through it, outside the chosen destination.
        let link = root.join("link");
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&empty, &link).unwrap();
            assert!(reject_occupied_repo_target(&link).is_err());
            // Dangling symlink too (symlink_metadata sees it; exists() would not).
            let dangling = root.join("dangling");
            std::os::unix::fs::symlink(root.join("nowhere"), &dangling).unwrap();
            assert!(reject_occupied_repo_target(&dangling).is_err());
        }

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn thread_engine_keys_cover_lead_and_worker_sessions() {
        let db = Db::connect("sqlite::memory:").await.unwrap();
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        let repo_ref = repo::add_repo_ref(&db, ws.id, "web", "/tmp/web", "main", "", true)
            .await
            .unwrap();
        let thread = repo::create_thread(&db, ws.id, "target", "feature", "claude")
            .await
            .unwrap();
        let other = repo::create_thread(&db, ws.id, "other", "feature", "claude")
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
        let other_direction = repo::create_direction(
            &db,
            other.id,
            "other task",
            "claude",
            repo_ref.id,
            "change",
            "plan+impl",
            "",
        )
        .await
        .unwrap();
        let worker = repo::create_session(&db, direction.id, repo_ref.id, "claude", "/tmp/wt")
            .await
            .unwrap();
        let other_worker =
            repo::create_session(&db, other_direction.id, repo_ref.id, "claude", "/tmp/other")
                .await
                .unwrap();

        // A thread's keys cover BOTH the lead (-thread_id) AND its worker sessions —
        // the worker session keys are exactly what delete_thread previously failed to
        // stop (it removed only the lead), leaking worker children.
        let keys = thread_engine_keys(&db, thread.id).await.unwrap();
        let expected = std::collections::BTreeSet::from([
            crate::lead_chat::commands::lead_key(thread.id),
            worker.id as i64,
        ]);
        assert_eq!(keys, expected);
        // A sibling thread's lead and workers must not be swept in.
        assert!(!keys.contains(&crate::lead_chat::commands::lead_key(other.id)));
        assert!(!keys.contains(&(other_worker.id as i64)));
    }

    #[tokio::test]
    async fn workspace_permission_effects_preserve_a_secondary_session_direction() {
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
        repo::create_session(&db, keep_direction.id, repo_ref.id, "claude", "/tmp/orphan")
            .await
            .unwrap();
        let asks = crate::ask::AskRegistry::new();
        let (remove_id, remove_rx) = asks.request(
            thread.id,
            "",
            "claude",
            "Run: rm",
            "rm -rf tmp",
            crate::ask::RiskLevel::Unknown,
            "rm -rf tmp",
        );
        let (repo_scoped_id, _repo_scoped_rx) = asks.request(
            keep_thread.id,
            &keep_direction.id.to_string(),
            "claude",
            "Run: clean",
            "rm -rf tmp",
            crate::ask::RiskLevel::Unknown,
            "rm -rf tmp",
        );
        let (keep_id, _keep_rx) = asks.request(
            keep_thread.id,
            "20",
            "claude",
            "Run: test",
            "pnpm test",
            crate::ask::RiskLevel::Unknown,
            "pnpm test",
        );

        purge_committed_permission_effects(&asks, &[thread.id], &[])
            .await
            .unwrap();

        assert!(remove_rx.await.is_err());
        assert_eq!(
            asks.open().iter().map(|ask| ask.id).collect::<Vec<_>>(),
            vec![repo_scoped_id, keep_id]
        );
        assert!(!asks.open().iter().any(|ask| ask.id == remove_id));
        assert!(asks.open().iter().any(|ask| ask.id == repo_scoped_id));
    }

    #[tokio::test]
    async fn cancel_workspace_asks_revokes_only_that_workspaces_standing_grants() {
        let db = Db::connect("sqlite::memory:").await.unwrap();
        let ws = repo::create_workspace(&db, "delete me").await.unwrap();
        let keep_ws = repo::create_workspace(&db, "keep me").await.unwrap();
        let thread = repo::create_thread(&db, ws.id, "remove", "feature", "claude")
            .await
            .unwrap();
        let keep_thread = repo::create_thread(&db, keep_ws.id, "keep", "feature", "claude")
            .await
            .unwrap();
        let asks = crate::ask::AskRegistry::new();
        asks.seed_grants(crate::ask::GrantSnapshot {
            full: vec![
                crate::ask::FullGrant {
                    thread: thread.id,
                    dir: "".into(),
                },
                crate::ask::FullGrant {
                    thread: keep_thread.id,
                    dir: "".into(),
                },
            ],
            always: vec![],
        });

        purge_committed_permission_effects(&asks, &[thread.id], &[])
            .await
            .unwrap();

        // the deleted workspace's grant is gone; the other workspace's survives
        assert!(asks
            .auto_decision(thread.id, "", crate::ask::RiskLevel::Unknown, "anything")
            .is_none());
        assert_eq!(
            asks.auto_decision(
                keep_thread.id,
                "",
                crate::ask::RiskLevel::Unknown,
                "anything"
            ),
            Some(crate::ask::Decision::Allow)
        );
    }

    #[tokio::test]
    async fn workspace_permission_effects_preserve_secondary_session_grants() {
        let db = Db::connect("sqlite::memory:").await.unwrap();
        let ws = repo::create_workspace(&db, "delete me").await.unwrap();
        let keep_ws = repo::create_workspace(&db, "keep me").await.unwrap();
        // ws owns a repo; a direction in keep_ws's thread has a SESSION using ws's
        // repo → it is a repo-routed direction in ws's ask scope.
        let ws_repo = repo::add_repo_ref(&db, ws.id, "web", "/tmp/web", "main", "", true)
            .await
            .unwrap();
        let keep_repo = repo::add_repo_ref(&db, keep_ws.id, "api", "/tmp/api", "main", "", true)
            .await
            .unwrap();
        let keep_thread = repo::create_thread(&db, keep_ws.id, "keep", "feature", "claude")
            .await
            .unwrap();
        let routed = repo::create_direction(
            &db,
            keep_thread.id,
            "routed",
            "claude",
            keep_repo.id,
            "why",
            "plan+impl",
            "",
        )
        .await
        .unwrap();
        repo::create_session(&db, routed.id, ws_repo.id, "claude", "/tmp/wt")
            .await
            .unwrap();
        let asks = crate::ask::AskRegistry::new();
        asks.seed_grants(crate::ask::GrantSnapshot {
            full: vec![crate::ask::FullGrant {
                thread: keep_thread.id,
                dir: routed.id.to_string(),
            }],
            always: vec![],
        });
        // active before the delete
        assert_eq!(
            asks.auto_decision(
                keep_thread.id,
                &routed.id.to_string(),
                crate::ask::RiskLevel::Unknown,
                "x"
            ),
            Some(crate::ask::Decision::Allow)
        );

        purge_committed_permission_effects(&asks, &[], &[])
            .await
            .unwrap();

        // The direction survives; deleting only its secondary session must not
        // revoke the permission footprint shared by its primary session.
        assert_eq!(
            asks.auto_decision(
                keep_thread.id,
                &routed.id.to_string(),
                crate::ask::RiskLevel::Unknown,
                "x"
            ),
            Some(crate::ask::Decision::Allow)
        );
    }

    #[tokio::test]
    async fn repo_permission_purge_preserves_a_surviving_secondary_session_direction() {
        let db = Db::connect("sqlite::memory:").await.unwrap();
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        let repo_a = repo::add_repo_ref(&db, ws.id, "a", "/tmp/a", "main", "", true)
            .await
            .unwrap();
        let repo_b = repo::add_repo_ref(&db, ws.id, "b", "/tmp/b", "main", "", true)
            .await
            .unwrap();
        let thread = repo::create_thread(&db, ws.id, "t", "feature", "claude")
            .await
            .unwrap();
        // dir_a is BOUND to repo_a (the deleted repo).
        let dir_a = repo::create_direction(
            &db,
            thread.id,
            "a",
            "claude",
            repo_a.id,
            "why",
            "plan+impl",
            "",
        )
        .await
        .unwrap();
        // dir_routed is bound to repo_b but has a SESSION using repo_a → repo-routed.
        let dir_routed = repo::create_direction(
            &db,
            thread.id,
            "routed",
            "claude",
            repo_b.id,
            "why",
            "plan+impl",
            "",
        )
        .await
        .unwrap();
        repo::create_session(&db, dir_routed.id, repo_a.id, "claude", "/tmp/wt")
            .await
            .unwrap();
        // dir_b is bound to repo_b with no repo_a session → survives.
        let dir_b = repo::create_direction(
            &db,
            thread.id,
            "b",
            "claude",
            repo_b.id,
            "why",
            "plan+impl",
            "",
        )
        .await
        .unwrap();
        let asks = crate::ask::AskRegistry::new();
        asks.seed_grants(crate::ask::GrantSnapshot {
            full: vec![
                crate::ask::FullGrant {
                    thread: thread.id,
                    dir: dir_a.id.to_string(),
                },
                crate::ask::FullGrant {
                    thread: thread.id,
                    dir: dir_routed.id.to_string(),
                },
                crate::ask::FullGrant {
                    thread: thread.id,
                    dir: dir_b.id.to_string(),
                },
            ],
            always: vec![],
        });
        // Both directions currently have asks, but only dir_a is actually
        // removed when repo_a is deleted.
        let (ask_a, _r1) = asks.request(
            thread.id,
            &dir_a.id.to_string(),
            "codex",
            "Run: x",
            "x",
            crate::ask::RiskLevel::Unknown,
            "x",
        );
        let (ask_routed, _r2) = asks.request(
            thread.id,
            &dir_routed.id.to_string(),
            "codex",
            "Run: y",
            "y",
            crate::ask::RiskLevel::Unknown,
            "y",
        );

        purge_committed_permission_effects(&asks, &[], &[(thread.id, dir_a.id)])
            .await
            .unwrap();

        // The bound direction is purged. A direction with only one secondary
        // repo_a session survives with its other sessions, ask, and grant.
        assert!(asks
            .auto_decision(
                thread.id,
                &dir_a.id.to_string(),
                crate::ask::RiskLevel::Unknown,
                "x"
            )
            .is_none());
        assert_eq!(
            asks.auto_decision(
                thread.id,
                &dir_routed.id.to_string(),
                crate::ask::RiskLevel::Unknown,
                "x"
            ),
            Some(crate::ask::Decision::Allow)
        );
        assert_eq!(
            asks.auto_decision(
                thread.id,
                &dir_b.id.to_string(),
                crate::ask::RiskLevel::Unknown,
                "x"
            ),
            Some(crate::ask::Decision::Allow)
        );
        // Only the removed direction's ask is cancelled.
        let open: Vec<u64> = asks.open().iter().map(|a| a.id).collect();
        assert!(!open.contains(&ask_a));
        assert!(open.contains(&ask_routed));
    }

    #[tokio::test]
    async fn repo_delete_cancels_only_questions_from_removed_secondary_sessions() {
        let db = Db::connect("sqlite::memory:").await.unwrap();
        let workspace = repo::create_workspace(&db, "ws").await.unwrap();
        let primary_repo = repo::add_repo_ref(
            &db,
            workspace.id,
            "primary",
            "/tmp/primary",
            "main",
            "",
            true,
        )
        .await
        .unwrap();
        let secondary_repo = repo::add_repo_ref(
            &db,
            workspace.id,
            "secondary",
            "/tmp/secondary",
            "main",
            "",
            true,
        )
        .await
        .unwrap();
        let thread = repo::create_thread(&db, workspace.id, "issue", "feature", "codex")
            .await
            .unwrap();
        let direction = repo::create_direction(
            &db,
            thread.id,
            "implementation",
            "codex",
            primary_repo.id,
            "why",
            "plan+impl",
            "",
        )
        .await
        .unwrap();
        let primary_session = repo::create_session(
            &db,
            direction.id,
            primary_repo.id,
            "codex",
            "/tmp/primary-wt",
        )
        .await
        .unwrap();
        let secondary_session = repo::create_session(
            &db,
            direction.id,
            secondary_repo.id,
            "codex",
            "/tmp/secondary-wt",
        )
        .await
        .unwrap();
        let keep = repo::create_human_request(
            &db,
            workspace.id,
            thread.id,
            &direction.id.to_string(),
            direction.id,
            1,
            0,
            primary_session.id,
            "Keep working in primary?",
        )
        .await
        .unwrap();
        let cancel = repo::create_human_request(
            &db,
            workspace.id,
            thread.id,
            &direction.id.to_string(),
            direction.id,
            1,
            0,
            secondary_session.id,
            "Keep working in secondary?",
        )
        .await
        .unwrap();
        let unattributed = repo::create_human_request(
            &db,
            workspace.id,
            thread.id,
            &direction.id.to_string(),
            direction.id,
            1,
            0,
            0,
            "Which session asked this?",
        )
        .await
        .unwrap();
        let bus = crate::bus::BusRegistry::new();
        let (events, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
        assert!(bus.set_ask_notifier(events).is_empty());
        for request in [&keep, &cancel, &unattributed] {
            assert!(bus.restore_human_request(
                thread.id,
                &direction.id.to_string(),
                &request.question,
                u64::try_from(request.id).unwrap(),
            ));
        }
        while event_rx.try_recv().is_ok() {}

        let effects = repo::delete_repo_cascade_with_human_cancellations(&db, secondary_repo.id)
            .await
            .unwrap();
        apply_committed_bus_delete_effects(
            &bus,
            &effects.cancelled_requests,
            &[],
            &effects.removed_directions,
            &std::collections::BTreeMap::new(),
        );
        assert!(
            effects.removed_directions.is_empty(),
            "the direction is bound to the primary repo and survives"
        );
        assert_eq!(
            repo::get_human_request(&db, keep.id)
                .await
                .unwrap()
                .unwrap()
                .status,
            repo::HUMAN_REQUEST_OPEN
        );
        assert_eq!(
            repo::get_human_request(&db, cancel.id)
                .await
                .unwrap()
                .unwrap()
                .status,
            repo::HUMAN_REQUEST_CANCELLED
        );
        assert_eq!(
            repo::get_human_request(&db, unattributed.id)
                .await
                .unwrap()
                .unwrap()
                .status,
            repo::HUMAN_REQUEST_OPEN,
            "source_session_id=0 is not safely attributable to the deleted session"
        );
        assert_eq!(
            bus.open_asks(thread.id)
                .into_iter()
                .map(|ask| ask.id)
                .collect::<Vec<_>>(),
            vec![
                u64::try_from(keep.id).unwrap(),
                u64::try_from(unattributed.id).unwrap(),
            ]
        );
        assert!(matches!(
            event_rx.try_recv().unwrap(),
            crate::bus::state::HumanAskEvent::Cancelled { ask_id, .. }
                if ask_id == u64::try_from(cancel.id).unwrap()
        ));

        assert!(repo::get_direction(&db, direction.id)
            .await
            .unwrap()
            .is_some());
        assert!(repo::get_session(&db, primary_session.id)
            .await
            .unwrap()
            .is_some());
        assert!(repo::get_session(&db, secondary_session.id)
            .await
            .unwrap()
            .is_none());
        assert_eq!(
            repo::get_human_request(&db, keep.id)
                .await
                .unwrap()
                .unwrap()
                .status,
            repo::HUMAN_REQUEST_OPEN
        );
    }

    #[tokio::test]
    async fn revoke_grant_durable_rolls_back_only_this_calls_removal_on_write_failure() {
        let asks = crate::ask::AskRegistry::new();
        // a writer that FAILS every ack'd write
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<crate::ask::PersistMsg>();
        asks.set_persist_notifier(tx);
        tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                if let Some(ack) = msg.ack {
                    let _ = ack.send(Err("boom".into()));
                }
            }
        });
        asks.seed_grants(crate::ask::GrantSnapshot {
            full: vec![
                crate::ask::FullGrant {
                    thread: 7,
                    dir: "42".into(),
                },
                // an UNRELATED grant that must be left exactly as-is by the rollback
                crate::ask::FullGrant {
                    thread: 8,
                    dir: "99".into(),
                },
            ],
            // Always is durable now (issue #89) — the rollback must restore it
            // too, not just Full. `removed.always` is no longer unconditionally
            // empty (see ask::tests::revoke_returns_exactly_what_it_removed), so
            // a task-level revoke's rollback must cover both.
            always: vec![crate::ask::AlwaysGrant {
                thread: 7,
                dir: "42".into(),
                action_key: "npm test".into(),
            }],
        });

        let r = revoke_grant_durable(&asks, 7, Some("42"), None).await;

        assert!(
            r.is_err(),
            "a failed durable write must surface as an error"
        );
        // the revoked Full grant is restored (memory matches the unchanged store)...
        assert_eq!(
            asks.auto_decision(7, "42", crate::ask::RiskLevel::Unknown, "x"),
            Some(crate::ask::Decision::Allow),
            "the Full grant must be restored on a failed write"
        );
        // ...and so is the revoked Always rule specifically (not just masked by
        // the restored Full grant above — check it directly via the snapshot).
        assert_eq!(
            asks.snapshot_grants().always,
            vec![crate::ask::AlwaysGrant {
                thread: 7,
                dir: "42".into(),
                action_key: "npm test".into(),
            }],
            "the Always grant must be restored on a failed write, not just Full"
        );
        // ...and the unrelated grant is untouched (no blind whole-set re-seed).
        assert_eq!(
            asks.auto_decision(8, "99", crate::ask::RiskLevel::Unknown, "x"),
            Some(crate::ask::Decision::Allow)
        );
    }

    #[tokio::test]
    async fn revoke_grant_durable_with_nothing_to_remove_skips_the_write() {
        let asks = crate::ask::AskRegistry::new();
        // a writer that FAILS every ack'd write — so if the revoke tried to flush,
        // it would surface an error. It must not, because nothing was removed.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<crate::ask::PersistMsg>();
        asks.set_persist_notifier(tx);
        tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                if let Some(ack) = msg.ack {
                    let _ = ack.send(Err("boom".into()));
                }
            }
        });

        // No grant for (7, "42") exists, so nothing is removed.
        let r = revoke_grant_durable(&asks, 7, Some("42"), None).await;

        assert!(
            r.is_ok(),
            "revoking a grant that isn't there must skip the write, not surface a spurious failure"
        );
    }

    #[tokio::test]
    async fn workspace_bus_effects_preserve_legacy_asks_on_surviving_directions() {
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
        repo::create_session(&db, keep_direction.id, repo_ref.id, "claude", "/tmp/orphan")
            .await
            .unwrap();
        let bus = crate::bus::BusRegistry::new();
        let remove_id = bus.ask_human(thread.id, "lead", "delete?");
        let repo_scoped_id = bus.ask_human(
            keep_thread.id,
            &keep_direction.id.to_string(),
            "delete repo?",
        );
        let keep_id = bus.ask_human(keep_thread.id, "lead", "keep?");

        repo::mark_workspace_deleting(&db, ws.id).await.unwrap();
        let scope = workspace_ask_scope(&db, ws.id).await.unwrap();
        let mut closing_asks = std::collections::BTreeMap::new();
        for thread_id in &scope.thread_ids {
            let (_, ask_ids) = bus.begin_thread_close(*thread_id);
            closing_asks.insert(*thread_id, ask_ids);
        }
        let effects = repo::delete_workspace_cascade_with_human_cancellations(&db, ws.id)
            .await
            .unwrap();
        apply_committed_bus_delete_effects(
            &bus,
            &effects.cancelled_requests,
            &effects.removed_threads,
            &effects.removed_directions,
            &closing_asks,
        );

        assert!(bus.open_asks(thread.id).is_empty());
        assert_eq!(
            bus.open_asks(keep_thread.id)
                .into_iter()
                .map(|ask| ask.id)
                .collect::<Vec<_>>(),
            vec![repo_scoped_id, keep_id]
        );
        assert_ne!(remove_id, keep_id);
        assert_ne!(repo_scoped_id, keep_id);
    }

    #[tokio::test]
    async fn failed_repo_delete_rolls_back_durable_and_retains_process_local_state() {
        use sea_orm::{ConnectionTrait, Statement};

        let db = Db::connect("sqlite::memory:").await.unwrap();
        let workspace = repo::create_workspace(&db, "ws").await.unwrap();
        let deleted_repo = repo::add_repo_ref(
            &db,
            workspace.id,
            "secondary",
            "/tmp/secondary",
            "main",
            "",
            true,
        )
        .await
        .unwrap();
        let primary_repo = repo::add_repo_ref(
            &db,
            workspace.id,
            "primary",
            "/tmp/primary",
            "main",
            "",
            true,
        )
        .await
        .unwrap();
        let thread = repo::create_thread(&db, workspace.id, "issue", "feature", "codex")
            .await
            .unwrap();
        let direction = repo::create_direction(
            &db,
            thread.id,
            "implementation",
            "codex",
            primary_repo.id,
            "why",
            "plan+impl",
            "",
        )
        .await
        .unwrap();
        let session = repo::create_session(
            &db,
            direction.id,
            deleted_repo.id,
            "codex",
            "/tmp/secondary-wt",
        )
        .await
        .unwrap();
        let request = repo::create_human_request(
            &db,
            workspace.id,
            thread.id,
            &direction.id.to_string(),
            direction.id,
            1,
            0,
            session.id,
            "Continue in secondary?",
        )
        .await
        .unwrap();
        let route = repo::HumanRequestImRoute {
            channel: "feishu".to_string(),
            account: "cli_test".to_string(),
            owner: "ou_owner".to_string(),
            message_id: "om_repo_rollback".to_string(),
            terminal_revision: 0,
        };
        repo::record_human_request_im_route(&db, request.id, &route)
            .await
            .unwrap();

        let bus = crate::bus::BusRegistry::new();
        let (events, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
        assert!(bus.set_ask_notifier(events).is_empty());
        assert!(bus.restore_human_request(
            thread.id,
            &direction.id.to_string(),
            &request.question,
            u64::try_from(request.id).unwrap(),
        ));
        let asks = crate::ask::AskRegistry::new();
        asks.seed_grants(crate::ask::GrantSnapshot {
            full: vec![crate::ask::FullGrant {
                thread: thread.id,
                dir: direction.id.to_string(),
            }],
            always: vec![],
        });
        let (permission_id, _permission_rx) = asks.request(
            thread.id,
            &direction.id.to_string(),
            "codex",
            "Run: test",
            "pnpm test",
            crate::ask::RiskLevel::Unknown,
            "pnpm test",
        );
        db.0.execute(Statement::from_string(
            sea_orm::DatabaseBackend::Sqlite,
            format!(
                "CREATE TRIGGER fail_target_repo_delete BEFORE DELETE ON repo_ref \
                     WHEN OLD.id = {} BEGIN SELECT RAISE(ABORT, 'forced repo delete failure'); END",
                deleted_repo.id
            ),
        ))
        .await
        .unwrap();

        assert!(
            repo::delete_repo_cascade_with_human_cancellations(&db, deleted_repo.id)
                .await
                .is_err()
        );

        assert!(repo::get_repo(&db, deleted_repo.id)
            .await
            .unwrap()
            .is_some());
        assert!(repo::get_session(&db, session.id).await.unwrap().is_some());
        let persisted = repo::get_human_request(&db, request.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(persisted.status, repo::HUMAN_REQUEST_OPEN);
        assert_eq!(persisted.revision, request.revision);
        assert!(repo::get_human_card_terminal_outbox_for_route(&db, &route)
            .await
            .unwrap()
            .is_none());
        assert_eq!(
            bus.open_asks(thread.id)
                .into_iter()
                .map(|ask| ask.id)
                .collect::<Vec<_>>(),
            vec![u64::try_from(request.id).unwrap()]
        );
        assert!(event_rx.try_recv().is_err());
        assert!(asks.open().iter().any(|ask| ask.id == permission_id));
        assert_eq!(
            asks.auto_decision(
                thread.id,
                &direction.id.to_string(),
                crate::ask::RiskLevel::Unknown,
                "x",
            ),
            Some(crate::ask::Decision::Allow)
        );
    }

    #[tokio::test]
    async fn repo_deleting_fence_freezes_direction_and_session_scope() {
        let db = Db::connect("sqlite::memory:").await.unwrap();
        let workspace = repo::create_workspace(&db, "ws").await.unwrap();
        let target_repo =
            repo::add_repo_ref(&db, workspace.id, "target", "/tmp/target", "main", "", true)
                .await
                .unwrap();
        let primary_repo = repo::add_repo_ref(
            &db,
            workspace.id,
            "primary",
            "/tmp/primary",
            "main",
            "",
            true,
        )
        .await
        .unwrap();
        let thread = repo::create_thread(&db, workspace.id, "issue", "feature", "codex")
            .await
            .unwrap();
        let surviving_direction = repo::create_direction(
            &db,
            thread.id,
            "implementation",
            "codex",
            primary_repo.id,
            "why",
            "plan+impl",
            "",
        )
        .await
        .unwrap();

        repo::mark_repo_deleting(&db, target_repo.id).await.unwrap();
        let session_error = repo::create_session(
            &db,
            surviving_direction.id,
            target_repo.id,
            "codex",
            "/tmp/target-wt",
        )
        .await
        .unwrap_err();
        assert!(session_error
            .to_string()
            .contains(&format!("repo {} is being deleted", target_repo.id)));
        let direction_error = repo::create_direction(
            &db,
            thread.id,
            "late",
            "codex",
            target_repo.id,
            "why",
            "plan+impl",
            "",
        )
        .await
        .unwrap_err();
        assert!(direction_error
            .to_string()
            .contains("direction_write_fenced_or_stale"));
        repo::clear_repo_deleting(&db, target_repo.id)
            .await
            .unwrap();
        assert!(repo::create_session(
            &db,
            surviving_direction.id,
            target_repo.id,
            "codex",
            "/tmp/target-wt",
        )
        .await
        .is_ok());
    }

    #[tokio::test]
    async fn failed_workspace_delete_rolls_back_owned_and_secondary_questions() {
        use sea_orm::{ConnectionTrait, Statement};

        let db = Db::connect("sqlite::memory:").await.unwrap();
        let workspace = repo::create_workspace(&db, "delete me").await.unwrap();
        let keep_workspace = repo::create_workspace(&db, "keep me").await.unwrap();
        let workspace_repo =
            repo::add_repo_ref(&db, workspace.id, "web", "/tmp/web", "main", "", true)
                .await
                .unwrap();
        let keep_repo =
            repo::add_repo_ref(&db, keep_workspace.id, "api", "/tmp/api", "main", "", true)
                .await
                .unwrap();
        let owned_thread = repo::create_thread(&db, workspace.id, "owned", "feature", "codex")
            .await
            .unwrap();
        let external_thread =
            repo::create_thread(&db, keep_workspace.id, "external", "feature", "codex")
                .await
                .unwrap();
        let external_direction = repo::create_direction(
            &db,
            external_thread.id,
            "implementation",
            "codex",
            keep_repo.id,
            "why",
            "plan+impl",
            "",
        )
        .await
        .unwrap();
        let secondary_session = repo::create_session(
            &db,
            external_direction.id,
            workspace_repo.id,
            "codex",
            "/tmp/web-wt",
        )
        .await
        .unwrap();
        let owned_request = repo::create_human_request(
            &db,
            workspace.id,
            owned_thread.id,
            "lead",
            0,
            1,
            0,
            0,
            "Delete this workspace?",
        )
        .await
        .unwrap();
        let secondary_request = repo::create_human_request(
            &db,
            keep_workspace.id,
            external_thread.id,
            &external_direction.id.to_string(),
            external_direction.id,
            1,
            0,
            secondary_session.id,
            "Continue in web?",
        )
        .await
        .unwrap();
        let route = repo::HumanRequestImRoute {
            channel: "feishu".to_string(),
            account: "cli_test".to_string(),
            owner: "ou_owner".to_string(),
            message_id: "om_workspace_rollback".to_string(),
            terminal_revision: 0,
        };
        repo::record_human_request_im_route(&db, secondary_request.id, &route)
            .await
            .unwrap();
        let bus = crate::bus::BusRegistry::new();
        let (events, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
        assert!(bus.set_ask_notifier(events).is_empty());
        for request in [&owned_request, &secondary_request] {
            assert!(bus.restore_human_request(
                request.thread_id,
                &request.direction_scope,
                &request.question,
                u64::try_from(request.id).unwrap(),
            ));
        }
        repo::mark_workspace_deleting(&db, workspace.id)
            .await
            .unwrap();
        let (_, owned_ask_ids) = bus.begin_thread_close(owned_thread.id);
        db.0
            .execute(Statement::from_string(
                sea_orm::DatabaseBackend::Sqlite,
                format!(
                    "CREATE TRIGGER fail_target_workspace_delete BEFORE DELETE ON workspace \
                     WHEN OLD.id = {} BEGIN SELECT RAISE(ABORT, 'forced workspace delete failure'); END",
                    workspace.id
                ),
            ))
            .await
            .unwrap();

        assert!(
            repo::delete_workspace_cascade_with_human_cancellations(&db, workspace.id)
                .await
                .is_err()
        );
        bus.rollback_thread_close(owned_thread.id);

        assert!(repo::list_workspaces(&db)
            .await
            .unwrap()
            .iter()
            .any(|row| row.id == workspace.id));
        assert!(repo::get_session(&db, secondary_session.id)
            .await
            .unwrap()
            .is_some());
        for request in [&owned_request, &secondary_request] {
            let persisted = repo::get_human_request(&db, request.id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(persisted.status, repo::HUMAN_REQUEST_OPEN);
            assert_eq!(persisted.revision, request.revision);
        }
        assert!(repo::get_human_card_terminal_outbox_for_route(&db, &route)
            .await
            .unwrap()
            .is_none());
        assert_eq!(
            bus.open_asks(owned_thread.id)
                .into_iter()
                .map(|ask| ask.id)
                .collect::<Vec<_>>(),
            owned_ask_ids
        );
        assert_eq!(
            bus.open_asks(external_thread.id)
                .into_iter()
                .map(|ask| ask.id)
                .collect::<Vec<_>>(),
            vec![u64::try_from(secondary_request.id).unwrap()]
        );
        assert!(event_rx.try_recv().is_err());
        repo::clear_workspace_deleting(&db, workspace.id)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn successful_thread_delete_waits_for_lifecycle_then_purges_hook_first_permission() {
        let db = Db::connect("sqlite::memory:").await.unwrap();
        let workspace = repo::create_workspace(&db, "delete permission").await.unwrap();
        let repo_ref = repo::add_repo_ref(
            &db,
            workspace.id,
            "repo",
            "/tmp/delete-permission",
            "main",
            "",
            true,
        )
        .await
        .unwrap();
        let thread = repo::create_thread(
            &db,
            workspace.id,
            "delete permission",
            "feature",
            "codex",
        )
        .await
        .unwrap();
        let direction = repo::create_direction(
            &db,
            thread.id,
            "worker",
            "codex",
            repo_ref.id,
            "why",
            "impl-only",
            "",
        )
        .await
        .unwrap();
        let scope = direction.id.to_string();
        let asks = crate::ask::AskRegistry::new();
        asks.seed_grants(crate::ask::GrantSnapshot {
            full: vec![crate::ask::FullGrant {
                thread: thread.id,
                dir: scope.clone(),
            }],
            always: vec![],
        });
        asks.grant_read_only_session(thread.id, &scope);
        asks.grant_read_only_issue(thread.id);
        let bus = crate::bus::BusRegistry::new();
        // Model the exact hook-first seam: handle_ask owns lifecycle, then
        // registers in AskRegistry before it drops that guard and starts its
        // one-hour wait.
        let held = bus.thread_lifecycle_gate(thread.id).lock_owned().await;
        let (ask_id, receiver) = asks.request(
            thread.id,
            &scope,
            "codex",
            "Run: touch late",
            "touch late",
            crate::ask::RiskLevel::Write,
            "touch late",
        );
        repo::mark_thread_deleting(&db, thread.id).await.unwrap();

        let delete_db = db.clone();
        let delete_bus = bus.clone();
        let delete_asks = asks.clone();
        let thread_id = thread.id;
        let mut delete_task = tokio::spawn(async move {
            delete_thread_cascade_after_bus_fence(
                &delete_db,
                &delete_bus,
                &delete_asks,
                thread_id,
                Vec::new(),
            )
            .await
        });
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), &mut delete_task)
                .await
                .is_err(),
            "delete must wait for the ask registration lifecycle owner"
        );
        assert_eq!(asks.open_in(thread.id).len(), 1);
        drop(held);

        delete_task.await.unwrap().unwrap();
        assert!(repo::get_thread(&db, thread.id).await.unwrap().is_none());
        assert!(asks.open_in(thread.id).is_empty());
        assert!(asks.snapshot_grants().is_empty());
        assert_eq!(
            asks.read_only_grants(),
            crate::ask::ReadOnlyGrants::default()
        );
        assert!(!asks.answer(ask_id, crate::ask::Answer::Full));
        assert!(receiver.await.is_err());
    }

    #[tokio::test]
    async fn failed_thread_delete_reopens_bus_and_rolls_back_question_cancellation() {
        use sea_orm::{ConnectionTrait, Statement};

        let db = Db::connect("sqlite::memory:").await.unwrap();
        let workspace = repo::create_workspace(&db, "keep after failure")
            .await
            .unwrap();
        let thread = repo::create_thread(&db, workspace.id, "still here", "feature", "codex")
            .await
            .unwrap();
        let request = repo::create_human_request(
            &db,
            workspace.id,
            thread.id,
            "lead",
            0,
            1,
            0,
            0,
            "Continue?",
        )
        .await
        .unwrap();
        let route = repo::HumanRequestImRoute {
            channel: "feishu".to_string(),
            account: "cli_test".to_string(),
            owner: "ou_owner".to_string(),
            message_id: "om_failed_delete".to_string(),
            terminal_revision: 0,
        };
        repo::record_human_request_im_route(&db, request.id, &route)
            .await
            .unwrap();

        let bus = crate::bus::BusRegistry::new();
        let (events, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
        assert!(bus.set_ask_notifier(events).is_empty());
        assert!(bus.restore_human_request(
            thread.id,
            "lead",
            &request.question,
            u64::try_from(request.id).unwrap(),
        ));
        db.0.execute(Statement::from_string(
            sea_orm::DatabaseBackend::Sqlite,
            format!(
                "CREATE TRIGGER fail_target_thread_delete BEFORE DELETE ON thread \
                     WHEN OLD.id = {} BEGIN SELECT RAISE(ABORT, 'forced delete failure'); END",
                thread.id
            ),
        ))
        .await
        .unwrap();

        assert!(
            delete_thread_cascade_after_bus_fence(
                &db,
                &bus,
                &crate::ask::AskRegistry::new(),
                thread.id,
                Vec::new(),
            )
            .await
            .is_err()
        );

        let persisted = repo::get_human_request(&db, request.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(persisted.status, repo::HUMAN_REQUEST_OPEN);
        assert_eq!(persisted.revision, request.revision);
        assert!(repo::get_human_card_terminal_outbox_for_route(&db, &route)
            .await
            .unwrap()
            .is_none());
        assert_eq!(
            bus.open_asks(thread.id)
                .into_iter()
                .map(|ask| ask.id)
                .collect::<Vec<_>>(),
            vec![u64::try_from(request.id).unwrap()]
        );
        assert!(
            event_rx.try_recv().is_err(),
            "a rolled-back delete emits no cancellation"
        );
        assert!(bus.answer_ask(thread.id, u64::try_from(request.id).unwrap(), "Yes",));
        assert!(matches!(
            event_rx.try_recv().unwrap(),
            crate::bus::state::HumanAskEvent::Answered { ask_id, .. }
                if ask_id == u64::try_from(request.id).unwrap()
        ));
    }

    #[tokio::test]
    async fn failed_thread_delete_preserves_durable_state_and_retry_commits_once() {
        use sea_orm::{ConnectionTrait, Statement};

        let db = Db::connect("sqlite::memory:").await.unwrap();
        let workspace = repo::create_workspace(&db, "retry delete").await.unwrap();
        let thread = repo::create_thread(&db, workspace.id, "retry me", "feature", "codex")
            .await
            .unwrap();
        let open_request = repo::create_human_request(
            &db,
            workspace.id,
            thread.id,
            "lead",
            0,
            1,
            0,
            0,
            "Open question",
        )
        .await
        .unwrap();
        let answered_request = repo::create_human_request(
            &db,
            workspace.id,
            thread.id,
            "lead",
            0,
            2,
            0,
            0,
            "Answered question",
        )
        .await
        .unwrap();
        let answered_request = repo::answer_human_request(
            &db,
            workspace.id,
            answered_request.id,
            answered_request.revision,
            "keep this answer",
        )
        .await
        .unwrap()
        .unwrap();
        let open_route = repo::HumanRequestImRoute {
            channel: "feishu".to_string(),
            account: "cli_test".to_string(),
            owner: "ou_owner".to_string(),
            message_id: "om_retry_open".to_string(),
            terminal_revision: 0,
        };
        let answered_route = repo::HumanRequestImRoute {
            channel: "feishu".to_string(),
            account: "cli_test".to_string(),
            owner: "ou_owner".to_string(),
            message_id: "om_retry_answered".to_string(),
            terminal_revision: 0,
        };
        repo::record_human_request_im_route(&db, open_request.id, &open_route)
            .await
            .unwrap();
        repo::record_human_request_im_route(&db, answered_request.id, &answered_route)
            .await
            .unwrap();

        let bus = crate::bus::BusRegistry::new();
        let (events, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
        assert!(bus.set_ask_notifier(events).is_empty());
        assert!(bus.restore_human_request(
            thread.id,
            "lead",
            &open_request.question,
            u64::try_from(open_request.id).unwrap(),
        ));
        assert!(bus.restore_durable_answer(
            thread.id,
            u64::try_from(answered_request.id).unwrap(),
            "lead",
            &answered_request.answer,
        ));
        bus.post(thread.id, "system", "lead", "ordinary bus history", "message");
        let baseline_log = bus.log(thread.id);
        let baseline_inbox = bus.inbox(thread.id, "lead");
        assert!(baseline_inbox.iter().any(|message| {
            message.request_id == Some(u64::try_from(answered_request.id).unwrap())
        }));
        assert!(baseline_inbox.iter().any(|message| message.request_id.is_none()));
        bus.restore_inbox(thread.id, "lead", baseline_inbox.clone());

        let open_before = repo::get_human_request(&db, open_request.id)
            .await
            .unwrap()
            .unwrap();
        let answered_before = repo::get_human_request(&db, answered_request.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(open_before.status, repo::HUMAN_REQUEST_OPEN);
        assert_eq!(answered_before.status, repo::HUMAN_REQUEST_ANSWERED);

        db.0.execute(Statement::from_string(
            sea_orm::DatabaseBackend::Sqlite,
            format!(
                "CREATE TRIGGER fail_retry_thread_delete BEFORE DELETE ON thread \
                     WHEN OLD.id = {} BEGIN SELECT RAISE(ABORT, 'forced retry delete failure'); END",
                thread.id
            ),
        ))
        .await
        .unwrap();
        let failed = delete_thread_cascade_after_bus_fence(
            &db,
            &bus,
            &crate::ask::AskRegistry::new(),
            thread.id,
            Vec::new(),
        )
        .await;
        assert!(failed
            .unwrap_err()
            .contains("forced retry delete failure"));

        let open_after_failure = repo::get_human_request(&db, open_request.id)
            .await
            .unwrap()
            .unwrap();
        let answered_after_failure = repo::get_human_request(&db, answered_request.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(open_after_failure, open_before);
        assert_eq!(answered_after_failure, answered_before);
        assert!(repo::get_human_card_terminal_outbox_for_route(&db, &open_route)
            .await
            .unwrap()
            .is_none());
        assert!(repo::get_human_card_terminal_outbox_for_route(&db, &answered_route)
            .await
            .unwrap()
            .is_none());
        assert_eq!(
            bus.open_asks(thread.id)
                .into_iter()
                .map(|ask| ask.id)
                .collect::<Vec<_>>(),
            vec![u64::try_from(open_request.id).unwrap()]
        );
        let inbox_after_failure = bus.inbox(thread.id, "lead");
        assert_eq!(inbox_after_failure, baseline_inbox);
        bus.restore_inbox(thread.id, "lead", inbox_after_failure);
        assert_eq!(bus.log(thread.id), baseline_log);
        assert!(event_rx.try_recv().is_err());

        db.0.execute_unprepared("DROP TRIGGER fail_retry_thread_delete")
            .await
            .unwrap();
        delete_thread_cascade_after_bus_fence(
            &db,
            &bus,
            &crate::ask::AskRegistry::new(),
            thread.id,
            Vec::new(),
        )
        .await
        .unwrap();
        assert!(repo::get_thread(&db, thread.id).await.unwrap().is_none());
        assert!(bus.open_asks(thread.id).is_empty());
        assert!(bus.inbox(thread.id, "lead").is_empty());
        assert!(bus.log(thread.id).is_empty());

        let mut cancelled_ids = Vec::new();
        while let Ok(crate::bus::state::HumanAskEvent::Cancelled { ask_id, .. }) =
            event_rx.try_recv()
        {
            cancelled_ids.push(ask_id);
        }
        cancelled_ids.sort_unstable();
        assert_eq!(
            cancelled_ids,
            vec![
                u64::try_from(open_request.id).unwrap(),
                u64::try_from(answered_request.id).unwrap(),
            ]
        );
        let open_outbox = repo::get_human_card_terminal_outbox_for_route(&db, &open_route)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(open_outbox.terminal_status, repo::HUMAN_REQUEST_CANCELLED);
        assert_eq!(open_outbox.terminal_revision, open_before.revision + 1);
        assert!(!open_outbox.delivered);
        let answered_outbox =
            repo::get_human_card_terminal_outbox_for_route(&db, &answered_route)
                .await
                .unwrap()
                .unwrap();
        assert_eq!(
            answered_outbox.terminal_revision,
            answered_before.revision + 1
        );
        assert!(!answered_outbox.delivered);

        let retry_error = delete_thread_cascade_after_bus_fence(
            &db,
            &bus,
            &crate::ask::AskRegistry::new(),
            thread.id,
            Vec::new(),
        )
        .await
        .unwrap_err();
        assert!(retry_error.contains("thread") && retry_error.contains("not found"));
        assert!(event_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn failed_thread_delete_releases_lifecycle_gate_before_answer_commits() {
        use sea_orm::{ConnectionTrait, Statement};

        let db = Db::connect("sqlite::memory:").await.unwrap();
        let workspace = repo::create_workspace(&db, "keep after failure")
            .await
            .unwrap();
        let thread = repo::create_thread(&db, workspace.id, "still here", "feature", "codex")
            .await
            .unwrap();
        let request = repo::create_human_request(
            &db,
            workspace.id,
            thread.id,
            "lead",
            0,
            1,
            0,
            0,
            "Continue?",
        )
        .await
        .unwrap();
        let bus = crate::bus::BusRegistry::new();
        let (events, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
        assert!(bus.set_ask_notifier(events).is_empty());
        assert!(bus.restore_human_request(
            thread.id,
            "lead",
            &request.question,
            u64::try_from(request.id).unwrap(),
        ));
        db.0.execute(Statement::from_string(
            sea_orm::DatabaseBackend::Sqlite,
            format!(
                "CREATE TRIGGER fail_concurrent_thread_delete BEFORE DELETE ON thread \
                     WHEN OLD.id = {} BEGIN SELECT RAISE(ABORT, 'forced delete failure'); END",
                thread.id
            ),
        ))
        .await
        .unwrap();

        let held_gate = bus.thread_lifecycle_gate(thread.id).lock_owned().await;
        let (delete_started_tx, delete_started_rx) = tokio::sync::oneshot::channel();
        let delete_db = db.clone();
        let delete_bus = bus.clone();
        let delete_asks = crate::ask::AskRegistry::new();
        let thread_id = thread.id;
        let delete_task = tokio::spawn(async move {
            let _ = delete_started_tx.send(());
            delete_thread_cascade_after_bus_fence(
                &delete_db,
                &delete_bus,
                &delete_asks,
                thread_id,
                Vec::new(),
            )
            .await
        });
        delete_started_rx.await.unwrap();
        tokio::task::yield_now().await;
        let answer_db = db.clone();
        let answer_bus = bus.clone();
        let answer_task = tokio::spawn(async move {
            crate::attention::answer_durable_human_request(
                &answer_db,
                &answer_bus,
                request.id,
                Some(thread_id),
                Some(workspace.id),
                Some(request.revision),
                "Yes",
            )
            .await
        });
        drop(held_gate);

        assert!(delete_task.await.unwrap().is_err());
        assert!(answer_task.await.unwrap().unwrap().is_some());
        let persisted = repo::get_human_request(&db, request.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(persisted.status, repo::HUMAN_REQUEST_ANSWERED);
        assert!(bus.open_asks(thread.id).is_empty());
        let inbox = bus.inbox(thread.id, "lead");
        assert_eq!(inbox.len(), 1);
        assert_eq!(inbox[0].request_id, u64::try_from(request.id).ok());
        assert!(matches!(
            event_rx.try_recv().unwrap(),
            crate::bus::state::HumanAskEvent::Answered { ask_id, .. }
                if ask_id == u64::try_from(request.id).unwrap()
        ));
        assert!(
            event_rx.try_recv().is_err(),
            "the failed delete emitted no cancellation"
        );
    }

    async fn rewind_repo_action_card(
        db: &Db,
        action_id: &str,
        action_kind: &str,
    ) -> (
        entities::workspace::Model,
        entities::thread::Model,
        entities::lead_message::Model,
        entities::lead_message::Model,
    ) {
        let workspace = repo::create_workspace(db, &format!("rewind-{action_id}"))
            .await
            .unwrap();
        let thread = repo::create_thread(db, workspace.id, "issue", "feature", "codex")
            .await
            .unwrap();
        let target = repo::insert_lead_message(
            db,
            thread.id,
            None,
            1,
            "user",
            "text",
            r#"{"text":"start over"}"#,
            "complete",
        )
        .await
        .unwrap();
        let card = repo::insert_lead_message(
            db,
            thread.id,
            None,
            1,
            "assistant",
            "action_card",
            &serde_json::json!({
                "title": "Repository action",
                "actions": [{"id": action_id, "kind": action_kind, "label": "Run"}],
            })
            .to_string(),
            "complete",
        )
        .await
        .unwrap();
        (workspace, thread, target, card)
    }

    #[tokio::test]
    async fn rewind_suppresses_completed_repo_action_feedback_before_drain() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let db = Db::connect("sqlite::memory:").await.unwrap();
        let root = tempfile::tempdir().unwrap();
        let local = init_main_repo(root.path(), "completed-target");
        let (workspace, thread, target, card) =
            rewind_repo_action_card(&db, "rewind-completed", "add").await;
        let completed = add_repo_ref_inner(
            &db,
            workspace.id,
            "completed-target".to_string(),
            local.to_string_lossy().into_owned(),
            Some(thread.id),
            Some(card.id),
            Some("rewind-completed".to_string()),
            Some("add".to_string()),
        )
        .await
        .unwrap();
        let execution_id = completed.execution_id.unwrap();
        let planned_ids = vec![target.id, card.id];
        let locked = lock_repo_action_cleanups(
            &db,
            repo::repo_action_executions_requiring_lock_for_message_ids(
                &db,
                thread.id,
                &planned_ids,
            )
            .await
            .unwrap(),
        )
        .await
        .unwrap();
        let cleanup_plans = repo_action_cleanup_plans(&locked);
        let rewind_plans = repo_action_rewind_plans(&locked);
        let (deleted, _) = repo::rewind_persist_with_repo_actions(
            &db,
            thread.id,
            None,
            target.id,
            None,
            None,
            &cleanup_plans,
            &rewind_plans,
        )
        .await
        .unwrap();
        cleanup_locked_repo_actions(&db, &locked).await;
        drop(locked);

        assert_eq!(deleted, planned_ids);
        assert!(repo::get_repo(&db, completed.repo.id)
            .await
            .unwrap()
            .is_some());
        let sink_count = std::sync::Arc::new(AtomicUsize::new(0));
        let sink = sink_count.clone();
        assert!(drain_repo_action_feedback_with(&db, execution_id, move |_, _| async move {
            sink.fetch_add(1, Ordering::SeqCst);
            Ok(true)
        })
        .await
        .unwrap());
        assert_eq!(sink_count.load(Ordering::SeqCst), 0);
        assert!(repo::get_repo_action_execution_by_id(&db, execution_id)
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn rewind_materialized_repo_action_runs_checked_cleanup_and_preserves_registered_target()
    {
        let db = Db::connect("sqlite::memory:").await.unwrap();
        let root = tempfile::tempdir().unwrap();
        let target_path = init_main_repo(root.path(), "materialized-target");
        let (workspace, thread, target_message, card) =
            rewind_repo_action_card(&db, "rewind-materialized", "clone").await;
        let token = new_repo_action_token();
        let staging_path = root
            .path()
            .join(format!(".weft-repo-action-{token}.staging"));
        let target_text = target_path.to_string_lossy().into_owned();
        let staging_text = staging_path.to_string_lossy().into_owned();
        let request = repo::RepoActionClaimRequest {
            workspace_id: workspace.id,
            thread_id: thread.id,
            message_id: card.id,
            action_id: "rewind-materialized",
            action_kind: "clone",
            expected_action_kind: "clone",
            invocation_fingerprint: "rewind-materialized-fingerprint",
            execution_token: &token,
            target_path: &target_text,
            staging_path: &staging_text,
        };
        let execution = repo::claim_repo_action_execution(&db, &request)
            .await
            .unwrap();
        std::fs::create_dir_all(&staging_path).unwrap();
        let owner = repo_action_owner_path(&staging_path, &token).unwrap();
        write_token_file(&owner, &token, true).unwrap();
        write_repo_action_target_marker(&target_path, &token).unwrap();
        let execution = repo::mark_repo_action_materialized(&db, execution.id, &token)
            .await
            .unwrap();
        let registered = repo::add_repo_ref(
            &db,
            workspace.id,
            "materialized-target",
            &target_text,
            "main",
            "",
            true,
        )
        .await
        .unwrap();

        let planned_ids = vec![target_message.id, card.id];
        let locked = lock_repo_action_cleanups(
            &db,
            repo::repo_action_executions_requiring_lock_for_message_ids(
                &db,
                thread.id,
                &planned_ids,
            )
            .await
            .unwrap(),
        )
        .await
        .unwrap();
        let cleanup_plans = repo_action_cleanup_plans(&locked);
        let rewind_plans = repo_action_rewind_plans(&locked);
        repo::rewind_persist_with_repo_actions(
            &db,
            thread.id,
            None,
            target_message.id,
            None,
            None,
            &cleanup_plans,
            &rewind_plans,
        )
        .await
        .unwrap();
        let journal = repo::get_repo_action_execution_by_id(&db, execution.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(journal.status, repo::REPO_ACTION_CLEANUP_PENDING);
        assert!(journal.cleanup_preserve_target);

        cleanup_locked_repo_actions(&db, &locked).await;
        drop(locked);
        assert!(repo::get_repo_action_execution_by_id(&db, execution.id)
            .await
            .unwrap()
            .is_none());
        assert!(repo::get_repo(&db, registered.id).await.unwrap().is_some());
        assert!(target_path.exists());
        assert!(!target_path
            .join(".git")
            .join(REPO_ACTION_TOKEN_MARKER)
            .exists());
        assert!(!staging_path.exists());
        assert!(!owner.exists());
    }

    // ---- issue #103: read-only propagation wiring (the command-layer glue,
    // not `AskRegistry::grant_read_only_issue`'s own boundary — that's covered
    // exhaustively in ask.rs's unit tests) ---------------------------------

    /// Confirming the proposal (the human's "approve dispatch") propagates a
    /// read-only auto-allow to the WHOLE issue. Uses a REAL git repo because
    /// `planner::confirm` materializes a worktree; holds `ENV_LOCK` because
    /// `WEFT_HOME` is a process-global env var shared with `planner.rs`'s own
    /// confirm/materialize tests running in this same binary.
    #[tokio::test]
    async fn confirm_proposal_propagates_read_only_to_the_whole_issue_but_never_write() {
        let _env = crate::paths::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tag = format!("weft-confirm-readonly-propagate-{}", std::process::id());
        let root = std::env::temp_dir().join(format!("{tag}-root"));
        let weft_home = std::env::temp_dir().join(format!("{tag}-home"));
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&weft_home);
        std::env::set_var("WEFT_HOME", weft_home.to_str().unwrap());
        init_main_repo(&root, "api");

        let db = Db::connect("sqlite::memory:").await.unwrap();
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        repo::add_repo_ref(
            &db,
            ws.id,
            "api",
            root.join("api").to_str().unwrap(),
            "main",
            "",
            true,
        )
        .await
        .unwrap();
        let t = repo::create_thread(&db, ws.id, "t1", "feature", "claude")
            .await
            .unwrap();
        let proposal = crate::planner::Proposal {
            rationale: "r".into(),
            directions: vec![crate::planner::ProposedDirection {
                name: "A".into(),
                repo: "api".into(),
                reason: "r".into(),
                mandate: "".into(),
                base_branch: "".into(),
                decision: "".into(),
                direction_id: 0,
            }],
        };
        crate::planner::save_proposal(&db, t.id, &proposal)
            .await
            .unwrap();

        let asks = crate::ask::AskRegistry::new();
        let ids = confirm_proposal_and_propagate_read_only(&db, &asks, t.id)
            .await
            .unwrap();
        assert_eq!(ids.len(), 1, "the single lane should materialize");
        let dir = ids[0].to_string();

        // the propagated grant covers a ReadOnly ask on the JUST-created direction...
        assert_eq!(
            asks.auto_decision(t.id, &dir, crate::ask::RiskLevel::ReadOnly, "ls"),
            Some(crate::ask::Decision::Allow)
        );
        // ...AND a direction that didn't exist at confirm time — the whole point of
        // ISSUE-wide (not just per-dir) propagation: a worker spawned later still
        // inherits it (issue #103's motivating pain point).
        assert_eq!(
            asks.auto_decision(t.id, "999999", crate::ask::RiskLevel::ReadOnly, "pwd"),
            Some(crate::ask::Decision::Allow)
        );
        // but a Write/Unknown ask on the SAME direction still must ask — the
        // safety boundary this feature exists to respect (#139's review lesson:
        // what the UI claims was authorized, the backend must actually enforce,
        // proven with a real assertion, not just an absence of a wider grant).
        assert!(asks
            .auto_decision(t.id, &dir, crate::ask::RiskLevel::Write, "rm -rf x")
            .is_none());
        assert!(asks
            .auto_decision(t.id, &dir, crate::ask::RiskLevel::Unknown, "mystery_tool")
            .is_none());

        let removed = repo::delete_thread_cascade(&db, t.id).await.unwrap();
        let _ = materialize::cleanup_worktrees(&db, &removed).await;
        std::env::remove_var("WEFT_HOME");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&weft_home);
    }
}
