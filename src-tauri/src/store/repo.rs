//! All DB reads/writes go through here. Keeps SeaORM specifics out of commands.

use super::entities::{
    app_setting, code_checkpoint, direction, im_route, lead_message, plan, pull_request,
    repo_profile, repo_ref, session, skill_enable, skill_source, test_plan, thread, workspace,
    worktree,
};
use super::Db;
use crate::slug::unique_slug;
use anyhow::Result;
use sea_orm::{
    sea_query::Expr, ActiveModelTrait, ColumnTrait, EntityTrait, QueryFilter, QueryOrder, Set,
    TryIntoModel,
};
use std::collections::HashMap;

/// A manual route selected for a reused direction before its first native
/// conversation. `session_id` is present only when an interrupted initial
/// open left a session row behind; that row must move with the direction so a
/// later cold open cannot recover the stale tool.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InitialDirectionRoutePin {
    pub direction_id: i32,
    pub session_id: Option<i32>,
    pub tool: String,
}

/// Test-only DB-write failure injection.
///
/// Why it exists: several already-shipped degradation paths differ from their
/// happy path ONLY when a single store write fails while the writes around it
/// succeed. `lead_chat::engine::recover_from_freeze`'s marker-gated native-id
/// clear (issue #93, PR #133) is the canonical one — and a mutation run proved
/// the entire suite stayed green with that gate deleted, because nothing in this
/// crate could make one chosen write fail on demand. The alternative, faking the
/// post-failure DB rows by hand, is worse than no test at all: it asserts a
/// shape production may never produce (a lesson this repo has already paid for).
/// This makes the REAL write return a REAL `Err`.
///
/// Boundary — why it cannot leak into production:
///   * The module is `#[cfg(test)]`, so it exists only while this crate is
///     compiled as its own test target (`cargo test --lib`). `cargo build`, the
///     Tauri bundle, and even this crate's `tests/*.rs` integration binaries
///     link the lib WITHOUT `cfg(test)`, so the arming API is simply absent.
///   * Call sites go through [`fail_write`], whose entire body sits inside a
///     `#[cfg(test)]` block: in a production build it expands to nothing — no
///     branch, no atomic, no static, no symbol. Zero runtime cost, not "cheap".
///   * Nothing is armed by default and nothing consults an env var, so even in
///     the test build a write fails only for the task that armed it.
///
/// Scope is the TASK, not the process (`tokio::task_local!`), which is what
/// keeps this from becoming the process-global-static hazard the session-gate
/// tests needed a serializing lock for: two tests can arm different writes — or
/// the same one — concurrently without seeing each other, and a panicking test
/// disarms by unwinding out of the scope. The trade-off is the spawn boundary: a
/// write performed on a task `spawn`ed from inside the scope does NOT inherit
/// the arming. Every current seam point is awaited directly, so that holds; a
/// future caller that spawns needs a different mechanism, not a wider scope.
#[cfg(test)]
pub(crate) mod fail_write {
    use std::future::Future;

    tokio::task_local! {
        /// The one write name armed for the current task, if any.
        static ARMED: &'static str;
    }

    /// Run `fut` with the store write named `name` forced to return `Err`.
    /// Every other write inside `fut` behaves normally — that selectivity is
    /// the whole point: the paths under test are the ones where the neighbours
    /// of a failed write all succeeded.
    pub(crate) async fn while_failing<T>(name: &'static str, fut: impl Future<Output = T>) -> T {
        ARMED.scope(name, fut).await
    }

    /// Whether the calling task armed `name`. Only [`super::fail_write`] calls
    /// this; outside a `while_failing` scope `try_with` fails, i.e. "not armed".
    pub(crate) fn is_armed(name: &str) -> bool {
        ARMED.try_with(|armed| *armed == name).unwrap_or(false)
    }

    /// The error an armed write returns. Deliberately self-identifying: if this
    /// text ever surfaces in a real log, the seam escaped its test build.
    pub(crate) fn injected(name: &str) -> anyhow::Error {
        anyhow::anyhow!("injected store-write failure at `{name}` (test-only seam)")
    }
}

/// Mark a store write as injectable by [`fail_write`]'s `name`. Expands to
/// NOTHING outside `cfg(test)` — see that module's doc for the boundary. Place
/// it as the first statement of a write that returns `anyhow::Result`, so an
/// armed failure lands before any partial mutation.
macro_rules! fail_write {
    ($name:literal) => {
        #[cfg(test)]
        {
            if $crate::store::repo::fail_write::is_armed($name) {
                return Err($crate::store::repo::fail_write::injected($name));
            }
        }
    };
}

fn now() -> String {
    // RFC3339 without pulling chrono: seconds since epoch is enough for ordering.
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{secs}")
}

/// Unix-secs as string, for skill_source.last_synced.
pub fn now_unix() -> String {
    now()
}

pub async fn create_workspace(db: &Db, name: &str) -> Result<workspace::Model> {
    let name = validate_display_name(name, "workspace name")?;
    let mut dup_query = workspace::Entity::find().filter(workspace::Column::Name.eq(name));
    if let Some(hidden_id) = hidden_concierge_workspace_id(db).await? {
        dup_query = dup_query.filter(workspace::Column::Id.ne(hidden_id));
    }
    let dup = dup_query.one(&db.0).await?;
    if dup.is_some() {
        anyhow::bail!("another workspace already named {name:?}");
    }
    let existing: Vec<String> = workspace::Entity::find()
        .all(&db.0)
        .await?
        .into_iter()
        .map(|w| w.slug)
        .collect();
    let m = workspace::ActiveModel {
        name: Set(name.to_string()),
        slug: Set(unique_slug(name, &existing)),
        created_at: Set(now()),
        ..Default::default()
    };
    Ok(m.insert(&db.0).await?)
}

pub async fn list_workspaces(db: &Db) -> Result<Vec<workspace::Model>> {
    Ok(workspace::Entity::find().all(&db.0).await?)
}

/// Rename = display-name only. slug (and anything derived from it — branches,
/// worktree paths) is a stable identifier and never changes after creation.
pub async fn rename_workspace(db: &Db, workspace_id: i32, name: &str) -> Result<workspace::Model> {
    let name = validate_display_name(name, "workspace name")?;
    let mut dup_query = workspace::Entity::find()
        .filter(workspace::Column::Name.eq(name))
        .filter(workspace::Column::Id.ne(workspace_id));
    if let Some(hidden_id) = hidden_concierge_workspace_id(db).await? {
        dup_query = dup_query.filter(workspace::Column::Id.ne(hidden_id));
    }
    let dup = dup_query.one(&db.0).await?;
    if dup.is_some() {
        anyhow::bail!("another workspace already named {name:?}");
    }
    let m = workspace::Entity::find_by_id(workspace_id)
        .one(&db.0)
        .await?
        .ok_or_else(|| anyhow::anyhow!("workspace {workspace_id} not found"))?;
    let mut a: workspace::ActiveModel = m.into();
    a.name = Set(name.to_string());
    Ok(a.update(&db.0).await?)
}

/// Trim and reject empty for any display field. Centralized so rename helpers
/// stay consistent and error wording can evolve in one place.
fn validate_display_name<'a>(input: &'a str, what: &str) -> Result<&'a str> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        anyhow::bail!("{what} cannot be empty");
    }
    Ok(trimmed)
}

async fn hidden_concierge_workspace_id(db: &Db) -> Result<Option<i32>> {
    Ok(get_setting(db, K_CONCIERGE_WORKSPACE)
        .await?
        .and_then(|value| value.parse::<i32>().ok()))
}

async fn ensure_workspace_exists(db: &Db, workspace_id: i32) -> Result<()> {
    if workspace::Entity::find_by_id(workspace_id)
        .one(&db.0)
        .await?
        .is_none()
    {
        anyhow::bail!("workspace {workspace_id} not found");
    }
    Ok(())
}

fn workspace_deleting_key(workspace_id: i32) -> String {
    format!("workspace.deleting.{workspace_id}")
}

pub async fn mark_workspace_deleting(db: &Db, workspace_id: i32) -> Result<()> {
    ensure_workspace_exists(db, workspace_id).await?;
    set_setting(db, &workspace_deleting_key(workspace_id), "1").await
}

pub async fn clear_workspace_deleting(db: &Db, workspace_id: i32) -> Result<()> {
    delete_setting(db, &workspace_deleting_key(workspace_id)).await
}

async fn ensure_workspace_accepts_writes(db: &Db, workspace_id: i32) -> Result<()> {
    ensure_workspace_exists(db, workspace_id).await?;
    if get_setting(db, &workspace_deleting_key(workspace_id)).await?.is_some() {
        anyhow::bail!("workspace {workspace_id} is being deleted");
    }
    Ok(())
}

pub async fn ensure_thread_workspace_accepts_writes(
    db: &Db,
    thread_id: i32,
) -> Result<thread::Model> {
    let t = thread::Entity::find_by_id(thread_id)
        .one(&db.0)
        .await?
        .ok_or_else(|| anyhow::anyhow!("thread {thread_id} not found"))?;
    ensure_workspace_accepts_writes(db, t.workspace_id).await?;
    Ok(t)
}

pub async fn ensure_repo_workspace_accepts_writes(
    db: &Db,
    repo_id: i32,
) -> Result<repo_ref::Model> {
    let repo_ref = repo_ref::Entity::find_by_id(repo_id)
        .one(&db.0)
        .await?
        .ok_or_else(|| anyhow::anyhow!("repo {repo_id} not found"))?;
    ensure_workspace_accepts_writes(db, repo_ref.workspace_id).await?;
    Ok(repo_ref)
}

pub async fn ensure_direction_workspace_accepts_writes(
    db: &Db,
    direction_id: i32,
) -> Result<direction::Model> {
    let d = direction::Entity::find_by_id(direction_id)
        .one(&db.0)
        .await?
        .ok_or_else(|| anyhow::anyhow!("direction {direction_id} not found"))?;
    ensure_thread_workspace_accepts_writes(db, d.thread_id).await?;
    Ok(d)
}

/// The most-recently created workspace (highest id), if any. Used as the
/// default-workspace bootstrap target for first-run onboarding.
pub async fn latest_workspace(db: &Db) -> Result<Option<workspace::Model>> {
    Ok(workspace::Entity::find()
        .order_by_desc(workspace::Column::Id)
        .one(&db.0)
        .await?)
}

pub async fn add_skill_source(
    db: &Db,
    git_url: &str,
    git_ref: Option<&str>,
) -> Result<skill_source::Model> {
    let ref_norm = git_ref.unwrap_or("").to_string();
    // Idempotent: same (url, ref) reuses the existing row so repeat clicks /
    // re-imports don't pile up duplicate clones under ~/.weft/skills/sources/.
    // A *different* ref on the same URL is still a distinct source.
    if let Some(existing) = skill_source::Entity::find()
        .filter(skill_source::Column::GitUrl.eq(git_url))
        .filter(skill_source::Column::GitRef.eq(&ref_norm))
        .one(&db.0)
        .await?
    {
        return Ok(existing);
    }
    let m = skill_source::ActiveModel {
        git_url: Set(git_url.to_string()),
        git_ref: Set(ref_norm),
        last_synced: Set(String::new()),
        last_status: Set("never".to_string()),
        ..Default::default()
    };
    Ok(m.insert(&db.0).await?)
}

pub async fn list_skill_sources(db: &Db) -> Result<Vec<skill_source::Model>> {
    Ok(skill_source::Entity::find().all(&db.0).await?)
}

pub async fn get_skill_source(db: &Db, id: i32) -> Result<Option<skill_source::Model>> {
    Ok(skill_source::Entity::find_by_id(id).one(&db.0).await?)
}

pub async fn set_skill_source_status(
    db: &Db,
    id: i32,
    status: &str,
    synced: Option<&str>,
) -> Result<()> {
    if let Some(m) = skill_source::Entity::find_by_id(id).one(&db.0).await? {
        let mut a: skill_source::ActiveModel = m.into();
        a.last_status = Set(status.to_string());
        if let Some(s) = synced {
            a.last_synced = Set(s.to_string());
        }
        a.update(&db.0).await?;
    }
    Ok(())
}

pub async fn remove_skill_source(db: &Db, id: i32) -> Result<()> {
    skill_enable::Entity::delete_many()
        .filter(skill_enable::Column::SourceId.eq(id))
        .exec(&db.0)
        .await?;
    skill_source::Entity::delete_by_id(id).exec(&db.0).await?;
    Ok(())
}

pub async fn set_skill_enable(
    db: &Db,
    source_id: i32,
    skill_name: &str,
    scope: &str,
    on: bool,
) -> Result<()> {
    let existing = skill_enable::Entity::find()
        .filter(skill_enable::Column::SourceId.eq(source_id))
        .filter(skill_enable::Column::SkillName.eq(skill_name))
        .filter(skill_enable::Column::Scope.eq(scope))
        .one(&db.0)
        .await?;
    match (on, existing) {
        (true, None) => {
            let m = skill_enable::ActiveModel {
                source_id: Set(source_id),
                skill_name: Set(skill_name.to_string()),
                scope: Set(scope.to_string()),
                ..Default::default()
            };
            m.insert(&db.0).await?;
        }
        (false, Some(m)) => {
            skill_enable::Entity::delete_by_id(m.id).exec(&db.0).await?;
        }
        _ => {}
    }
    Ok(())
}

pub async fn list_skill_enable(db: &Db) -> Result<Vec<skill_enable::Model>> {
    Ok(skill_enable::Entity::find().all(&db.0).await?)
}

pub async fn get_setting(db: &Db, key: &str) -> Result<Option<String>> {
    Ok(app_setting::Entity::find_by_id(key)
        .one(&db.0)
        .await?
        .map(|m| m.value))
}

pub async fn set_setting(db: &Db, key: &str, value: &str) -> Result<()> {
    let m = app_setting::ActiveModel {
        key: Set(key.to_string()),
        value: Set(value.to_string()),
    };
    app_setting::Entity::insert(m)
        .on_conflict(
            sea_orm::sea_query::OnConflict::column(app_setting::Column::Key)
                .update_column(app_setting::Column::Value)
                .to_owned(),
        )
        .exec(&db.0)
        .await?;
    Ok(())
}

/// Remove an app_setting row. No-op when the key is absent. Used to clear a
/// stored value entirely so `get_setting` reads `None` again — distinct from
/// `set_setting(key, "")`, which would still read as `Some("")`.
pub async fn delete_setting(db: &Db, key: &str) -> Result<()> {
    app_setting::Entity::delete_by_id(key.to_string())
        .exec(&db.0)
        .await?;
    Ok(())
}

/// The user-configured coding-agent command overrides (identity → command),
/// parsed from the `tool_commands` app_setting. Empty when none are set.
pub async fn get_tool_commands(db: &Db) -> Result<HashMap<String, String>> {
    let raw = get_setting(db, crate::tool_command::K_TOOL_COMMANDS).await?;
    Ok(raw
        .as_deref()
        .map(crate::tool_command::parse_overrides)
        .unwrap_or_default())
}

/// Set (or clear, when `command` is blank / equals the identity) the override for
/// one tool, and reconcile existing sessions of that tool:
///
/// - `apply_to_existing = true`: CLEAR any per-session pins for this tool so all
///   existing sessions follow the (new) global command — also the only path to
///   un-pin rows frozen by an earlier opt-out, including when clearing an alias.
/// - `apply_to_existing = false`: PIN un-pinned existing sessions to their prior
///   effective command, so only sessions created from here on adopt the alias.
///
/// Returns `(override map, prior effective command)`. The prior command is what
/// callers freeze live in-memory engines to when pinning (the DB pin only takes
/// effect when an engine is rebuilt).
pub async fn set_tool_command(
    db: &Db,
    tool: &str,
    command: &str,
    apply_to_existing: bool,
) -> Result<(HashMap<String, String>, String)> {
    let mut map = get_tool_commands(db).await?;
    let command = command.trim();
    // The command existing sessions resolve to TODAY (before this change).
    let prev = map.get(tool).cloned().unwrap_or_else(|| tool.to_string());

    if apply_to_existing {
        thread::Entity::update_many()
            .col_expr(thread::Column::LeadCommand, Expr::value(Option::<String>::None))
            .filter(thread::Column::LeadTool.eq(tool))
            .filter(thread::Column::LeadCommand.is_not_null())
            .exec(&db.0)
            .await?;
        session::Entity::update_many()
            .col_expr(session::Column::Command, Expr::value(Option::<String>::None))
            .filter(session::Column::Tool.eq(tool))
            .filter(session::Column::Command.is_not_null())
            .exec(&db.0)
            .await?;
    } else {
        thread::Entity::update_many()
            .col_expr(thread::Column::LeadCommand, Expr::value(prev.clone()))
            .filter(thread::Column::LeadTool.eq(tool))
            .filter(thread::Column::LeadCommand.is_null())
            .exec(&db.0)
            .await?;
        session::Entity::update_many()
            .col_expr(session::Column::Command, Expr::value(prev.clone()))
            .filter(session::Column::Tool.eq(tool))
            .filter(session::Column::Command.is_null())
            .exec(&db.0)
            .await?;
    }

    if command.is_empty() || command == tool {
        map.remove(tool);
    } else {
        map.insert(tool.to_string(), command.to_string());
    }
    set_setting(
        db,
        crate::tool_command::K_TOOL_COMMANDS,
        &crate::tool_command::to_json(&map),
    )
    .await?;
    Ok((map, prev))
}

/// app_setting key for the workspace's synthesized repo-map markdown document.
fn repo_map_doc_key(workspace_id: i32) -> String {
    format!("repomap.doc.{workspace_id}")
}

/// Persist the analyst-synthesized markdown repo-map for a workspace.
pub async fn set_repo_map_doc(db: &Db, workspace_id: i32, markdown: &str) -> Result<()> {
    set_setting(db, &repo_map_doc_key(workspace_id), markdown).await
}

/// Read the analyst-synthesized markdown repo-map for a workspace.
/// Returns `None` when none has been generated yet.
pub async fn get_repo_map_doc(db: &Db, workspace_id: i32) -> Result<Option<String>> {
    get_setting(db, &repo_map_doc_key(workspace_id)).await
}

/// Drop a workspace's persisted repo-map doc so `get_repo_map_doc` reads `None`
/// and the map pane falls back to its empty/regenerate state. Used when the
/// workspace can no longer produce a meaningful cross-repo map (dropped below the
/// 2-profiled-repo threshold), so the pane never shows markdown for repos/edges
/// that are no longer in the graph.
pub async fn clear_repo_map_doc(db: &Db, workspace_id: i32) -> Result<()> {
    delete_setting(db, &repo_map_doc_key(workspace_id)).await
}

/// Workspace container used by per-IM-conversation Concierge threads.
pub const K_CONCIERGE_WORKSPACE: &str = "concierge.workspace_id";

/// app_setting key holding a workspace's hidden curator-chat thread id.
fn curator_thread_key(workspace_id: i32) -> String {
    format!("curator.thread.{workspace_id}")
}

/// Return the hidden curator thread id when it has already been created. This
/// is read-only and never creates a chat just to attach an analysis marker.
pub async fn curator_thread_for_workspace(db: &Db, workspace_id: i32) -> Result<Option<i32>> {
    let Some(id) = get_setting(db, &curator_thread_key(workspace_id))
        .await?
        .and_then(|value| value.parse::<i32>().ok())
    else {
        return Ok(None);
    };
    match get_thread(db, id).await? {
        Some(thread) if thread.kind == "curator" => Ok(Some(id)),
        _ => Ok(None),
    }
}

/// Get-or-create the hidden curator-chat thread for a workspace (mirrors the
/// Concierge get-or-create). The id is stable (persisted in app_setting); the
/// thread is `kind="curator"` so board views can filter it out.
pub async fn ensure_curator_thread(db: &Db, workspace_id: i32, lead_tool: &str) -> Result<i32> {
    let key = curator_thread_key(workspace_id);
    if let Some(id) = get_setting(db, &key).await?.and_then(|s| s.parse::<i32>().ok()) {
        if let Some(t) = get_thread(db, id).await? {
            if t.kind == "curator" {
                return Ok(id);
            }
        }
    }
    let t = create_thread(db, workspace_id, "Dependency curator", "curator", lead_tool).await?;
    set_setting(db, &key, &t.id.to_string()).await?;
    Ok(t.id)
}

/// Register a repo in a workspace. Idempotent at the workspace level: if a repo
/// with the same local path OR the same `origin` remote (compared via
/// `git::git_url_key`) is already present, the existing row is returned and
/// nothing is inserted — so re-adding or re-pasting the same repo is a silent
/// no-op, and the same remote cloned at two paths isn't duplicated. Dedup is
/// scoped to the workspace; the same repo in two workspaces is intentional.
pub async fn add_repo_ref(
    db: &Db,
    workspace_id: i32,
    name: &str,
    local_git_path: &str,
    base_ref: &str,
    remote_url: &str,
    base_ref_is_default: bool,
) -> Result<repo_ref::Model> {
    ensure_workspace_accepts_writes(db, workspace_id).await?;
    let existing = repo_ref::Entity::find()
        .filter(repo_ref::Column::WorkspaceId.eq(workspace_id))
        .all(&db.0)
        .await?;
    // Same-repo dedup: identical local path, or a non-empty remote normalizing to
    // the same key (an empty key never matches, so local-only repos stay distinct).
    let key = crate::git::git_url_key(remote_url);
    if let Some(dup) = existing.iter().find(|r| {
        r.local_git_path == local_git_path
            || (!key.is_empty() && crate::git::git_url_key(&r.remote_url) == key)
    }) {
        // R42-1: re-adding with a VETTED default (is_default=true) repairs a legacy/stale
        // marker on the existing row — an upgraded row may still carry base_ref_is_default=false
        // (or a stale base_ref), which makes later blank-base materialization ignore the known
        // default and fall through to main/master.
        if base_ref_is_default && (!dup.base_ref_is_default || dup.base_ref != base_ref) {
            ensure_workspace_accepts_writes(db, workspace_id).await?;
            let mut am: repo_ref::ActiveModel = dup.clone().into();
            am.base_ref = Set(base_ref.to_string());
            am.base_ref_is_default = Set(true);
            return Ok(am.update(&db.0).await?);
        }
        ensure_workspace_accepts_writes(db, workspace_id).await?;
        return Ok(dup.clone());
    }
    let slugs: Vec<String> = existing.into_iter().map(|r| r.slug).collect();
    ensure_workspace_accepts_writes(db, workspace_id).await?;
    let m = repo_ref::ActiveModel {
        workspace_id: Set(workspace_id),
        name: Set(name.to_string()),
        slug: Set(unique_slug(name, &slugs)),
        local_git_path: Set(local_git_path.to_string()),
        base_ref: Set(base_ref.to_string()),
        remote_url: Set(remote_url.to_string()),
        base_ref_is_default: Set(base_ref_is_default),
        ..Default::default()
    };
    let inserted = m.insert(&db.0).await?;
    if let Err(err) = ensure_workspace_accepts_writes(db, workspace_id).await {
        let _ = repo_ref::Entity::delete_by_id(inserted.id).exec(&db.0).await;
        return Err(err);
    }
    Ok(inserted)
}

pub async fn create_thread(
    db: &Db,
    workspace_id: i32,
    title: &str,
    kind: &str,
    lead_tool: &str,
) -> Result<thread::Model> {
    let title = validate_display_name(title, "issue title")?;
    let kind = validate_display_name(kind, "issue kind")?;
    ensure_workspace_accepts_writes(db, workspace_id).await?;
    let existing: Vec<String> = thread::Entity::find()
        .filter(thread::Column::WorkspaceId.eq(workspace_id))
        .all(&db.0)
        .await?
        .into_iter()
        .map(|t| t.slug)
        .collect();
    ensure_workspace_accepts_writes(db, workspace_id).await?;
    let m = thread::ActiveModel {
        workspace_id: Set(workspace_id),
        title: Set(title.to_string()),
        slug: Set(unique_slug(title, &existing)),
        kind: Set(kind.to_string()),
        lead_tool: Set(lead_tool.to_string()),
        // The configured/default tool is only a fallback. A user pin is set
        // by the explicit switch/approval path, never by construction.
        engine_pinned: Set(false),
        created_at: Set(now()),
        ..Default::default()
    };
    let inserted = m.insert(&db.0).await?;
    if let Err(err) = ensure_workspace_accepts_writes(db, workspace_id).await {
        let _ = thread::Entity::delete_by_id(inserted.id).exec(&db.0).await;
        return Err(err);
    }
    Ok(inserted)
}

pub async fn list_threads(db: &Db, workspace_id: i32) -> Result<Vec<thread::Model>> {
    Ok(thread::Entity::find()
        .filter(thread::Column::WorkspaceId.eq(workspace_id))
        .all(&db.0)
        .await?)
}

pub async fn list_repos(db: &Db, workspace_id: i32) -> Result<Vec<repo_ref::Model>> {
    Ok(repo_ref::Entity::find()
        .filter(repo_ref::Column::WorkspaceId.eq(workspace_id))
        .all(&db.0)
        .await?)
}

pub async fn get_repo(db: &Db, repo_id: i32) -> Result<Option<repo_ref::Model>> {
    Ok(repo_ref::Entity::find_by_id(repo_id).one(&db.0).await?)
}

pub async fn get_thread(db: &Db, thread_id: i32) -> Result<Option<thread::Model>> {
    Ok(thread::Entity::find_by_id(thread_id).one(&db.0).await?)
}

/// Refresh an initial automatic lead route only while no manual choice has
/// landed. The conditional write prevents a stale resolver result from
/// overwriting a concurrent manual pin.
pub async fn refresh_unpinned_thread_route(
    db: &Db,
    thread_id: i32,
    tool: &str,
) -> Result<bool> {
    let write = thread::Entity::update_many()
        .col_expr(thread::Column::LeadTool, Expr::value(tool.to_string()))
        .filter(thread::Column::Id.eq(thread_id))
        .filter(thread::Column::EnginePinned.eq(false))
        .exec(&db.0)
        .await?;
    Ok(write.rows_affected != 0)
}

pub async fn set_thread_engine_pinned(db: &Db, thread_id: i32, pinned: bool) -> Result<()> {
    thread::Entity::update_many()
        .col_expr(thread::Column::EnginePinned, Expr::value(pinned))
        .filter(thread::Column::Id.eq(thread_id))
        .exec(&db.0)
        .await?;
    Ok(())
}

/// Display-title only; slug stays (see rename_workspace).
pub async fn rename_thread(db: &Db, thread_id: i32, title: &str) -> Result<thread::Model> {
    let title = validate_display_name(title, "issue title")?;
    let m = thread::Entity::find_by_id(thread_id)
        .one(&db.0)
        .await?
        .ok_or_else(|| anyhow::anyhow!("thread {thread_id} not found"))?;
    let dup = thread::Entity::find()
        .filter(thread::Column::WorkspaceId.eq(m.workspace_id))
        .filter(thread::Column::Title.eq(title))
        .filter(thread::Column::Id.ne(thread_id))
        .one(&db.0)
        .await?;
    if dup.is_some() {
        anyhow::bail!("another issue in this workspace already titled {title:?}");
    }
    let mut a: thread::ActiveModel = m.into();
    a.title = Set(title.to_string());
    Ok(a.update(&db.0).await?)
}

/// Test-only rendezvous immediately after a switch transaction's FIRST
/// statement — the point where "did this take the write lock yet?" is
/// answerable (PR #140 review round 15).
///
/// A barrier rather than a signal, and placed AFTER the statement rather than
/// before, because neither alternative works: a probe before the first
/// statement leaves a scheduling gap and cannot discriminate, and a plain
/// signal after it would let the test race ahead. Note also why the test does
/// not simply hold the write lock and watch the switch block — with the lock
/// held, a write-first transaction's opening statement blocks INSIDE the
/// critical section, so a rendezvous placed after it could never be reached
/// and the correct implementation would deadlock. Nothing is held while the
/// probe runs; the test asks a third connection whether the lock is taken.
///
/// `#[cfg(test)]` throughout, like `fail_write` (#144): production builds
/// contain no expansion at all.
#[cfg(test)]
pub(crate) mod txn_probe {
    tokio::task_local! {
        pub static AFTER_FIRST_STATEMENT: std::sync::Arc<tokio::sync::Barrier>;
    }
}

/// Rendezvous with an armed [`txn_probe`], or nothing at all.
macro_rules! probe_after_first_statement {
    () => {
        #[cfg(test)]
        {
            let armed = crate::store::repo::txn_probe::AFTER_FIRST_STATEMENT
                .try_with(std::sync::Arc::clone)
                .ok();
            if let Some(barrier) = armed {
                // TWICE: the first rendezvous tells the test the statement has
                // run, the second holds the transaction here while the test
                // probes the lock. Between them it touches nothing.
                barrier.wait().await;
                barrier.wait().await;
            }
        }
    };
}

/// Switch a thread's lead engine identity + model override (issue #96/#98).
/// `model=None` clears any override (follow the CLI's own default). Also
/// clears `lead_command`: a per-tool alias pin (e.g. `claude` → `cc-claude`)
/// is meaningless once `lead_tool` names a DIFFERENT tool identity, and
/// carrying it forward would silently try to spawn the old alias as the new
/// tool's binary. Does NOT touch `native_id` or any live in-memory engine —
/// the caller (lead_chat::commands::switch_lead_tool) owns that half of the
/// switch (tear down the live engine, clear native id, reconstruct fresh) so
/// this stays a plain, independently-testable field update. No-op fields
/// (same tool, same model) still write through — callers may use this to
/// force-reload an engine so an externally-edited CLI config takes effect.
pub async fn switch_lead_engine_txn(
    db: &Db,
    thread_id: i32,
    tool: &str,
    model: Option<&str>,
) -> Result<()> {
    switch_lead_engine_txn_with_pin(db, thread_id, tool, model, true).await
}

/// `pinned` is false only for the structured automatic quota failover path.
/// Keep it in the same transaction as the tool identity so an interrupted
/// write cannot later reinterpret the route as a user choice.
pub async fn switch_lead_engine_txn_with_pin(
    db: &Db,
    thread_id: i32,
    tool: &str,
    model: Option<&str>,
    pinned: bool,
) -> Result<()> {
    fail_write!("switch_lead_engine_txn");
    use sea_orm::TransactionTrait;
    let txn = db.0.begin().await?;
    // The FIRST statement is a write, deliberately. `begin()` opens a DEFERRED
    // transaction, and under WAL a deferred read→write upgrade fails with
    // SQLITE_BUSY_SNAPSHOT whenever ANY other writer commits after the
    // snapshot — a stale snapshot the busy timeout cannot repair, unlike
    // ordinary writer contention. `insert_lead_message` documents the same
    // hazard and is why it is not a transaction at all. Weft's background
    // activity/status writes make it reachable here, and the cost is high: the
    // command has already torn the live engine down by this point, so a
    // spurious abort is not a no-op. Taking the write lock with the opening
    // statement means every read below runs under it.
    //
    // An UPDATE … WHERE rather than find-then-update for that reason:
    // `rows_affected` carries the "thread is gone" case that the read used to.
    let touched = thread::Entity::update_many()
        .col_expr(thread::Column::LeadTool, Expr::value(tool))
        .col_expr(thread::Column::LeadCommand, Expr::value(Option::<String>::None))
        .col_expr(thread::Column::LeadModel, Expr::value(model.map(str::to_string)))
        .col_expr(thread::Column::EnginePinned, Expr::value(pinned))
        .filter(thread::Column::Id.eq(thread_id))
        .exec(&txn)
        .await?;
    if touched.rows_affected == 0 {
        anyhow::bail!("thread {thread_id} not found");
    }
    probe_after_first_statement!();

    // The native-id clear, in the SAME transaction — the identical strip
    // `set_lead_native_id_opt(.., None)` performs, against `txn`. The lead's
    // native id lives in a `kind = "meta"` row rather than a column, which is
    // why this is spelled out here instead of reusing that function.
    if let Some(meta) = lead_message::Entity::find()
        .filter(lead_message::Column::ThreadId.eq(thread_id))
        .filter(lead_message::Column::Kind.eq("meta"))
        .one(&txn)
        .await?
    {
        let mut v: serde_json::Value =
            serde_json::from_str(&meta.content).unwrap_or_else(|_| serde_json::json!({}));
        if let Some(obj) = v.as_object_mut() {
            obj.remove("native_id");
        }
        if v.as_object().is_some_and(|o| o.is_empty()) {
            lead_message::Entity::delete_by_id(meta.id).exec(&txn).await?;
        } else {
            let mut ma: lead_message::ActiveModel = meta.into();
            ma.content = Set(v.to_string());
            ma.update(&txn).await?;
        }
    }
    // …and the grace marker is written in the SAME commit. This is the whole
    // fix: "the native id is gone" and "there is evidence this surface ran"
    // are two halves of one invariant (`revive::has_resumable_context`), and a
    // transaction is what makes them unable to disagree. Everything else this
    // PR tried — stamping first and gating, retracting on failure, a pending
    // kind promoted later — existed only to make an EARLIER stamp safe, and
    // none of it is needed once the two writes are atomic.
    insert_marker_row(&txn, thread_id, None, MARKER_KIND_RECOVERED).await?;
    txn.commit().await?;
    Ok(())
}

pub async fn get_plan(db: &Db, thread_id: i32) -> Result<Option<plan::Model>> {
    Ok(plan::Entity::find()
        .filter(plan::Column::ThreadId.eq(thread_id))
        .one(&db.0)
        .await?)
}

async fn ensure_plan_write_survived_workspace_fence(db: &Db, thread_id: i32) -> Result<()> {
    if let Err(err) = ensure_thread_workspace_accepts_writes(db, thread_id).await {
        let _ = plan::Entity::delete_many()
            .filter(plan::Column::ThreadId.eq(thread_id))
            .exec(&db.0)
            .await;
        return Err(err);
    }
    Ok(())
}

/// Insert or update a thread's plan/proposal.
pub async fn upsert_plan(
    db: &Db,
    thread_id: i32,
    proposal: &str,
    status: &str,
    created_at: &str,
) -> Result<plan::Model> {
    ensure_thread_workspace_accepts_writes(db, thread_id).await?;
    let mut a = match get_plan(db, thread_id).await? {
        Some(m) => m.into(),
        None => plan::ActiveModel {
            thread_id: Set(thread_id),
            created_at: Set(created_at.to_string()),
            ..Default::default()
        },
    };
    a.proposal = Set(proposal.to_string());
    a.status = Set(status.to_string());
    ensure_thread_workspace_accepts_writes(db, thread_id).await?;
    let saved = a.save(&db.0).await?.try_into_model()?;
    ensure_plan_write_survived_workspace_fence(db, thread_id).await?;
    Ok(saved)
}

/// Set a plan's `created_at`, which doubles as the proposal VERSION ("last proposed at").
/// `save_proposal` bumps it on every re-propose (R50-2) so the frontend can reset a dirty base
/// edit on ANY re-proposal. (Distinct from `upsert_plan`, which intentionally PRESERVES
/// `created_at` on update — the targeted-edit / CAS / test-seam paths rely on that.) No-op if the
/// plan row is absent.
pub async fn set_plan_created_at(db: &Db, thread_id: i32, created_at: &str) -> Result<()> {
    use sea_orm::sea_query::Expr;
    ensure_thread_workspace_accepts_writes(db, thread_id).await?;
    let res = plan::Entity::update_many()
        .col_expr(plan::Column::CreatedAt, Expr::value(created_at.to_string()))
        .filter(plan::Column::ThreadId.eq(thread_id))
        .exec(&db.0)
        .await?;
    if res.rows_affected > 0 {
        ensure_plan_write_survived_workspace_fence(db, thread_id).await?;
    }
    Ok(())
}

/// Compare-and-swap the stored proposal: write `new_proposal` + `status` ONLY if the
/// row's current proposal still equals `expected` AND its current status still equals
/// `status` (no re-propose AND no confirm landed since the caller read it). Returns true
/// when applied, false when the proposal OR status changed (or the plan is gone) — so a
/// targeted base/decision edit rejects rather than clobbering a fresh re-propose with a
/// stale full proposal, OR reopening a just-confirmed plan back to "proposed".
/// `created_at` is intentionally left untouched.
pub async fn update_plan_proposal_cas(
    db: &Db,
    thread_id: i32,
    new_proposal: &str,
    expected: &str,
    status: &str,
) -> Result<bool> {
    use sea_orm::sea_query::Expr;
    ensure_thread_workspace_accepts_writes(db, thread_id).await?;
    let res = plan::Entity::update_many()
        .col_expr(plan::Column::Proposal, Expr::value(new_proposal.to_string()))
        .col_expr(plan::Column::Status, Expr::value(status.to_string()))
        .filter(plan::Column::ThreadId.eq(thread_id))
        .filter(plan::Column::Proposal.eq(expected))
        // Pin status too: a targeted edit reads the plan at one status; if `confirm`
        // flips that SAME proposal JSON to "confirmed" before this CAS runs, the
        // proposal predicate still matches and the SET would write the stale status
        // back, reopening a materialized plan. Predicating on the read status makes
        // a drifted row match 0 rows (rejecting the edit) while an in-status edit is
        // a no-op on the status column (SET writes the same value).
        .filter(plan::Column::Status.eq(status))
        .exec(&db.0)
        .await?;
    if res.rows_affected > 0 {
        ensure_plan_write_survived_workspace_fence(db, thread_id).await?;
    }
    Ok(res.rows_affected > 0)
}

/// Persist one selected route under a transaction that has already claimed the
/// SQLite writer lock with its plan update.
///
/// A sessionless route must remain sessionless. A `session_id` identifies the
/// latest interrupted initial session: it may be updated only while it has not
/// captured a native conversation or become independently pinned. The planner
/// checks the in-memory live-engine registry before choosing that shape; this
/// store boundary keeps the durable native-session race fail-closed.
async fn pin_initial_direction_route(
    txn: &sea_orm::DatabaseTransaction,
    thread_id: i32,
    pin: &InitialDirectionRoutePin,
    operation: &str,
) -> Result<()> {
    let direction_write = direction::Entity::update_many()
        .col_expr(direction::Column::Tool, Expr::value(pin.tool.clone()))
        .col_expr(direction::Column::EnginePinned, Expr::value(true))
        .filter(direction::Column::Id.eq(pin.direction_id))
        .filter(direction::Column::ThreadId.eq(thread_id))
        .filter(direction::Column::EnginePinned.eq(false))
        .exec(txn)
        .await?;
    if direction_write.rows_affected == 0 {
        anyhow::bail!(
            "direction {} became manually pinned while {operation} its route",
            pin.direction_id
        );
    }

    let Some(session_id) = pin.session_id else {
        let opened_session = session::Entity::find()
            .filter(session::Column::DirectionId.eq(pin.direction_id))
            .one(txn)
            .await?;
        if opened_session.is_some() {
            anyhow::bail!(
                "direction {} opened while {operation} its manual route",
                pin.direction_id
            );
        }
        return Ok(());
    };

    let latest_session = session::Entity::find()
        .filter(session::Column::DirectionId.eq(pin.direction_id))
        .order_by_desc(session::Column::Id)
        .one(txn)
        .await?;
    let can_refresh_session = latest_session.is_some_and(|session| {
        session.id == session_id
            && session.native_session_id.is_none()
            && !session.engine_pinned
    });
    if !can_refresh_session {
        anyhow::bail!(
            "direction {} opened while {operation} its manual route",
            pin.direction_id
        );
    }

    let session_write = session::Entity::update_many()
        .col_expr(session::Column::Tool, Expr::value(pin.tool.clone()))
        .col_expr(session::Column::EnginePinned, Expr::value(true))
        .col_expr(session::Column::Command, Expr::value(Option::<String>::None))
        .col_expr(session::Column::Model, Expr::value(Option::<String>::None))
        .col_expr(
            session::Column::NativeSessionId,
            Expr::value(Option::<String>::None),
        )
        .filter(session::Column::Id.eq(session_id))
        .filter(session::Column::DirectionId.eq(pin.direction_id))
        .filter(session::Column::EnginePinned.eq(false))
        .filter(session::Column::NativeSessionId.is_null())
        .exec(txn)
        .await?;
    if session_write.rows_affected == 0 {
        anyhow::bail!(
            "session {session_id} became established while {operation} its manual route"
        );
    }
    Ok(())
}

/// Atomically record a reused lane's approval and its explicit manual route.
/// The worktree has already materialized before this is called, but the plan
/// must never become approved without the manual pin that justified dispatch.
/// A first plan write takes SQLite's writer lock before the session check, so a
/// concurrent worker either wins first (and this rejects) or waits and reads
/// the committed manual route when it creates its session.
pub async fn commit_reused_approval_with_direction_pin_cas(
    db: &Db,
    thread_id: i32,
    new_proposal: &str,
    expected_proposal: &str,
    expected_status: &str,
    manual_pin: &InitialDirectionRoutePin,
) -> Result<bool> {
    use sea_orm::TransactionTrait;

    ensure_thread_workspace_accepts_writes(db, thread_id).await?;
    let txn = db.0.begin().await?;
    let plan_write = plan::Entity::update_many()
        .col_expr(plan::Column::Proposal, Expr::value(new_proposal.to_string()))
        .col_expr(plan::Column::Status, Expr::value(expected_status.to_string()))
        .filter(plan::Column::ThreadId.eq(thread_id))
        .filter(plan::Column::Proposal.eq(expected_proposal))
        .filter(plan::Column::Status.eq(expected_status))
        .exec(&txn)
        .await?;
    if plan_write.rows_affected == 0 {
        txn.rollback().await?;
        return Ok(false);
    }

    if let Err(err) = pin_initial_direction_route(&txn, thread_id, manual_pin, "approving").await {
        let _ = txn.rollback().await;
        return Err(err);
    }

    txn.commit().await?;
    ensure_plan_write_survived_workspace_fence(db, thread_id).await?;
    Ok(true)
}

/// Mark a thread's plan "confirmed" ONLY if its proposal AND status are still what the caller
/// read at the start — i.e. no re-propose and no concurrent confirm landed in between. Unlike
/// `update_plan_proposal_cas` (which pins expected==new status), this flips a NON-confirmed
/// status to "confirmed", so it takes a SEPARATE `expected_status`. Returns true when applied,
/// false when the proposal OR status drifted (or the plan is gone). Leaves proposal +
/// created_at untouched.
///
/// Called by `planner::auto_settle_if_fully_decided` (issue #104) to close a proposal that got
/// resolved entirely lane-by-lane (approve_direction / deny_direction) without an explicit batch
/// `confirm()`, so its card doesn't linger "proposed" forever waiting on a confirm click that
/// would create nothing new. (`confirm` itself uses the sibling `commit_confirmed_plan_cas`,
/// which also rewrites the proposal in the same atomic update.)
pub async fn mark_plan_confirmed_cas(
    db: &Db,
    thread_id: i32,
    expected_proposal: &str,
    expected_status: &str,
) -> Result<bool> {
    use sea_orm::sea_query::Expr;
    ensure_thread_workspace_accepts_writes(db, thread_id).await?;
    let res = plan::Entity::update_many()
        .col_expr(plan::Column::Status, Expr::value("confirmed"))
        .filter(plan::Column::ThreadId.eq(thread_id))
        .filter(plan::Column::Proposal.eq(expected_proposal))
        .filter(plan::Column::Status.eq(expected_status))
        .exec(&db.0)
        .await?;
    if res.rows_affected > 0 {
        ensure_plan_write_survived_workspace_fence(db, thread_id).await?;
    }
    Ok(res.rows_affected > 0)
}

/// Like `mark_plan_confirmed_cas`, but ALSO rewrites the proposal to `new_proposal` in the same
/// atomic update — used by `confirm` to persist the proposal with each lane's RECORDED direction
/// id while flipping the status to "confirmed". The CAS baseline is still the START snapshot
/// (`expected_proposal` + `expected_status`), so a re-propose that landed in between still makes
/// this match 0 rows and reject — the concurrency invariant is unchanged. Returns true when
/// applied, false when the proposal OR status drifted (or the plan is gone). `created_at`
/// (the proposal version) is intentionally left untouched.
pub async fn commit_confirmed_plan_cas(
    db: &Db,
    thread_id: i32,
    new_proposal: &str,
    expected_proposal: &str,
    expected_status: &str,
) -> Result<bool> {
    use sea_orm::sea_query::Expr;
    ensure_thread_workspace_accepts_writes(db, thread_id).await?;
    let res = plan::Entity::update_many()
        .col_expr(plan::Column::Proposal, Expr::value(new_proposal.to_string()))
        .col_expr(plan::Column::Status, Expr::value("confirmed"))
        .filter(plan::Column::ThreadId.eq(thread_id))
        .filter(plan::Column::Proposal.eq(expected_proposal))
        .filter(plan::Column::Status.eq(expected_status))
        .exec(&db.0)
        .await?;
    if res.rows_affected > 0 {
        ensure_plan_write_survived_workspace_fence(db, thread_id).await?;
    }
    Ok(res.rows_affected > 0)
}

/// Commit a confirmed plan and the manual route pins selected for reused,
/// not-yet-established directions in one transaction. A failed confirmation
/// must not leave a direction or an interrupted initial session pinned to a
/// route the plan never consumed.
pub async fn commit_confirmed_plan_with_direction_pins_cas(
    db: &Db,
    thread_id: i32,
    new_proposal: &str,
    expected_proposal: &str,
    expected_status: &str,
    manual_pins: &[InitialDirectionRoutePin],
) -> Result<bool> {
    use sea_orm::TransactionTrait;

    ensure_thread_workspace_accepts_writes(db, thread_id).await?;
    let txn = db.0.begin().await?;
    let plan_write = plan::Entity::update_many()
        .col_expr(plan::Column::Proposal, Expr::value(new_proposal.to_string()))
        .col_expr(plan::Column::Status, Expr::value("confirmed"))
        .filter(plan::Column::ThreadId.eq(thread_id))
        .filter(plan::Column::Proposal.eq(expected_proposal))
        .filter(plan::Column::Status.eq(expected_status))
        .exec(&txn)
        .await?;
    if plan_write.rows_affected == 0 {
        txn.rollback().await?;
        return Ok(false);
    }

    for manual_pin in manual_pins {
        // The plan write above is the transaction's first statement, so it
        // holds SQLite's writer lock before this read. A worker session that
        // committed first is visible here; one that starts later must wait for
        // this pin and then re-read the current direction route before insert.
        if let Err(err) = pin_initial_direction_route(&txn, thread_id, manual_pin, "confirming").await {
            let _ = txn.rollback().await;
            return Err(err);
        }
    }

    txn.commit().await?;
    ensure_plan_write_survived_workspace_fence(db, thread_id).await?;
    Ok(true)
}

pub async fn get_repo_profile(db: &Db, repo_id: i32) -> Result<Option<repo_profile::Model>> {
    Ok(repo_profile::Entity::find()
        .filter(repo_profile::Column::RepoId.eq(repo_id))
        .one(&db.0)
        .await?)
}

/// Insert or update a repo's profile from the agent curator. `tier` is the
/// architectural tier ("frontend"|"backend"|""), `stack`/`components`
/// are JSON arrays. The vestigial `published`/`deps` columns are pinned to "[]".
/// `relations` are left untouched on update so re-analysis of facts never wipes
/// the agent's cross-repo findings (and stay "[]" on a fresh row).
#[allow(clippy::too_many_arguments)]
pub async fn upsert_repo_profile(
    db: &Db,
    repo_id: i32,
    tier: &str,
    stack: &str,
    summary: &str,
    components: &str,
    source: &str,
    profiled_commit: &str,
) -> Result<repo_profile::Model> {
    if get_repo(db, repo_id).await?.is_none() {
        anyhow::bail!("repo {repo_id} not found");
    }
    let mut a = match get_repo_profile(db, repo_id).await? {
        Some(m) => m.into(),
        None => repo_profile::ActiveModel {
            repo_id: Set(repo_id),
            relations: Set("[]".to_string()),
            published: Set("[]".to_string()),
            deps: Set("[]".to_string()),
            ..Default::default()
        },
    };
    a.role = Set(tier.to_string());
    a.stack = Set(stack.to_string());
    a.summary = Set(summary.to_string());
    a.components = Set(components.to_string());
    a.published = Set("[]".to_string());
    a.deps = Set("[]".to_string());
    a.source = Set(source.to_string());
    a.profiled_commit = Set(profiled_commit.to_string());
    Ok(a.save(&db.0).await?.try_into_model()?)
}

/// Persist the agent curator's inferred relations (JSON array of
/// `profile::AgentRelation`) for a repo, leaving its deterministic facts intact.
/// No-op if the repo has no profile row yet (profiling is eager on add).
pub async fn set_repo_relations(db: &Db, repo_id: i32, relations: &str) -> Result<()> {
    let Some(repo) = get_repo(db, repo_id).await? else {
        return Ok(());
    };
    if let Some(m) = get_repo_profile(db, repo_id).await? {
        let mut a: repo_profile::ActiveModel = m.into();
        a.relations = Set(relations.to_string());
        a.update(&db.0).await?;
        // Any relation mutation makes the workspace's synthesized map doc stale (it
        // narrates the pre-mutation edges). Invalidate it CENTRALLY at this single
        // chokepoint so every path is covered — the agent pass, the manifest seed,
        // and manual calibration all write relations through here. `analyze_relations`
        // re-writes fresh markdown as its LAST step, so the happy path repopulates it;
        // a pass that omits markdown (or a manual calibration) leaves it cleared.
        let _ = clear_repo_map_doc(db, repo.workspace_id).await;
    }
    Ok(())
}

/// Persist the agent-assigned `category` and `domains` JSON for a repo. Only
/// these two columns are touched; all other profile fields (relations, tier, …)
/// are left unchanged. No-op if the repo has no profile row yet.
pub async fn set_repo_category_domains(
    db: &Db,
    repo_id: i32,
    category: &str,
    domains_json: &str,
) -> Result<()> {
    if let Some(m) = get_repo_profile(db, repo_id).await? {
        let mut a: repo_profile::ActiveModel = m.into();
        a.category = Set(category.to_string());
        a.domains = Set(domains_json.to_string());
        a.update(&db.0).await?;
    }
    Ok(())
}

/// Persist the cross-repo curator pass's architectural `layer` label + `layer_rank`
/// for a repo. Only these two columns are touched; all other profile fields are left
/// unchanged. No-op if the repo has no profile row yet.
pub async fn set_repo_layer_rank(
    db: &Db,
    repo_id: i32,
    layer: &str,
    layer_rank: i32,
) -> Result<()> {
    if let Some(m) = get_repo_profile(db, repo_id).await? {
        let mut a: repo_profile::ActiveModel = m.into();
        a.layer = Set(layer.to_string());
        a.layer_rank = Set(layer_rank);
        a.update(&db.0).await?;
    }
    Ok(())
}

/// Persist a repo's analysis run-state (durable across restarts). Clears the
/// error unless state == "failed".
///
/// For a brand-new repo that has no profile row yet, we create a minimal
/// placeholder (role/summary blank) so running/failed states persist and the
/// startup resume scan can find this repo. The placeholder has role="" and
/// summary="" so is_fully_profiled() returns false and it is excluded from the
/// cross-repo relation pass. When state == "idle" and no row exists we skip the
/// insert: idle is the column default, so nothing needs to be persisted.
pub async fn set_analysis_state(
    db: &Db,
    repo_id: i32,
    state: &str,
    error: Option<&str>,
) -> Result<()> {
    if get_repo_profile(db, repo_id).await?.is_none() {
        if state == "idle" {
            return Ok(());
        }
        // Guard against a deletion race: an analysis finishing after
        // delete_repo_cascade must not recreate an orphaned profile row (repo_profile
        // has no enforced foreign key). Mirror edit_profile's guard. (Finding 5)
        if get_repo(db, repo_id).await?.is_none() {
            return Ok(());
        }
        // First-ever analysis: create a minimal placeholder so running/failed
        // persists and the startup resume scan can find this repo.
        upsert_repo_profile(db, repo_id, "", "[]", "", "[]", "agent", "").await?;
    }
    if let Some(m) = get_repo_profile(db, repo_id).await? {
        let mut a: repo_profile::ActiveModel = m.into();
        a.analysis_state = Set(state.to_string());
        a.analysis_error = Set(error.map(|s| s.to_string()));
        a.update(&db.0).await?;
    }
    Ok(())
}

/// Return all `repo_ref` rows whose `repo_profile.analysis_state` matches `state`.
/// Queries profile rows first (no SQL join needed: profiles are keyed by repo_id),
/// then loads the corresponding repo_ref rows, skipping any whose repo was deleted.
pub async fn repos_with_analysis_state(db: &Db, state: &str) -> Result<Vec<repo_ref::Model>> {
    let profiles = repo_profile::Entity::find()
        .filter(repo_profile::Column::AnalysisState.eq(state))
        .all(&db.0)
        .await?;
    let mut out = Vec::with_capacity(profiles.len());
    for p in profiles {
        if let Some(r) = repo_ref::Entity::find_by_id(p.repo_id).one(&db.0).await? {
            out.push(r);
        }
    }
    Ok(out)
}

/// Set a repo's captured `origin` remote URL. Used to backfill rows added before
/// the `remote_url` column existed, so workspace remote-dedup can match them.
/// No-op if the repo is gone.
pub async fn set_repo_remote(db: &Db, repo_id: i32, remote_url: &str) -> Result<()> {
    if let Some(m) = repo_ref::Entity::find_by_id(repo_id).one(&db.0).await? {
        let mut a: repo_ref::ActiveModel = m.into();
        a.remote_url = Set(remote_url.to_string());
        a.update(&db.0).await?;
    }
    Ok(())
}

/// Persist a newly-learned default branch for a repo. Called when materialize
/// discovers (via `live_default_branch`) that the remote's default has changed
/// since the repo was registered, so future offline fallbacks use the current value.
/// Also marks `base_ref_is_default = true`: this value IS the live default, so the
/// offline fallback may now trust it over the main/master chain.
/// Best-effort: a write hiccup (row gone, DB error) is silently ignored.
pub async fn set_repo_base_ref(db: &Db, repo_id: i32, base_ref: &str) -> Result<()> {
    if let Some(m) = repo_ref::Entity::find_by_id(repo_id).one(&db.0).await? {
        let mut a: repo_ref::ActiveModel = m.into();
        a.base_ref = Set(base_ref.to_string());
        a.base_ref_is_default = Set(true);
        a.update(&db.0).await?;
    }
    Ok(())
}

/// Repoint a repo at a new local checkout path. Used when remote-dedup matches a
/// row whose old checkout has gone missing — we keep the fresh clone and update
/// the row rather than orphaning the user. Returns the updated row; None if gone.
pub async fn set_repo_path(db: &Db, repo_id: i32, local_git_path: &str) -> Result<Option<repo_ref::Model>> {
    if let Some(m) = repo_ref::Entity::find_by_id(repo_id).one(&db.0).await? {
        let mut a: repo_ref::ActiveModel = m.into();
        a.local_git_path = Set(local_git_path.to_string());
        return Ok(Some(a.update(&db.0).await?));
    }
    Ok(None)
}

/// Apply one human calibration to a producer repo's relations. `action="add"`
/// upserts a user-sourced relation for `(to, kind)`; `action="remove"` writes a
/// user `rejected` tombstone for that pair so the edge disappears and the auto
/// pass won't resurrect it. Replaces any prior entry for the same `(to, kind)`.
/// Creates a minimal profile row if the producer has none yet (an "analyzing"
/// placeholder), so a human calibration persists instead of silently no-op'ing;
/// the pinned relation is `source="user"` and survives later agent passes.
pub async fn calibrate_repo_relation(
    db: &Db,
    from_id: i32,
    to_id: i32,
    kind: &str,
    via: &str,
    action: &str,
) -> Result<()> {
    // Don't resurrect a deleted repo (no enforced FK on repo_profile): a stale
    // calibration after delete_repo is a no-op rather than an orphaned row.
    if get_repo(db, from_id).await?.is_none() {
        return Ok(());
    }
    let p = match get_repo_profile(db, from_id).await? {
        Some(p) => p,
        None => upsert_repo_profile(db, from_id, "", "[]", "", "[]", "agent", "").await?,
    };
    let mut rels: Vec<crate::profile::AgentRelation> =
        serde_json::from_str(&p.relations).unwrap_or_default();
    // Replace the entry this calibration targets. With a `via`, replace only the
    // SAME-evidence (to, kind, via) entry so a distinct edge isn't erased. A
    // REMOVE with no `via` is a "drop this whole dependency kind": clear every
    // (to, kind) entry immediately so the visible agent edges go away now, not
    // just on a later relation pass.
    let broad_remove = action == "remove" && via.is_empty();
    rels.retain(|r| {
        if broad_remove {
            !(r.to == to_id && r.kind == kind)
        } else {
            !(r.to == to_id && r.kind == kind && r.via == via)
        }
    });
    rels.push(crate::profile::AgentRelation {
        to: to_id,
        kind: kind.to_string(),
        via: via.to_string(),
        confidence: 100,
        source: "user".to_string(),
        rejected: action == "remove",
        ..Default::default()
    });
    let json = serde_json::to_string(&rels).unwrap_or_else(|_| "[]".into());
    set_repo_relations(db, from_id, &json).await?;
    // A pinned edge changes the relative ordering of BOTH endpoints — the consumer's
    // depth AND where the target must sit relative to it (a target whose stored layer
    // currently ranks it ABOVE its new consumer would contradict the edge). The map reads
    // layers, not edges, so clear both stored `layer`/`layer_rank` → both fall back to the
    // tier/category band until the next cross-repo pass re-derives. (No-op for an endpoint
    // without a profile row.)
    set_repo_layer_rank(db, from_id, "", 0).await?;
    set_repo_layer_rank(db, to_id, "", 0).await
}

pub async fn list_directions(db: &Db, thread_id: i32) -> Result<Vec<direction::Model>> {
    Ok(direction::Entity::find()
        .filter(direction::Column::ThreadId.eq(thread_id))
        .all(&db.0)
        .await?)
}

/// Delete a direction row (and any worktree rows referencing it). Used to roll back
/// a half-created direction when materialize fails, so a corrected retry starts clean.
pub async fn delete_direction(db: &Db, direction_id: i32) -> Result<()> {
    worktree::Entity::delete_many()
        .filter(worktree::Column::DirectionId.eq(direction_id))
        .exec(&db.0)
        .await?;
    direction::Entity::delete_by_id(direction_id).exec(&db.0).await?;
    Ok(())
}

/// Create a direction bound to exactly one write repo + a reason (scope rework,
/// spec Part 1). The worktree is materialized separately by `materialize`.
pub async fn create_direction(
    db: &Db,
    thread_id: i32,
    name: &str,
    tool: &str,
    repo_id: i32,
    reason: &str,
    mandate: &str,
    base_branch: &str,
) -> Result<direction::Model> {
    create_direction_with_engine_pin(
        db,
        thread_id,
        name,
        tool,
        repo_id,
        reason,
        mandate,
        base_branch,
        false,
    )
    .await
}

/// Create a direction with the provenance of its selected engine. Manual routing
/// must persist the pin in the same insert as the direction so a process exit
/// cannot leave an otherwise reusable direction unpinned.
pub async fn create_direction_with_engine_pin(
    db: &Db,
    thread_id: i32,
    name: &str,
    tool: &str,
    repo_id: i32,
    reason: &str,
    mandate: &str,
    base_branch: &str,
    engine_pinned: bool,
) -> Result<direction::Model> {
    let t = thread::Entity::find_by_id(thread_id)
        .one(&db.0)
        .await?
        .ok_or_else(|| anyhow::anyhow!("thread {thread_id} not found"))?;
    ensure_workspace_accepts_writes(db, t.workspace_id).await?;
    let existing: Vec<String> = direction::Entity::find()
        .filter(direction::Column::ThreadId.eq(thread_id))
        .all(&db.0)
        .await?
        .into_iter()
        .map(|d| d.slug)
        .collect();
    let repo_ref = ensure_repo_workspace_accepts_writes(db, repo_id).await?;
    let slug = unique_slug(name, &existing);
    let branch_title = if t.title.trim().is_empty() {
        name
    } else {
        &t.title
    };
    // Branches/worktrees are keyed per repo, so dedup against branches ALREADY
    // reserved by other directions on this repo — not just git refs. Otherwise two
    // directions created before the first worktree materializes derive the same
    // branch from the same title and collide on `.worktrees/weft/<branch>`.
    let reserved: Vec<String> = direction::Entity::find()
        .filter(direction::Column::RepoId.eq(repo_id))
        .all(&db.0)
        .await?
        .into_iter()
        .map(|d| d.branch)
        .filter(|b| !b.is_empty())
        .collect();
    let branch = crate::git::choose_branch_name(
        std::path::Path::new(&repo_ref.local_git_path),
        &t.kind,
        branch_title,
        &reserved,
    );
    ensure_workspace_accepts_writes(db, t.workspace_id).await?;
    let dir = direction::ActiveModel {
        thread_id: Set(thread_id),
        name: Set(name.to_string()),
        slug: Set(slug),
        tool: Set(tool.to_string()),
        branch: Set(branch),
        status: Set("queued".to_string()),
        repo_id: Set(repo_id),
        reason: Set(reason.to_string()),
        engine_pinned: Set(engine_pinned),
        mandate: Set(normalize_mandate(mandate).to_string()),
        base_branch: Set(base_branch.trim().to_string()),
        target_branch: Set(base_branch.trim().to_string()),
        created_at: Set(now()),
        ..Default::default()
    }
    .insert(&db.0)
    .await?;
    let accepted = match ensure_workspace_accepts_writes(db, t.workspace_id).await {
        Ok(_) => ensure_repo_workspace_accepts_writes(db, repo_id).await.map(|_| ()),
        Err(err) => Err(err),
    };
    if let Err(err) = accepted {
        let _ = delete_direction(db, dir.id).await;
        return Err(err);
    }
    Ok(dir)
}

/// Anything that isn't explicitly "impl-only" is the default "plan+impl".
pub fn normalize_mandate(m: &str) -> &'static str {
    if m == "impl-only" {
        "impl-only"
    } else {
        "plan+impl"
    }
}

pub async fn get_direction(db: &Db, direction_id: i32) -> Result<Option<direction::Model>> {
    Ok(direction::Entity::find_by_id(direction_id)
        .one(&db.0)
        .await?)
}

pub async fn set_direction_engine_pinned(
    db: &Db,
    direction_id: i32,
    pinned: bool,
) -> Result<()> {
    direction::Entity::update_many()
        .col_expr(direction::Column::EnginePinned, Expr::value(pinned))
        .filter(direction::Column::Id.eq(direction_id))
        .exec(&db.0)
        .await?;
    Ok(())
}

/// Refresh the initial automatic route of a direction that has never been
/// manually pinned. When a session row already exists but has no native
/// conversation, both persistent identities move together so the next open
/// cannot recreate the stale engine selected at plan-confirm time.
pub async fn refresh_unpinned_direction_route(
    db: &Db,
    direction_id: i32,
    session_id: Option<i32>,
    tool: &str,
) -> Result<()> {
    refresh_unpinned_direction_route_with_pin(db, direction_id, session_id, tool, false).await
}

/// See [`refresh_unpinned_direction_route`]. A reused, never-started direction
/// can receive an explicit manual selection during a retry, which must become
/// a pin in the same conditional write as its refreshed tool.
pub async fn refresh_unpinned_direction_route_with_pin(
    db: &Db,
    direction_id: i32,
    session_id: Option<i32>,
    tool: &str,
    engine_pinned: bool,
) -> Result<()> {
    use sea_orm::TransactionTrait;

    let txn = db.0.begin().await?;
    let direction_write = direction::Entity::update_many()
        .col_expr(direction::Column::Tool, Expr::value(tool))
        .col_expr(direction::Column::EnginePinned, Expr::value(engine_pinned))
        .filter(direction::Column::Id.eq(direction_id))
        .filter(direction::Column::EnginePinned.eq(false))
        .exec(&txn)
        .await?;
    if direction_write.rows_affected == 0 {
        anyhow::bail!("direction {direction_id} became manually pinned while refreshing its route");
    }

    if let Some(session_id) = session_id {
        let session_write = session::Entity::update_many()
            .col_expr(session::Column::Tool, Expr::value(tool))
            .col_expr(session::Column::EnginePinned, Expr::value(engine_pinned))
            .col_expr(session::Column::Command, Expr::value(Option::<String>::None))
            .col_expr(session::Column::Model, Expr::value(Option::<String>::None))
            .col_expr(
                session::Column::NativeSessionId,
                Expr::value(Option::<String>::None),
            )
            .filter(session::Column::Id.eq(session_id))
            .filter(session::Column::DirectionId.eq(direction_id))
            .filter(session::Column::EnginePinned.eq(false))
            // A freshly captured native id establishes a real conversation.
            // Do not clear it under a stale automatic-route snapshot.
            .filter(session::Column::NativeSessionId.is_null())
            .exec(&txn)
            .await;
        let session_write = match session_write {
            Ok(session_write) => session_write,
            Err(err) => {
                let _ = txn.rollback().await;
                return Err(err.into());
            }
        };
        if session_write.rows_affected == 0 {
            let _ = txn.rollback().await;
            anyhow::bail!(
                "session {session_id} became established or pinned while refreshing its route"
            );
        }
    }

    txn.commit().await?;
    Ok(())
}

/// Persist a manual route only while a direction is still sessionless. The
/// direction update takes SQLite's writer lock before the session check, so a
/// concurrent worker open either commits first (this returns an error) or waits
/// and observes the manual route before it creates its session.
pub async fn pin_unstarted_unpinned_direction_route(
    db: &Db,
    direction_id: i32,
    tool: &str,
) -> Result<()> {
    use sea_orm::TransactionTrait;

    let txn = db.0.begin().await?;
    let direction_write = direction::Entity::update_many()
        .col_expr(direction::Column::Tool, Expr::value(tool))
        .col_expr(direction::Column::EnginePinned, Expr::value(true))
        .filter(direction::Column::Id.eq(direction_id))
        .filter(direction::Column::EnginePinned.eq(false))
        .exec(&txn)
        .await?;
    if direction_write.rows_affected == 0 {
        txn.rollback().await?;
        anyhow::bail!("direction {direction_id} became manually pinned while pinning its route");
    }
    let opened_session = session::Entity::find()
        .filter(session::Column::DirectionId.eq(direction_id))
        .one(&txn)
        .await?;
    if opened_session.is_some() {
        txn.rollback().await?;
        anyhow::bail!("direction {direction_id} opened while pinning its manual route");
    }
    txn.commit().await?;
    Ok(())
}

/// Set a direction's lifecycle status (agent- or human-driven). No-op if gone.
pub async fn set_direction_status(db: &Db, direction_id: i32, status: &str) -> Result<()> {
    if let Some(d) = direction::Entity::find_by_id(direction_id)
        .one(&db.0)
        .await?
    {
        let mut a: direction::ActiveModel = d.into();
        a.status = Set(status.to_string());
        a.update(&db.0).await?;
    }
    Ok(())
}

/// Display-name only; slug AND branch stay (live worktrees keep working).
pub async fn rename_direction(db: &Db, direction_id: i32, name: &str) -> Result<direction::Model> {
    let name = validate_display_name(name, "task name")?;
    let m = direction::Entity::find_by_id(direction_id)
        .one(&db.0)
        .await?
        .ok_or_else(|| anyhow::anyhow!("direction {direction_id} not found"))?;
    let dup = direction::Entity::find()
        .filter(direction::Column::ThreadId.eq(m.thread_id))
        .filter(direction::Column::Name.eq(name))
        .filter(direction::Column::Id.ne(direction_id))
        .one(&db.0)
        .await?;
    if dup.is_some() {
        anyhow::bail!("another task in this issue already named {name:?}");
    }
    let mut a: direction::ActiveModel = m.into();
    a.name = Set(name.to_string());
    Ok(a.update(&db.0).await?)
}

/// Atomically switch BOTH halves of a worker's engine identity/model override
/// (issue #96/#98) — `direction.tool` (the durable side `chat_open_worker_impl`
/// reads whenever it (re)creates a session, e.g. the very next open after this
/// switch cleared the session's native id, which flips that function's
/// resume-vs-recreate branch to "recreate") AND `session.tool`/`session.model`
/// (the live side `worker_engine`/every `chat_send` reads). ONE transaction,
/// not two independent `.update()` calls: a torn write — the direction commits
/// but the session write fails, or vice versa — would leave the two readers
/// disagreeing about which tool this worker is actually running, silently
/// reintroducing #96's core confusion in a harder-to-notice shape (the panel
/// shows the new tool; the next message goes to the old one). Also clears
/// `session.command`: a per-tool alias pin from the OLD tool would otherwise
/// try to spawn the NEW tool identity under the old alias binary. No-op for
/// the session half if that row is gone (a session can be reclaimed between
/// the caller's lookup and this write — moot, not a failure, same posture as
/// the old `switch_session_tool`); the direction half is required (not found
/// is a real error, same as before).
pub async fn switch_worker_engine_txn(
    db: &Db,
    direction_id: i32,
    session_id: i32,
    tool: &str,
    model: Option<&str>,
) -> Result<()> {
    switch_worker_engine_txn_with_pin(db, direction_id, session_id, tool, model, true).await
}

/// See [`switch_lead_engine_txn_with_pin`]. The direction and its live session
/// carry the same provenance, so both are updated atomically.
pub async fn switch_worker_engine_txn_with_pin(
    db: &Db,
    direction_id: i32,
    session_id: i32,
    tool: &str,
    model: Option<&str>,
    pinned: bool,
) -> Result<()> {
    use sea_orm::TransactionTrait;
    let txn = db.0.begin().await?;
    // Write first — see `switch_lead_engine_txn` for why a deferred
    // read→write upgrade is not safe under WAL.
    let touched = direction::Entity::update_many()
        .col_expr(direction::Column::Tool, Expr::value(tool))
        .col_expr(direction::Column::EnginePinned, Expr::value(pinned))
        .filter(direction::Column::Id.eq(direction_id))
        .exec(&txn)
        .await?;
    if touched.rows_affected == 0 {
        anyhow::bail!("direction {direction_id} not found");
    }
    probe_after_first_statement!();
    let thread_id = direction::Entity::find_by_id(direction_id)
        .one(&txn)
        .await?
        .map(|d| d.thread_id)
        .ok_or_else(|| anyhow::anyhow!("direction {direction_id} vanished mid-transaction"))?;

    if let Some(s) = session::Entity::find_by_id(session_id).one(&txn).await? {
        let mut sa: session::ActiveModel = s.into();
        sa.tool = Set(tool.to_string());
        sa.engine_pinned = Set(pinned);
        sa.command = Set(None);
        sa.model = Set(model.map(str::to_string));
        // The native-id clear rides the SAME row update (issue #96 pitfall 1).
        // It used to be a separate write after this transaction committed,
        // which meant a failure there left the new tool paired with the OLD
        // engine's native id — a pair `worker_engine` would then try to resume
        // across engines, and one that no grace window repairs (adversarial
        // re-review of PR #140, round 6). Atomic here, that pair cannot exist.
        sa.native_session_id = Set(None);
        sa.update(&txn).await?;
    }
    // Same commit as the writes above — see the lead twin.
    insert_marker_row(&txn, thread_id, Some(session_id), MARKER_KIND_RECOVERED).await?;
    txn.commit().await?;
    Ok(())
}

/// A direction's diff "vs target" config: `(stored, base_ref)` where `stored`
/// is the per-task target branch ("" = use the repo default) and `base_ref` is
/// the bound repo's default branch (the effective default). Both empty if the
/// direction or its repo is gone.
pub async fn direction_target_branch(db: &Db, direction_id: i32) -> Result<(String, String)> {
    let Some(d) = direction::Entity::find_by_id(direction_id)
        .one(&db.0)
        .await?
    else {
        return Ok((String::new(), String::new()));
    };
    let base_ref = if d.repo_id == 0 {
        String::new()
    } else {
        repo_ref::Entity::find_by_id(d.repo_id)
            .one(&db.0)
            .await?
            .map(|r| r.base_ref)
            .unwrap_or_default()
    };
    Ok((d.target_branch, base_ref))
}

/// Persist a direction's diff target branch. Trimmed; "" means "use the repo
/// default". No-op if the direction is gone.
pub async fn set_direction_target_branch(
    db: &Db,
    direction_id: i32,
    target: &str,
) -> Result<()> {
    if let Some(d) = direction::Entity::find_by_id(direction_id)
        .one(&db.0)
        .await?
    {
        let mut a: direction::ActiveModel = d.into();
        a.target_branch = Set(target.trim().to_string());
        a.update(&db.0).await?;
    }
    Ok(())
}

/// Persist a direction's base branch (the immutable ref its worktree was branched
/// off). Set once at materialize for the default-base case; not user-editable after.
pub async fn set_direction_base_branch(
    db: &Db,
    direction_id: i32,
    base: &str,
) -> Result<()> {
    if let Some(d) = direction::Entity::find_by_id(direction_id)
        .one(&db.0)
        .await?
    {
        let mut a: direction::ActiveModel = d.into();
        a.base_branch = Set(base.trim().to_string());
        a.update(&db.0).await?;
    }
    Ok(())
}

/// The single write repo bound to a direction (scope rework). None if the
/// direction has no repo set (repo_id = 0) or the repo row is gone.
pub async fn direction_repo_of(db: &Db, direction_id: i32) -> Result<Option<repo_ref::Model>> {
    let Some(d) = direction::Entity::find_by_id(direction_id)
        .one(&db.0)
        .await?
    else {
        return Ok(None);
    };
    if d.repo_id == 0 {
        return Ok(None);
    }
    Ok(repo_ref::Entity::find_by_id(d.repo_id).one(&db.0).await?)
}

pub async fn record_worktree(
    db: &Db,
    repo_id: i32,
    direction_id: i32,
    branch: &str,
    path: &str,
    created_branch: bool,
    created_checkout: bool,
    base_commit: &str,
) -> Result<worktree::Model> {
    let direction = direction::Entity::find_by_id(direction_id)
        .one(&db.0)
        .await?
        .ok_or_else(|| anyhow::anyhow!("direction {direction_id} not found"))?;
    let thread = thread::Entity::find_by_id(direction.thread_id)
        .one(&db.0)
        .await?
        .ok_or_else(|| anyhow::anyhow!("thread {} not found", direction.thread_id))?;
    ensure_workspace_accepts_writes(db, thread.workspace_id).await?;
    ensure_repo_workspace_accepts_writes(db, repo_id).await?;
    let inserted = worktree::ActiveModel {
        repo_id: Set(repo_id),
        direction_id: Set(direction_id),
        branch: Set(branch.to_string()),
        path: Set(path.to_string()),
        created_at: Set(now()),
        created_branch: Set(created_branch),
        created_checkout: Set(created_checkout),
        base_commit: Set(base_commit.to_string()),
        ..Default::default()
    }
    .insert(&db.0)
    .await?;
    let accepted = match ensure_workspace_accepts_writes(db, thread.workspace_id).await {
        Ok(_) => ensure_repo_workspace_accepts_writes(db, repo_id).await.map(|_| ()),
        Err(err) => Err(err),
    };
    if let Err(err) = accepted {
        let _ = worktree::Entity::delete_by_id(inserted.id).exec(&db.0).await;
        return Err(err);
    }
    Ok(inserted)
}

/// Persist the recorded fork-point commit on a worktree row. Used on RE-materialize when a
/// reclaimed lane's recreate CREATED a fresh branch (the original was deleted) and the row's
/// base_commit was still empty (legacy/reuse) — so the new fork point becomes the stable
/// ancestry anchor. Callers MUST NOT overwrite a non-empty base_commit: the ORIGINAL fork
/// point is the authoritative one.
pub async fn set_worktree_base_commit(
    db: &Db,
    worktree_id: i32,
    base_commit: &str,
) -> Result<()> {
    worktree::ActiveModel {
        id: Set(worktree_id),
        base_commit: Set(base_commit.to_string()),
        ..Default::default()
    }
    .update(&db.0)
    .await?;
    Ok(())
}

/// Persist updated ownership flags on a worktree row. Used when re-materializing a
/// reclaimed worktree CREATES a fresh branch/checkout because the original was deleted:
/// weft now owns what it just made, so the flags are OR'd up (never cleared) and later
/// cleanup/rollback correctly tears the new branch/checkout down.
pub async fn set_worktree_ownership(
    db: &Db,
    worktree_id: i32,
    created_branch: bool,
    created_checkout: bool,
) -> Result<()> {
    worktree::ActiveModel {
        id: Set(worktree_id),
        created_branch: Set(created_branch),
        created_checkout: Set(created_checkout),
        ..Default::default()
    }
    .update(&db.0)
    .await?;
    Ok(())
}

pub async fn list_worktrees(db: &Db, direction_id: Option<i32>) -> Result<Vec<worktree::Model>> {
    let q = worktree::Entity::find();
    let q = match direction_id {
        Some(id) => q.filter(worktree::Column::DirectionId.eq(id)),
        None => q,
    };
    Ok(q.all(&db.0).await?)
}

pub async fn worktree_for(
    db: &Db,
    direction_id: i32,
    repo_id: i32,
) -> Result<Option<worktree::Model>> {
    Ok(worktree::Entity::find()
        .filter(worktree::Column::DirectionId.eq(direction_id))
        .filter(worktree::Column::RepoId.eq(repo_id))
        .one(&db.0)
        .await?)
}

/// Remove a repo from a workspace and all Weft state derived from it: its
/// profile, the directions bound to it (a direction has one write repo) with
/// their sessions, and its worktree rows. Returns the worktrees
/// (worktree_id, repo_id, path, branch, created_branch, created_checkout) the
/// caller must physically `git worktree remove` — DB rows are gone after this.
/// `created_branch` gates whether the branch is deleted; `created_checkout`
/// gates whether `git worktree remove` is called (a reused pre-existing
/// checkout path must survive). NEVER touches the user's actual repo directory
/// at `local_git_path`.
pub async fn delete_repo_cascade(
    db: &Db,
    repo_id: i32,
) -> Result<Vec<(i32, i32, String, String, bool, bool)>> {
    // The workspace's repo-map doc enumerates repos/edges, so removing a repo makes
    // it stale. Capture the workspace before the repo_ref row is deleted below; the
    // doc is invalidated at the end (it regenerates on the next analysis pass or a
    // manual Regenerate). Nothing else clears it on delete, so without this the map
    // pane keeps showing the deleted repo until a later manual analysis.
    let workspace_id = get_repo(db, repo_id).await?.map(|r| r.workspace_id);
    // Worktrees registered for this repo (each direction's worktree is keyed to
    // its write repo, so this covers the bound directions' worktrees too).
    let removed: Vec<(i32, i32, String, String, bool, bool)> = worktree::Entity::find()
        .filter(worktree::Column::RepoId.eq(repo_id))
        .all(&db.0)
        .await?
        .into_iter()
        .map(|w| (w.id, w.repo_id, w.path, w.branch, w.created_branch, w.created_checkout))
        .collect();
    // Sessions of the directions bound to this repo, plus any keyed to the repo.
    let dirs = direction::Entity::find()
        .filter(direction::Column::RepoId.eq(repo_id))
        .all(&db.0)
        .await?;
    for d in &dirs {
        session::Entity::delete_many()
            .filter(session::Column::DirectionId.eq(d.id))
            .exec(&db.0)
            .await?;
    }
    session::Entity::delete_many()
        .filter(session::Column::RepoId.eq(repo_id))
        .exec(&db.0)
        .await?;
    // Code checkpoints die with their worktrees (rows here; the caller removes
    // the shadow repos — see delete_repo).
    for (wt_id, ..) in &removed {
        delete_code_checkpoints_for_worktree(db, *wt_id).await?;
    }
    worktree::Entity::delete_many()
        .filter(worktree::Column::RepoId.eq(repo_id))
        .exec(&db.0)
        .await?;
    direction::Entity::delete_many()
        .filter(direction::Column::RepoId.eq(repo_id))
        .exec(&db.0)
        .await?;
    repo_profile::Entity::delete_many()
        .filter(repo_profile::Column::RepoId.eq(repo_id))
        .exec(&db.0)
        .await?;
    repo_ref::Entity::delete_by_id(repo_id).exec(&db.0).await?;
    // Best-effort: invalidate the now-stale workspace map doc (see top of fn).
    if let Some(ws) = workspace_id {
        let _ = clear_repo_map_doc(db, ws).await;
    }
    Ok(removed)
}

/// Delete a workspace and every Weft-owned row under it. Returns worktree
/// cleanup tuples for the command layer, which still owns filesystem cleanup.
/// The canonical user repos at `repo_ref.local_git_path` are never removed.
pub async fn delete_workspace_cascade(
    db: &Db,
    workspace_id: i32,
) -> Result<Vec<(i32, i32, String, String, bool, bool)>> {
    mark_workspace_deleting(db, workspace_id).await?;

    let repos = list_repos(db, workspace_id).await?;
    let repo_ids: Vec<i32> = repos.iter().map(|r| r.id).collect();
    let threads = list_threads(db, workspace_id).await?;
    let thread_ids: Vec<i32> = threads.iter().map(|t| t.id).collect();
    let mut directions = Vec::new();
    for thread_id in &thread_ids {
        directions.extend(list_directions(db, *thread_id).await?);
    }
    let direction_ids: Vec<i32> = directions.iter().map(|d| d.id).collect();
    let repo_session_ids: Vec<i32> = if repo_ids.is_empty() {
        Vec::new()
    } else {
        session::Entity::find()
            .filter(session::Column::RepoId.is_in(repo_ids.clone()))
            .all(&db.0)
            .await?
            .into_iter()
            .map(|s| s.id)
            .collect()
    };

    let mut removed = Vec::new();
    for worktree in worktree::Entity::find().all(&db.0).await? {
        if repo_ids.contains(&worktree.repo_id) || direction_ids.contains(&worktree.direction_id) {
            removed.push((
                worktree.id,
                worktree.repo_id,
                worktree.path,
                worktree.branch,
                worktree.created_branch,
                worktree.created_checkout,
            ));
        }
    }
    // Code checkpoints die with their worktrees (rows here; the caller removes
    // the shadow repos — see cleanup_worktrees).
    for (wt_id, ..) in &removed {
        delete_code_checkpoints_for_worktree(db, *wt_id).await?;
    }

    for thread_id in &thread_ids {
        im_route::Entity::delete_many()
            .filter(im_route::Column::ThreadId.eq(*thread_id))
            .exec(&db.0)
            .await?;
        lead_message::Entity::delete_many()
            .filter(lead_message::Column::ThreadId.eq(*thread_id))
            .exec(&db.0)
            .await?;
        plan::Entity::delete_many()
            .filter(plan::Column::ThreadId.eq(*thread_id))
            .exec(&db.0)
            .await?;
        test_plan::Entity::delete_many()
            .filter(test_plan::Column::ThreadId.eq(*thread_id))
            .exec(&db.0)
            .await?;
    }
    if !repo_session_ids.is_empty() {
        lead_message::Entity::delete_many()
            .filter(lead_message::Column::SessionId.is_in(repo_session_ids.clone()))
            .exec(&db.0)
            .await?;
    }

    for direction_id in &direction_ids {
        session::Entity::delete_many()
            .filter(session::Column::DirectionId.eq(*direction_id))
            .exec(&db.0)
            .await?;
        worktree::Entity::delete_many()
            .filter(worktree::Column::DirectionId.eq(*direction_id))
            .exec(&db.0)
            .await?;
        direction::Entity::delete_by_id(*direction_id).exec(&db.0).await?;
    }

    for repo_id in &repo_ids {
        session::Entity::delete_many()
            .filter(session::Column::RepoId.eq(*repo_id))
            .exec(&db.0)
            .await?;
        worktree::Entity::delete_many()
            .filter(worktree::Column::RepoId.eq(*repo_id))
            .exec(&db.0)
            .await?;
        direction::Entity::delete_many()
            .filter(direction::Column::RepoId.eq(*repo_id))
            .exec(&db.0)
            .await?;
        repo_profile::Entity::delete_many()
            .filter(repo_profile::Column::RepoId.eq(*repo_id))
            .exec(&db.0)
            .await?;
        repo_ref::Entity::delete_by_id(*repo_id).exec(&db.0).await?;
    }

    for thread_id in &thread_ids {
        thread::Entity::delete_by_id(*thread_id).exec(&db.0).await?;
    }

    skill_enable::Entity::delete_many()
        .filter(skill_enable::Column::Scope.eq(format!("ws:{workspace_id}")))
        .exec(&db.0)
        .await?;
    let _ = clear_repo_map_doc(db, workspace_id).await;
    let _ = delete_setting(db, &curator_thread_key(workspace_id)).await;
    workspace::Entity::delete_by_id(workspace_id).exec(&db.0).await?;
    let _ = clear_workspace_deleting(db, workspace_id).await;

    Ok(removed)
}

/// Delete a thread and everything under it. Returns the worktree paths that the
/// caller must physically remove via git (DB rows are gone after this).
/// Each tuple is (worktree_id, repo_id, path, branch, created_branch,
/// created_checkout): `created_branch` gates branch deletion; `created_checkout`
/// gates worktree directory removal (a reused pre-existing checkout must
/// survive); `worktree_id` names the shadow repo of code checkpoints to remove.
pub async fn delete_thread_cascade(
    db: &Db,
    thread_id: i32,
) -> Result<Vec<(i32, i32, String, String, bool, bool)>> {
    use sea_orm::TransactionTrait;
    // One TRANSACTION for the whole cascade. Atomicity gives both halves of
    // the safety story at once: concurrent writers can't observe (and race)
    // the intermediate state between the thread row's delete and the
    // owned-row sweep, and a crash/error mid-cascade rolls back to a fully
    // retryable issue instead of stranding orphaned owned rows whose anchor
    // is already gone.
    let txn = db.0.begin().await?;
    let dirs = direction::Entity::find()
        .filter(direction::Column::ThreadId.eq(thread_id))
        .all(&txn)
        .await?;
    // (worktree_id, repo_id, worktree path, branch, created_branch, created_checkout)
    let mut removed: Vec<(i32, i32, String, String, bool, bool)> = Vec::new();
    for d in &dirs {
        let wts = worktree::Entity::find()
            .filter(worktree::Column::DirectionId.eq(d.id))
            .all(&txn)
            .await?;
        for w in wts {
            removed.push((w.id, w.repo_id, w.path.clone(), w.branch.clone(), w.created_branch, w.created_checkout));
            // Code checkpoints die with their worktree (the shadow repo itself
            // is removed by the caller's cleanup pass).
            code_checkpoint::Entity::delete_many()
                .filter(code_checkpoint::Column::WorktreeId.eq(w.id))
                .exec(&txn)
                .await?;
            worktree::Entity::delete_by_id(w.id).exec(&txn).await?;
        }
        session::Entity::delete_many()
            .filter(session::Column::DirectionId.eq(d.id))
            .exec(&txn)
            .await?;
        direction::Entity::delete_by_id(d.id).exec(&txn).await?;
    }
    // The thread row anchors the thread write fence
    // (ensure_thread_workspace_accepts_writes errs once it is gone); inside
    // the transaction its delete becomes visible together with the owned-row
    // sweep, so a racing save/sentinel either lands entirely before the
    // cascade (then dies with it) or is rejected by the fence after commit.
    thread::Entity::delete_by_id(thread_id).exec(&txn).await?;
    // Thread-owned rows (no FK cascades in sqlite here): chat history, the
    // pending plan, IM bindings, and the test-case document all die with the
    // issue — otherwise deleted-issue content lingers in weft.db and backups.
    im_route::Entity::delete_many()
        .filter(im_route::Column::ThreadId.eq(thread_id))
        .exec(&txn)
        .await?;
    lead_message::Entity::delete_many()
        .filter(lead_message::Column::ThreadId.eq(thread_id))
        .exec(&txn)
        .await?;
    plan::Entity::delete_many()
        .filter(plan::Column::ThreadId.eq(thread_id))
        .exec(&txn)
        .await?;
    test_plan::Entity::delete_many()
        .filter(test_plan::Column::ThreadId.eq(thread_id))
        .exec(&txn)
        .await?;
    txn.commit().await?;
    Ok(removed)
}

pub async fn create_session(
    db: &Db,
    direction_id: i32,
    repo_id: i32,
    tool: &str,
    cwd: &str,
) -> Result<session::Model> {
    let direction = ensure_direction_workspace_accepts_writes(db, direction_id).await?;
    ensure_repo_workspace_accepts_writes(db, repo_id).await?;
    let inserted = session::ActiveModel {
        direction_id: Set(direction_id),
        repo_id: Set(repo_id),
        tool: Set(tool.to_string()),
        engine_pinned: Set(direction.engine_pinned),
        cwd: Set(cwd.to_string()),
        native_session_id: Set(None),
        status: Set("starting".to_string()),
        created_at: Set(now()),
        ..Default::default()
    }
    .insert(&db.0)
    .await?;
    let accepted = match ensure_thread_workspace_accepts_writes(db, direction.thread_id).await {
        Ok(_) => ensure_repo_workspace_accepts_writes(db, repo_id).await.map(|_| ()),
        Err(err) => Err(err),
    };
    if let Err(err) = accepted {
        let _ = session::Entity::delete_by_id(inserted.id).exec(&db.0).await;
        return Err(err);
    }
    Ok(inserted)
}

/// Create a worker session from the direction route currently stored in the
/// database. The opening write reserves the route before it is read, so a
/// concurrent manual pin cannot leave a newly inserted session on an older
/// automatic tool.
pub async fn create_session_for_current_direction(
    db: &Db,
    direction_id: i32,
    repo_id: i32,
    cwd: &str,
) -> Result<session::Model> {
    ensure_direction_workspace_accepts_writes(db, direction_id).await?;
    ensure_repo_workspace_accepts_writes(db, repo_id).await?;

    use sea_orm::TransactionTrait;

    let txn = db.0.begin().await?;
    // Acquire the writer lock before reading the route. A no-op assignment is
    // enough and keeps the direction's durable values unchanged.
    let lock = direction::Entity::update_many()
        .col_expr(
            direction::Column::EnginePinned,
            Expr::col(direction::Column::EnginePinned).into(),
        )
        .filter(direction::Column::Id.eq(direction_id))
        .exec(&txn)
        .await?;
    if lock.rows_affected == 0 {
        txn.rollback().await?;
        anyhow::bail!("direction {direction_id} not found");
    }
    let direction = direction::Entity::find_by_id(direction_id)
        .one(&txn)
        .await?
        .ok_or_else(|| anyhow::anyhow!("direction {direction_id} not found"))?;
    let inserted = session::ActiveModel {
        direction_id: Set(direction_id),
        repo_id: Set(repo_id),
        tool: Set(direction.tool.clone()),
        engine_pinned: Set(direction.engine_pinned),
        cwd: Set(cwd.to_string()),
        native_session_id: Set(None),
        status: Set("starting".to_string()),
        created_at: Set(now()),
        ..Default::default()
    }
    .insert(&txn)
    .await?;
    txn.commit().await?;

    let accepted = match ensure_thread_workspace_accepts_writes(db, direction.thread_id).await {
        Ok(_) => ensure_repo_workspace_accepts_writes(db, repo_id).await.map(|_| ()),
        Err(err) => Err(err),
    };
    if let Err(err) = accepted {
        let _ = session::Entity::delete_by_id(inserted.id).exec(&db.0).await;
        return Err(err);
    }
    Ok(inserted)
}

pub async fn set_session_native_id(db: &Db, session_id: i32, native_id: &str) -> Result<()> {
    if let Some(s) = session::Entity::find_by_id(session_id).one(&db.0).await? {
        let mut a: session::ActiveModel = s.into();
        a.native_session_id = Set(Some(native_id.to_string()));
        // Capturing the native id does NOT mean a turn is running. The readers
        // call this on every attach (including an idle re-attach for command
        // discovery), so writing "running" here would make the boot revive sweep
        // treat an idle resume as interrupted work. Status is owned by the turn
        // boundaries (persist_activity); a real turn is already "running" by now.
        a.update(&db.0).await?;
    }
    Ok(())
}

/// `set_session_native_id` variant that can also CLEAR the id: conversation
/// rewind uses None for "back to before the first message" (the next turn
/// starts a brand-new native session).
pub async fn set_session_native_id_opt(
    db: &Db,
    session_id: i32,
    native_id: Option<&str>,
) -> Result<()> {
    if let Some(s) = session::Entity::find_by_id(session_id).one(&db.0).await? {
        let mut a: session::ActiveModel = s.into();
        a.native_session_id = Set(native_id.map(str::to_string));
        a.update(&db.0).await?;
    }
    Ok(())
}

/// Stamp a (thread, session) as having just gone through the turn-freeze
/// auto-recovery (issue #93): an invisible timeline marker row (`kind`
/// excluded from the frontend's text/tool timeline allowlist, same as
/// `"meta"`). `session_id = None` for the lead. Uses the SAME
/// deletion-fenced insert as the rest of the timeline (`insert_lead_message`),
/// so a thread deleted mid-recovery can't leave an orphaned row.
///
/// This row IS the issue #116 coordination point: its `created_at`, read back
/// via [`last_turn_freeze_recovery_secs`], is what
/// `lead_chat::revive::freeze_recovery_state` consults to withhold a
/// just-self-healed lead/worker from the idle re-drive for one grace window,
/// rather than racing this recovery straight back into the same wedge.
///
/// History, because the shape here only makes sense with it (review round 4,
/// P2): #116 originally landed WITHOUT that consult — `revive.rs` never read
/// this marker, and the getter had no caller outside this file's own
/// round-trip test. What kept the re-drive off a just-recovered direction in
/// the meantime was an unrelated SIDE EFFECT: `recover_from_freeze` also
/// clears the session's `native_session_id` (see `set_session_native_id_opt` /
/// `set_lead_native_id_opt`), and `revive::stalled_direction_ids` only selects
/// a direction whose `native_session_id.is_some()`. Real protection, but
/// accidental — it depended entirely on THAT field staying cleared at THAT
/// moment, and would have vanished silently under a refactor that stopped
/// clearing it, or a re-drive path that stopped gating on it. The grace window
/// no longer RIDES on that: it reads this marker directly, and has tests that
/// go red if the read is removed.
///
/// The native-id clear is still there, but it is NOT a second guard — `revive`
/// deliberately stopped treating a missing native id as "not selectable",
/// because that is what made a freeze-recovered session invisible to the
/// re-drive forever instead of for one window. The dependency now runs the
/// other way: `recover_from_freeze` clears the id ONLY if this row was
/// stamped, since this row is the sole evidence separating "never ran" from
/// "ran, and the recovery cleared its id" (`revive::has_resumable_context`).
/// Clearing after a failed stamp would erase that evidence and strand the
/// session permanently.
///
/// Reused (not just freeze recovery) by `lead_chat::commands::{switch_lead_tool,
/// switch_worker_tool}` (issue #96/#98, adversarial re-review of PR #139, P2):
/// a deliberate engine/model switch also clears the native id and lands the
/// engine at idle, which is the EXACT shape `revive`'s stall sweep looks for —
/// without this stamp, a thread/session that had EVER gone through a genuine
/// freeze-recovery at any point in its (possibly much older) history would
/// read `has_resumable_context() == true` from that stale marker alone once
/// its OWN grace window had long since elapsed, letting the very next sweep
/// tick (every 60s) auto-redrive the freshly-switched, not-yet-human-verified
/// engine into a "resume stalled work" prompt — a false positive with no
/// connection to the switch that just happened. Calling this on a switch too
/// re-stamps the grace window with the CURRENT time, so `revive`'s existing
/// (unmodified) cooldown check holds off exactly the way it already does
/// after a real freeze recovery. The name stays freeze-scoped (renaming would
/// touch `recover_from_freeze`'s established call site for no behavioral
/// gain); read it as "the native context was deliberately reset and the next
/// automated re-drive should back off for one grace window", of which a
/// self-healed freeze is one cause and a human-initiated switch is another.
///
/// The switch path gates on this row the same way `recover_from_freeze` does,
/// only harder: `lead_chat::commands::persist_switch` stamps it FIRST and
/// aborts the entire switch if it fails, because a switch cannot fall back on
/// "skip the clear and let it stall again" — by then the id belongs to an
/// engine the thread no longer runs. Both writers therefore honour the same
/// contract: this row exists before the native id is allowed to go missing.
/// The grace marker's kind. Written by a freeze auto-recovery
/// ([`mark_turn_freeze_recovered`]) and by an engine/model switch — the latter
/// from inside its own transaction, so the row and the native-id clear it
/// vouches for commit together.
pub const MARKER_KIND_RECOVERED: &str = "turn_freeze_recovered";
/// One grace-marker row, deletion-fenced like every other timeline insert.
/// Generic over the connection so the switch transactions can use it
/// inside the switch's transaction.
async fn insert_marker_row<C: sea_orm::ConnectionTrait>(
    conn: &C,
    thread_id: i32,
    session_id: Option<i32>,
    kind: &str,
) -> Result<i32> {
    let res = conn
        .execute(sea_orm::Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Sqlite,
            "INSERT INTO lead_message \
             (thread_id, session_id, turn_id, role, kind, content, status, created_at) \
             SELECT ?, ?, 0, 'system', ?, '{}', 'complete', ? \
             WHERE EXISTS (SELECT 1 FROM thread WHERE id = ?)",
            [
                thread_id.into(),
                session_id.into(),
                kind.into(),
                now().into(),
                thread_id.into(),
            ],
        ))
        .await?;
    if res.rows_affected() == 0 {
        anyhow::bail!("thread {thread_id} no longer exists (deleted)");
    }
    i32::try_from(res.last_insert_id()).map_err(|_| anyhow::anyhow!("marker id out of i32 range"))
}

pub async fn mark_turn_freeze_recovered(
    db: &Db,
    thread_id: i32,
    session_id: Option<i32>,
) -> Result<()> {
    // Seam point: the failure this write's CALLERS must degrade correctly for
    // (`engine::stamp_freeze_marker` → the gated native-id clear) has no other
    // way to be reached from a test. See `fail_write`'s doc for the boundary.
    fail_write!("mark_turn_freeze_recovered");
    insert_lead_message(
        db,
        thread_id,
        session_id,
        0,
        "system",
        MARKER_KIND_RECOVERED,
        "{}",
        "complete",
    )
    .await?;
    Ok(())
}

/// The most recent turn-freeze auto-recovery for a (thread, session), as
/// unix-seconds (same clock as `now()`/`created_at`) — the read side of
/// [`mark_turn_freeze_recovered`]. `None` if it never happened (the common
/// case) or the stamp fails to parse (defensive — never panics on a bad row).
pub async fn last_turn_freeze_recovery_secs(
    db: &Db,
    thread_id: i32,
    session_id: Option<i32>,
) -> Result<Option<u64>> {
    last_marker_secs(db, thread_id, session_id, MARKER_KIND_RECOVERED).await
}

/// Newest marker of one kind for a (thread, session), as unix-seconds.
async fn last_marker_secs(
    db: &Db,
    thread_id: i32,
    session_id: Option<i32>,
    kind: &str,
) -> Result<Option<u64>> {
    let q = lead_message::Entity::find()
        .filter(lead_message::Column::ThreadId.eq(thread_id))
        .filter(lead_message::Column::Kind.eq(kind))
        .order_by_desc(lead_message::Column::Id);
    let q = match session_id {
        Some(id) => q.filter(lead_message::Column::SessionId.eq(id)),
        None => q.filter(lead_message::Column::SessionId.is_null()),
    };
    Ok(q.one(&db.0).await?.and_then(|m| m.created_at.parse().ok()))
}

/// Set a worker session's activity status directly. Unlike
/// `set_session_native_id` (which forces `running` as a side effect of
/// capturing the id), this writes whatever caller-chosen value — e.g.
/// flipping a live session to `idle` once its turn drains, or a boot sweep
/// marking a crash-interrupted row. No-op if the row is gone.
pub async fn set_session_status(db: &Db, session_id: i32, status: &str) -> Result<()> {
    if let Some(s) = session::Entity::find_by_id(session_id).one(&db.0).await? {
        let mut a: session::ActiveModel = s.into();
        a.status = Set(status.to_string());
        a.update(&db.0).await?;
    }
    Ok(())
}

/// One-time upgrade reconcile: before honest activity status existed, `status`
/// was a write-once high-water-mark (`running` on attach, never reset to idle),
/// so every legacy worker row reads `running`/`starting` regardless of whether
/// its turn finished. Reset those to `idle` so the boot revive sweep doesn't
/// resume+nudge every old idle/review worker on the first launch after upgrade.
/// Run by migration M0017; from then on the engine writes status honestly.
/// Generic over the connection so the migration (`SchemaManagerConnection`) and
/// tests (`DatabaseConnection`) share one implementation.
pub async fn reset_stale_running_sessions<C: sea_orm::ConnectionTrait>(conn: &C) -> Result<()> {
    // Raw UPDATE rather than `Entity::find()`: this runs inside M0017, which
    // executes BEFORE later migrations add columns (e.g. session.command in
    // M0019). Loading the full entity model would SELECT a column that does not
    // exist yet on an upgrading DB and fail the migration. A column-explicit
    // statement stays correct regardless of which columns the entity later gains.
    conn.execute(sea_orm::Statement::from_string(
        conn.get_database_backend(),
        "UPDATE session SET status = 'idle' WHERE status IN ('running', 'starting')",
    ))
    .await?;
    Ok(())
}

pub async fn get_session(db: &Db, session_id: i32) -> Result<Option<session::Model>> {
    Ok(session::Entity::find_by_id(session_id).one(&db.0).await?)
}

/// The most-recent session row for a (direction, repo) slot, by insertion order.
/// Used to decide resume-vs-fresh when no live PTY is tracked in memory.
pub async fn latest_session_for(
    db: &Db,
    direction_id: i32,
    repo_id: i32,
) -> Result<Option<session::Model>> {
    Ok(session::Entity::find()
        .filter(session::Column::DirectionId.eq(direction_id))
        .filter(session::Column::RepoId.eq(repo_id))
        .order_by_desc(session::Column::Id)
        .one(&db.0)
        .await?)
}

/// The most-recent session row for a direction (any repo) — the coordinator's
/// route from a bus wake target to its chat engine.
pub async fn latest_session_for_direction(
    db: &Db,
    direction_id: i32,
) -> Result<Option<session::Model>> {
    Ok(session::Entity::find()
        .filter(session::Column::DirectionId.eq(direction_id))
        .order_by_desc(session::Column::Id)
        .one(&db.0)
        .await?)
}

pub async fn sessions_for_thread(db: &Db, thread_id: i32) -> Result<Vec<session::Model>> {
    let direction_ids: Vec<i32> = direction::Entity::find()
        .filter(direction::Column::ThreadId.eq(thread_id))
        .all(&db.0)
        .await?
        .into_iter()
        .map(|d| d.id)
        .collect();
    if direction_ids.is_empty() {
        return Ok(Vec::new());
    }
    Ok(session::Entity::find()
        .filter(session::Column::DirectionId.is_in(direction_ids))
        .all(&db.0)
        .await?)
}

pub async fn sessions_for_repo(db: &Db, repo_id: i32) -> Result<Vec<session::Model>> {
    Ok(session::Entity::find()
        .filter(session::Column::RepoId.eq(repo_id))
        .all(&db.0)
        .await?)
}

/// Directions whose write repo is `repo_id`. Used to revoke their persisted
/// authorization grants before a repo delete cascades the direction rows away.
pub async fn directions_for_repo(db: &Db, repo_id: i32) -> Result<Vec<direction::Model>> {
    Ok(direction::Entity::find()
        .filter(direction::Column::RepoId.eq(repo_id))
        .all(&db.0)
        .await?)
}

/// Every session ever opened for one (direction, repo) slot — normally one,
/// but racing opens can create more; rewind's sibling-busy guard needs them.
pub async fn sessions_for(db: &Db, direction_id: i32, repo_id: i32) -> Result<Vec<session::Model>> {
    Ok(session::Entity::find()
        .filter(session::Column::DirectionId.eq(direction_id))
        .filter(session::Column::RepoId.eq(repo_id))
        .all(&db.0)
        .await?)
}

// ---- chat timeline (lead console + chat-mode workers) ----

#[allow(clippy::too_many_arguments)]
pub async fn insert_lead_message(
    db: &Db,
    thread_id: i32,
    session_id: Option<i32>,
    turn_id: i32,
    role: &str,
    kind: &str,
    content: &str,
    status: &str,
) -> Result<lead_message::Model> {
    use sea_orm::ConnectionTrait;
    // Deletion fence, atomic via ONE conditional INSERT: delete_thread cascades
    // the rows away BEFORE stopping the engines, so a still-running
    // reader/consumer can reach this insert after the cascade — and a bare
    // exists-check could observe the thread just before the cascade commits yet
    // insert after it (no FK rejects the orphan).
    //
    // Why not a transaction: under WAL (store/mod.rs enables it) a deferred
    // read→write upgrade fails with SQLITE_BUSY_SNAPSHOT whenever ANY writer —
    // not just a delete — commits after the snapshot, so two ordinary
    // concurrent sends (or a send racing a status/meta write) would spuriously
    // fail message delivery. A single INSERT … SELECT … WHERE EXISTS statement
    // is atomic under SQLite and only waits on the normal busy_timeout for
    // writer contention: actual deletions land as rows_affected == 0, unrelated
    // writers never poison it. Bound-parameter Statement, the idiom this module
    // already uses where the ORM cannot express the shape.
    //
    // This is the single INSERT choke point (set_lead_status /
    // set_lead_native_id route their meta inserts through here); UPDATE-shaped
    // writes are naturally fenced — their rows are gone.
    let res = db
        .0
        .execute(sea_orm::Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Sqlite,
            "INSERT INTO lead_message \
             (thread_id, session_id, turn_id, role, kind, content, status, created_at) \
             SELECT ?, ?, ?, ?, ?, ?, ?, ? \
             WHERE EXISTS (SELECT 1 FROM thread WHERE id = ?)",
            [
                thread_id.into(),
                session_id.into(),
                turn_id.into(),
                role.into(),
                kind.into(),
                content.into(),
                status.into(),
                now().into(),
                thread_id.into(),
            ],
        ))
        .await?;
    if res.rows_affected() == 0 {
        anyhow::bail!("thread {thread_id} no longer exists (deleted)");
    }
    let id = i32::try_from(res.last_insert_id())
        .map_err(|_| anyhow::anyhow!("lead_message id out of i32 range"))?;
    lead_message::Entity::find_by_id(id)
        .one(&db.0)
        .await?
        .ok_or_else(|| anyhow::anyhow!("inserted lead_message {id} not found"))
}

pub async fn update_lead_message(db: &Db, id: i32, content: &str, status: &str) -> Result<()> {
    if let Some(m) = lead_message::Entity::find_by_id(id).one(&db.0).await? {
        let mut a: lead_message::ActiveModel = m.into();
        a.content = Set(content.to_string());
        a.status = Set(status.to_string());
        a.update(&db.0).await?;
    }
    Ok(())
}

/// Record a turn's native rewind anchor on the user row that opened it
/// (claude: last assistant event uuid; codex app-server: turn id). No-op if
/// the row is gone.
pub async fn set_lead_message_anchor(db: &Db, message_id: i32, anchor: &str) -> Result<()> {
    lead_message::Entity::update_many()
        .col_expr(lead_message::Column::NativeAnchor, Expr::value(anchor))
        .filter(lead_message::Column::Id.eq(message_id))
        .exec(&db.0)
        .await?;
    Ok(())
}

/// Conversation rewind: delete the target row itself (its text is handed back
/// to the composer) and every later row of ONE (thread, session) timeline —
/// queued rows included (they belong to the abandoned future). Other sessions
/// and the lead timeline (session_id NULL) are untouched. Returns rows deleted.
/// Conversation rewind: delete the target row itself (its text is handed back
/// to the composer) and every LATER row of ONE (thread, session) timeline —
/// queued rows included (they belong to the abandoned future). "Later" is the
/// timeline's delivery order (`COALESCE(seq, id), id`), NOT raw id order: a
/// reordered queued row can carry a smaller id than a row it was delivered
/// after, and an id-based cut would keep abandoned user rows while deleting
/// retained assistant rows. Other sessions and the lead timeline (session_id
/// NULL) are untouched. Returns the deleted rows' ids (empty when the target
/// isn't on this timeline).
pub async fn truncate_lead_messages(
    c: &impl sea_orm::ConnectionTrait,
    thread_id: i32,
    session_id: Option<i32>,
    from_message_id: i32,
) -> Result<Vec<i32>> {
    use sea_orm::Order;
    let mut q = lead_message::Entity::find().filter(lead_message::Column::ThreadId.eq(thread_id));
    q = match session_id {
        Some(id) => q.filter(lead_message::Column::SessionId.eq(id)),
        None => q.filter(lead_message::Column::SessionId.is_null()),
    };
    let rows = q
        .order_by(Expr::cust("COALESCE(seq, id)"), Order::Asc)
        .order_by_asc(lead_message::Column::Id)
        .all(c)
        .await?;
    let Some(pos) = rows.iter().position(|m| m.id == from_message_id) else {
        return Ok(Vec::new());
    };
    let ids: Vec<i32> = rows[pos..].iter().map(|m| m.id).collect();
    lead_message::Entity::delete_many()
        .filter(lead_message::Column::Id.is_in(ids.iter().copied()))
        .exec(c)
        .await?;
    Ok(ids)
}

// ---- code checkpoints (shadow-repo pre-turn snapshots) ----

/// Record a pre-turn code checkpoint for a worker session (the engine's
/// turn-start hook calls this after snapshotting the worktree into the shadow
/// repo). Keyed by the user row that opened the turn.
#[allow(clippy::too_many_arguments)]
pub async fn insert_code_checkpoint(
    db: &Db,
    worktree_id: i32,
    session_id: i32,
    lead_message_id: i32,
    turn_id: i32,
    shadow_sha: &str,
    head_sha: &str,
    nested_repos: &str,
    index_tree: &str,
) -> Result<code_checkpoint::Model> {
    let a = code_checkpoint::ActiveModel {
        worktree_id: Set(worktree_id),
        session_id: Set(session_id),
        lead_message_id: Set(lead_message_id),
        turn_id: Set(turn_id),
        shadow_sha: Set(shadow_sha.to_string()),
        head_sha: Set(head_sha.to_string()),
        nested_repos: Set(nested_repos.to_string()),
        index_tree: Set(index_tree.to_string()),
        created_at: Set(now()),
        ..Default::default()
    };
    Ok(a.insert(&db.0).await?)
}

/// The checkpoint recorded for one turn's opening user row — a code rewind's
/// restore target.
pub async fn code_checkpoint_for(
    db: &Db,
    worktree_id: i32,
    lead_message_id: i32,
) -> Result<Option<code_checkpoint::Model>> {
    Ok(code_checkpoint::Entity::find()
        .filter(code_checkpoint::Column::WorktreeId.eq(worktree_id))
        .filter(code_checkpoint::Column::LeadMessageId.eq(lead_message_id))
        .one(&db.0)
        .await?)
}

/// Conversation rewind drops the abandoned future's checkpoints: every
/// checkpoint keyed by one of the deleted timeline rows (the same
/// delivery-ordered suffix `truncate_lead_messages` removed — NOT an id-based
/// span, which reordered queued rows can break). The restore consumed the
/// target's checkpoint BEFORE this runs. Returns rows deleted.
pub async fn truncate_code_checkpoints(
    c: &impl sea_orm::ConnectionTrait,
    worktree_id: i32,
    lead_message_ids: &[i32],
) -> Result<u64> {
    if lead_message_ids.is_empty() {
        return Ok(0);
    }
    Ok(code_checkpoint::Entity::delete_many()
        .filter(code_checkpoint::Column::WorktreeId.eq(worktree_id))
        .filter(code_checkpoint::Column::LeadMessageId.is_in(lead_message_ids.iter().copied()))
        .exec(c)
        .await?
        .rows_affected)
}

/// Conversation rewind's persistence in ONE transaction: timeline truncation,
/// the code-checkpoint sweep, and the fork's native id (worker session row or
/// lead meta row). Any failure rolls ALL of it back — never a truncated
/// timeline still pointing at the old native history (review P1).
pub async fn rewind_persist(
    db: &Db,
    thread_id: i32,
    session_id: Option<i32>,
    from_message_id: i32,
    worktree_id: Option<i32>,
    native_id: Option<&str>,
) -> Result<Vec<i32>> {
    use sea_orm::TransactionTrait;
    let txn = db.0.begin().await?;
    let deleted_ids = truncate_lead_messages(&txn, thread_id, session_id, from_message_id).await?;
    if let Some(w) = worktree_id {
        truncate_code_checkpoints(&txn, w, &deleted_ids).await?;
    }
    match session_id {
        Some(sid) => {
            if let Some(s) = session::Entity::find_by_id(sid).one(&txn).await? {
                let mut a: session::ActiveModel = s.into();
                a.native_session_id = Set(native_id.map(str::to_string));
                a.update(&txn).await?;
            }
        }
        None => set_lead_native_id_txn(&txn, thread_id, native_id).await?,
    }
    txn.commit().await?;
    Ok(deleted_ids)
}

/// The lead native-id write inside [`rewind_persist`]'s transaction — mirrors
/// `set_lead_native_id_opt` (merge into the meta row; clear deletes the row
/// when nothing else is stored) but runs on the txn and skips
/// insert_lead_message's thread fence (the truncation just proved it exists).
async fn set_lead_native_id_txn(
    c: &impl sea_orm::ConnectionTrait,
    thread_id: i32,
    native_id: Option<&str>,
) -> Result<()> {
    let meta = lead_message::Entity::find()
        .filter(lead_message::Column::ThreadId.eq(thread_id))
        .filter(lead_message::Column::Kind.eq("meta"))
        .one(c)
        .await?;
    match (meta, native_id) {
        (Some(m), Some(id)) => {
            let mut v: serde_json::Value =
                serde_json::from_str(&m.content).unwrap_or_else(|_| serde_json::json!({}));
            v["native_id"] = serde_json::json!(id);
            let mut a: lead_message::ActiveModel = m.into();
            a.content = Set(v.to_string());
            a.update(c).await?;
        }
        (Some(m), None) => {
            let mut v: serde_json::Value =
                serde_json::from_str(&m.content).unwrap_or_else(|_| serde_json::json!({}));
            if let Some(obj) = v.as_object_mut() {
                obj.remove("native_id");
            }
            if v.as_object().is_some_and(|o| o.is_empty()) {
                lead_message::Entity::delete_by_id(m.id).exec(c).await?;
            } else {
                let mut a: lead_message::ActiveModel = m.into();
                a.content = Set(v.to_string());
                a.update(c).await?;
            }
        }
        (None, Some(id)) => {
            let content = serde_json::json!({ "native_id": id }).to_string();
            let a = lead_message::ActiveModel {
                thread_id: Set(thread_id),
                turn_id: Set(0),
                role: Set("system".to_string()),
                kind: Set("meta".to_string()),
                content: Set(content),
                status: Set("complete".to_string()),
                created_at: Set(now()),
                ..Default::default()
            };
            a.insert(c).await?;
        }
        (None, None) => {}
    }
    Ok(())
}

/// Drop every checkpoint row of a worktree — cascade cleanup when the worktree
/// (or its owning direction/thread/workspace/repo) is removed.
pub async fn delete_code_checkpoints_for_worktree(db: &Db, worktree_id: i32) -> Result<u64> {
    Ok(code_checkpoint::Entity::delete_many()
        .filter(code_checkpoint::Column::WorktreeId.eq(worktree_id))
        .exec(&db.0)
        .await?
        .rows_affected)
}

/// Unix milliseconds as a string — `test_plan.updated_at`'s clock. Millisecond
/// resolution (vs the store's usual seconds) lets the lead-emit CAS separate
/// "saved just before this turn started" from "saved mid-turn".
fn now_millis() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
        .to_string()
}

/// Upsert the issue's test-case document (0..1 per thread — UNIQUE thread_id).
/// `source` records the last writer: "lead" (sentinel) or "user" (panel edit).
/// Fenced like every other thread-owned write: the thread must still exist and
/// its workspace must accept writes — a late panel save or lead sentinel after
/// `delete_thread_cascade` must not recreate an orphan row (no FK cascades).
pub async fn upsert_test_plan(
    db: &Db,
    thread_id: i32,
    content: &str,
    source: &str,
) -> Result<test_plan::Model> {
    ensure_thread_workspace_accepts_writes(db, thread_id).await?;
    let written = if let Some(existing) = test_plan::Entity::find()
        .filter(test_plan::Column::ThreadId.eq(thread_id))
        .one(&db.0)
        .await?
    {
        let mut a: test_plan::ActiveModel = existing.into();
        a.content = Set(content.to_string());
        a.source = Set(source.to_string());
        a.updated_at = Set(now_millis());
        a.update(&db.0).await?
    } else {
        let a = test_plan::ActiveModel {
            thread_id: Set(thread_id),
            content: Set(content.to_string()),
            source: Set(source.to_string()),
            updated_at: Set(now_millis()),
            ..Default::default()
        };
        a.insert(&db.0).await?
    };
    // Post-write fence (same shape as create_thread/add_repo_ref): a cascade
    // that passed its test_plan delete pass between our pre-check and this
    // write would leave this row an unreachable orphan — re-check and undo.
    if let Err(err) = ensure_thread_workspace_accepts_writes(db, thread_id).await {
        let _ = test_plan::Entity::delete_by_id(written.id).exec(&db.0).await;
        return Err(err);
    }
    Ok(written)
}

/// Lead-emit upsert with an ATOMIC supersede check: the condition rides the SQL
/// UPDATE itself (not a separate read), so a user save landing between any
/// pre-read and this write still wins. A USER-sourced row stamped at/after the
/// emitting turn began (`turn_started_millis`, same clock as `updated_at`) was
/// saved mid-turn — the emit predates it. Returns false when superseded.
pub async fn lead_upsert_test_plan(
    db: &Db,
    thread_id: i32,
    content: &str,
    turn_started_millis: u64,
) -> Result<bool> {
    ensure_thread_workspace_accepts_writes(db, thread_id).await?;
    let updated = test_plan::Entity::update_many()
        .col_expr(test_plan::Column::Content, Expr::value(content))
        .col_expr(test_plan::Column::Source, Expr::value("lead"))
        .col_expr(test_plan::Column::UpdatedAt, Expr::value(now_millis()))
        .filter(test_plan::Column::ThreadId.eq(thread_id))
        .filter(Expr::cust_with_values(
            // updated_at holds decimal digits only; CAST keeps legacy
            // second-resolution rows (shorter strings) comparing numerically.
            "NOT (source = 'user' AND CAST(updated_at AS INTEGER) >= ?)",
            [turn_started_millis as i64],
        ))
        .exec(&db.0)
        .await?;
    if updated.rows_affected == 0 {
        let exists = test_plan::Entity::find()
            .filter(test_plan::Column::ThreadId.eq(thread_id))
            .one(&db.0)
            .await?
            .is_some();
        if exists {
            return Ok(false); // superseded by a newer user save
        }
        // First document for this thread. A user save racing this insert hits
        // the UNIQUE(thread_id) — that specific conflict means "superseded".
        // Anything else (locked db, I/O, schema) is a real failure and must
        // propagate, not masquerade as a user edit winning.
        let a = test_plan::ActiveModel {
            thread_id: Set(thread_id),
            content: Set(content.to_string()),
            source: Set("lead".to_string()),
            updated_at: Set(now_millis()),
            ..Default::default()
        };
        if let Err(e) = a.insert(&db.0).await {
            if e.to_string().contains("UNIQUE constraint failed") {
                return Ok(false);
            }
            return Err(e.into());
        }
    }
    // Post-write fence, mirroring upsert_test_plan.
    if let Err(err) = ensure_thread_workspace_accepts_writes(db, thread_id).await {
        let _ = test_plan::Entity::delete_many()
            .filter(test_plan::Column::ThreadId.eq(thread_id))
            .exec(&db.0)
            .await;
        return Err(err);
    }
    Ok(true)
}

/// The issue's test-case document, if one has been derived.
pub async fn get_test_plan(db: &Db, thread_id: i32) -> Result<Option<test_plan::Model>> {
    Ok(test_plan::Entity::find()
        .filter(test_plan::Column::ThreadId.eq(thread_id))
        .one(&db.0)
        .await?)
}

/// Persist the lead engine's last-known meta snapshot (JSON `PersistedMeta`)
/// so the Session panel survives an app relaunch. Single-column UPDATE — never
/// a whole-row read-modify-write, which could clobber a concurrent write to a
/// sibling column (e.g. the turn-end status flip). A missing thread (deleted
/// mid-turn) is a no-op — callers are fire-and-forget.
pub async fn save_lead_meta(db: &Db, thread_id: i32, json: &str) -> Result<()> {
    thread::Entity::update_many()
        .col_expr(thread::Column::LeadMeta, Expr::value(json))
        .filter(thread::Column::Id.eq(thread_id))
        .exec(&db.0)
        .await?;
    Ok(())
}

/// Mirror of [`save_lead_meta`] for a chat-mode worker session row. The
/// single-column UPDATE matters even more here: `session.status` is the honest
/// activity flag, and a whole-row write racing the turn-end `idle` flip could
/// resurrect `running` on a cleanly finished worker.
pub async fn save_session_meta(db: &Db, session_id: i32, json: &str) -> Result<()> {
    session::Entity::update_many()
        .col_expr(session::Column::Meta, Expr::value(json))
        .filter(session::Column::Id.eq(session_id))
        .exec(&db.0)
        .await?;
    Ok(())
}

/// Stamp an action_card row as resolved (its repo flow succeeded) and return the
/// updated row, so the settled state survives reload. Merges `{"resolved":
/// <name>}` into the card's JSON. None if the row is gone.
pub async fn resolve_action_card(
    db: &Db,
    id: i32,
    name: &str,
) -> Result<Option<lead_message::Model>> {
    let Some(m) = lead_message::Entity::find_by_id(id).one(&db.0).await? else {
        return Ok(None);
    };
    let mut v: serde_json::Value =
        serde_json::from_str(&m.content).unwrap_or_else(|_| serde_json::json!({}));
    if let Some(obj) = v.as_object_mut() {
        obj.insert("resolved".into(), serde_json::Value::String(name.to_string()));
    }
    let mut a: lead_message::ActiveModel = m.into();
    a.content = Set(v.to_string());
    Ok(Some(a.update(&db.0).await?))
}

/// Close rows left `streaming` by a previous app process. Live turn state is
/// memory-only; after restart these rows can no longer receive deltas, so show
/// them as interrupted instead of a forever-typing assistant.
pub async fn mark_incomplete_turns_interrupted(
    db: &Db,
    thread_id: i32,
    session_id: Option<i32>,
) -> Result<()> {
    // Close only the half-streamed assistant row; orphaned "queued" user messages
    // are handled separately by fail_queued so they surface as resendable errors.
    let mut q = lead_message::Entity::find()
        .filter(lead_message::Column::ThreadId.eq(thread_id))
        .filter(lead_message::Column::Status.eq("streaming"));
    q = match session_id {
        Some(id) => q.filter(lead_message::Column::SessionId.eq(id)),
        None => q.filter(lead_message::Column::SessionId.is_null()),
    };
    for m in q.all(&db.0).await? {
        let mut a: lead_message::ActiveModel = m.into();
        a.status = Set("interrupted".to_string());
        a.update(&db.0).await?;
    }
    Ok(())
}

pub async fn list_lead_messages(db: &Db, thread_id: i32) -> Result<Vec<lead_message::Model>> {
    use sea_orm::Order;
    // COALESCE(seq, id) ensures delivered-queued rows appear in send order while
    // all other rows keep creation order. id ASC breaks same-effective-key ties.
    Ok(lead_message::Entity::find()
        .filter(lead_message::Column::ThreadId.eq(thread_id))
        .order_by(Expr::cust("COALESCE(seq, id)"), Order::Asc)
        .order_by_asc(lead_message::Column::Id)
        .all(&db.0)
        .await?)
}

/// The next turn number for a thread's timeline (1-based).
pub async fn next_turn_id(db: &Db, thread_id: i32) -> Result<i32> {
    Ok(list_lead_messages(db, thread_id)
        .await?
        .iter()
        .map(|m| m.turn_id)
        .max()
        .unwrap_or(0)
        + 1)
}

/// Flip the OLDEST queued user message for one lead/worker surface. Queue order
/// equals row insertion order, so position (not content) is the identity.
pub async fn complete_queued(
    db: &Db,
    thread_id: i32,
    session_id: Option<i32>,
) -> Result<Option<lead_message::Model>> {
    update_oldest_queued_status(db, thread_id, session_id, "complete").await
}

/// Flip a specific queued row to complete by id (reorder-safe delivery).
/// Returns Ok(None) if the row doesn't exist or isn't currently "queued".
pub async fn complete_queued_by_id(
    db: &Db,
    message_id: i32,
) -> Result<Option<lead_message::Model>> {
    let Some(m) = lead_message::Entity::find_by_id(message_id).one(&db.0).await? else {
        return Ok(None);
    };
    if m.status != "queued" {
        return Ok(None);
    }
    let mut a: lead_message::ActiveModel = m.into();
    a.status = Set("complete".to_string());
    Ok(Some(a.update(&db.0).await?))
}

/// Outcome of [`mark_message_consumed`] — the caller (the one-shot
/// `note_turn_activity` gate, engine.rs) needs to tell "settled, never try
/// again" apart from "not eligible YET, retry the next activity event": a
/// still-`queued` row (queued deliveries flip to `status == "complete"` only
/// AFTER the turn is already dispatched — see `mark_queued_delivered`'s
/// callers) is exactly the transient case a real per-turn/app-server race can
/// hit (PR #117 review, P2): the agent's first event can land before that
/// flip, and treating it the same as "permanently ineligible" would burn the
/// one-shot gate on a no-op and leave the receipt stuck at "delivered"
/// forever even though the agent is actively working.
pub enum ConsumeMark {
    /// Freshly marked just now — carries the updated row.
    Marked(lead_message::Model),
    /// Already had `consumed_at` set (idempotent no-op) — a real terminal
    /// state, never retry.
    AlreadyConsumed,
    /// Missing / wrong role / not yet `status == "complete"` — retry-worthy,
    /// NOT a reason to permanently give up.
    NotEligible,
}

/// Stamp the delivery receipt's third tier: the agent produced its first
/// observed activity for the turn this "user" row opened (issue #94 — "已被
/// agent 消费"). Idempotent and narrowly guarded so a stray/late caller can't
/// misuse it as a general status-setter: only marks a row that is role
/// `"user"`, already `status == "complete"` (delivered — a queued row hasn't
/// reached the agent yet, so it cannot be "consumed"), and not already
/// marked. `consumed_at` is otherwise independent of `status`: it never
/// overwrites it, so the existing queued/complete/error/interrupted lifecycle
/// (and everything that reads it, e.g. rewind's anchor matching) is
/// untouched — and the partial `Set` (see the stale-snapshot tests below)
/// can't clobber a concurrent write to any OTHER column either.
pub async fn mark_message_consumed(db: &Db, message_id: i32) -> Result<ConsumeMark> {
    let Some(m) = lead_message::Entity::find_by_id(message_id).one(&db.0).await? else {
        return Ok(ConsumeMark::NotEligible);
    };
    if m.consumed_at.is_some() {
        return Ok(ConsumeMark::AlreadyConsumed);
    }
    if m.role != "user" || m.status != "complete" {
        return Ok(ConsumeMark::NotEligible);
    }
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default();
    let mut a: lead_message::ActiveModel = m.into();
    a.consumed_at = Set(Some(millis));
    Ok(ConsumeMark::Marked(a.update(&db.0).await?))
}

/// 删除一条消息行（仅用于取消未交付的 queued 行）。
pub async fn delete_message(db: &Db, message_id: i32) -> Result<()> {
    lead_message::Entity::delete_by_id(message_id).exec(&db.0).await?;
    Ok(())
}

/// Stamp a delivered queued row with seq = max(COALESCE(seq, id)) + 1 over its
/// thread so it sorts after all currently-ordered rows in list_lead_messages.
/// Called for every tracked queued delivery to preserve reorder-then-deliver order.
pub async fn assign_delivery_seq(db: &Db, thread_id: i32, message_id: i32) -> Result<i64> {
    use sea_orm::{ConnectionTrait, Order, QuerySelect};
    // Find the current max effective sort key for this thread.
    let rows = lead_message::Entity::find()
        .filter(lead_message::Column::ThreadId.eq(thread_id))
        .order_by(Expr::cust("COALESCE(seq, id)"), Order::Desc)
        .limit(1)
        .all(&db.0)
        .await?;
    let next_seq: i64 = rows
        .first()
        .map(|m| m.seq.unwrap_or(m.id as i64) + 1)
        .unwrap_or(1);
    // Raw UPDATE: seq is not in the entity's ActiveModel update path in older
    // SeaORM versions; use a raw statement to avoid depending on the column ordering.
    let updated = db
        .0
        .execute(sea_orm::Statement::from_sql_and_values(
            db.0.get_database_backend(),
            "UPDATE lead_message SET seq = ? WHERE id = ?",
            [next_seq.into(), message_id.into()],
        ))
        .await?;
    if updated.rows_affected() == 0 {
        anyhow::bail!("lead_message {message_id} not found while assigning delivery sequence");
    }
    Ok(next_seq)
}

/// 查一条消息行（用于读取原始 content 再局部改写）。
pub async fn get_message(
    db: &Db,
    message_id: i32,
) -> Result<Option<crate::store::entities::lead_message::Model>> {
    Ok(lead_message::Entity::find_by_id(message_id).one(&db.0).await?)
}

/// 覆盖一条消息行的 content（编辑排队消息文本用）。
pub async fn update_message_content(db: &Db, message_id: i32, content: &str) -> Result<()> {
    if let Some(m) = lead_message::Entity::find_by_id(message_id).one(&db.0).await? {
        let mut a: lead_message::ActiveModel = m.into();
        a.content = Set(content.to_string());
        a.update(&db.0).await?;
    }
    Ok(())
}

pub async fn fail_queued(
    db: &Db,
    thread_id: i32,
    session_id: Option<i32>,
) -> Result<Vec<lead_message::Model>> {
    set_queued_status(db, thread_id, session_id, "error").await
}

pub async fn set_queued_status(
    db: &Db,
    thread_id: i32,
    session_id: Option<i32>,
    status: &str,
) -> Result<Vec<lead_message::Model>> {
    update_all_queued_status(db, thread_id, session_id, status).await
}

/// Flip the given rows to `status` — only those still `queued`. By-id variant of
/// [`set_queued_status`] for callers that must finalize ONLY the rows they
/// drained: a blanket per-session sweep could catch a CONCURRENT send's row,
/// inserted after the caller released the engine lock, and finalize a message
/// that is about to be delivered.
pub async fn set_queued_status_by_ids(
    db: &Db,
    ids: &[i32],
    status: &str,
) -> Result<Vec<lead_message::Model>> {
    let mut updated = Vec::with_capacity(ids.len());
    for id in ids {
        let Some(m) = lead_message::Entity::find_by_id(*id).one(&db.0).await? else {
            continue;
        };
        if m.status != "queued" {
            continue;
        }
        let mut a: lead_message::ActiveModel = m.into();
        a.status = Set(status.to_string());
        updated.push(a.update(&db.0).await?);
    }
    Ok(updated)
}

fn queued_query(thread_id: i32, session_id: Option<i32>) -> sea_orm::Select<lead_message::Entity> {
    let q = lead_message::Entity::find()
        .filter(lead_message::Column::ThreadId.eq(thread_id))
        .filter(lead_message::Column::Status.eq("queued"))
        .order_by_asc(lead_message::Column::Id);
    match session_id {
        Some(id) => q.filter(lead_message::Column::SessionId.eq(id)),
        None => q.filter(lead_message::Column::SessionId.is_null()),
    }
}

async fn update_oldest_queued_status(
    db: &Db,
    thread_id: i32,
    session_id: Option<i32>,
    status: &str,
) -> Result<Option<lead_message::Model>> {
    if let Some(m) = queued_query(thread_id, session_id).one(&db.0).await? {
        let mut a: lead_message::ActiveModel = m.into();
        a.status = Set(status.to_string());
        return Ok(Some(a.update(&db.0).await?));
    }
    Ok(None)
}

async fn update_all_queued_status(
    db: &Db,
    thread_id: i32,
    session_id: Option<i32>,
    status: &str,
) -> Result<Vec<lead_message::Model>> {
    let rows = queued_query(thread_id, session_id).all(&db.0).await?;
    let mut updated = Vec::with_capacity(rows.len());
    for m in rows {
        let mut a: lead_message::ActiveModel = m.into();
        a.status = Set(status.to_string());
        updated.push(a.update(&db.0).await?);
    }
    Ok(updated)
}

/// The lead's persisted engine metadata (native session id) lives in a single
/// role=system kind=meta row per thread, invisible to the timeline UI.
pub async fn lead_native_id(db: &Db, thread_id: i32) -> Result<Option<String>> {
    Ok(lead_message::Entity::find()
        .filter(lead_message::Column::ThreadId.eq(thread_id))
        .filter(lead_message::Column::Kind.eq("meta"))
        .one(&db.0)
        .await?
        .and_then(|m| {
            serde_json::from_str::<serde_json::Value>(&m.content)
                .ok()?
                .get("native_id")?
                .as_str()
                .map(String::from)
        }))
}

pub async fn set_lead_native_id(db: &Db, thread_id: i32, native_id: &str) -> Result<()> {
    let existing = lead_message::Entity::find()
        .filter(lead_message::Column::ThreadId.eq(thread_id))
        .filter(lead_message::Column::Kind.eq("meta"))
        .one(&db.0)
        .await?;
    match existing {
        // Merge, don't replace: the meta row may already carry a `status` field
        // (set first by `set_lead_status`); blowing the whole object away would
        // clobber it. Read → set one key → write back.
        Some(m) => {
            let mut v: serde_json::Value =
                serde_json::from_str(&m.content).unwrap_or_else(|_| serde_json::json!({}));
            v["native_id"] = serde_json::json!(native_id);
            let mut a: lead_message::ActiveModel = m.into();
            a.content = Set(v.to_string());
            a.update(&db.0).await?;
        }
        None => {
            let content = serde_json::json!({ "native_id": native_id }).to_string();
            insert_lead_message(
                db, thread_id, None, 0, "system", "meta", &content, "complete",
            )
            .await?;
        }
    }
    Ok(())
}

/// `set_lead_native_id` variant that can also CLEAR the id: conversation
/// rewind uses None for "back to before the first message" (the next turn
/// starts a brand-new native session). Clearing removes the `native_id` key
/// from the meta row — preserving other fields (status) — and drops the row
/// entirely once it holds nothing else, so the next turn starts fresh.
pub async fn set_lead_native_id_opt(
    db: &Db,
    thread_id: i32,
    native_id: Option<&str>,
) -> Result<()> {
    let Some(id) = native_id else {
        let Some(m) = lead_message::Entity::find()
            .filter(lead_message::Column::ThreadId.eq(thread_id))
            .filter(lead_message::Column::Kind.eq("meta"))
            .one(&db.0)
            .await?
        else {
            return Ok(());
        };
        let mut v: serde_json::Value =
            serde_json::from_str(&m.content).unwrap_or_else(|_| serde_json::json!({}));
        if let Some(obj) = v.as_object_mut() {
            obj.remove("native_id");
        }
        if v.as_object().is_some_and(|o| o.is_empty()) {
            lead_message::Entity::delete_by_id(m.id).exec(&db.0).await?;
        } else {
            let mut a: lead_message::ActiveModel = m.into();
            a.content = Set(v.to_string());
            a.update(&db.0).await?;
        }
        return Ok(());
    };
    set_lead_native_id(db, thread_id, id).await
}

/// The lead's persisted activity status, co-located with `native_id` in the
/// single role=system kind=meta row. None until first written.
pub async fn lead_status(db: &Db, thread_id: i32) -> Result<Option<String>> {
    Ok(lead_message::Entity::find()
        .filter(lead_message::Column::ThreadId.eq(thread_id))
        .filter(lead_message::Column::Kind.eq("meta"))
        .one(&db.0)
        .await?
        .and_then(|m| {
            serde_json::from_str::<serde_json::Value>(&m.content)
                .ok()?
                .get("status")?
                .as_str()
                .map(String::from)
        }))
}

/// Upsert the per-thread lead `meta` row's `status` field, preserving any other
/// fields it already holds (notably `native_id`).
pub async fn set_lead_status(db: &Db, thread_id: i32, status: &str) -> Result<()> {
    let existing = lead_message::Entity::find()
        .filter(lead_message::Column::ThreadId.eq(thread_id))
        .filter(lead_message::Column::Kind.eq("meta"))
        .one(&db.0)
        .await?;
    match existing {
        Some(m) => {
            let mut v: serde_json::Value =
                serde_json::from_str(&m.content).unwrap_or_else(|_| serde_json::json!({}));
            v["status"] = serde_json::json!(status);
            let mut a: lead_message::ActiveModel = m.into();
            a.content = Set(v.to_string());
            a.update(&db.0).await?;
        }
        None => {
            // Fence against deleted threads: delete_thread cascades the rows away
            // and THEN stops the engines, whose status persistence lands here —
            // inserting a fresh meta row at that point would recreate orphan
            // timeline data for a thread that no longer exists.
            let thread_exists = thread::Entity::find_by_id(thread_id)
                .one(&db.0)
                .await?
                .is_some();
            if !thread_exists {
                return Ok(());
            }
            let content = serde_json::json!({ "status": status }).to_string();
            insert_lead_message(
                db, thread_id, None, 0, "system", "meta", &content, "complete",
            )
            .await?;
        }
    }
    Ok(())
}

// ─────────────────────────── im_route (M2) ───────────────────────────

async fn ensure_im_route_write_survived_workspace_fence(db: &Db, thread_id: i32) -> Result<()> {
    if let Err(err) = ensure_thread_workspace_accepts_writes(db, thread_id).await {
        let _ = im_route::Entity::delete_many()
            .filter(im_route::Column::ThreadId.eq(thread_id))
            .exec(&db.0)
            .await;
        return Err(err);
    }
    Ok(())
}

/// Bind an issue (thread) to an IM thread. Upserts on `thread_id`: re-binding the
/// same issue replaces its target. Returns the resulting row.
pub async fn bind_im_route(
    db: &Db,
    thread_id: i32,
    channel: &str,
    chat_id: &str,
    im_thread_ref: &str,
) -> Result<im_route::Model> {
    ensure_thread_workspace_accepts_writes(db, thread_id).await?;
    if let Some(existing) = im_route::Entity::find()
        .filter(im_route::Column::ThreadId.eq(thread_id))
        .one(&db.0)
        .await?
    {
        let mut a: im_route::ActiveModel = existing.into();
        a.channel = Set(channel.to_string());
        a.chat_id = Set(chat_id.to_string());
        a.im_thread_ref = Set(im_thread_ref.to_string());
        ensure_thread_workspace_accepts_writes(db, thread_id).await?;
        let m = a.update(&db.0).await?;
        ensure_im_route_write_survived_workspace_fence(db, thread_id).await?;
        return Ok(m);
    }
    let now = now();
    ensure_thread_workspace_accepts_writes(db, thread_id).await?;
    let am = im_route::ActiveModel {
        channel: Set(channel.to_string()),
        chat_id: Set(chat_id.to_string()),
        im_thread_ref: Set(im_thread_ref.to_string()),
        thread_id: Set(thread_id),
        created_at: Set(now),
        ..Default::default()
    };
    let m = am.insert(&db.0).await?.try_into_model()?;
    ensure_im_route_write_survived_workspace_fence(db, thread_id).await?;
    Ok(m)
}

pub async fn unbind_im_route(db: &Db, thread_id: i32) -> Result<()> {
    im_route::Entity::delete_many()
        .filter(im_route::Column::ThreadId.eq(thread_id))
        .exec(&db.0)
        .await?;
    Ok(())
}

pub async fn list_im_routes(db: &Db) -> Result<Vec<im_route::Model>> {
    Ok(im_route::Entity::find().all(&db.0).await?)
}

pub async fn im_route_of_thread(db: &Db, thread_id: i32) -> Result<Option<im_route::Model>> {
    Ok(im_route::Entity::find()
        .filter(im_route::Column::ThreadId.eq(thread_id))
        .one(&db.0)
        .await?)
}

/// Broad lookup by channel + chat. Used by Concierge because its latest reply
/// target changes per inbound message while the chat-level conversation stays one.
pub async fn im_route_of_channel_chat(
    db: &Db,
    channel: &str,
    chat_id: &str,
) -> Result<Option<im_route::Model>> {
    Ok(im_route::Entity::find()
        .filter(im_route::Column::Channel.eq(channel))
        .filter(im_route::Column::ChatId.eq(chat_id))
        .one(&db.0)
        .await?)
}

/// Reverse lookup: which issue is this IM thread/topic bound to?
pub async fn im_route_of_thread_ref(
    db: &Db,
    channel: &str,
    chat_id: &str,
    im_thread_ref: &str,
) -> Result<Option<im_route::Model>> {
    Ok(im_route::Entity::find()
        .filter(im_route::Column::Channel.eq(channel))
        .filter(im_route::Column::ChatId.eq(chat_id))
        .filter(im_route::Column::ImThreadRef.eq(im_thread_ref))
        .one(&db.0)
        .await?)
}

// --- pull_request (issue #110 T1) ------------------------------------------
//
// Registration is agent-initiated (the `register_pr` bus tool, called right
// after `gh pr create` / a future `glab mr create` succeeds); the background
// monitor (`crate::host::monitor`) owns every write from then on. Both paths
// funnel through the functions below so there is exactly one place that knows
// the row shape.

/// Find a tracked row by its natural host-side key. Registration upserts on
/// this so re-registering the same PR/MR (e.g. after a restart, or a lead
/// re-reporting it) updates context instead of duplicating the row.
pub async fn find_pull_request(
    db: &Db,
    host_kind: &str,
    host_owner: &str,
    host_repo: &str,
    number: i32,
) -> Result<Option<pull_request::Model>> {
    Ok(pull_request::Entity::find()
        .filter(pull_request::Column::HostKind.eq(host_kind))
        .filter(pull_request::Column::HostOwner.eq(host_owner))
        .filter(pull_request::Column::HostRepo.eq(host_repo))
        .filter(pull_request::Column::Number.eq(number))
        .one(&db.0)
        .await?)
}

pub async fn get_pull_request(db: &Db, id: i32) -> Result<Option<pull_request::Model>> {
    Ok(pull_request::Entity::find_by_id(id).one(&db.0).await?)
}

/// Every row the monitor still needs to sweep — i.e. still `open`. A merged
/// or closed row falls out of this query permanently: there is nothing left
/// to poll once the change unit itself is resolved.
pub async fn list_open_pull_requests(db: &Db) -> Result<Vec<pull_request::Model>> {
    Ok(pull_request::Entity::find()
        .filter(pull_request::Column::Lifecycle.eq("open"))
        .all(&db.0)
        .await?)
}

pub async fn list_pull_requests_for_direction(
    db: &Db,
    direction_id: i32,
) -> Result<Vec<pull_request::Model>> {
    Ok(pull_request::Entity::find()
        .filter(pull_request::Column::DirectionId.eq(direction_id))
        .all(&db.0)
        .await?)
}

/// Register a newly-opened PR/MR, or refresh an already-tracked one's context
/// (thread/direction/repo can legitimately change across a re-registration —
/// e.g. a direction's PR reopened under a new task after a rebase-and-reopen).
#[allow(clippy::too_many_arguments)]
pub async fn register_pull_request(
    db: &Db,
    thread_id: i32,
    direction_id: i32,
    repo_id: i32,
    host_kind: &str,
    host_base: &str,
    host_owner: &str,
    host_repo: &str,
    number: i32,
    url: &str,
    title: &str,
) -> Result<pull_request::Model> {
    if let Some(existing) = find_pull_request(db, host_kind, host_owner, host_repo, number).await? {
        let mut a: pull_request::ActiveModel = existing.into();
        a.thread_id = Set(thread_id);
        a.direction_id = Set(direction_id);
        a.repo_id = Set(repo_id);
        a.host_base = Set(host_base.to_string());
        if !url.is_empty() {
            a.url = Set(url.to_string());
        }
        if !title.is_empty() {
            a.title = Set(title.to_string());
        }
        return Ok(a.update(&db.0).await?);
    }
    let a = pull_request::ActiveModel {
        thread_id: Set(thread_id),
        direction_id: Set(direction_id),
        repo_id: Set(repo_id),
        host_kind: Set(host_kind.to_string()),
        host_base: Set(host_base.to_string()),
        host_owner: Set(host_owner.to_string()),
        host_repo: Set(host_repo.to_string()),
        number: Set(number),
        url: Set(url.to_string()),
        title: Set(title.to_string()),
        lifecycle: Set("open".to_string()),
        created_at: Set(now()),
        ..Default::default()
    };
    Ok(a.insert(&db.0).await?)
}

/// Apply a freshly, SUCCESSFULLY fetched snapshot: overwrite every observed
/// field, clear any prior probe error, and stamp the check time. No-op if the
/// row is gone (e.g. deleted concurrently). See
/// `mark_pull_request_probe_error` for the failure counterpart, which
/// deliberately leaves these observed fields untouched — a failed probe is a
/// fact about the ATTEMPT, not new information about the PR/MR's real state.
pub async fn apply_pull_request_snapshot(
    db: &Db,
    id: i32,
    snapshot: &crate::host::PrSnapshot,
    readiness: &crate::host::MergeReadiness,
) -> Result<()> {
    let Some(row) = pull_request::Entity::find_by_id(id).one(&db.0).await? else {
        return Ok(());
    };
    let mut a: pull_request::ActiveModel = row.into();
    a.head_sha = Set(snapshot.head_sha.clone());
    a.base_ref = Set(snapshot.base_ref.clone());
    if !snapshot.url.is_empty() {
        a.url = Set(snapshot.url.clone());
    }
    if !snapshot.title.is_empty() {
        a.title = Set(snapshot.title.clone());
    }
    a.lifecycle = Set(snapshot.lifecycle.as_str().to_string());
    a.ci_status = Set(serde_json::to_string(&snapshot.ci).unwrap_or_default());
    a.review_status = Set(serde_json::to_string(&snapshot.review).unwrap_or_default());
    a.conflict_status = Set(serde_json::to_string(&snapshot.conflict).unwrap_or_default());
    a.merge_readiness = Set(serde_json::to_string(readiness).unwrap_or_default());
    a.last_checked_at = Set(now());
    a.last_error = Set(String::new());
    a.update(&db.0).await?;
    Ok(())
}

/// Record a failed probe attempt without touching the last known snapshot.
pub async fn mark_pull_request_probe_error(db: &Db, id: i32, message: &str) -> Result<()> {
    let Some(row) = pull_request::Entity::find_by_id(id).one(&db.0).await? else {
        return Ok(());
    };
    let mut a: pull_request::ActiveModel = row.into();
    a.last_checked_at = Set(now());
    a.last_error = Set(message.to_string());
    a.update(&db.0).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Db;

    async fn mem() -> Db {
        Db::connect("sqlite::memory:").await.unwrap()
    }

    /// A live thread id for message tests: insert_lead_message refuses to write
    /// rows for a deleted/nonexistent thread (the deletion fence), so tests must
    /// target a real thread row instead of a bare literal id.
    async fn live_thread(db: &Db) -> i32 {
        let ws = create_workspace(db, "msg-ws").await.unwrap();
        create_thread(db, ws.id, "msg-t", "feature", "claude")
            .await
            .unwrap()
            .id
    }

    async fn worker_fixture(
        db: &Db,
    ) -> (workspace::Model, repo_ref::Model, thread::Model, direction::Model) {
        let ws = create_workspace(db, "ws").await.unwrap();
        let repo = add_repo_ref(db, ws.id, "repo", "/tmp/repo", "main", "", true)
            .await
            .unwrap();
        let thread = create_thread(db, ws.id, "issue", "feature", "codex")
            .await
            .unwrap();
        let direction =
            create_direction(db, thread.id, "task", "codex", repo.id, "why", "impl-only", "")
                .await
                .unwrap();
        (ws, repo, thread, direction)
    }

    // ---- the test-only write-failure seam's own contract ----

    /// The property every caller of [`fail_write`] depends on: arming ONE write
    /// fails exactly that write and leaves its neighbours alone. Without that
    /// selectivity the seam could not reproduce the situation the gated
    /// degradation paths exist for — "this write failed, the ones around it
    /// succeeded" — it would just look like a dead database.
    #[tokio::test]
    async fn fail_write_only_fails_the_armed_write() {
        let db = mem().await;
        let thread_id = live_thread(&db).await;

        fail_write::while_failing("mark_turn_freeze_recovered", async {
            assert!(
                mark_turn_freeze_recovered(&db, thread_id, None).await.is_err(),
                "the armed write must fail"
            );
            // A neighbour sharing the very same INSERT choke point
            // (`insert_lead_message`) is untouched — the seam keys on the named
            // write, not on the statement underneath it.
            assert!(
                insert_lead_message(&db, thread_id, None, 1, "assistant", "text", "{}", "complete")
                    .await
                    .is_ok(),
                "an unarmed write through the same choke point must still succeed"
            );
            // …and so is the other write the freeze recovery performs around it.
            assert!(set_lead_native_id_opt(&db, thread_id, None).await.is_ok());
        })
        .await;
    }

    /// Arming is scoped to the task that armed it, and ends with the scope:
    /// nothing is left armed for the rest of the process (which is what lets
    /// `cargo test`'s parallel threads arm freely without a serializing lock).
    #[tokio::test]
    async fn fail_write_arming_ends_with_its_scope() {
        let db = mem().await;
        let thread_id = live_thread(&db).await;

        fail_write::while_failing("mark_turn_freeze_recovered", async {
            assert!(mark_turn_freeze_recovered(&db, thread_id, None).await.is_err());
        })
        .await;

        assert!(
            mark_turn_freeze_recovered(&db, thread_id, None).await.is_ok(),
            "outside the scope the same write must behave normally"
        );
    }

    /// An armed write fails BEFORE it mutates anything — the seam has to model a
    /// write that didn't happen, not a half-applied one, or every test built on
    /// it would be asserting against a state production never reaches.
    #[tokio::test]
    async fn fail_write_leaves_no_partial_row() {
        let db = mem().await;
        let thread_id = live_thread(&db).await;

        fail_write::while_failing("mark_turn_freeze_recovered", async {
            let _ = mark_turn_freeze_recovered(&db, thread_id, None).await;
        })
        .await;

        assert_eq!(
            last_turn_freeze_recovery_secs(&db, thread_id, None).await.unwrap(),
            None,
            "no marker row may survive an injected failure"
        );
    }

    #[tokio::test]
    async fn add_repo_ref_dedupes_by_path_and_remote_within_workspace() {
        let db = mem().await;
        let ws = create_workspace(&db, "ws").await.unwrap();

        let a = add_repo_ref(
            &db,
            ws.id,
            "web-app",
            "/code/web",
            "main",
            "https://github.com/acme/web.git",
            true,
        )
        .await
        .unwrap();

        // Same local path (any name/remote) → returns the existing row, no insert.
        let same_path = add_repo_ref(&db, ws.id, "renamed", "/code/web", "main", "", true)
            .await
            .unwrap();
        assert_eq!(same_path.id, a.id, "same path must not create a second repo");

        // Same remote (normalized: host-case + .git differ), DIFFERENT path — e.g.
        // the same GitHub repo cloned elsewhere → deduped to the first row.
        let same_remote = add_repo_ref(
            &db,
            ws.id,
            "web-2",
            "/elsewhere/web",
            "main",
            "https://GitHub.com/acme/web",
            true,
        )
        .await
        .unwrap();
        assert_eq!(
            same_remote.id, a.id,
            "same remote (normalized) must dedup across paths"
        );

        // A genuinely different repo → a new row.
        let other = add_repo_ref(
            &db,
            ws.id,
            "api",
            "/code/api",
            "main",
            "https://github.com/acme/api.git",
            true,
        )
        .await
        .unwrap();
        assert_ne!(other.id, a.id);

        // Two local repos with NO remote and different paths both exist — an empty
        // remote key must never collapse distinct repos.
        let l1 = add_repo_ref(&db, ws.id, "local-1", "/code/l1", "main", "", true)
            .await
            .unwrap();
        let l2 = add_repo_ref(&db, ws.id, "local-2", "/code/l2", "main", "", true)
            .await
            .unwrap();
        assert_ne!(l1.id, l2.id, "empty remote must not collapse distinct repos");

        // Dedup is workspace-scoped: the same repo in another workspace is allowed.
        let ws2 = create_workspace(&db, "ws2").await.unwrap();
        let elsewhere = add_repo_ref(
            &db,
            ws2.id,
            "web-app",
            "/code/web",
            "main",
            "https://github.com/acme/web.git",
            true,
        )
        .await
        .unwrap();
        assert_ne!(
            elsewhere.id, a.id,
            "same repo in another workspace is a distinct row"
        );

        // ws holds exactly: a, other, l1, l2 (same_path + same_remote deduped).
        assert_eq!(list_repos(&db, ws.id).await.unwrap().len(), 4);
    }

    /// R42-1: re-adding a repo with a VETTED default (is_default=true + a real base_ref)
    /// repairs a legacy/stale marker on the existing row, but re-adding without a vetted
    /// default must NOT clobber an already-true marker or its vetted base_ref.
    #[tokio::test]
    async fn add_repo_ref_re_add_repairs_legacy_default_marker() {
        let db = mem().await;
        let ws = create_workspace(&db, "ws").await.unwrap();

        // Legacy row: registered before the default was vetted — marker is false and the
        // base_ref is a stale guess.
        let legacy = add_repo_ref(&db, ws.id, "web", "/code/web", "stale", "", false)
            .await
            .unwrap();
        assert!(!legacy.base_ref_is_default, "precondition: legacy marker is false");
        assert_eq!(legacy.base_ref, "stale");

        // Re-add the SAME local path with a vetted default → repairs the row in place
        // (same id; marker flips true; base_ref updated) — no second row.
        let repaired = add_repo_ref(&db, ws.id, "web", "/code/web", "develop", "", true)
            .await
            .unwrap();
        assert_eq!(repaired.id, legacy.id, "re-add must repair in place, not insert");
        assert!(repaired.base_ref_is_default, "vetted default repaired the marker");
        assert_eq!(repaired.base_ref, "develop", "vetted base_ref was written through");
        assert_eq!(list_repos(&db, ws.id).await.unwrap().len(), 1, "no duplicate row");

        // Re-add again WITHOUT a vetted default (is_default=false) must NOT clobber the
        // now-true marker nor the vetted base_ref.
        let unchanged = add_repo_ref(&db, ws.id, "web", "/code/web", "whatever", "", false)
            .await
            .unwrap();
        assert_eq!(unchanged.id, legacy.id);
        assert!(
            unchanged.base_ref_is_default,
            "a non-vetted re-add must not downgrade an already-true marker"
        );
        assert_eq!(
            unchanged.base_ref, "develop",
            "a non-vetted re-add must not overwrite the vetted base_ref"
        );
    }

    #[tokio::test]
    async fn delete_repo_cascade_removes_repo_and_its_deps_only() {
        let db = mem().await;
        let ws = create_workspace(&db, "ws").await.unwrap();
        let a = add_repo_ref(&db, ws.id, "a", "/tmp/a", "main", "", true)
            .await
            .unwrap();
        let b = add_repo_ref(&db, ws.id, "b", "/tmp/b", "main", "", true)
            .await
            .unwrap();
        upsert_repo_profile(&db, a.id, "backend", "[]", "", "[]", "agent", "")
            .await
            .unwrap();
        upsert_repo_profile(&db, b.id, "backend", "[]", "", "[]", "agent", "")
            .await
            .unwrap();
        let t = create_thread(&db, ws.id, "T", "feature", "claude")
            .await
            .unwrap();
        // a direction bound to repo `a`, with a session + worktree
        let dir = create_direction(&db, t.id, "d", "claude", a.id, "reason", "plan+impl", "")
            .await
            .unwrap();
        let sess = create_session(&db, dir.id, a.id, "claude", "/tmp/a-wt")
            .await
            .unwrap();
        record_worktree(&db, a.id, dir.id, &dir.branch, "/tmp/a-wt", false, true, "")
            .await
            .unwrap();
        // a direction bound to repo `b` — must SURVIVE the delete of `a`
        let dir_b = create_direction(&db, t.id, "db", "claude", b.id, "reason", "plan+impl", "")
            .await
            .unwrap();
        // A stored workspace map doc (enumerates repos) must be invalidated on delete.
        set_repo_map_doc(&db, ws.id, "## Inventory\n- a (backend)\n- b (backend)").await.unwrap();

        let removed = delete_repo_cascade(&db, a.id).await.unwrap();
        // returns repo `a`'s worktree(s) for the caller to physically remove
        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0].2, "/tmp/a-wt");

        // repo `a` + its profile/direction/session/worktree are gone…
        assert!(get_repo(&db, a.id).await.unwrap().is_none());
        assert!(get_repo_profile(&db, a.id).await.unwrap().is_none());
        assert!(get_direction(&db, dir.id).await.unwrap().is_none());
        assert!(get_session(&db, sess.id).await.unwrap().is_none());
        assert!(list_worktrees(&db, Some(dir.id)).await.unwrap().is_empty());
        // …while repo `b` and the direction bound to it are untouched.
        assert!(get_repo(&db, b.id).await.unwrap().is_some());
        assert!(get_repo_profile(&db, b.id).await.unwrap().is_some());
        assert!(get_direction(&db, dir_b.id).await.unwrap().is_some());
        // …and the stale workspace map doc was cleared (regenerates on next analysis).
        assert!(
            get_repo_map_doc(&db, ws.id).await.unwrap().is_none(),
            "deleting a repo must invalidate the workspace map doc"
        );
    }

    /// R15-1: delete_repo_cascade must carry created_branch in its 4-tuple so
    #[tokio::test]
    async fn update_plan_proposal_cas_rejects_a_stale_write() {
        let db = mem().await;
        let ws = create_workspace(&db, "ws").await.unwrap();
        let t = create_thread(&db, ws.id, "t", "feature", "claude").await.unwrap();
        upsert_plan(&db, t.id, "P1", "proposed", "t0").await.unwrap();
        let plan_a = get_plan(&db, t.id).await.unwrap().unwrap(); // read v1 (proposal == "P1")
        // A re-propose lands AFTER the read but before the CAS write.
        upsert_plan(&db, t.id, "P2", "proposed", "t0").await.unwrap();
        // A CAS expecting the STALE P1 must NOT apply (the live proposal is P2).
        assert!(
            !update_plan_proposal_cas(&db, t.id, "P3", &plan_a.proposal, "proposed").await.unwrap(),
            "CAS must reject a write whose expected proposal is stale"
        );
        assert_eq!(
            get_plan(&db, t.id).await.unwrap().unwrap().proposal, "P2",
            "the stale write left the fresh re-propose intact"
        );
        // A CAS expecting the CURRENT P2 applies.
        assert!(
            update_plan_proposal_cas(&db, t.id, "P3", "P2", "proposed").await.unwrap(),
            "CAS applies when expected matches the live proposal"
        );
        assert_eq!(get_plan(&db, t.id).await.unwrap().unwrap().proposal, "P3");
    }

    #[tokio::test]
    async fn mark_plan_confirmed_cas_only_applies_when_proposal_and_status_match() {
        // R42-4: confirm's final write must flip status -> "confirmed" ONLY if the proposal AND
        // status are still what it read at the start; a re-propose (or concurrent confirm) in
        // between must reject, so the fresh proposal isn't marked confirmed with stale lanes.
        let db = mem().await;
        let ws = create_workspace(&db, "ws").await.unwrap();
        let t = create_thread(&db, ws.id, "t", "feature", "claude").await.unwrap();
        upsert_plan(&db, t.id, "P1", "scoped", "t0").await.unwrap();
        // Stale proposal -> reject.
        assert!(
            !mark_plan_confirmed_cas(&db, t.id, "P0", "scoped").await.unwrap(),
            "must reject when the expected proposal differs (re-proposed)"
        );
        // Drifted status -> reject.
        assert!(
            !mark_plan_confirmed_cas(&db, t.id, "P1", "proposed").await.unwrap(),
            "must reject when the expected status differs"
        );
        assert_eq!(
            get_plan(&db, t.id).await.unwrap().unwrap().status, "scoped",
            "a rejected CAS left the status untouched"
        );
        // Matching proposal + status -> applies; status becomes confirmed, proposal untouched.
        assert!(
            mark_plan_confirmed_cas(&db, t.id, "P1", "scoped").await.unwrap(),
            "must apply when proposal+status match"
        );
        let p = get_plan(&db, t.id).await.unwrap().unwrap();
        assert_eq!(p.status, "confirmed");
        assert_eq!(p.proposal, "P1", "proposal left untouched by the status CAS");
        // Absent plan -> false.
        let no_plan = create_thread(&db, ws.id, "no plan", "feature", "claude")
            .await
            .unwrap();
        assert!(
            !mark_plan_confirmed_cas(&db, no_plan.id, "P1", "scoped").await.unwrap(),
            "must be false when the plan is absent"
        );
    }

    /// R32-3: the CAS predicate must also pin `status`. A targeted base/decision edit
    /// reads a "proposed" plan; if `confirm` flips that SAME proposal JSON to
    /// "confirmed" before the CAS runs, the proposal still matches — but writing the
    /// stale "proposed" status back would reopen an already-materialized plan. The
    /// status guard makes the CAS reject (0 rows) when status drifted.
    #[tokio::test]
    async fn update_plan_proposal_cas_preserves_confirmed_status() {
        let db = mem().await;
        let ws = create_workspace(&db, "ws").await.unwrap();
        let t = create_thread(&db, ws.id, "t", "feature", "claude").await.unwrap();
        upsert_plan(&db, t.id, "P1", "proposed", "t0").await.unwrap();
        // The edit read the plan while it was "proposed" (expected status = "proposed").
        // Meanwhile confirm marked the SAME proposal JSON "confirmed".
        upsert_plan(&db, t.id, "P1", "confirmed", "t0").await.unwrap();
        // A CAS whose expected proposal matches the live row but whose status differs
        // (live="confirmed", call passes "proposed") must NOT apply.
        assert!(
            !update_plan_proposal_cas(&db, t.id, "P2", "P1", "proposed").await.unwrap(),
            "CAS must reject when the live status drifted away from the expected status"
        );
        let after = get_plan(&db, t.id).await.unwrap().unwrap();
        assert_eq!(after.proposal, "P1", "stale-status write must not touch the proposal");
        assert_eq!(after.status, "confirmed", "the confirmed status must survive the rejected edit");
        // A CAS that agrees on BOTH proposal and the live status applies.
        assert!(
            update_plan_proposal_cas(&db, t.id, "P2", "P1", "confirmed").await.unwrap(),
            "CAS applies when both proposal and status match the live row"
        );
        assert_eq!(get_plan(&db, t.id).await.unwrap().unwrap().proposal, "P2");
    }

    /// the caller can gate branch deletion. A worktree row with created_branch=false
    #[tokio::test]
    async fn worktree_created_branch_defaults_to_true_when_unset() {
        // A worktree row inserted WITHOUT created_branch (a legacy/pre-column row) must
        // default to TRUE — pre-this-change worktrees had their branch created by Weft,
        // so cascade cleanup must still tear those branches down (zero-accumulation).
        use sea_orm::{ActiveModelTrait, EntityTrait, Set};
        let db = mem().await;
        let ws = create_workspace(&db, "ws").await.unwrap();
        let r = add_repo_ref(&db, ws.id, "r", "/tmp/r", "main", "", true).await.unwrap();
        let t = create_thread(&db, ws.id, "t", "feature", "claude").await.unwrap();
        let d = create_direction(&db, t.id, "d", "claude", r.id, "x", "plan+impl", "")
            .await
            .unwrap();
        let inserted = worktree::ActiveModel {
            repo_id: Set(r.id),
            direction_id: Set(d.id),
            branch: Set("feat/x".into()),
            path: Set("/tmp/wt".into()),
            created_at: Set(now()),
            created_checkout: Set(true),
            // created_branch intentionally NotSet → the DB column default applies.
            ..Default::default()
        }
        .insert(&db.0)
        .await
        .unwrap();
        // Re-fetch to read what the DB actually persisted (the default), not the
        // ActiveModel's unset Rust-side value.
        let row = worktree::Entity::find_by_id(inserted.id)
            .one(&db.0)
            .await
            .unwrap()
            .unwrap();
        assert!(row.created_branch, "created_branch must default to true when unset");
    }

    /// M0028: worktree.base_commit round-trips and an UNSET column defaults to "" (legacy/
    /// pre-column rows, which the reuse-time fork-commit validation then SKIPS).
    #[tokio::test]
    async fn worktree_base_commit_round_trips_and_defaults_to_empty() {
        use sea_orm::{ActiveModelTrait, EntityTrait, Set};
        let db = mem().await;
        let ws = create_workspace(&db, "ws").await.unwrap();
        let r = add_repo_ref(&db, ws.id, "r", "/tmp/r", "main", "", true).await.unwrap();
        let t = create_thread(&db, ws.id, "t", "feature", "claude").await.unwrap();
        let d = create_direction(&db, t.id, "d", "claude", r.id, "x", "plan+impl", "")
            .await
            .unwrap();

        // (1) Inserted WITHOUT base_commit (a legacy/pre-column row) → defaults to "".
        let legacy = worktree::ActiveModel {
            repo_id: Set(r.id),
            direction_id: Set(d.id),
            branch: Set("feat/legacy".into()),
            path: Set("/tmp/wt-legacy".into()),
            created_at: Set(now()),
            created_branch: Set(true),
            created_checkout: Set(true),
            // base_commit intentionally NotSet → the DB column default ("") applies.
            ..Default::default()
        }
        .insert(&db.0)
        .await
        .unwrap();
        let legacy_row = worktree::Entity::find_by_id(legacy.id)
            .one(&db.0)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(legacy_row.base_commit, "", "base_commit must default to empty when unset");

        // (2) record_worktree persists a non-empty base_commit, and set_worktree_base_commit
        // updates it — both round-trip through the column.
        let d2 = create_direction(&db, t.id, "d2", "claude", r.id, "x", "plan+impl", "")
            .await
            .unwrap();
        let rec = record_worktree(&db, r.id, d2.id, "feat/rec", "/tmp/wt-rec", true, true, "abc123")
            .await
            .unwrap();
        assert_eq!(rec.base_commit, "abc123", "record_worktree persists base_commit");
        set_worktree_base_commit(&db, rec.id, "def456").await.unwrap();
        let updated = worktree::Entity::find_by_id(rec.id)
            .one(&db.0)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(updated.base_commit, "def456", "set_worktree_base_commit updates the row");
    }

    /// (pre-existing branch reused by the -b fallback) must have its flag preserved.
    #[tokio::test]
    async fn delete_repo_cascade_carries_created_branch_flag() {
        let db = mem().await;
        let ws = create_workspace(&db, "ws").await.unwrap();
        let r = add_repo_ref(&db, ws.id, "repo", "/tmp/r", "main", "", true)
            .await
            .unwrap();
        let t = create_thread(&db, ws.id, "T", "feature", "claude")
            .await
            .unwrap();
        let dir = create_direction(&db, t.id, "d", "claude", r.id, "reason", "plan+impl", "")
            .await
            .unwrap();

        // Record one worktree with created_branch=false (pre-existing branch).
        record_worktree(&db, r.id, dir.id, "feat/preexist", "/tmp/r-wt", false, true, "")
            .await
            .unwrap();
        // Record another with created_branch=true (weft-created branch).
        let dir2 = create_direction(&db, t.id, "d2", "claude", r.id, "reason2", "plan+impl", "")
            .await
            .unwrap();
        record_worktree(&db, r.id, dir2.id, "feat/weft-created", "/tmp/r-wt2", true, true, "")
            .await
            .unwrap();

        let removed = delete_repo_cascade(&db, r.id).await.unwrap();
        assert_eq!(removed.len(), 2);

        // Both tuples must carry the correct created_branch flag.
        let preexist = removed.iter().find(|t| t.2 == "/tmp/r-wt").unwrap();
        assert!(!preexist.4, "pre-existing branch must have created_branch=false");
        assert!(preexist.5, "created_checkout defaults to true");
        let created = removed.iter().find(|t| t.2 == "/tmp/r-wt2").unwrap();
        assert!(created.4, "weft-created branch must have created_branch=true");
        assert!(created.5, "created_checkout defaults to true");
    }

    #[tokio::test]
    async fn calibrate_repo_relation_clears_both_endpoint_layers() {
        // Pinning/removing an edge changes the relative ordering of BOTH endpoints; the
        // map now reads layers (not edges), so both stale layers must be cleared
        // (→ tier/category fallback) until the next pass re-derives.
        let db = mem().await;
        let ws = create_workspace(&db, "ws").await.unwrap();
        let web = add_repo_ref(&db, ws.id, "web", "/tmp/web", "main", "", true).await.unwrap();
        let api = add_repo_ref(&db, ws.id, "api", "/tmp/api", "main", "", true).await.unwrap();
        upsert_repo_profile(&db, web.id, "frontend", "[]", "", "[]", "agent", "").await.unwrap();
        upsert_repo_profile(&db, api.id, "backend", "[]", "", "[]", "agent", "").await.unwrap();
        set_repo_layer_rank(&db, web.id, "Client", 5).await.unwrap();
        set_repo_layer_rank(&db, api.id, "Service", 4).await.unwrap();

        calibrate_repo_relation(&db, web.id, api.id, "grpc", "Pricing.Quote", "add")
            .await
            .unwrap();

        let from = get_repo_profile(&db, web.id).await.unwrap().unwrap();
        assert_eq!((from.layer.as_str(), from.layer_rank), ("", 0), "consumer layer cleared");
        let to = get_repo_profile(&db, api.id).await.unwrap().unwrap();
        assert_eq!((to.layer.as_str(), to.layer_rank), ("", 0), "target layer cleared too");
    }

    #[tokio::test]
    async fn calibrate_repo_relation_adds_user_edge_then_tombstones_removal() {
        let db = mem().await;
        let ws = create_workspace(&db, "ws").await.unwrap();
        let web = add_repo_ref(&db, ws.id, "web", "/tmp/web", "main", "", true)
            .await
            .unwrap();
        let api = add_repo_ref(&db, ws.id, "api", "/tmp/api", "main", "", true)
            .await
            .unwrap();
        upsert_repo_profile(&db, web.id, "frontend", "[]", "", "[]", "agent", "")
            .await
            .unwrap();
        let read = |db: &Db, id| {
            let db = db.clone();
            async move {
                let p = get_repo_profile(&db, id).await.unwrap().unwrap();
                serde_json::from_str::<Vec<crate::profile::AgentRelation>>(&p.relations).unwrap()
            }
        };

        // add → one user-sourced relation for the pair
        calibrate_repo_relation(&db, web.id, api.id, "grpc", "Pricing.Quote", "add")
            .await
            .unwrap();
        let rels = read(&db, web.id).await;
        assert_eq!(rels.len(), 1);
        assert_eq!((rels[0].to, rels[0].kind.as_str()), (api.id, "grpc"));
        assert_eq!(rels[0].source, "user");
        assert!(!rels[0].rejected);

        // remove the SAME (to, kind, via) → replaces it with a single tombstone.
        calibrate_repo_relation(&db, web.id, api.id, "grpc", "Pricing.Quote", "remove")
            .await
            .unwrap();
        let rels = read(&db, web.id).await;
        assert_eq!(rels.len(), 1);
        assert!(rels[0].rejected, "removal writes a tombstone");
        assert_eq!(rels[0].source, "user");

        // A distinct edge (same to/kind, different via) is a SEPARATE entry — it
        // doesn't replace the tombstone above.
        calibrate_repo_relation(&db, web.id, api.id, "grpc", "Other.Call", "add")
            .await
            .unwrap();
        let rels = read(&db, web.id).await;
        assert_eq!(rels.len(), 2, "distinct via is a separate calibration");

        // a producer with no profile row yet (an "analyzing" placeholder) gets a
        // minimal row created so the calibration persists instead of vanishing.
        let lib = add_repo_ref(&db, ws.id, "lib", "/tmp/lib", "main", "", true)
            .await
            .unwrap();
        assert!(get_repo_profile(&db, lib.id).await.unwrap().is_none());
        calibrate_repo_relation(&db, lib.id, api.id, "http", "GET /x", "add")
            .await
            .unwrap();
        let rels = read(&db, lib.id).await;
        assert_eq!(rels.len(), 1, "calibration on a placeholder persists");
        assert_eq!((rels[0].to, rels[0].kind.as_str(), rels[0].source.as_str()), (api.id, "http", "user"));
    }

    #[tokio::test]
    async fn ensure_curator_thread_is_idempotent_and_kinded() {
        let db = mem().await;
        let ws = create_workspace(&db, "ws").await.unwrap();
        let a = ensure_curator_thread(&db, ws.id, "codex").await.unwrap();
        assert!(
            !get_thread(&db, a).await.unwrap().unwrap().engine_pinned,
            "a configured/default curator engine is not a manual pin"
        );
        set_thread_engine_pinned(&db, a, true).await.unwrap();
        let b = ensure_curator_thread(&db, ws.id, "codex").await.unwrap();
        assert_eq!(a, b, "the same curator thread is reused");
        let t = get_thread(&db, a).await.unwrap().unwrap();
        assert_eq!(t.kind, "curator");
        assert_eq!(t.lead_tool, "codex", "uses the provided default tool, not hard-coded claude");
        assert!(t.engine_pinned, "reusing a curator must preserve a user pin");
        // a normal issue coexists; the board view filters curator out.
        create_thread(&db, ws.id, "Real issue", "feature", "claude")
            .await
            .unwrap();
        let board: Vec<_> = list_threads(&db, ws.id)
            .await
            .unwrap()
            .into_iter()
            .filter(|t| t.kind != "curator")
            .collect();
        assert!(board.iter().all(|t| t.kind != "curator"));
        assert!(board.iter().any(|t| t.kind == "feature"));
    }

    #[tokio::test]
    async fn lead_message_roundtrip() {
        let db = mem().await;
        let t = live_thread(&db).await;
        let m = insert_lead_message(
            &db,
            t,
            None,
            1,
            "user",
            "text",
            r#"{"text":"hi"}"#,
            "complete",
        )
        .await
        .unwrap();
        assert_eq!(m.thread_id, t);
        update_lead_message(&db, m.id, r#"{"text":"hi!"}"#, "complete")
            .await
            .unwrap();
        let all = list_lead_messages(&db, t).await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].content, r#"{"text":"hi!"}"#);
        assert_eq!(next_turn_id(&db, t).await.unwrap(), 2);
    }

    #[tokio::test]
    async fn resolve_action_card_persists_resolved_marker() {
        let db = mem().await;
        let t = live_thread(&db).await;
        let card = insert_lead_message(
            &db,
            t,
            None,
            1,
            "system",
            "action_card",
            r#"{"title":"Add a repo","actions":[{"id":"add","label":"Import","kind":"add"}]}"#,
            "complete",
        )
        .await
        .unwrap();
        let updated = resolve_action_card(&db, card.id, "weft")
            .await
            .unwrap()
            .expect("card exists");
        let v: serde_json::Value = serde_json::from_str(&updated.content).unwrap();
        assert_eq!(v["resolved"], "weft");
        // existing fields are preserved, not clobbered
        assert_eq!(v["title"], "Add a repo");
        // and it survives reload (persisted, not session-local)
        let all = list_lead_messages(&db, t).await.unwrap();
        assert_eq!(all[0].content, updated.content);
        // a missing row is a no-op
        assert!(resolve_action_card(&db, 9999, "x").await.unwrap().is_none());
    }

    /// Deleting an issue removes every thread-owned row — chat history, plan,
    /// IM routes, and the test-case document — not just directions/sessions.
    #[tokio::test]
    async fn thread_cascade_deletes_thread_owned_rows() {
        let db = mem().await;
        let ws = create_workspace(&db, "w").await.unwrap();
        let t = create_thread(&db, ws.id, "issue", "feature", "claude")
            .await
            .unwrap();
        upsert_test_plan(&db, t.id, "# doc\n- case\n", "lead").await.unwrap();
        insert_lead_message(&db, t.id, None, 1, "assistant", "text", "{\"text\":\"hi\"}", "complete")
            .await
            .unwrap();
        delete_thread_cascade(&db, t.id).await.unwrap();
        assert!(get_test_plan(&db, t.id).await.unwrap().is_none());
        assert!(list_lead_messages(&db, t.id).await.unwrap().is_empty());
        assert!(get_thread(&db, t.id).await.unwrap().is_none());
        // The write fence: a late save/sentinel can't recreate an orphan row.
        assert!(
            upsert_test_plan(&db, t.id, "# late\n- x\n", "user").await.is_err(),
            "upsert after deletion must be rejected"
        );
        assert!(get_test_plan(&db, t.id).await.unwrap().is_none());
    }

    /// test_plan upsert enforces 0..1 per thread (M0035 UNIQUE thread_id):
    /// the second write updates in place and flips the source.
    #[tokio::test]
    async fn test_plan_upserts_one_doc_per_thread() {
        let db = mem().await;
        let ws = create_workspace(&db, "w").await.unwrap();
        let t = create_thread(&db, ws.id, "issue", "feature", "claude")
            .await
            .unwrap();
        assert!(get_test_plan(&db, t.id).await.unwrap().is_none());
        let first = upsert_test_plan(&db, t.id, "# v1\n- a\n", "lead").await.unwrap();
        assert_eq!(first.source, "lead");
        let second = upsert_test_plan(&db, t.id, "# v2\n- a\n- b\n", "user")
            .await
            .unwrap();
        assert_eq!(second.id, first.id, "same row updated, not a new one");
        let read = get_test_plan(&db, t.id).await.unwrap().expect("doc exists");
        assert_eq!(read.content, "# v2\n- a\n- b\n");
        assert_eq!(read.source, "user");
    }

    /// The lead-emit CAS lives in the SQL predicate itself: a user row saved at
    /// or after the turn began wins; older user rows and lead rows are
    /// replaced; a missing row inserts.
    #[tokio::test]
    async fn lead_upsert_cas_respects_newer_user_saves() {
        let db = mem().await;
        let ws = create_workspace(&db, "w").await.unwrap();
        let t = create_thread(&db, ws.id, "issue", "feature", "claude")
            .await
            .unwrap();
        // No row yet → insert.
        assert!(lead_upsert_test_plan(&db, t.id, "# v1\n- a\n", 5_000).await.unwrap());
        // Simulate a USER save stamped at t=10_000ms.
        upsert_test_plan(&db, t.id, "# user\n- edited\n", "user").await.unwrap();
        test_plan::Entity::update_many()
            .col_expr(test_plan::Column::UpdatedAt, Expr::value("10000"))
            .filter(test_plan::Column::ThreadId.eq(t.id))
            .exec(&db.0)
            .await
            .unwrap();
        // A turn that started BEFORE the save (t=9_000) is stale → rejected.
        assert!(!lead_upsert_test_plan(&db, t.id, "# stale\n- x\n", 9_000).await.unwrap());
        let row = get_test_plan(&db, t.id).await.unwrap().unwrap();
        assert_eq!(row.content, "# user\n- edited\n");
        assert_eq!(row.source, "user");
        // Same-millisecond boundary: still the user's (>= is conservative).
        assert!(!lead_upsert_test_plan(&db, t.id, "# stale\n- x\n", 10_000).await.unwrap());
        // A turn that started AFTER the save (t=11_000) saw it as input → wins.
        assert!(lead_upsert_test_plan(&db, t.id, "# revised\n- y\n", 11_000).await.unwrap());
        let row = get_test_plan(&db, t.id).await.unwrap().unwrap();
        assert_eq!(row.source, "lead");
        assert_eq!(row.content, "# revised\n- y\n");
    }

    /// Engine meta snapshots roundtrip through thread.lead_meta / session.meta,
    /// and a missing row is a fire-and-forget no-op.
    #[tokio::test]
    async fn engine_meta_snapshot_roundtrips() {
        let db = mem().await;
        let ws = create_workspace(&db, "w").await.unwrap();
        let t = create_thread(&db, ws.id, "issue", "feature", "claude")
            .await
            .unwrap();
        assert_eq!(t.lead_meta, "");
        save_lead_meta(&db, t.id, r#"{"context_tokens":42}"#)
            .await
            .unwrap();
        let t2 = get_thread(&db, t.id).await.unwrap().unwrap();
        assert_eq!(t2.lead_meta, r#"{"context_tokens":42}"#);
        // Missing rows are tolerated (engine may outlive a deleted thread).
        save_lead_meta(&db, 9999, "{}").await.unwrap();

        let r = add_repo_ref(&db, ws.id, "svc", "/tmp/svc-meta", "main", "", true)
            .await
            .unwrap();
        let d = create_direction(&db, t.id, "dir", "claude", r.id, "why", "plan+impl", "")
            .await
            .unwrap();
        let s = create_session(&db, d.id, r.id, "claude", "/tmp/cwd").await.unwrap();
        assert_eq!(s.meta, "");
        save_session_meta(&db, s.id, r#"{"model":"gpt-5"}"#).await.unwrap();
        let s2 = get_session(&db, s.id).await.unwrap().unwrap();
        assert_eq!(s2.meta, r#"{"model":"gpt-5"}"#);
        save_session_meta(&db, 9999, "{}").await.unwrap();
    }

    #[tokio::test]
    async fn stale_streaming_messages_mark_interrupted_on_reopen() {
        let db = mem().await;
        let t = live_thread(&db).await;
        let streaming = insert_lead_message(
            &db,
            t,
            Some(9),
            1,
            "assistant",
            "text",
            r#"{"text":"partial"}"#,
            "streaming",
        )
        .await
        .unwrap();
        let queued = insert_lead_message(
            &db,
            t,
            Some(9),
            2,
            "user",
            "text",
            r#"{"text":"next"}"#,
            "queued",
        )
        .await
        .unwrap();

        mark_incomplete_turns_interrupted(&db, t, Some(9))
            .await
            .unwrap();

        let all = list_lead_messages(&db, t).await.unwrap();
        assert_eq!(
            all.iter().find(|m| m.id == streaming.id).unwrap().status,
            "interrupted"
        );
        // Orphaned "queued" rows are NOT touched here; fail_queued (called by
        // revive) flips them to "error" so they surface as resendable, not stuck.
        assert_eq!(
            all.iter().find(|m| m.id == queued.id).unwrap().status,
            "queued"
        );
    }

    #[tokio::test]
    async fn reset_stale_running_sessions_idles_legacy_rows() {
        let db = mem().await;
        let (_, repo, _, dir) = worker_fixture(&db).await;
        // Pre-fix rows: status was a write-once high-water-mark, so an idle worker
        // reads "running" (or "starting" before it ever attached).
        let running = create_session(&db, dir.id, repo.id, "codex", "/tmp/a")
            .await
            .unwrap();
        set_session_status(&db, running.id, "running")
            .await
            .unwrap();
        let starting = create_session(&db, dir.id, repo.id, "codex", "/tmp/b")
            .await
            .unwrap();
        set_session_status(&db, starting.id, "starting")
            .await
            .unwrap();
        let idle = create_session(&db, dir.id, repo.id, "codex", "/tmp/c")
            .await
            .unwrap();
        set_session_status(&db, idle.id, "idle").await.unwrap();

        reset_stale_running_sessions(&db.0).await.unwrap();

        assert_eq!(
            get_session(&db, running.id).await.unwrap().unwrap().status,
            "idle"
        );
        assert_eq!(
            get_session(&db, starting.id).await.unwrap().unwrap().status,
            "idle"
        );
        assert_eq!(
            get_session(&db, idle.id).await.unwrap().unwrap().status,
            "idle"
        );
    }
    #[tokio::test]
    async fn queued_flips_to_complete() {
        let db = mem().await;
        let t = live_thread(&db).await;
        insert_lead_message(
            &db,
            t,
            None,
            2,
            "user",
            "text",
            r#"{"text":"later"}"#,
            "queued",
        )
        .await
        .unwrap();
        let updated = complete_queued(&db, t, None).await.unwrap().unwrap();
        assert_eq!(updated.status, "complete");
        let all = list_lead_messages(&db, t).await.unwrap();
        assert_eq!(all[0].status, "complete");
    }

    #[tokio::test]
    async fn queued_status_updates_are_session_scoped() {
        let db = mem().await;
        let t = live_thread(&db).await;
        let lead = insert_lead_message(
            &db,
            t,
            None,
            1,
            "user",
            "text",
            r#"{"text":"lead"}"#,
            "queued",
        )
        .await
        .unwrap();
        let worker = insert_lead_message(
            &db,
            t,
            Some(3),
            1,
            "user",
            "text",
            r#"{"text":"worker"}"#,
            "queued",
        )
        .await
        .unwrap();

        let completed = complete_queued(&db, t, Some(3)).await.unwrap().unwrap();
        assert_eq!(completed.id, worker.id);
        let failed = set_queued_status(&db, t, None, "interrupted")
            .await
            .unwrap();

        assert_eq!(
            failed.iter().map(|m| m.id).collect::<Vec<_>>(),
            vec![lead.id]
        );
        let all = list_lead_messages(&db, t).await.unwrap();
        assert_eq!(
            all.iter().find(|m| m.id == worker.id).unwrap().status,
            "complete"
        );
        assert_eq!(
            all.iter().find(|m| m.id == lead.id).unwrap().status,
            "interrupted"
        );
    }

    #[tokio::test]
    async fn lead_message_anchor_roundtrip() {
        let db = mem().await;
        let t = live_thread(&db).await;
        let m = insert_lead_message(
            &db,
            t,
            Some(7),
            1,
            "user",
            "text",
            r#"{"text":"hi"}"#,
            "complete",
        )
        .await
        .unwrap();
        assert_eq!(m.native_anchor, None);
        set_lead_message_anchor(&db, m.id, "uuid-1").await.unwrap();
        let all = list_lead_messages(&db, t).await.unwrap();
        assert_eq!(all[0].native_anchor.as_deref(), Some("uuid-1"));
    }

    #[tokio::test]
    async fn truncate_lead_messages_scoped_to_thread_and_session() {
        let db = mem().await;
        let t = live_thread(&db).await;
        // Target session: one row before the cut, then the cut row itself, a
        // later assistant row, and a queued row (the abandoned future).
        let keep = insert_lead_message(&db, t, Some(7), 1, "user", "text", "{}", "complete")
            .await
            .unwrap();
        let cut = insert_lead_message(&db, t, Some(7), 2, "user", "text", "{}", "complete")
            .await
            .unwrap();
        let after = insert_lead_message(&db, t, Some(7), 2, "assistant", "text", "{}", "complete")
            .await
            .unwrap();
        let queued = insert_lead_message(&db, t, Some(7), 3, "user", "text", "{}", "queued")
            .await
            .unwrap();
        // Same thread, other session + lead rows (higher ids) must survive.
        let other = insert_lead_message(&db, t, Some(8), 1, "user", "text", "{}", "complete")
            .await
            .unwrap();
        let lead = insert_lead_message(&db, t, None, 1, "user", "text", "{}", "complete")
            .await
            .unwrap();

        let deleted = truncate_lead_messages(&db.0, t, Some(7), cut.id)
            .await
            .unwrap();
        assert_eq!(deleted.len(), 3);
        assert!(deleted.contains(&cut.id) && deleted.contains(&after.id) && deleted.contains(&queued.id));

        let remaining: Vec<i32> = list_lead_messages(&db, t)
            .await
            .unwrap()
            .iter()
            .map(|m| m.id)
            .collect();
        assert!(remaining.contains(&keep.id), "row before the cut stays");
        assert!(remaining.contains(&other.id), "other session untouched");
        assert!(remaining.contains(&lead.id), "lead timeline untouched");
        assert!(!remaining.contains(&cut.id), "target row itself is deleted");
        assert!(!remaining.contains(&after.id), "later rows are deleted");
        assert!(!remaining.contains(&queued.id), "queued rows are deleted");
    }

    /// Codex-review regression: queued rows are stamped a delivery `seq` at
    /// dequeue (`max(COALESCE(seq,id)) + 1`), so a message queued earlier can
    /// be delivered LATER while keeping a smaller id. The truncation must
    /// follow the delivery order (`COALESCE(seq, id), id`), not raw id order.
    #[tokio::test]
    async fn truncate_lead_messages_follows_delivery_order() {
        let db = mem().await;
        let t = live_thread(&db).await;
        // Production shape: turn 1 completes; b and a queue behind it; the
        // queue delivers a FIRST (a.seq < b.seq), then b. Reply rows are
        // inserted as their turns run, so b (queued first) holds a smaller id
        // than a yet displays after a's whole exchange.
        let first = insert_lead_message(&db, t, Some(7), 1, "user", "text", "{}", "complete")
            .await
            .unwrap();
        let first_reply = insert_lead_message(&db, t, Some(7), 1, "assistant", "text", "{}", "complete")
            .await
            .unwrap();
        let b = insert_lead_message(&db, t, Some(7), 2, "user", "text", "{}", "complete")
            .await
            .unwrap();
        let a = insert_lead_message(&db, t, Some(7), 3, "user", "text", "{}", "complete")
            .await
            .unwrap();
        assign_delivery_seq(&db, t, a.id).await.unwrap(); // a dequeued first
        let a_reply = insert_lead_message(&db, t, Some(7), 3, "assistant", "text", "{}", "complete")
            .await
            .unwrap();
        assign_delivery_seq(&db, t, b.id).await.unwrap(); // b dequeued second
        let b_reply = insert_lead_message(&db, t, Some(7), 2, "assistant", "text", "{}", "complete")
            .await
            .unwrap();

        // Sanity: delivery order is first, first_reply, a, a_reply, b, b_reply
        // — with b.id < a.id despite b displaying later.
        assert!(b.id < a.id);
        let ordered: Vec<i32> = list_lead_messages(&db, t)
            .await
            .unwrap()
            .iter()
            .map(|m| m.id)
            .collect();
        assert_eq!(
            ordered,
            vec![first.id, first_reply.id, a.id, a_reply.id, b.id, b_reply.id]
        );

        // Rewind to before b: only b and b_reply may go — an id-based cut
        // (id >= b.id) would also kill a and a_reply.
        let deleted = truncate_lead_messages(&db.0, t, Some(7), b.id)
            .await
            .unwrap();
        assert_eq!(deleted, vec![b.id, b_reply.id]);
        let remaining: Vec<i32> = list_lead_messages(&db, t)
            .await
            .unwrap()
            .iter()
            .map(|m| m.id)
            .collect();
        assert_eq!(remaining, vec![first.id, first_reply.id, a.id, a_reply.id]);
    }

    /// rewind_persist commits truncation + checkpoint sweep + native id in one
    /// transaction (worker path here): all three effects visible after commit.
    #[tokio::test]
    async fn rewind_persist_is_atomic_and_complete() {
        let db = mem().await;
        let (_ws, r, _t, d) = worker_fixture(&db).await;
        let s = create_session(&db, d.id, r.id, "claude", "/tmp/cwd")
            .await
            .unwrap();
        set_session_native_id_opt(&db, s.id, Some("old-native"))
            .await
            .unwrap();
        let t = d.thread_id;
        let m1 = insert_lead_message(&db, t, Some(s.id), 1, "user", "text", "{}", "complete")
            .await
            .unwrap();
        insert_lead_message(&db, t, Some(s.id), 2, "user", "text", "{}", "complete")
            .await
            .unwrap();
        insert_code_checkpoint(&db, 11, s.id, m1.id, 1, "sha-1", "head-1", "[]", "idx-1")
            .await
            .unwrap();
        // A checkpoint for the KEPT turn must survive the sweep.
        insert_code_checkpoint(&db, 11, s.id, 999, 1, "sha-0", "head-0", "[]", "")
            .await
            .unwrap();

        let deleted = rewind_persist(&db, t, Some(s.id), m1.id, Some(11), Some("new-native"))
            .await
            .unwrap();
        assert_eq!(deleted.len(), 2, "both timeline rows deleted");
        assert!(list_lead_messages(&db, t).await.unwrap().is_empty());
        assert!(
            code_checkpoint_for(&db, 11, m1.id).await.unwrap().is_none(),
            "abandoned turn's checkpoint swept"
        );
        assert!(
            code_checkpoint_for(&db, 11, 999).await.unwrap().is_some(),
            "unrelated checkpoint kept"
        );
        assert_eq!(
            get_session(&db, s.id).await.unwrap().unwrap().native_session_id,
            Some("new-native".to_string())
        );
    }

    #[tokio::test]
    async fn session_native_id_opt_sets_and_clears() {
        let db = mem().await;
        let (_ws, r, _t, d) = worker_fixture(&db).await;
        let s = create_session(&db, d.id, r.id, "codex", "/tmp/cwd")
            .await
            .unwrap();
        set_session_native_id_opt(&db, s.id, Some("native-1"))
            .await
            .unwrap();
        assert_eq!(
            get_session(&db, s.id).await.unwrap().unwrap().native_session_id,
            Some("native-1".to_string())
        );
        set_session_native_id_opt(&db, s.id, None).await.unwrap();
        assert_eq!(
            get_session(&db, s.id).await.unwrap().unwrap().native_session_id,
            None
        );
    }

    #[tokio::test]
    async fn lead_native_id_upserts() {
        let db = mem().await;
        let t = live_thread(&db).await;
        assert!(lead_native_id(&db, t).await.unwrap().is_none());
        set_lead_native_id(&db, t, "abc").await.unwrap();
        set_lead_native_id(&db, t, "def").await.unwrap();
        assert_eq!(
            lead_native_id(&db, t).await.unwrap().as_deref(),
            Some("def")
        );
        // meta row stays single + out of turn numbering
        assert_eq!(list_lead_messages(&db, t).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn lead_native_id_opt_clears_preserving_status() {
        let db = mem().await;
        let t = live_thread(&db).await;
        set_lead_status(&db, t, "idle").await.unwrap();
        set_lead_native_id_opt(&db, t, Some("nat-1")).await.unwrap();
        assert_eq!(
            lead_native_id(&db, t).await.unwrap().as_deref(),
            Some("nat-1")
        );
        // Clearing keeps the meta row's other fields (status) but drops the id.
        set_lead_native_id_opt(&db, t, None).await.unwrap();
        assert!(lead_native_id(&db, t).await.unwrap().is_none());
        assert_eq!(lead_status(&db, t).await.unwrap().as_deref(), Some("idle"));

        // A meta row holding ONLY native_id is deleted outright on clear, so
        // the next turn starts completely fresh.
        let ws = create_workspace(&db, "ws2").await.unwrap();
        let t2 = create_thread(&db, ws.id, "t2", "feature", "claude")
            .await
            .unwrap()
            .id;
        set_lead_native_id_opt(&db, t2, Some("nat-x")).await.unwrap();
        assert_eq!(list_lead_messages(&db, t2).await.unwrap().len(), 1);
        set_lead_native_id_opt(&db, t2, None).await.unwrap();
        assert!(
            list_lead_messages(&db, t2).await.unwrap().is_empty(),
            "meta row holding only native_id is deleted on clear"
        );
    }

    #[tokio::test]
    async fn code_checkpoint_insert_lookup_truncate() {
        let db = mem().await;
        // Rows need no live worktree/session (no FKs), so plain literals do.
        let c1 = insert_code_checkpoint(&db, 11, 7, 100, 1, "sha-1", "head-1", "[\"gen\"]", "idx-1")
            .await
            .unwrap();
        insert_code_checkpoint(&db, 11, 7, 200, 2, "sha-2", "head-2", "[]", "")
            .await
            .unwrap();
        insert_code_checkpoint(&db, 22, 8, 100, 1, "sha-other", "head-other", "[]", "")
            .await
            .unwrap();

        let found = code_checkpoint_for(&db, 11, 100).await.unwrap().unwrap();
        assert_eq!(found.id, c1.id);
        assert_eq!(found.shadow_sha, "sha-1");
        assert_eq!(found.head_sha, "head-1");
        assert_eq!(found.nested_repos, "[\"gen\"]");
        assert_eq!(found.session_id, 7);
        assert_eq!(found.turn_id, 1);
        assert!(code_checkpoint_for(&db, 11, 999).await.unwrap().is_none());

        // Truncate drops the checkpoints keyed by the deleted timeline rows of
        // THIS worktree only.
        let deleted = truncate_code_checkpoints(&db.0, 11, &[100, 200]).await.unwrap();
        assert_eq!(deleted, 2);
        assert!(code_checkpoint_for(&db, 11, 100).await.unwrap().is_none());
        assert!(
            code_checkpoint_for(&db, 22, 100).await.unwrap().is_some(),
            "other worktree untouched"
        );

        let deleted = delete_code_checkpoints_for_worktree(&db, 22).await.unwrap();
        assert_eq!(deleted, 1);
        assert!(code_checkpoint_for(&db, 22, 100).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn session_status_round_trips() {
        let db = mem().await;
        let (_, repo, _, dir) = worker_fixture(&db).await;
        let s = create_session(&db, dir.id, repo.id, "codex", "/tmp/wt")
            .await
            .unwrap();
        set_session_status(&db, s.id, "idle").await.unwrap();
        assert_eq!(
            get_session(&db, s.id).await.unwrap().unwrap().status,
            "idle"
        );
        set_session_status(&db, s.id, "running").await.unwrap();
        assert_eq!(
            get_session(&db, s.id).await.unwrap().unwrap().status,
            "running"
        );
    }

    #[tokio::test]
    async fn lead_status_round_trips_and_preserves_native_id() {
        let db = mem().await;
        // Real thread rows: set_lead_status only INSERTS its meta row for threads
        // that still exist (the fence below), so the round-trip cases must run
        // against live threads — which is also what production does.
        let ws = create_workspace(&db, "ws").await.unwrap();
        let t7 = create_thread(&db, ws.id, "a", "feature", "claude")
            .await
            .unwrap();
        let t8 = create_thread(&db, ws.id, "b", "feature", "claude")
            .await
            .unwrap();
        set_lead_native_id(&db, t7.id, "nat-xyz").await.unwrap();
        set_lead_status(&db, t7.id, "running").await.unwrap();
        assert_eq!(
            lead_status(&db, t7.id).await.unwrap().as_deref(),
            Some("running")
        );
        assert_eq!(
            lead_native_id(&db, t7.id).await.unwrap().as_deref(),
            Some("nat-xyz")
        );
        // opposite write order must also coexist (status first, native id second)
        set_lead_status(&db, t8.id, "idle").await.unwrap();
        set_lead_native_id(&db, t8.id, "nat-8").await.unwrap();
        assert_eq!(
            lead_status(&db, t8.id).await.unwrap().as_deref(),
            Some("idle")
        );
        assert_eq!(
            lead_native_id(&db, t8.id).await.unwrap().as_deref(),
            Some("nat-8")
        );
        // The fence: a deleted/nonexistent thread gets NO meta row — stop() after
        // delete_thread's cascade must not recreate orphan timeline data.
        set_lead_status(&db, 999, "stopped").await.unwrap();
        assert_eq!(
            lead_status(&db, 999).await.unwrap(),
            None,
            "no meta row may be inserted for a deleted thread"
        );
    }

    #[tokio::test]
    async fn im_route_bind_and_lookup() {
        let db = mem().await;
        let ws = create_workspace(&db, "ws").await.unwrap();
        let thread = create_thread(&db, ws.id, "issue", "feature", "claude")
            .await
            .unwrap();
        let r = bind_im_route(&db, thread.id, "feishu", "oc_chat", "th_1")
            .await
            .unwrap();
        assert_eq!(r.thread_id, thread.id);
        // forward lookup by thread_id
        let got = im_route_of_thread(&db, thread.id).await.unwrap().unwrap();
        assert_eq!(got.im_thread_ref, "th_1");
        // reverse lookup by (channel, chat_id, im_thread_ref)
        let got = im_route_of_thread_ref(&db, "feishu", "oc_chat", "th_1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(got.thread_id, thread.id);
        // re-bind same issue: row count stays 1, target replaced
        bind_im_route(&db, thread.id, "feishu", "oc_chat", "th_2")
            .await
            .unwrap();
        assert_eq!(list_im_routes(&db).await.unwrap().len(), 1);
        assert!(im_route_of_thread_ref(&db, "feishu", "oc_chat", "th_1")
            .await
            .unwrap()
            .is_none());
        // unbind
        unbind_im_route(&db, thread.id).await.unwrap();
        assert!(im_route_of_thread(&db, thread.id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn im_route_thread_ref_is_unique_across_issues() {
        // Same (channel, chat_id, im_thread_ref) cannot bind to two different issues.
        let db = mem().await;
        let ws = create_workspace(&db, "ws").await.unwrap();
        let first = create_thread(&db, ws.id, "first issue", "feature", "claude")
            .await
            .unwrap();
        let second = create_thread(&db, ws.id, "second issue", "feature", "claude")
            .await
            .unwrap();
        bind_im_route(&db, first.id, "feishu", "oc_chat", "th_1")
            .await
            .unwrap();
        let err = bind_im_route(&db, second.id, "feishu", "oc_chat", "th_1").await;
        assert!(err.is_err(), "second bind should violate unique index");
    }

    #[tokio::test]
    async fn create_and_cascade_delete() {
        let db = mem().await;
        let ws = create_workspace(&db, "Demo WS").await.unwrap();
        assert_eq!(ws.slug, "demo-ws");
        let repo = add_repo_ref(&db, ws.id, "web-app", "/tmp/x", "main", "", true)
            .await
            .unwrap();
        let t = create_thread(&db, ws.id, "Add login", "feature", "claude")
            .await
            .unwrap();
        let dir = create_direction(
            &db,
            t.id,
            "main",
            "claude",
            repo.id,
            "build the feature",
            "plan+impl",
            "",
        )
        .await
        .unwrap();
        assert_eq!(dir.branch, "feature/add-login");
        assert_eq!(dir.repo_id, repo.id);
        assert_eq!(dir.reason, "build the feature");

        // pretend it was materialized
        record_worktree(&db, repo.id, dir.id, &dir.branch, "/tmp/wt", false, true, "")
            .await
            .unwrap();
        assert_eq!(list_worktrees(&db, Some(dir.id)).await.unwrap().len(), 1);
        let wt_id = list_worktrees(&db, Some(dir.id)).await.unwrap()[0].id;
        assert!(direction_repo_of(&db, dir.id).await.unwrap().is_some());

        // cascade delete returns the path to clean and empties the rows
        let removed = delete_thread_cascade(&db, t.id).await.unwrap();
        assert_eq!(
            removed,
            vec![(
                wt_id,
                repo.id,
                "/tmp/wt".to_string(),
                "feature/add-login".to_string(),
                false,
                true
            )]
        );
        assert_eq!(list_workspaces(&db).await.unwrap().len(), 1); // ws survives
        assert_eq!(list_threads(&db, ws.id).await.unwrap().len(), 0);
        assert_eq!(list_worktrees(&db, None).await.unwrap().len(), 0);
    }

    #[tokio::test]
    async fn create_workspace_rejects_empty_name() {
        let db = mem().await;

        assert!(create_workspace(&db, "   ").await.is_err());
        assert!(list_workspaces(&db).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn create_workspace_rejects_duplicate_name() {
        let db = mem().await;
        create_workspace(&db, "Demo WS").await.unwrap();

        let err = create_workspace(&db, "Demo WS").await.unwrap_err();

        assert!(err.to_string().contains("already named"));
        assert_eq!(list_workspaces(&db).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn workspace_name_collision_ignores_hidden_concierge() {
        let db = mem().await;
        let hidden = create_workspace(&db, "Concierge").await.unwrap();
        set_setting(&db, K_CONCIERGE_WORKSPACE, &hidden.id.to_string())
            .await
            .unwrap();

        let visible = create_workspace(&db, "Concierge").await.unwrap();

        assert_ne!(visible.id, hidden.id);
        assert_eq!(list_workspaces(&db).await.unwrap().len(), 2);
        let err = create_workspace(&db, "Concierge").await.unwrap_err();
        assert!(err.to_string().contains("already named"));

        let db = mem().await;
        let hidden = create_workspace(&db, "Concierge").await.unwrap();
        set_setting(&db, K_CONCIERGE_WORKSPACE, &hidden.id.to_string())
            .await
            .unwrap();
        let rename_target = create_workspace(&db, "Demo").await.unwrap();
        let renamed = rename_workspace(&db, rename_target.id, "Concierge")
            .await
            .unwrap();

        assert_eq!(renamed.name, "Concierge");
        assert_eq!(list_workspaces(&db).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn add_repo_ref_rolls_back_when_delete_marker_appears_after_insert() {
        use sea_orm::ConnectionTrait;

        let db = mem().await;
        let ws = create_workspace(&db, "delete me").await.unwrap();
        db.0.execute(sea_orm::Statement::from_string(
            db.0.get_database_backend(),
            format!(
                "CREATE TRIGGER repo_ref_mark_deleting AFTER INSERT ON repo_ref BEGIN \
                 INSERT OR REPLACE INTO app_setting(key, value) \
                 VALUES ('{}', '1'); END",
                workspace_deleting_key(ws.id)
            ),
        ))
        .await
        .unwrap();

        let err = add_repo_ref(&db, ws.id, "late", "/tmp/late", "main", "", true)
            .await
            .unwrap_err();

        assert!(err
            .to_string()
            .contains(&format!("workspace {} is being deleted", ws.id)));
        assert!(list_repos(&db, ws.id).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn upsert_plan_rolls_back_when_delete_marker_appears_after_insert() {
        use sea_orm::ConnectionTrait;

        let db = mem().await;
        let ws = create_workspace(&db, "delete me").await.unwrap();
        let thread = create_thread(&db, ws.id, "issue", "feature", "claude")
            .await
            .unwrap();
        db.0
            .execute(sea_orm::Statement::from_string(
                db.0.get_database_backend(),
                format!(
                    "CREATE TRIGGER plan_mark_deleting AFTER INSERT ON plan BEGIN \
                     INSERT OR REPLACE INTO app_setting(key, value) \
                     VALUES ('{}', '1'); END",
                    workspace_deleting_key(ws.id)
                ),
            ))
            .await
            .unwrap();

        let err = upsert_plan(&db, thread.id, "{}", "proposed", "1")
            .await
            .unwrap_err();

        assert!(err
            .to_string()
            .contains(&format!("workspace {} is being deleted", ws.id)));
        assert!(get_plan(&db, thread.id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn bind_im_route_rolls_back_when_delete_marker_appears_after_insert() {
        use sea_orm::ConnectionTrait;

        let db = mem().await;
        let ws = create_workspace(&db, "delete me").await.unwrap();
        let thread = create_thread(&db, ws.id, "issue", "feature", "claude")
            .await
            .unwrap();
        db.0
            .execute(sea_orm::Statement::from_string(
                db.0.get_database_backend(),
                format!(
                    "CREATE TRIGGER im_route_mark_deleting AFTER INSERT ON im_route BEGIN \
                     INSERT OR REPLACE INTO app_setting(key, value) \
                     VALUES ('{}', '1'); END",
                    workspace_deleting_key(ws.id)
                ),
            ))
            .await
            .unwrap();

        let err = bind_im_route(&db, thread.id, "feishu", "chat", "thread")
            .await
            .unwrap_err();

        assert!(err
            .to_string()
            .contains(&format!("workspace {} is being deleted", ws.id)));
        assert!(im_route_of_thread(&db, thread.id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn delete_workspace_cascade_removes_workspace_owned_state() {
        let db = mem().await;
        let ws = create_workspace(&db, "delete me").await.unwrap();
        let keep_ws = create_workspace(&db, "keep me").await.unwrap();
        let source = add_skill_source(&db, "https://example.com/skills.git", None)
            .await
            .unwrap();
        set_skill_enable(&db, source.id, "ship", &format!("ws:{}", ws.id), true)
            .await
            .unwrap();
        set_skill_enable(&db, source.id, "keep", &format!("ws:{}", keep_ws.id), true)
            .await
            .unwrap();
        set_repo_map_doc(&db, ws.id, "stale map").await.unwrap();
        set_repo_map_doc(&db, keep_ws.id, "keep map").await.unwrap();

        let repo = add_repo_ref(&db, ws.id, "web", "/tmp/delete-web", "main", "", true)
            .await
            .unwrap();
        let keep_repo = add_repo_ref(&db, keep_ws.id, "api", "/tmp/keep-api", "main", "", true)
            .await
            .unwrap();
        let thread = create_thread(&db, ws.id, "remove issue", "feature", "claude")
            .await
            .unwrap();
        let keep_thread = create_thread(&db, keep_ws.id, "keep issue", "feature", "claude")
            .await
            .unwrap();
        let direction = create_direction(
            &db,
            thread.id,
            "web task",
            "claude",
            repo.id,
            "change web",
            "plan+impl",
            "",
        )
        .await
        .unwrap();
        let external_direction = create_direction(
            &db,
            keep_thread.id,
            "external repo task",
            "claude",
            repo.id,
            "change deleted repo",
            "impl-only",
            "",
        )
        .await
        .unwrap();
        let external_session =
            create_session(&db, external_direction.id, repo.id, "claude", "/tmp/external-wt")
                .await
                .unwrap();
        upsert_plan(&db, thread.id, "{}", "proposed", "1")
            .await
            .unwrap();
        insert_lead_message(
            &db,
            thread.id,
            None,
            1,
            "assistant",
            "text",
            r#"{"text":"hi"}"#,
            "complete",
        )
        .await
        .unwrap();
        insert_lead_message(
            &db,
            keep_thread.id,
            Some(external_session.id),
            1,
            "assistant",
            "text",
            r#"{"text":"worker"}"#,
            "complete",
        )
        .await
        .unwrap();
        let keep_lead_message = insert_lead_message(
            &db,
            keep_thread.id,
            None,
            1,
            "assistant",
            "text",
            r#"{"text":"keep lead"}"#,
            "complete",
        )
        .await
        .unwrap();
        bind_im_route(&db, thread.id, "feishu", "chat", "thread")
            .await
            .unwrap();
        record_worktree(
            &db,
            repo.id,
            direction.id,
            "feature/remove",
            "/tmp/delete-wt",
            true,
            true,
            "abc",
        )
        .await
        .unwrap();
        let wt_id = list_worktrees(&db, Some(direction.id)).await.unwrap()[0].id;

        let removed = delete_workspace_cascade(&db, ws.id).await.unwrap();

        assert_eq!(
            removed,
            vec![(
                wt_id,
                repo.id,
                "/tmp/delete-wt".to_string(),
                "feature/remove".to_string(),
                true,
                true,
            )]
        );
        assert_eq!(list_workspaces(&db).await.unwrap(), vec![keep_ws.clone()]);
        assert_eq!(list_repos(&db, keep_ws.id).await.unwrap(), vec![keep_repo]);
        assert_eq!(
            list_threads(&db, keep_ws.id).await.unwrap(),
            vec![keep_thread.clone()]
        );
        assert!(list_repos(&db, ws.id).await.unwrap().is_empty());
        assert!(list_threads(&db, ws.id).await.unwrap().is_empty());
        assert!(list_worktrees(&db, None).await.unwrap().is_empty());
        assert!(get_plan(&db, thread.id).await.unwrap().is_none());
        assert!(list_lead_messages(&db, thread.id).await.unwrap().is_empty());
        let keep_messages = list_lead_messages(&db, keep_thread.id).await.unwrap();
        assert_eq!(keep_messages.len(), 1);
        assert_eq!(keep_messages[0].id, keep_lead_message.id);
        assert_eq!(keep_messages[0].session_id, None);
        assert!(list_im_routes(&db).await.unwrap().is_empty());
        assert!(get_repo_map_doc(&db, ws.id).await.unwrap().is_none());
        assert_eq!(
            get_repo_map_doc(&db, keep_ws.id).await.unwrap().as_deref(),
            Some("keep map"),
        );
        let scopes: Vec<String> = list_skill_enable(&db)
            .await
            .unwrap()
            .into_iter()
            .map(|row| row.scope)
            .collect();
        assert_eq!(scopes, vec![format!("ws:{}", keep_ws.id)]);
    }

    #[tokio::test]
    async fn workspace_owned_writes_reject_deleted_workspace() {
        let db = mem().await;
        let ws = create_workspace(&db, "delete me").await.unwrap();
        let repo = add_repo_ref(&db, ws.id, "web", "/tmp/web", "main", "", true)
            .await
            .unwrap();
        let thread = create_thread(&db, ws.id, "issue", "feature", "claude")
            .await
            .unwrap();
        let direction = create_direction(
            &db,
            thread.id,
            "task",
            "claude",
            repo.id,
            "change",
            "plan+impl",
            "",
        )
        .await
        .unwrap();
        let keep_ws = create_workspace(&db, "keep me").await.unwrap();
        let keep_thread = create_thread(&db, keep_ws.id, "keep issue", "feature", "claude")
            .await
            .unwrap();
        let external_direction = create_direction(
            &db,
            keep_thread.id,
            "external task",
            "claude",
            repo.id,
            "change deleted repo",
            "impl-only",
            "",
        )
        .await
        .unwrap();
        upsert_plan(&db, thread.id, r#"{"directions":[]}"#, "proposed", "1")
            .await
            .unwrap();
        bind_im_route(&db, thread.id, "feishu", "chat", "thread")
            .await
            .unwrap();
        let deleting_key = workspace_deleting_key(ws.id);
        set_setting(&db, &deleting_key, "1").await.unwrap();

        let deleting_add_err = add_repo_ref(&db, ws.id, "late", "/tmp/late", "main", "", true)
            .await
            .unwrap_err();
        assert!(deleting_add_err
            .to_string()
            .contains(&format!("workspace {} is being deleted", ws.id)));

        let deleting_thread_err = create_thread(&db, ws.id, "late issue", "feature", "claude")
            .await
            .unwrap_err();
        assert!(deleting_thread_err
            .to_string()
            .contains(&format!("workspace {} is being deleted", ws.id)));

        let deleting_direction_err = create_direction(
            &db,
            thread.id,
            "late task",
            "claude",
            repo.id,
            "change",
            "plan+impl",
            "",
        )
        .await
        .unwrap_err();
        assert!(deleting_direction_err
            .to_string()
            .contains(&format!("workspace {} is being deleted", ws.id)));

        let deleting_repo_direction_err = create_direction(
            &db,
            keep_thread.id,
            "late external task",
            "claude",
            repo.id,
            "change deleted repo",
            "impl-only",
            "",
        )
        .await
        .unwrap_err();
        assert!(deleting_repo_direction_err
            .to_string()
            .contains(&format!("workspace {} is being deleted", ws.id)));

        let deleting_worktree_err = record_worktree(
            &db,
            repo.id,
            direction.id,
            "feature/task",
            "/tmp/wt",
            true,
            true,
            "",
        )
        .await
        .unwrap_err();
        assert!(deleting_worktree_err
            .to_string()
            .contains(&format!("workspace {} is being deleted", ws.id)));

        let deleting_repo_worktree_err = record_worktree(
            &db,
            repo.id,
            external_direction.id,
            "feature/external",
            "/tmp/external-wt",
            true,
            true,
            "",
        )
        .await
        .unwrap_err();
        assert!(deleting_repo_worktree_err
            .to_string()
            .contains(&format!("workspace {} is being deleted", ws.id)));

        let deleting_session_err = create_session(&db, direction.id, repo.id, "claude", "/tmp/wt")
            .await
            .unwrap_err();
        assert!(deleting_session_err
            .to_string()
            .contains(&format!("workspace {} is being deleted", ws.id)));

        let deleting_repo_session_err =
            create_session(&db, external_direction.id, repo.id, "claude", "/tmp/external-wt")
                .await
                .unwrap_err();
        assert!(deleting_repo_session_err
            .to_string()
            .contains(&format!("workspace {} is being deleted", ws.id)));

        let deleting_plan_err = upsert_plan(&db, thread.id, "{}", "withdrawn", "2")
            .await
            .unwrap_err();
        assert!(deleting_plan_err
            .to_string()
            .contains(&format!("workspace {} is being deleted", ws.id)));

        let deleting_plan_created_at_err = set_plan_created_at(&db, thread.id, "2")
            .await
            .unwrap_err();
        assert!(deleting_plan_created_at_err
            .to_string()
            .contains(&format!("workspace {} is being deleted", ws.id)));

        let deleting_plan_cas_err =
            update_plan_proposal_cas(&db, thread.id, "{}", r#"{"directions":[]}"#, "proposed")
                .await
                .unwrap_err();
        assert!(deleting_plan_cas_err
            .to_string()
            .contains(&format!("workspace {} is being deleted", ws.id)));

        let deleting_mark_confirmed_err =
            mark_plan_confirmed_cas(&db, thread.id, r#"{"directions":[]}"#, "proposed")
                .await
                .unwrap_err();
        assert!(deleting_mark_confirmed_err
            .to_string()
            .contains(&format!("workspace {} is being deleted", ws.id)));

        let deleting_plan_confirm_err =
            commit_confirmed_plan_cas(&db, thread.id, "{}", r#"{"directions":[]}"#, "proposed")
                .await
                .unwrap_err();
        assert!(deleting_plan_confirm_err
            .to_string()
            .contains(&format!("workspace {} is being deleted", ws.id)));

        let deleting_route_err = bind_im_route(&db, thread.id, "feishu", "chat", "thread-2")
            .await
            .unwrap_err();
        assert!(deleting_route_err
            .to_string()
            .contains(&format!("workspace {} is being deleted", ws.id)));
        delete_setting(&db, &deleting_key).await.unwrap();

        delete_workspace_cascade(&db, ws.id).await.unwrap();
        assert!(get_setting(&db, &deleting_key).await.unwrap().is_none());

        let add_err = add_repo_ref(&db, ws.id, "late", "/tmp/late", "main", "", true)
            .await
            .unwrap_err();
        assert!(add_err
            .to_string()
            .contains(&format!("workspace {} not found", ws.id)));

        let thread_err = create_thread(&db, ws.id, "late issue", "feature", "claude")
            .await
            .unwrap_err();
        assert!(thread_err
            .to_string()
            .contains(&format!("workspace {} not found", ws.id)));
        assert!(list_repos(&db, ws.id).await.unwrap().is_empty());
        assert!(list_threads(&db, ws.id).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn latest_session_for_returns_newest_with_native() {
        let db = mem().await;
        let ws = create_workspace(&db, "Demo WS").await.unwrap();
        let repo = add_repo_ref(&db, ws.id, "web-app", "/tmp/x", "main", "", true)
            .await
            .unwrap();
        let thread = create_thread(&db, ws.id, "T", "feature", "claude")
            .await
            .unwrap();
        let dir = create_direction(&db, thread.id, "D", "claude", repo.id, "r", "impl-only", "")
            .await
            .unwrap();
        // older session (no native), then newer (native captured)
        let _s1 = create_session(&db, dir.id, repo.id, "claude", "/tmp/x")
            .await
            .unwrap();
        let s2 = create_session(&db, dir.id, repo.id, "claude", "/tmp/x")
            .await
            .unwrap();
        set_session_native_id(&db, s2.id, "abc-123").await.unwrap();

        let latest = latest_session_for(&db, dir.id, repo.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(latest.id, s2.id);
        assert_eq!(latest.native_session_id.as_deref(), Some("abc-123"));
        assert!(latest_session_for(&db, dir.id, 99999)
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn direction_repo_of_none_when_unset() {
        let db = mem().await;
        let ws = create_workspace(&db, "Demo WS").await.unwrap();
        let t = create_thread(&db, ws.id, "Add login", "feature", "claude")
            .await
            .unwrap();
        // A direction with repo_id == 0 (unset) has no bound write repo.
        let dir = direction::ActiveModel {
            thread_id: Set(t.id),
            name: Set("main".to_string()),
            slug: Set("main".to_string()),
            tool: Set("claude".to_string()),
            branch: Set("feature/add-login".to_string()),
            status: Set("queued".to_string()),
            repo_id: Set(0),
            reason: Set(String::new()),
            created_at: Set(now()),
            ..Default::default()
        }
        .insert(&db.0)
        .await
        .unwrap();
        assert_eq!(dir.repo_id, 0);
        assert!(direction_repo_of(&db, dir.id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn create_thread_stamps_lead_tool() {
        let db = mem().await;
        let ws = create_workspace(&db, "w").await.unwrap();
        let t = create_thread(&db, ws.id, "Add feature", "feature", "codex")
            .await
            .unwrap();
        assert_eq!(t.lead_tool, "codex");
        assert!(!t.engine_pinned, "the configured default is not a manual pin");
    }

    #[tokio::test]
    async fn create_thread_rejects_missing_title_or_kind() {
        let db = mem().await;
        let ws = create_workspace(&db, "w").await.unwrap();

        assert!(create_thread(&db, ws.id, "   ", "feature", "codex")
            .await
            .is_err());
        assert!(create_thread(&db, ws.id, "Add feature", "   ", "codex")
            .await
            .is_err());
        assert!(list_threads(&db, ws.id).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn app_setting_roundtrip() {
        let db = mem().await;
        assert_eq!(get_setting(&db, "default_tool").await.unwrap(), None);
        set_setting(&db, "default_tool", "codex").await.unwrap();
        assert_eq!(
            get_setting(&db, "default_tool").await.unwrap(),
            Some("codex".to_string())
        );
        // Overwrite, not duplicate.
        set_setting(&db, "default_tool", "claude").await.unwrap();
        assert_eq!(
            get_setting(&db, "default_tool").await.unwrap(),
            Some("claude".to_string())
        );
    }

    #[tokio::test]
    async fn tool_commands_roundtrip_and_clear() {
        let db = mem().await;
        assert!(get_tool_commands(&db).await.unwrap().is_empty());

        // Setting an alias persists it (apply_to_existing irrelevant with no rows).
        set_tool_command(&db, "claude", "cc-claude", true)
            .await
            .unwrap();
        assert_eq!(
            get_tool_commands(&db).await.unwrap().get("claude").map(String::as_str),
            Some("cc-claude")
        );

        // Clearing (blank) removes the entry; identity value also clears.
        set_tool_command(&db, "claude", "  ", true).await.unwrap();
        assert!(get_tool_commands(&db).await.unwrap().is_empty());
        set_tool_command(&db, "claude", "cc-claude", true)
            .await
            .unwrap();
        set_tool_command(&db, "claude", "claude", true).await.unwrap();
        assert!(get_tool_commands(&db).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn apply_to_existing_false_pins_old_sessions_only() {
        let db = mem().await;
        let ws = create_workspace(&db, "w").await.unwrap();
        let repo = add_repo_ref(&db, ws.id, "r", "/tmp/x", "main", "", true).await.unwrap();
        // An existing claude lead + worker, created before any alias.
        let old_thread = create_thread(&db, ws.id, "old", "feature", "claude")
            .await
            .unwrap();
        let dir = create_direction(&db, old_thread.id, "d", "claude", repo.id, "why", "impl-only", "")
            .await
            .unwrap();
        let old_sess = create_session(&db, dir.id, repo.id, "claude", "/tmp/wt")
            .await
            .unwrap();
        assert_eq!(old_thread.lead_command, None);
        assert_eq!(old_sess.command, None);

        // Configure the alias but EXCLUDE existing sessions.
        set_tool_command(&db, "claude", "cc-claude", false)
            .await
            .unwrap();

        // Old lead + worker are pinned to their prior command ("claude").
        let pinned_thread = get_thread(&db, old_thread.id).await.unwrap().unwrap();
        assert_eq!(pinned_thread.lead_command.as_deref(), Some("claude"));
        let pinned_sess = get_session(&db, old_sess.id).await.unwrap().unwrap();
        assert_eq!(pinned_sess.command.as_deref(), Some("claude"));

        // A NEW thread/worker created after the change is NOT pinned (NULL), so it
        // follows the global override and spawns cc-claude.
        let new_thread = create_thread(&db, ws.id, "new", "feature", "claude")
            .await
            .unwrap();
        assert_eq!(new_thread.lead_command, None);

        // A different tool's rows are untouched by a claude alias.
        let codex_thread = create_thread(&db, ws.id, "cx", "feature", "codex")
            .await
            .unwrap();
        set_tool_command(&db, "claude", "cc-claude-2", false)
            .await
            .unwrap();
        let codex_after = get_thread(&db, codex_thread.id).await.unwrap().unwrap();
        assert_eq!(codex_after.lead_command, None);
    }

    #[tokio::test]
    async fn apply_to_existing_true_clears_pins_so_rows_follow_global() {
        let db = mem().await;
        let ws = create_workspace(&db, "w").await.unwrap();
        let old_thread = create_thread(&db, ws.id, "old", "feature", "claude")
            .await
            .unwrap();
        // First an opt-out pins the existing lead.
        set_tool_command(&db, "claude", "cc-claude", false)
            .await
            .unwrap();
        assert_eq!(
            get_thread(&db, old_thread.id).await.unwrap().unwrap().lead_command.as_deref(),
            Some("claude")
        );
        // A later apply-to-existing clears the pin so the row follows the global map.
        set_tool_command(&db, "claude", "cc-claude", true)
            .await
            .unwrap();
        assert_eq!(
            get_thread(&db, old_thread.id).await.unwrap().unwrap().lead_command,
            None
        );
    }

    #[tokio::test]
    async fn rename_updates_display_name_only() {
        let db = mem().await;
        let ws = create_workspace(&db, "Demo WS").await.unwrap();
        let repo = add_repo_ref(&db, ws.id, "web-app", "/tmp/x", "main", "", true)
            .await
            .unwrap();
        let t = create_thread(&db, ws.id, "Add login", "feature", "claude")
            .await
            .unwrap();
        let d = create_direction(&db, t.id, "main", "claude", repo.id, "r", "plan+impl", "")
            .await
            .unwrap();

        // trim + 只更新显示字段；slug / branch 都保持创建时的值
        let ws2 = rename_workspace(&db, ws.id, "  New WS  ").await.unwrap();
        assert_eq!(ws2.name, "New WS");
        assert_eq!(ws2.slug, "demo-ws");

        let t2 = rename_thread(&db, t.id, "Add SSO login").await.unwrap();
        assert_eq!(t2.title, "Add SSO login");
        assert_eq!(t2.slug, "add-login");

        let d2 = rename_direction(&db, d.id, "api work").await.unwrap();
        assert_eq!(d2.name, "api work");
        assert_eq!(d2.slug, "main");
        assert_eq!(d2.branch, "feature/add-login");
    }

    #[tokio::test]
    async fn switch_thread_tool_updates_identity_and_clears_stale_pin() {
        let db = mem().await;
        let ws = create_workspace(&db, "ws").await.unwrap();
        let t = create_thread(&db, ws.id, "Issue", "feature", "claude").await.unwrap();
        // A command alias pin for the OLD tool must not survive onto the new one.
        // `applyToExisting=false` pins the existing thread to its prior resolved
        // command ("claude") — see `apply_to_existing_true_clears_pins_so_rows_
        // follow_global` above for why `true` would instead CLEAR the pin.
        set_tool_command(&db, "claude", "cc-claude", false).await.unwrap();
        assert_eq!(get_thread(&db, t.id).await.unwrap().unwrap().lead_command.as_deref(), Some("claude"));

        switch_lead_engine_txn(&db, t.id, "codex", Some("gpt-5.5-high"))
            .await
            .unwrap();
        // Read back rather than trusting a returned model: what matters is
        // what COMMITTED, and the transaction no longer hands one out.
        let switched = get_thread(&db, t.id).await.unwrap().unwrap();
        assert_eq!(switched.lead_tool, "codex");
        assert_eq!(switched.lead_model.as_deref(), Some("gpt-5.5-high"));
        assert_eq!(switched.lead_command, None, "stale claude alias pin must be cleared");

        // A model override clears the same way when the caller passes None.
        switch_lead_engine_txn(&db, t.id, "codex", None).await.unwrap();
        assert_eq!(get_thread(&db, t.id).await.unwrap().unwrap().lead_model, None);

        assert!(switch_lead_engine_txn(&db, 9999, "codex", None).await.is_err());
    }

    #[tokio::test]
    async fn switch_direction_and_session_tool_stay_in_lockstep() {
        let db = mem().await;
        let ws = create_workspace(&db, "ws").await.unwrap();
        let repo = add_repo_ref(&db, ws.id, "web-app", "/tmp/x", "main", "", true)
            .await
            .unwrap();
        let t = create_thread(&db, ws.id, "Issue", "feature", "claude").await.unwrap();
        let d = create_direction(&db, t.id, "main", "claude", repo.id, "r", "plan+impl", "")
            .await
            .unwrap();
        let s = create_session(&db, d.id, repo.id, "claude", "/tmp/cwd").await.unwrap();
        set_session_native_id(&db, s.id, "native-1").await.unwrap();
        // A stale alias pin for the OLD tool must not survive the switch either.
        {
            let mut a: session::ActiveModel = get_session(&db, s.id).await.unwrap().unwrap().into();
            a.command = Set(Some("cc-claude".to_string()));
            a.update(&db.0).await.unwrap();
        }

        switch_worker_engine_txn(&db, d.id, s.id, "opencode", Some("kimi-for-coding/k2p6"))
            .await
            .unwrap();

        let d2 = get_direction(&db, d.id).await.unwrap().unwrap();
        assert_eq!(d2.tool, "opencode", "direction.tool must follow the switch — chat_open_worker_impl's cold-recreate path reads it, not session.tool");
        let s2 = get_session(&db, s.id).await.unwrap().unwrap();
        assert_eq!(s2.tool, "opencode");
        assert_eq!(s2.model.as_deref(), Some("kimi-for-coding/k2p6"));
        assert_eq!(s2.command, None, "stale claude alias pin must be cleared");
        // The native-id clear rides this same transaction as of PR #140 round
        // 6. It used to be a separate write the caller made afterwards, which
        // could fail on its own and leave the new tool paired with the OLD
        // engine's native id — a pair `worker_engine` would then try to resume
        // across engines. Atomic here, that pair cannot exist.
        assert_eq!(s2.native_session_id, None, "the switch clears it in the same write");

        assert!(switch_worker_engine_txn(&db, 9999, s.id, "codex", None).await.is_err());
        // A missing session is tolerated (not an error) on the session half —
        // see the function doc — as long as the direction is real.
        assert!(switch_worker_engine_txn(&db, d.id, 9999, "codex", None).await.is_ok());
    }

    #[tokio::test]
    async fn refresh_unpinned_direction_route_updates_a_no_native_initial_session_too() {
        let db = mem().await;
        let ws = create_workspace(&db, "ws").await.unwrap();
        let repo = add_repo_ref(&db, ws.id, "web-app", "/tmp/x", "main", "", true)
            .await
            .unwrap();
        let thread = create_thread(&db, ws.id, "Issue", "feature", "codex")
            .await
            .unwrap();
        let direction = create_direction(
            &db,
            thread.id,
            "main",
            "codex",
            repo.id,
            "r",
            "plan+impl",
            "",
        )
        .await
        .unwrap();
        let session = create_session(&db, direction.id, repo.id, "codex", "/tmp/cwd")
            .await
            .unwrap();
        {
            let mut active: session::ActiveModel = get_session(&db, session.id)
                .await
                .unwrap()
                .unwrap()
                .into();
            active.command = Set(Some("cc-codex".to_string()));
            active.model = Set(Some("gpt-5.5-high".to_string()));
            active.update(&db.0).await.unwrap();
        }

        refresh_unpinned_direction_route(&db, direction.id, Some(session.id), "claude")
            .await
            .unwrap();

        let refreshed_direction = get_direction(&db, direction.id).await.unwrap().unwrap();
        let refreshed_session = get_session(&db, session.id).await.unwrap().unwrap();
        assert_eq!(refreshed_direction.tool, "claude");
        assert!(!refreshed_direction.engine_pinned);
        assert_eq!(refreshed_session.tool, "claude");
        assert!(!refreshed_session.engine_pinned);
        assert_eq!(refreshed_session.command, None);
        assert_eq!(refreshed_session.model, None);
        assert_eq!(refreshed_session.native_session_id, None);
    }

    #[tokio::test]
    async fn refresh_unpinned_direction_route_preserves_a_newly_established_session() {
        let db = mem().await;
        let ws = create_workspace(&db, "ws").await.unwrap();
        let repo = add_repo_ref(&db, ws.id, "web-app", "/tmp/x", "main", "", true)
            .await
            .unwrap();
        let thread = create_thread(&db, ws.id, "Issue", "feature", "codex")
            .await
            .unwrap();
        let direction = create_direction(
            &db,
            thread.id,
            "main",
            "codex",
            repo.id,
            "r",
            "plan+impl",
            "",
        )
        .await
        .unwrap();
        let session = create_session(&db, direction.id, repo.id, "codex", "/tmp/cwd")
            .await
            .unwrap();
        set_session_native_id(&db, session.id, "native-1")
            .await
            .unwrap();

        let err = refresh_unpinned_direction_route(&db, direction.id, Some(session.id), "claude")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("became established"));

        let unchanged_direction = get_direction(&db, direction.id).await.unwrap().unwrap();
        let unchanged_session = get_session(&db, session.id).await.unwrap().unwrap();
        assert_eq!(unchanged_direction.tool, "codex");
        assert!(!unchanged_direction.engine_pinned);
        assert_eq!(unchanged_session.tool, "codex");
        assert!(!unchanged_session.engine_pinned);
        assert_eq!(unchanged_session.native_session_id.as_deref(), Some("native-1"));
    }

    #[tokio::test]
    async fn refresh_unpinned_thread_route_never_overwrites_a_manual_pin() {
        let db = mem().await;
        let ws = create_workspace(&db, "ws").await.unwrap();
        let thread = create_thread(&db, ws.id, "Issue", "feature", "codex")
            .await
            .unwrap();

        assert!(refresh_unpinned_thread_route(&db, thread.id, "claude")
            .await
            .unwrap());
        assert_eq!(get_thread(&db, thread.id).await.unwrap().unwrap().lead_tool, "claude");

        set_thread_engine_pinned(&db, thread.id, true).await.unwrap();
        assert!(!refresh_unpinned_thread_route(&db, thread.id, "codex")
            .await
            .unwrap());
        let current = get_thread(&db, thread.id).await.unwrap().unwrap();
        assert_eq!(current.lead_tool, "claude");
        assert!(current.engine_pinned);
    }

    #[tokio::test]
    async fn manual_direction_pin_refuses_a_direction_that_already_opened() {
        let db = mem().await;
        let (_, repo, _, direction) = worker_fixture(&db).await;
        create_session(&db, direction.id, repo.id, "codex", "/tmp/cwd")
            .await
            .unwrap();

        let err = pin_unstarted_unpinned_direction_route(&db, direction.id, "opencode")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("opened while pinning"));
        let after = get_direction(&db, direction.id).await.unwrap().unwrap();
        assert_eq!(after.tool, "codex");
        assert!(!after.engine_pinned);
    }

    #[tokio::test]
    async fn current_route_session_creation_observes_a_manual_pin() {
        let db = mem().await;
        let (_, repo, _, direction) = worker_fixture(&db).await;
        pin_unstarted_unpinned_direction_route(&db, direction.id, "opencode")
            .await
            .unwrap();

        let session = create_session_for_current_direction(&db, direction.id, repo.id, "/tmp/cwd")
            .await
            .unwrap();
        assert_eq!(session.tool, "opencode");
        assert!(session.engine_pinned);
    }

    #[tokio::test]
    async fn confirmed_plan_pin_rolls_back_when_a_worker_session_exists() {
        let db = mem().await;
        let (_, repo, thread, direction) = worker_fixture(&db).await;
        upsert_plan(&db, thread.id, "before", "proposed", "1")
            .await
            .unwrap();
        create_session(&db, direction.id, repo.id, "codex", "/tmp/cwd")
            .await
            .unwrap();

        let err = commit_confirmed_plan_with_direction_pins_cas(
            &db,
            thread.id,
            "after",
            "before",
            "proposed",
            &[InitialDirectionRoutePin {
                direction_id: direction.id,
                session_id: None,
                tool: "opencode".to_string(),
            }],
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("opened while confirming"));
        let plan = get_plan(&db, thread.id).await.unwrap().unwrap();
        assert_eq!(plan.proposal, "before");
        assert_eq!(plan.status, "proposed");
        let after = get_direction(&db, direction.id).await.unwrap().unwrap();
        assert_eq!(after.tool, "codex");
        assert!(!after.engine_pinned);
    }

    #[tokio::test]
    async fn confirmed_plan_pin_updates_an_unestablished_session_atomically() {
        let db = mem().await;
        let (_, repo, thread, direction) = worker_fixture(&db).await;
        upsert_plan(&db, thread.id, "before", "proposed", "1")
            .await
            .unwrap();
        let session = create_session(&db, direction.id, repo.id, "codex", "/tmp/cwd")
            .await
            .unwrap();

        let applied = commit_confirmed_plan_with_direction_pins_cas(
            &db,
            thread.id,
            "after",
            "before",
            "proposed",
            &[InitialDirectionRoutePin {
                direction_id: direction.id,
                session_id: Some(session.id),
                tool: "opencode".to_string(),
            }],
        )
        .await
        .unwrap();

        assert!(applied);
        let plan = get_plan(&db, thread.id).await.unwrap().unwrap();
        assert_eq!(plan.proposal, "after");
        assert_eq!(plan.status, "confirmed");
        let direction = get_direction(&db, direction.id).await.unwrap().unwrap();
        assert_eq!(direction.tool, "opencode");
        assert!(direction.engine_pinned);
        let session = get_session(&db, session.id).await.unwrap().unwrap();
        assert_eq!(session.tool, "opencode");
        assert!(session.engine_pinned);
        assert!(session.native_session_id.is_none());
    }

    #[tokio::test]
    async fn reused_approval_commits_manual_pin_with_the_plan_decision() {
        let db = mem().await;
        let (_, _repo, thread, direction) = worker_fixture(&db).await;
        upsert_plan(&db, thread.id, "before", "proposed", "1")
            .await
            .unwrap();

        let applied = commit_reused_approval_with_direction_pin_cas(
            &db,
            thread.id,
            "after",
            "before",
            "proposed",
            &InitialDirectionRoutePin {
                direction_id: direction.id,
                session_id: None,
                tool: "opencode".to_string(),
            },
        )
        .await
        .unwrap();

        assert!(applied);
        let plan = get_plan(&db, thread.id).await.unwrap().unwrap();
        assert_eq!(plan.proposal, "after");
        let after = get_direction(&db, direction.id).await.unwrap().unwrap();
        assert_eq!(after.tool, "opencode");
        assert!(after.engine_pinned);
    }

    #[tokio::test]
    async fn reused_approval_updates_an_unestablished_session_atomically() {
        let db = mem().await;
        let (_, repo, thread, direction) = worker_fixture(&db).await;
        upsert_plan(&db, thread.id, "before", "proposed", "1")
            .await
            .unwrap();
        let session = create_session(&db, direction.id, repo.id, "codex", "/tmp/cwd")
            .await
            .unwrap();

        let applied = commit_reused_approval_with_direction_pin_cas(
            &db,
            thread.id,
            "after",
            "before",
            "proposed",
            &InitialDirectionRoutePin {
                direction_id: direction.id,
                session_id: Some(session.id),
                tool: "opencode".to_string(),
            },
        )
        .await
        .unwrap();

        assert!(applied);
        assert_eq!(get_plan(&db, thread.id).await.unwrap().unwrap().proposal, "after");
        let direction = get_direction(&db, direction.id).await.unwrap().unwrap();
        assert_eq!(direction.tool, "opencode");
        assert!(direction.engine_pinned);
        let session = get_session(&db, session.id).await.unwrap().unwrap();
        assert_eq!(session.tool, "opencode");
        assert!(session.engine_pinned);
        assert!(session.native_session_id.is_none());
    }

    #[tokio::test]
    async fn reused_approval_rolls_back_when_a_worker_session_exists() {
        let db = mem().await;
        let (_, repo, thread, direction) = worker_fixture(&db).await;
        upsert_plan(&db, thread.id, "before", "proposed", "1")
            .await
            .unwrap();
        create_session(&db, direction.id, repo.id, "codex", "/tmp/cwd")
            .await
            .unwrap();

        let err = commit_reused_approval_with_direction_pin_cas(
            &db,
            thread.id,
            "after",
            "before",
            "proposed",
            &InitialDirectionRoutePin {
                direction_id: direction.id,
                session_id: None,
                tool: "opencode".to_string(),
            },
        )
        .await
        .unwrap_err();

        assert!(err.to_string().contains("opened while approving"));
        let plan = get_plan(&db, thread.id).await.unwrap().unwrap();
        assert_eq!(plan.proposal, "before");
        let after = get_direction(&db, direction.id).await.unwrap().unwrap();
        assert_eq!(after.tool, "codex");
        assert!(!after.engine_pinned);
    }

    #[tokio::test]
    async fn engine_pin_tracks_manual_choices_but_not_automatic_failover() {
        let db = mem().await;
        let ws = create_workspace(&db, "ws").await.unwrap();
        let repo = add_repo_ref(&db, ws.id, "web-app", "/tmp/x", "main", "", true)
            .await
            .unwrap();
        let thread = create_thread(&db, ws.id, "Issue", "feature", "claude")
            .await
            .unwrap();
        let direction = create_direction(
            &db,
            thread.id,
            "main",
            "claude",
            repo.id,
            "r",
            "plan+impl",
            "",
        )
        .await
        .unwrap();
        let session = create_session(&db, direction.id, repo.id, "claude", "/tmp/cwd")
            .await
            .unwrap();
        assert!(!thread.engine_pinned);
        assert!(!direction.engine_pinned);
        assert!(!session.engine_pinned);

        switch_lead_engine_txn_with_pin(&db, thread.id, "codex", None, false)
            .await
            .unwrap();
        switch_worker_engine_txn_with_pin(&db, direction.id, session.id, "codex", None, false)
            .await
            .unwrap();
        assert!(!get_thread(&db, thread.id).await.unwrap().unwrap().engine_pinned);
        assert!(!get_direction(&db, direction.id).await.unwrap().unwrap().engine_pinned);
        assert!(!get_session(&db, session.id).await.unwrap().unwrap().engine_pinned);

        switch_lead_engine_txn(&db, thread.id, "claude", None)
            .await
            .unwrap();
        switch_worker_engine_txn(&db, direction.id, session.id, "claude", None)
            .await
            .unwrap();
        assert!(get_thread(&db, thread.id).await.unwrap().unwrap().engine_pinned);
        assert!(get_direction(&db, direction.id).await.unwrap().unwrap().engine_pinned);
        assert!(get_session(&db, session.id).await.unwrap().unwrap().engine_pinned);
    }

    /// Two `Db` handles onto one WAL file, so a concurrent commit is a real
    /// event rather than a simulated one. Migrations run once; the second
    /// handle just opens the same file.
    async fn shared_file_db(dir: &std::path::Path) -> (Db, Db) {
        use sea_orm::ConnectionTrait;
        let url = format!("sqlite://{}?mode=rwc", dir.join("weft.db").to_string_lossy());
        let open = |url: String| async move {
            let conn = sea_orm::Database::connect(url).await.unwrap();
            conn.execute_unprepared("PRAGMA journal_mode=WAL;").await.unwrap();
            conn.execute_unprepared("PRAGMA busy_timeout=2000;").await.unwrap();
            conn
        };
        let a = open(url.clone()).await;
        use sea_orm_migration::MigratorTrait;
        crate::store::migration::Migrator::up(&a, None).await.unwrap();
        let b = open(url).await;
        (Db(a, false), Db(b, false))
    }

    /// PR #140 rounds 11/15: the switch transactions take the write lock with
    /// their FIRST statement, because a deferred read→write upgrade is not
    /// safe under WAL — `insert_lead_message` documents the same hazard and is
    /// why it is not a transaction at all. The busy timeout cannot repair a
    /// stale snapshot, and by the time this runs the command has already torn
    /// the live engine down, so a spurious abort is not a harmless retry.
    ///
    /// DETERMINISTIC, with no sleeps: the transaction rendezvouses with the
    /// test immediately after its first statement (`probe_after_first_statement!`),
    /// and while it is parked there a THIRD connection asks the only question
    /// that separates the two shapes — is the write lock already held?
    ///
    ///   - write-first: the opening `UPDATE` took it, so the probe is refused.
    ///   - read-first:  the opening `SELECT` took nothing, so the probe wins,
    ///                  and its commit then poisons the transaction's snapshot.
    ///
    /// Two earlier versions of this test were timing-based and could silently
    /// stop discriminating on a loaded runner; this one has no timing in it.
    #[tokio::test]
    async fn the_switch_transaction_holds_the_write_lock_from_its_first_statement() {
        use sea_orm::ConnectionTrait;
        let dir = tempfile::tempdir().expect("tempdir");
        let (a, probe) = shared_file_db(dir.path()).await;
        let ws = create_workspace(&a, "ws").await.unwrap();
        let t = create_thread(&a, ws.id, "Issue", "feature", "claude").await.unwrap();
        // The probe must fail fast rather than wait for the lock, or it would
        // block until the parked transaction commits.
        probe.0.execute_unprepared("PRAGMA busy_timeout=0;").await.unwrap();

        let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(2));
        // A third handle for the transaction itself, so `a` stays available
        // here for the read-back at the end.
        let (writer, _spare) = shared_file_db(dir.path()).await;
        let switching = {
            let barrier = std::sync::Arc::clone(&barrier);
            let tid = t.id;
            tokio::spawn(async move {
                txn_probe::AFTER_FIRST_STATEMENT
                    .scope(barrier, switch_lead_engine_txn(&writer, tid, "codex", Some("opus")))
                    .await
            })
        };

        // Parked after its first statement — no clock involved, the barrier is
        // what orders this.
        barrier.wait().await;
        let contended = probe
            .0
            .execute_unprepared("UPDATE thread SET title = 'probe' WHERE id = 1;")
            .await;
        assert!(
            contended.is_err(),
            "the transaction must already hold the write lock after its FIRST statement; a \
             read-first shape would leave it free here and then abort on the stale snapshot"
        );

        barrier.wait().await;
        switching.await.expect("join").expect("the switch itself must succeed");
        let after = get_thread(&a, t.id).await.unwrap().unwrap();
        assert_eq!(after.lead_tool, "codex");
        assert_eq!(after.lead_model.as_deref(), Some("opus"));
        assert!(last_turn_freeze_recovery_secs(&a, t.id, None).await.unwrap().is_some());
    }

    /// PR #140 round 6: the lead's tool/model write and its native-id clear are
    /// ONE transaction, so they cannot half-apply.
    ///
    /// Same technique as the worker test below — force the SECOND half to fail
    /// with a genuine DB error while the first is perfectly healthy, then
    /// assert the first rolled back. The lead's native id lives in a
    /// `kind = "meta"` lead_message row rather than a column, so renaming that
    /// table away is what breaks the clear specifically.
    ///
    /// Without the transaction this leaves `lead_tool = "codex"` next to the
    /// old engine's native id — the pair the next send would try to resume
    /// across engines, and one no grace window repairs.
    #[tokio::test]
    async fn switch_lead_engine_txn_rolls_back_the_tool_when_the_native_clear_fails() {
        use sea_orm::ConnectionTrait;
        let db = mem().await;
        let ws = create_workspace(&db, "ws").await.unwrap();
        let t = create_thread(&db, ws.id, "Issue", "feature", "claude").await.unwrap();
        set_lead_native_id(&db, t.id, "lead-native-1").await.unwrap();

        // Claimed BEFORE the table goes away — this is the switch's own step 0,
        // and the transaction below is what would have promoted it.
        db.0.execute_unprepared("ALTER TABLE lead_message RENAME TO lead_message_renamed_for_test")
            .await
            .unwrap();
        let err = switch_lead_engine_txn(&db, t.id, "codex", Some("gpt-5.5-high")).await;
        assert!(err.is_err(), "the native-id half must fail (table renamed away)");
        db.0.execute_unprepared("ALTER TABLE lead_message_renamed_for_test RENAME TO lead_message")
            .await
            .unwrap();

        let t2 = get_thread(&db, t.id).await.unwrap().unwrap();
        assert_eq!(
            t2.lead_tool, "claude",
            "lead_tool must be ROLLED BACK — a committed tool next to the old native id is \
             exactly the half-applied pair this transaction exists to prevent"
        );
        assert_eq!(t2.lead_model, None);
        assert_eq!(
            lead_native_id(&db, t.id).await.unwrap().as_deref(),
            Some("lead-native-1"),
            "and the id is untouched, so the surface is still coherently on the OLD engine"
        );
    }

    /// Adversarial re-review of PR #139, P1: the direction/session writes used
    /// to be two independent `.update()` calls — a failure on the SECOND write
    /// left the FIRST one committed, so `direction.tool` and `session.tool`
    /// could disagree (exactly the "which engine is this worker really
    /// talking to" confusion issue #96 exists to fix, recurring in a
    /// harder-to-notice shape). Proves the fix is a REAL transaction, not just
    /// a "no error happened" happy-path check: forces the session half to fail
    /// with a genuine DB error (renaming the `session` table out from under a
    /// live transaction — deterministic, no timing/locking dependency, unlike
    /// a busy-snapshot race) while the direction half is perfectly healthy,
    /// then asserts the direction was rolled back too.
    #[tokio::test]
    async fn switch_worker_engine_txn_rolls_back_direction_when_session_write_fails() {
        use sea_orm::ConnectionTrait;
        let db = mem().await;
        let ws = create_workspace(&db, "ws").await.unwrap();
        let repo = add_repo_ref(&db, ws.id, "web-app", "/tmp/x", "main", "", true)
            .await
            .unwrap();
        let t = create_thread(&db, ws.id, "Issue", "feature", "claude").await.unwrap();
        let d = create_direction(&db, t.id, "main", "claude", repo.id, "r", "plan+impl", "")
            .await
            .unwrap();
        let s = create_session(&db, d.id, repo.id, "claude", "/tmp/cwd").await.unwrap();

        // Make the session half of the transaction fail with a real DB error
        // (not a simulated one) while leaving `direction` completely healthy.
        db.0.execute_unprepared("ALTER TABLE session RENAME TO session_renamed_for_test")
            .await
            .unwrap();

        let err =
            switch_worker_engine_txn(&db, d.id, s.id, "opencode", Some("gpt-5.5-high")).await;
        assert!(err.is_err(), "the session-table write must fail (table renamed away)");

        // Restore the table so the read-back below (and any other test using
        // this connection) sees the schema it expects.
        db.0.execute_unprepared("ALTER TABLE session_renamed_for_test RENAME TO session")
            .await
            .unwrap();

        let d2 = get_direction(&db, d.id).await.unwrap().unwrap();
        assert_eq!(
            d2.tool, "claude",
            "direction.tool must be ROLLED BACK, not left at the new value, when the session half fails"
        );
        let s2 = get_session(&db, s.id).await.unwrap().unwrap();
        assert_eq!(s2.tool, "claude", "session.tool untouched — the write never committed");
        assert_eq!(s2.model, None);
    }

    #[tokio::test]
    async fn rename_rejects_empty_and_missing() {
        let db = mem().await;
        let ws = create_workspace(&db, "w").await.unwrap();
        assert!(rename_workspace(&db, ws.id, "   ").await.is_err());
        assert!(rename_workspace(&db, 9999, "x").await.is_err());
        assert!(rename_thread(&db, 9999, "x").await.is_err());
        assert!(rename_direction(&db, 9999, "x").await.is_err());
    }

    #[tokio::test]
    async fn rename_rejects_sibling_collisions() {
        let db = mem().await;
        let ws_a = create_workspace(&db, "Alpha").await.unwrap();
        let ws_b = create_workspace(&db, "Beta").await.unwrap();
        // same name as another workspace → rejected; renaming to its own
        // current name is a no-op-style allowed (filtered by id-ne).
        assert!(rename_workspace(&db, ws_b.id, "Alpha").await.is_err());
        assert!(rename_workspace(&db, ws_a.id, "Alpha").await.is_ok());

        let repo = add_repo_ref(&db, ws_a.id, "web-app", "/tmp/x", "main", "", true)
            .await
            .unwrap();
        let t1 = create_thread(&db, ws_a.id, "Login", "feature", "claude")
            .await
            .unwrap();
        let t2 = create_thread(&db, ws_a.id, "Signup", "feature", "claude")
            .await
            .unwrap();
        // duplicate within same workspace → rejected
        assert!(rename_thread(&db, t2.id, "Login").await.is_err());
        // same title in a DIFFERENT workspace is fine
        let t3 = create_thread(&db, ws_b.id, "Other", "feature", "claude")
            .await
            .unwrap();
        assert!(rename_thread(&db, t3.id, "Login").await.is_ok());

        let d1 = create_direction(&db, t1.id, "api", "claude", repo.id, "r", "plan+impl", "")
            .await
            .unwrap();
        let d2 = create_direction(&db, t1.id, "ui", "claude", repo.id, "r", "plan+impl", "")
            .await
            .unwrap();
        assert!(rename_direction(&db, d2.id, "api").await.is_err());
        // same direction name under a DIFFERENT thread is fine
        let d3 = create_direction(&db, t2.id, "main", "claude", repo.id, "r", "plan+impl", "")
            .await
            .unwrap();
        assert!(rename_direction(&db, d3.id, "api").await.is_ok());
        let _ = d1;
    }

    #[tokio::test]
    async fn skill_source_and_enable_roundtrip() {
        let db = mem().await;
        let s = add_skill_source(&db, "https://example.com/skills.git", None)
            .await
            .unwrap();
        assert_eq!(s.git_url, "https://example.com/skills.git");
        assert_eq!(s.last_status, "never");
        // update status
        set_skill_source_status(&db, s.id, "ok", Some("123"))
            .await
            .unwrap();
        let got = list_skill_sources(&db).await.unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].last_status, "ok");
        assert_eq!(got[0].last_synced, "123");
        // enable a skill globally, then list
        set_skill_enable(&db, s.id, "deploy", "global", true)
            .await
            .unwrap();
        let en = list_skill_enable(&db).await.unwrap();
        assert_eq!(en.len(), 1);
        assert_eq!(
            (en[0].skill_name.as_str(), en[0].scope.as_str()),
            ("deploy", "global")
        );
        // toggling off removes it
        set_skill_enable(&db, s.id, "deploy", "global", false)
            .await
            .unwrap();
        assert!(list_skill_enable(&db).await.unwrap().is_empty());
        // remove source cascades its enables
        set_skill_enable(&db, s.id, "x", "ws:1", true)
            .await
            .unwrap();
        remove_skill_source(&db, s.id).await.unwrap();
        assert!(list_skill_sources(&db).await.unwrap().is_empty());
        assert!(list_skill_enable(&db).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn add_skill_source_is_idempotent_on_same_url_ref() {
        let db = mem().await;
        let url = "https://example.com/skills.git";
        let a = add_skill_source(&db, url, None).await.unwrap();
        let b = add_skill_source(&db, url, None).await.unwrap();
        let c = add_skill_source(&db, url, Some("")).await.unwrap();
        assert_eq!(a.id, b.id, "same url+empty ref must reuse row");
        assert_eq!(a.id, c.id, "None and Some(\"\") must collapse");
        assert_eq!(list_skill_sources(&db).await.unwrap().len(), 1);

        // Different ref on same URL is a distinct source.
        let d = add_skill_source(&db, url, Some("main")).await.unwrap();
        assert_ne!(a.id, d.id);
        assert_eq!(list_skill_sources(&db).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn create_direction_persists_base_and_defaults_target() {
        use std::process::Command as Cmd;
        let _env = crate::paths::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let root = std::env::temp_dir().join(format!("weft-cdbase-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let repo_path = root.join("api");
        std::fs::create_dir_all(&repo_path).unwrap();
        for args in [
            vec!["init", "-q"],
            vec!["config", "user.email", "t@t.t"],
            vec!["config", "user.name", "t"],
        ] {
            Cmd::new("git").args(&args).current_dir(&repo_path).status().unwrap();
        }
        std::fs::write(repo_path.join("README.md"), "# x\n").unwrap();
        Cmd::new("git").args(["add", "-A"]).current_dir(&repo_path).status().unwrap();
        Cmd::new("git").args(["commit", "-q", "-m", "init"]).current_dir(&repo_path).status().unwrap();

        let db = Db::connect("sqlite::memory:").await.unwrap();
        let ws = create_workspace(&db, "ws").await.unwrap();
        let r = add_repo_ref(&db, ws.id, "api", repo_path.to_str().unwrap(), "main", "", true)
            .await
            .unwrap();
        let t = create_thread(&db, ws.id, "t1", "feature", "claude").await.unwrap();

        // A concrete base → stored, and target_branch defaults to it.
        let d = create_direction_with_engine_pin(
            &db,
            t.id,
            "x",
            "claude",
            r.id,
            "r",
            "plan+impl",
            "develop",
            true,
        )
        .await
        .unwrap();
        assert_eq!(d.base_branch, "develop");
        assert_eq!(d.target_branch, "develop", "target defaults to the chosen base");
        assert!(d.engine_pinned, "the pin must be part of the inserted direction");

        // Empty base → both empty (each resolves to the repo default later).
        let d2 = create_direction(&db, t.id, "y", "claude", r.id, "r", "plan+impl", "")
            .await
            .unwrap();
        assert_eq!(d2.base_branch, "");
        assert_eq!(d2.target_branch, "", "empty base leaves target empty (= repo default)");
        assert!(!d2.engine_pinned);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn complete_by_id_targets_the_named_row() {
        let db = mem().await;
        let t = live_thread(&db).await;
        let a = insert_lead_message(&db, t, None, 1, "user", "text", "{}", "queued")
            .await
            .unwrap();
        let b = insert_lead_message(&db, t, None, 2, "user", "text", "{}", "queued")
            .await
            .unwrap();
        // deliver b first (simulates reorder: b before a)
        let done = complete_queued_by_id(&db, b.id).await.unwrap().unwrap();
        assert_eq!(done.id, b.id);
        assert_eq!(done.status, "complete");
        // a must still be queued
        let still = lead_message::Entity::find_by_id(a.id)
            .one(&db.0)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(still.status, "queued");
    }

    /// Unwraps a `ConsumeMark::Marked`, panicking with the actual variant name
    /// otherwise — keeps the "must have marked" assertions below readable.
    fn expect_marked(outcome: ConsumeMark) -> lead_message::Model {
        match outcome {
            ConsumeMark::Marked(m) => m,
            ConsumeMark::AlreadyConsumed => panic!("expected Marked, got AlreadyConsumed"),
            ConsumeMark::NotEligible => panic!("expected Marked, got NotEligible"),
        }
    }

    /// A delivered ("complete") user row flips NULL -> Some(millis) exactly once.
    #[tokio::test]
    async fn mark_message_consumed_flips_null_to_some() {
        let db = mem().await;
        let t = live_thread(&db).await;
        let m = insert_lead_message(&db, t, None, 1, "user", "text", "{}", "complete")
            .await
            .unwrap();
        assert_eq!(m.consumed_at, None, "fresh delivered row starts unconsumed");
        let consumed = expect_marked(mark_message_consumed(&db, m.id).await.unwrap());
        assert_eq!(consumed.id, m.id);
        assert!(consumed.consumed_at.is_some(), "must stamp a millis timestamp");
        // status is untouched — consumed_at is an orthogonal signal.
        assert_eq!(consumed.status, "complete");
    }

    /// A second mark reports AlreadyConsumed (a real terminal state, the
    /// caller must NOT retry it): the DB timestamp never gets overwritten by
    /// a later "first activity" observation racing in.
    #[tokio::test]
    async fn mark_message_consumed_idempotent_second_call_reports_already_consumed() {
        let db = mem().await;
        let t = live_thread(&db).await;
        let m = insert_lead_message(&db, t, None, 1, "user", "text", "{}", "complete")
            .await
            .unwrap();
        let first = expect_marked(mark_message_consumed(&db, m.id).await.unwrap());
        let second = mark_message_consumed(&db, m.id).await.unwrap();
        assert!(
            matches!(second, ConsumeMark::AlreadyConsumed),
            "an already-consumed row must report AlreadyConsumed, not re-fire or read as retry-worthy"
        );
        let still = lead_message::Entity::find_by_id(m.id).one(&db.0).await.unwrap().unwrap();
        assert_eq!(still.consumed_at, first.consumed_at, "timestamp must not change");
    }

    /// A still-queued row hasn't reached the agent yet — it cannot be marked
    /// "consumed" ahead of "delivered" (queued -> complete -> consumed only).
    /// Reports NotEligible (retry-worthy), not AlreadyConsumed: PR #117
    /// review P2 — a queued delivery's row flips to "complete" only AFTER its
    /// turn is already dispatched (`mark_queued_delivered`'s callers), so the
    /// agent's first activity CAN legitimately race ahead of that flip, and
    /// the caller (engine.rs's one-shot gate) must retry rather than give up.
    #[tokio::test]
    async fn mark_message_consumed_reports_not_eligible_for_a_queued_row() {
        let db = mem().await;
        let t = live_thread(&db).await;
        let m = insert_lead_message(&db, t, None, 1, "user", "text", "{}", "queued")
            .await
            .unwrap();
        let res = mark_message_consumed(&db, m.id).await.unwrap();
        assert!(
            matches!(res, ConsumeMark::NotEligible),
            "a queued row must be NotEligible (retry-worthy), not AlreadyConsumed"
        );
        let still = lead_message::Entity::find_by_id(m.id).one(&db.0).await.unwrap().unwrap();
        assert_eq!(still.consumed_at, None);
    }

    /// The retry the above test motivates: once the SAME row transitions
    /// queued -> complete (a real `complete_queued_by_id` delivery), it
    /// becomes markable — proving NotEligible really is transient, not a
    /// permanent rejection reachable only by construction.
    #[tokio::test]
    async fn mark_message_consumed_succeeds_after_a_queued_row_is_delivered() {
        let db = mem().await;
        let t = live_thread(&db).await;
        let m = insert_lead_message(&db, t, None, 1, "user", "text", "{}", "queued")
            .await
            .unwrap();
        assert!(matches!(
            mark_message_consumed(&db, m.id).await.unwrap(),
            ConsumeMark::NotEligible
        ));
        complete_queued_by_id(&db, m.id).await.unwrap();
        let consumed = expect_marked(mark_message_consumed(&db, m.id).await.unwrap());
        assert!(consumed.consumed_at.is_some());
    }

    /// Only the human's own row carries the receipt — an assistant/system row
    /// (even if somehow passed in) is never a valid target.
    #[tokio::test]
    async fn mark_message_consumed_reports_not_eligible_for_non_user_role() {
        let db = mem().await;
        let t = live_thread(&db).await;
        let m = insert_lead_message(&db, t, None, 1, "assistant", "text", "{}", "complete")
            .await
            .unwrap();
        let res = mark_message_consumed(&db, m.id).await.unwrap();
        assert!(
            matches!(res, ConsumeMark::NotEligible),
            "a non-user row must not be markable as consumed"
        );
    }

    /// A missing row (e.g. deleted between the activity event and the mark)
    /// fails soft — NotEligible, not an error the caller must handle specially.
    #[tokio::test]
    async fn mark_message_consumed_reports_not_eligible_for_a_missing_row() {
        let db = mem().await;
        let res = mark_message_consumed(&db, 999_999).await.unwrap();
        assert!(matches!(res, ConsumeMark::NotEligible));
    }

    /// PR #117 review (P2, repo.rs:2310 x2): questioned whether
    /// `mark_message_consumed`'s `let mut a: ActiveModel = m.into(); a.field
    /// = Set(x); a.update()` idiom clobbers OTHER columns a concurrent write
    /// landed on between this function's read and its update (e.g. the
    /// rewind anchor / delivery seq, both written by separate code paths
    /// while a "consumed" mark is in flight).
    ///
    /// It does not: SeaORM's `Model -> ActiveModel` conversion sets every
    /// field to `ActiveValue::Unchanged`, and `.update()` only includes
    /// explicitly-`Set` columns in the UPDATE's SET clause — `Unchanged`
    /// columns are excluded, not re-written with the stale snapshot's value.
    /// This proves it directly (write a concurrent value, then run a
    /// stale-snapshot partial update touching an unrelated column, then
    /// assert the concurrent value survived) rather than trusting the
    /// pattern by inspection — and it covers the SAME idiom `complete_queued_by_id`
    /// already relies on, not just this one call site.
    #[tokio::test]
    async fn stale_snapshot_partial_update_does_not_clobber_concurrent_columns() {
        let db = mem().await;
        let t = live_thread(&db).await;
        let m = insert_lead_message(&db, t, None, 1, "user", "text", "{}", "complete")
            .await
            .unwrap();
        // A "concurrent" write lands on a DIFFERENT column, after `m` above
        // was read as a snapshot with native_anchor == None.
        set_lead_message_anchor(&db, m.id, "concurrent-anchor").await.unwrap();

        // The exact stale-snapshot-then-partial-Set-then-update idiom shared
        // by mark_message_consumed / complete_queued_by_id, applied to `m`
        // (captured BEFORE the concurrent write) touching an unrelated field.
        let mut a: lead_message::ActiveModel = m.into();
        a.status = Set("interrupted".to_string());
        a.update(&db.0).await.unwrap();

        let after = lead_message::Entity::find_by_id(1).one(&db.0).await.unwrap().unwrap();
        assert_eq!(
            after.native_anchor.as_deref(),
            Some("concurrent-anchor"),
            "a stale Model's Unchanged fields must not overwrite a concurrent \
             write on the same row — only explicitly Set columns land in the UPDATE"
        );
    }

    /// The same proof, specific to `mark_message_consumed`'s own two writable
    /// paths a real race could hit: the rewind anchor (set on TurnEnd) and
    /// the delivery seq (set when a queued row is dequeued) — both real
    /// concurrent writers to a "user" row's OTHER columns while a "consumed"
    /// mark is in flight for that same row.
    #[tokio::test]
    async fn mark_message_consumed_preserves_concurrently_written_anchor_and_seq() {
        let db = mem().await;
        let t = live_thread(&db).await;
        let m = insert_lead_message(&db, t, None, 1, "user", "text", "{}", "complete")
            .await
            .unwrap();
        set_lead_message_anchor(&db, m.id, "concurrent-anchor").await.unwrap();
        let seq = assign_delivery_seq(&db, t, m.id).await.unwrap();

        let consumed = expect_marked(mark_message_consumed(&db, m.id).await.unwrap());

        assert_eq!(
            consumed.native_anchor.as_deref(),
            Some("concurrent-anchor"),
            "must not clobber a concurrently-written rewind anchor"
        );
        assert_eq!(consumed.seq, Some(seq), "must not clobber a concurrently-assigned delivery seq");
        assert!(consumed.consumed_at.is_some(), "the actual mark must still land");
    }

    /// M0030: analysis_state/error round-trip and upsert_repo_profile preserves them.
    #[tokio::test]
    async fn analysis_state_roundtrips_and_upsert_preserves() {
        let db = mem().await;
        let ws = create_workspace(&db, "ws").await.unwrap();
        let r = add_repo_ref(&db, ws.id, "api", "/tmp/api", "main", "", true)
            .await
            .unwrap();
        // Create a minimal profile row.
        upsert_repo_profile(&db, r.id, "backend", "[]", "", "[]", "agent", "")
            .await
            .unwrap();

        // (1) Set running/None → read back.
        set_analysis_state(&db, r.id, "running", None)
            .await
            .unwrap();
        let p = get_repo_profile(&db, r.id).await.unwrap().unwrap();
        assert_eq!(p.analysis_state, "running");
        assert_eq!(p.analysis_error, None);

        // (2) Set failed/Some("boom") → read back.
        set_analysis_state(&db, r.id, "failed", Some("boom"))
            .await
            .unwrap();
        let p = get_repo_profile(&db, r.id).await.unwrap().unwrap();
        assert_eq!(p.analysis_state, "failed");
        assert_eq!(p.analysis_error.as_deref(), Some("boom"));

        // (3) A normal upsert_repo_profile (agent re-classify) must NOT clobber
        //     the state set above — analysis_state/error are preserved.
        upsert_repo_profile(&db, r.id, "frontend", "[]", "summary", "[]", "agent", "abc")
            .await
            .unwrap();
        let p = get_repo_profile(&db, r.id).await.unwrap().unwrap();
        assert_eq!(
            p.analysis_state, "failed",
            "upsert must not reset analysis_state"
        );
        assert_eq!(
            p.analysis_error.as_deref(),
            Some("boom"),
            "upsert must not reset analysis_error"
        );
        // But the profiling fields were updated normally.
        assert_eq!(p.role, "frontend");
        assert_eq!(p.profiled_commit, "abc");
    }

    /// First-run resume: set_analysis_state("running") on a repo with no profile row
    /// must create a placeholder so the startup resume scan can find it.
    #[tokio::test]
    async fn set_analysis_state_creates_placeholder_for_new_repo() {
        let db = mem().await;
        let ws = create_workspace(&db, "ws").await.unwrap();
        let r = add_repo_ref(&db, ws.id, "new-repo", "/tmp/new", "main", "", true)
            .await
            .unwrap();
        // No profile row yet.
        assert!(
            get_repo_profile(&db, r.id).await.unwrap().is_none(),
            "precondition: no profile row"
        );

        // set_analysis_state("running") must create a placeholder row.
        set_analysis_state(&db, r.id, "running", None)
            .await
            .unwrap();
        let p = get_repo_profile(&db, r.id)
            .await
            .unwrap()
            .expect("placeholder row must exist after set_analysis_state(running)");
        assert_eq!(p.analysis_state, "running");
        assert_eq!(p.analysis_error, None);
        // Placeholder must NOT count as fully profiled (role and summary are blank).
        assert!(
            p.role.is_empty() && p.summary.is_empty(),
            "placeholder must have blank role/summary"
        );
    }

    /// Finding 5: set_analysis_state must not create a placeholder for a deleted repo.
    /// Simulates the deletion race: analysis finishes after delete_repo_cascade, so
    /// the repo_ref row is gone but there is no profile row either.
    #[tokio::test]
    async fn set_analysis_state_noop_for_deleted_repo() {
        let db = mem().await;
        let ws = create_workspace(&db, "ws").await.unwrap();
        let r = add_repo_ref(&db, ws.id, "gone-repo", "/tmp/gone", "main", "", true)
            .await
            .unwrap();
        // Simulate cascade delete: remove the repo_ref row (no profile row exists).
        delete_repo_cascade(&db, r.id).await.unwrap();
        assert!(
            get_repo(&db, r.id).await.unwrap().is_none(),
            "precondition: repo_ref must be gone"
        );
        assert!(
            get_repo_profile(&db, r.id).await.unwrap().is_none(),
            "precondition: no profile row"
        );

        // set_analysis_state("running") on a nonexistent repo must be a no-op —
        // it must NOT create an orphaned profile row.
        set_analysis_state(&db, r.id, "running", None).await.unwrap();
        assert!(
            get_repo_profile(&db, r.id).await.unwrap().is_none(),
            "set_analysis_state must not create a profile row for a deleted repo"
        );

        // Same for "failed".
        set_analysis_state(&db, r.id, "failed", Some("timeout")).await.unwrap();
        assert!(
            get_repo_profile(&db, r.id).await.unwrap().is_none(),
            "set_analysis_state(failed) must not create a profile row for a deleted repo"
        );
    }

    /// set_analysis_state("idle") on a no-row repo must remain a no-op (idle is the default).
    #[tokio::test]
    async fn set_analysis_state_idle_no_row_is_noop() {
        let db = mem().await;
        let ws = create_workspace(&db, "ws").await.unwrap();
        let r = add_repo_ref(&db, ws.id, "other-repo", "/tmp/other", "main", "", true)
            .await
            .unwrap();
        assert!(
            get_repo_profile(&db, r.id).await.unwrap().is_none(),
            "precondition: no profile row"
        );

        set_analysis_state(&db, r.id, "idle", None).await.unwrap();
        assert!(
            get_repo_profile(&db, r.id).await.unwrap().is_none(),
            "idle on no-row must remain a no-op"
        );
    }

    /// repos_with_analysis_state returns exactly the repos whose profile has the
    /// given state, not idle or failed ones.
    #[tokio::test]
    async fn repos_with_analysis_state_returns_only_matching() {
        let db = mem().await;
        let ws = create_workspace(&db, "ws").await.unwrap();
        let running = add_repo_ref(&db, ws.id, "running-repo", "/tmp/running", "main", "", true)
            .await
            .unwrap();
        let idle = add_repo_ref(&db, ws.id, "idle-repo", "/tmp/idle", "main", "", true)
            .await
            .unwrap();

        // Seed profiles: running-repo gets analysis_state="running" via the
        // placeholder-creating path; idle-repo gets a full profile but stays idle.
        set_analysis_state(&db, running.id, "running", None).await.unwrap();
        upsert_repo_profile(&db, idle.id, "backend", "[]", "summary", "[]", "agent", "sha")
            .await
            .unwrap();
        // idle-repo's analysis_state column defaults to "idle" — no explicit set needed.

        let got = repos_with_analysis_state(&db, "running").await.unwrap();
        assert_eq!(got.len(), 1, "only the running repo must be returned");
        assert_eq!(got[0].id, running.id, "returned repo must be the running one");

        // The idle-repo must NOT appear in the running results.
        assert!(
            !got.iter().any(|r| r.id == idle.id),
            "idle repo must not appear in running results"
        );
    }

    /// M0031: set_repo_category_domains writes and reads back; upsert_repo_profile
    /// does NOT touch category/domains (preservation invariant).
    #[tokio::test]
    async fn category_domains_roundtrip_and_upsert_preserves() {
        let db = mem().await;
        let ws = create_workspace(&db, "ws").await.unwrap();
        let r = add_repo_ref(&db, ws.id, "svc", "/tmp/svc", "main", "", true)
            .await
            .unwrap();
        upsert_repo_profile(&db, r.id, "backend", "[]", "", "[]", "agent", "")
            .await
            .unwrap();

        // (1) Set and read back category/domains.
        set_repo_category_domains(&db, r.id, "biz", r#"["orders","payments"]"#)
            .await
            .unwrap();
        let p = get_repo_profile(&db, r.id).await.unwrap().unwrap();
        assert_eq!(p.category, "biz");
        assert_eq!(p.domains, r#"["orders","payments"]"#);

        // (2) A subsequent upsert_repo_profile (agent re-classify) must NOT clobber
        //     category/domains — they are preserved (Unchanged in the ActiveModel).
        upsert_repo_profile(&db, r.id, "frontend", "[]", "new summary", "[]", "agent", "sha2")
            .await
            .unwrap();
        let p = get_repo_profile(&db, r.id).await.unwrap().unwrap();
        assert_eq!(
            p.category, "biz",
            "upsert must not reset category"
        );
        assert_eq!(
            p.domains, r#"["orders","payments"]"#,
            "upsert must not reset domains"
        );
        // But profiling fields were updated normally.
        assert_eq!(p.role, "frontend");
        assert_eq!(p.profiled_commit, "sha2");
    }

    /// M0031: a fresh profile row (first upsert, no prior set_repo_category_domains)
    /// must default category="" and domains="[]".
    #[tokio::test]
    async fn category_domains_default_on_fresh_profile() {
        let db = mem().await;
        let ws = create_workspace(&db, "ws").await.unwrap();
        let r = add_repo_ref(&db, ws.id, "new-svc", "/tmp/new-svc", "main", "", true)
            .await
            .unwrap();
        upsert_repo_profile(&db, r.id, "backend", "[]", "", "[]", "agent", "")
            .await
            .unwrap();
        let p = get_repo_profile(&db, r.id).await.unwrap().unwrap();
        assert_eq!(p.category, "", "fresh row: category defaults to empty string");
        assert_eq!(p.domains, "[]", "fresh row: domains defaults to '[]'");
    }

    /// M0033: set_repo_layer_rank writes and reads back; upsert_repo_profile does NOT
    /// touch layer/layer_rank (preservation invariant — agent re-classify keeps the
    /// cross-repo pass's layering until that pass reruns).
    #[tokio::test]
    async fn layer_rank_roundtrip_and_upsert_preserves() {
        let db = mem().await;
        let ws = create_workspace(&db, "ws").await.unwrap();
        let r = add_repo_ref(&db, ws.id, "svc", "/tmp/svc", "main", "", true)
            .await
            .unwrap();
        upsert_repo_profile(&db, r.id, "backend", "[]", "", "[]", "agent", "")
            .await
            .unwrap();

        // (1) Set and read back layer/layer_rank.
        set_repo_layer_rank(&db, r.id, "Core 核心", 3).await.unwrap();
        let p = get_repo_profile(&db, r.id).await.unwrap().unwrap();
        assert_eq!(p.layer, "Core 核心");
        assert_eq!(p.layer_rank, 3);

        // (2) A subsequent upsert_repo_profile (per-repo re-classify) must NOT clobber
        //     layer/layer_rank — they are preserved (Unchanged in the ActiveModel).
        upsert_repo_profile(&db, r.id, "frontend", "[]", "new summary", "[]", "agent", "sha2")
            .await
            .unwrap();
        let p = get_repo_profile(&db, r.id).await.unwrap().unwrap();
        assert_eq!(p.layer, "Core 核心", "upsert must not reset layer");
        assert_eq!(p.layer_rank, 3, "upsert must not reset layer_rank");
        assert_eq!(p.role, "frontend");
        assert_eq!(p.profiled_commit, "sha2");
    }

    /// M0033: a fresh profile row (first upsert, no prior set_repo_layer_rank) must
    /// default layer="" and layer_rank=0.
    #[tokio::test]
    async fn layer_rank_default_on_fresh_profile() {
        let db = mem().await;
        let ws = create_workspace(&db, "ws").await.unwrap();
        let r = add_repo_ref(&db, ws.id, "new-svc", "/tmp/new-svc", "main", "", true)
            .await
            .unwrap();
        upsert_repo_profile(&db, r.id, "backend", "[]", "", "[]", "agent", "")
            .await
            .unwrap();
        let p = get_repo_profile(&db, r.id).await.unwrap().unwrap();
        assert_eq!(p.layer, "", "fresh row: layer defaults to empty string");
        assert_eq!(p.layer_rank, 0, "fresh row: layer_rank defaults to 0");
    }

    /// set_repo_map_doc / get_repo_map_doc round-trip: store and retrieve a
    /// markdown doc keyed per workspace, and confirm absent workspaces return None.
    #[tokio::test]
    async fn repo_map_doc_round_trip() {
        let db = mem().await;
        let ws = create_workspace(&db, "ws").await.unwrap();

        // Nothing stored yet → None.
        let doc = get_repo_map_doc(&db, ws.id).await.unwrap();
        assert!(doc.is_none(), "no doc before first set");

        // Store a markdown document.
        let md = "## Inventory\n- web (frontend): SPA\n\n## Domain index\n- auth: [api]";
        set_repo_map_doc(&db, ws.id, md).await.unwrap();
        let doc = get_repo_map_doc(&db, ws.id).await.unwrap();
        assert_eq!(doc.as_deref(), Some(md), "retrieved doc must equal stored doc");

        // Overwrite with a new doc (upsert semantics).
        let md2 = "## Inventory v2\n- api (backend): REST API";
        set_repo_map_doc(&db, ws.id, md2).await.unwrap();
        let doc2 = get_repo_map_doc(&db, ws.id).await.unwrap();
        assert_eq!(doc2.as_deref(), Some(md2), "second set overwrites the first");

        // A different workspace id has its own slot — no cross-workspace bleed.
        let ws2 = create_workspace(&db, "ws2").await.unwrap();
        let doc_ws2 = get_repo_map_doc(&db, ws2.id).await.unwrap();
        assert!(doc_ws2.is_none(), "different workspace has no doc");
    }

    /// clear_repo_map_doc deletes the row so the doc reads None again (the map
    /// pane falls back to its empty state, not a stale Some("")).
    #[tokio::test]
    async fn clear_repo_map_doc_resets_to_none() {
        let db = mem().await;
        let ws = create_workspace(&db, "ws").await.unwrap();

        set_repo_map_doc(&db, ws.id, "## Inventory\n- web").await.unwrap();
        assert!(get_repo_map_doc(&db, ws.id).await.unwrap().is_some());

        clear_repo_map_doc(&db, ws.id).await.unwrap();
        assert!(
            get_repo_map_doc(&db, ws.id).await.unwrap().is_none(),
            "cleared doc must read as None, not Some(\"\")"
        );

        // Clearing an already-absent doc is a no-op, not an error.
        clear_repo_map_doc(&db, ws.id).await.unwrap();
    }

    /// Central invariant: writing relations invalidates the workspace map doc.
    /// Covers the "successful relation pass omits markdown" case — persist_relations
    /// writes through here, so with no replacement markdown the doc must not keep
    /// serving the pre-pass narrative.
    #[tokio::test]
    async fn set_repo_relations_invalidates_map_doc() {
        let db = mem().await;
        let ws = create_workspace(&db, "ws").await.unwrap();
        let r = add_repo_ref(&db, ws.id, "a", "/tmp/a", "main", "", true).await.unwrap();
        upsert_repo_profile(&db, r.id, "backend", "[]", "", "[]", "agent", "").await.unwrap();
        set_repo_map_doc(&db, ws.id, "## old map").await.unwrap();

        set_repo_relations(&db, r.id, "[]").await.unwrap();

        assert!(
            get_repo_map_doc(&db, ws.id).await.unwrap().is_none(),
            "writing relations must invalidate the stale workspace map doc"
        );
    }

    #[tokio::test]
    async fn set_repo_relations_noops_when_repo_ref_was_deleted() {
        let db = mem().await;
        let ws = create_workspace(&db, "ws").await.unwrap();
        let r = add_repo_ref(&db, ws.id, "a", "/tmp/a", "main", "", true).await.unwrap();
        upsert_repo_profile(&db, r.id, "backend", "[]", "", "[]", "agent", "")
            .await
            .unwrap();
        repo_ref::Entity::delete_by_id(r.id).exec(&db.0).await.unwrap();

        set_repo_relations(&db, r.id, r#"[{"to":99,"kind":"http"}]"#)
            .await
            .unwrap();

        let profile = get_repo_profile(&db, r.id).await.unwrap().unwrap();
        assert_eq!(profile.relations, "[]");
    }

    #[tokio::test]
    async fn upsert_repo_profile_rejects_deleted_repo_ref() {
        let db = mem().await;
        let ws = create_workspace(&db, "ws").await.unwrap();
        let r = add_repo_ref(&db, ws.id, "a", "/tmp/a", "main", "", true).await.unwrap();
        repo_ref::Entity::delete_by_id(r.id).exec(&db.0).await.unwrap();

        let err = upsert_repo_profile(&db, r.id, "backend", "[]", "summary", "[]", "agent", "")
            .await
            .unwrap_err();

        assert!(err.to_string().contains("not found"));
        assert!(get_repo_profile(&db, r.id).await.unwrap().is_none());
    }

    /// A manual edge calibration mutates relations → the stored map doc (describing
    /// the pre-calibration edges) must be cleared. Goes through set_repo_relations.
    #[tokio::test]
    async fn calibrate_repo_relation_invalidates_map_doc() {
        let db = mem().await;
        let ws = create_workspace(&db, "ws").await.unwrap();
        let a = add_repo_ref(&db, ws.id, "a", "/tmp/a", "main", "", true).await.unwrap();
        let b = add_repo_ref(&db, ws.id, "b", "/tmp/b", "main", "", true).await.unwrap();
        upsert_repo_profile(&db, a.id, "backend", "[]", "", "[]", "agent", "").await.unwrap();
        set_repo_map_doc(&db, ws.id, "## old map").await.unwrap();

        calibrate_repo_relation(&db, a.id, b.id, "http", "GET /x", "add").await.unwrap();

        assert!(
            get_repo_map_doc(&db, ws.id).await.unwrap().is_none(),
            "manual edge calibration must invalidate the stale workspace map doc"
        );
    }

    #[tokio::test]
    async fn next_turn_id_increments_from_last_row() {
        let db = Db::connect("sqlite::memory:").await.unwrap();
        let ws = create_workspace(&db, "ws_turn").await.unwrap();
        let t = create_thread(&db, ws.id, "curator", "curator", "claude").await.unwrap();
        // Empty thread → 1.
        assert_eq!(next_turn_id(&db, t.id).await.unwrap(), 1);
        // Insert a row with turn_id 4 → next is 5.
        insert_lead_message(&db, t.id, None, 4, "user", "text", r#"{"text":"hi"}"#, "complete")
            .await
            .unwrap();
        assert_eq!(next_turn_id(&db, t.id).await.unwrap(), 5);
    }

    /// FIX 4: assign_delivery_seq makes a reordered-then-delivered row sort after
    /// rows with lower ids that were NOT yet assigned a seq.
    /// Insert A (id=low), B (id=mid), C (id=high). Deliver B first (simulating a
    /// reorder). list_lead_messages must show A, C, B (B's seq > C's effective key).
    #[tokio::test]
    async fn delivery_seq_overrides_id_order() {
        let db = mem().await;
        let t = live_thread(&db).await;
        let a = insert_lead_message(&db, t, None, 1, "user", "text", r#"{"text":"A"}"#, "complete")
            .await
            .unwrap();
        let b = insert_lead_message(&db, t, None, 2, "user", "text", r#"{"text":"B"}"#, "complete")
            .await
            .unwrap();
        let c = insert_lead_message(&db, t, None, 3, "user", "text", r#"{"text":"C"}"#, "complete")
            .await
            .unwrap();

        // Assign a delivery seq to B as if it was delivered after C (reorder scenario).
        // max(COALESCE(seq,id)) over [a.id, b.id, c.id] = c.id, so B.seq = c.id + 1.
        let seq = assign_delivery_seq(&db, t, b.id).await.unwrap();
        assert_eq!(seq, c.id as i64 + 1);

        let msgs = list_lead_messages(&db, t).await.unwrap();
        let ids: Vec<i32> = msgs.iter().map(|m| m.id).collect();
        // COALESCE(seq, id) ordering: A → a.id, C → c.id, B → c.id+1
        assert_eq!(ids, vec![a.id, c.id, b.id], "B must sort after C once its seq > C.id");
    }

    // ---- pull_request (issue #110 T1) ----

    #[tokio::test]
    async fn register_pull_request_creates_then_upserts_by_natural_key() {
        let db = mem().await;
        let (_ws, repo, thread, dir) = worker_fixture(&db).await;

        let first = register_pull_request(
            &db, thread.id, dir.id, repo.id, "github", "github.com", "acme", "widgets", 42,
            "https://github.com/acme/widgets/pull/42", "first title",
        )
        .await
        .unwrap();
        assert_eq!(first.lifecycle, "open");
        assert_eq!(first.title, "first title");

        // Re-registering the SAME (host_kind, owner, repo, number) updates the
        // existing row instead of creating a second one.
        let second = register_pull_request(
            &db, thread.id, dir.id, repo.id, "github", "github.com", "acme", "widgets", 42,
            "https://github.com/acme/widgets/pull/42", "updated title",
        )
        .await
        .unwrap();
        assert_eq!(second.id, first.id, "must upsert, not duplicate");
        assert_eq!(second.title, "updated title");

        let all = pull_request::Entity::find().all(&db.0).await.unwrap();
        assert_eq!(all.len(), 1);
    }

    #[tokio::test]
    async fn list_open_pull_requests_excludes_merged_and_closed() {
        let db = mem().await;
        let (_ws, repo, thread, dir) = worker_fixture(&db).await;
        let open = register_pull_request(
            &db, thread.id, dir.id, repo.id, "github", "github.com", "acme", "widgets", 1, "", "",
        )
        .await
        .unwrap();
        let merged = register_pull_request(
            &db, thread.id, dir.id, repo.id, "github", "github.com", "acme", "widgets", 2, "", "",
        )
        .await
        .unwrap();
        let mut a: pull_request::ActiveModel = merged.clone().into();
        a.lifecycle = Set("merged".to_string());
        a.update(&db.0).await.unwrap();

        let listed = list_open_pull_requests(&db).await.unwrap();
        let ids: Vec<i32> = listed.iter().map(|p| p.id).collect();
        assert_eq!(ids, vec![open.id]);
    }

    #[tokio::test]
    async fn apply_snapshot_overwrites_axes_and_clears_prior_error() {
        let db = mem().await;
        let (_ws, repo, thread, dir) = worker_fixture(&db).await;
        let pr = register_pull_request(
            &db, thread.id, dir.id, repo.id, "github", "github.com", "acme", "widgets", 7, "", "",
        )
        .await
        .unwrap();
        mark_pull_request_probe_error(&db, pr.id, "boom: gh not authenticated")
            .await
            .unwrap();

        let snapshot = crate::host::PrSnapshot {
            head_sha: "abc123".to_string(),
            base_ref: "main".to_string(),
            url: "https://github.com/acme/widgets/pull/7".to_string(),
            title: "fix things".to_string(),
            lifecycle: crate::host::PrLifecycle::Open,
            ci: crate::host::CiStatus::Passing,
            review: crate::host::ReviewStatus::Approved,
            conflict: crate::host::ConflictStatus::Clean,
        };
        let readiness = crate::host::judge::merge_readiness(&snapshot.ci, &snapshot.review, &snapshot.conflict);
        apply_pull_request_snapshot(&db, pr.id, &snapshot, &readiness)
            .await
            .unwrap();

        let reloaded = get_pull_request(&db, pr.id).await.unwrap().unwrap();
        assert_eq!(reloaded.head_sha, "abc123");
        assert_eq!(reloaded.last_error, "", "a successful apply clears any prior probe error");
        assert!(!reloaded.last_checked_at.is_empty());
        assert!(reloaded.ci_status.contains("passing"));
        assert!(reloaded.merge_readiness.contains("ready"));
    }

    #[tokio::test]
    async fn probe_error_leaves_last_known_snapshot_untouched() {
        let db = mem().await;
        let (_ws, repo, thread, dir) = worker_fixture(&db).await;
        let pr = register_pull_request(
            &db, thread.id, dir.id, repo.id, "github", "github.com", "acme", "widgets", 9, "", "",
        )
        .await
        .unwrap();
        let snapshot = crate::host::PrSnapshot {
            head_sha: "known-good-sha".to_string(),
            base_ref: "main".to_string(),
            url: String::new(),
            title: String::new(),
            lifecycle: crate::host::PrLifecycle::Open,
            ci: crate::host::CiStatus::Passing,
            review: crate::host::ReviewStatus::Approved,
            conflict: crate::host::ConflictStatus::Clean,
        };
        let readiness = crate::host::judge::merge_readiness(&snapshot.ci, &snapshot.review, &snapshot.conflict);
        apply_pull_request_snapshot(&db, pr.id, &snapshot, &readiness)
            .await
            .unwrap();

        // A later probe failure (e.g. a transient network blip) must NOT erase
        // the last known-good snapshot — only `last_checked_at`/`last_error` move.
        mark_pull_request_probe_error(&db, pr.id, "network blip")
            .await
            .unwrap();
        let reloaded = get_pull_request(&db, pr.id).await.unwrap().unwrap();
        assert_eq!(reloaded.head_sha, "known-good-sha", "probe failure must not blank the last known snapshot");
        assert_eq!(reloaded.last_error, "network blip");
    }
}
