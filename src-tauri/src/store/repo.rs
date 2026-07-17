//! All DB reads/writes go through here. Keeps SeaORM specifics out of commands.

use super::entities::{
    app_setting, code_checkpoint, direction, im_route, lead_message, plan, repo_profile,
    repo_ref, session, skill_enable, skill_source, test_plan, thread, workspace, worktree,
};
use super::Db;
use crate::slug::unique_slug;
use anyhow::Result;
use sea_orm::{
    sea_query::Expr, ActiveModelTrait, ColumnTrait, EntityTrait, QueryFilter, QueryOrder, Set,
    TryIntoModel,
};
use std::collections::HashMap;

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

/// Mark a thread's plan "confirmed" ONLY if its proposal AND status are still what `confirm`
/// read at the start — i.e. no re-propose and no concurrent confirm landed in between. Unlike
/// `update_plan_proposal_cas` (which pins expected==new status), this flips a NON-confirmed
/// status to "confirmed", so it takes a SEPARATE `expected_status`. Returns true when applied,
/// false when the proposal OR status drifted (or the plan is gone). Leaves proposal +
/// created_at untouched.
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
    db: &Db,
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
        .all(&db.0)
        .await?;
    let Some(pos) = rows.iter().position(|m| m.id == from_message_id) else {
        return Ok(Vec::new());
    };
    let ids: Vec<i32> = rows[pos..].iter().map(|m| m.id).collect();
    lead_message::Entity::delete_many()
        .filter(lead_message::Column::Id.is_in(ids.iter().copied()))
        .exec(&db.0)
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
) -> Result<code_checkpoint::Model> {
    let a = code_checkpoint::ActiveModel {
        worktree_id: Set(worktree_id),
        session_id: Set(session_id),
        lead_message_id: Set(lead_message_id),
        turn_id: Set(turn_id),
        shadow_sha: Set(shadow_sha.to_string()),
        head_sha: Set(head_sha.to_string()),
        nested_repos: Set(nested_repos.to_string()),
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
    db: &Db,
    worktree_id: i32,
    lead_message_ids: &[i32],
) -> Result<u64> {
    if lead_message_ids.is_empty() {
        return Ok(0);
    }
    Ok(code_checkpoint::Entity::delete_many()
        .filter(code_checkpoint::Column::WorktreeId.eq(worktree_id))
        .filter(code_checkpoint::Column::LeadMessageId.is_in(lead_message_ids.iter().copied()))
        .exec(&db.0)
        .await?
        .rows_affected)
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

/// 删除一条消息行（仅用于取消未交付的 queued 行）。
pub async fn delete_message(db: &Db, message_id: i32) -> Result<()> {
    lead_message::Entity::delete_by_id(message_id).exec(&db.0).await?;
    Ok(())
}

/// Stamp a delivered queued row with seq = max(COALESCE(seq, id)) + 1 over its
/// thread so it sorts after all currently-ordered rows in list_lead_messages.
/// Called for every tracked queued delivery to preserve reorder-then-deliver order.
pub async fn assign_delivery_seq(db: &Db, thread_id: i32, message_id: i32) -> Result<()> {
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
    db.0.execute(sea_orm::Statement::from_sql_and_values(
        db.0.get_database_backend(),
        "UPDATE lead_message SET seq = ? WHERE id = ?",
        [next_seq.into(), message_id.into()],
    ))
    .await?;
    Ok(())
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
        let b = ensure_curator_thread(&db, ws.id, "codex").await.unwrap();
        assert_eq!(a, b, "the same curator thread is reused");
        let t = get_thread(&db, a).await.unwrap().unwrap();
        assert_eq!(t.kind, "curator");
        assert_eq!(t.lead_tool, "codex", "uses the provided default tool, not hard-coded claude");
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

        let deleted = truncate_lead_messages(&db, t, Some(7), cut.id)
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
        let deleted = truncate_lead_messages(&db, t, Some(7), b.id)
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
        let c1 = insert_code_checkpoint(&db, 11, 7, 100, 1, "sha-1", "head-1", "[\"gen\"]")
            .await
            .unwrap();
        insert_code_checkpoint(&db, 11, 7, 200, 2, "sha-2", "head-2", "[]")
            .await
            .unwrap();
        insert_code_checkpoint(&db, 22, 8, 100, 1, "sha-other", "head-other", "[]")
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
        let deleted = truncate_code_checkpoints(&db, 11, &[100, 200]).await.unwrap();
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
        let d = create_direction(&db, t.id, "x", "claude", r.id, "r", "plan+impl", "develop")
            .await
            .unwrap();
        assert_eq!(d.base_branch, "develop");
        assert_eq!(d.target_branch, "develop", "target defaults to the chosen base");

        // Empty base → both empty (each resolves to the repo default later).
        let d2 = create_direction(&db, t.id, "y", "claude", r.id, "r", "plan+impl", "")
            .await
            .unwrap();
        assert_eq!(d2.base_branch, "");
        assert_eq!(d2.target_branch, "", "empty base leaves target empty (= repo default)");

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
        assign_delivery_seq(&db, t, b.id).await.unwrap();

        let msgs = list_lead_messages(&db, t).await.unwrap();
        let ids: Vec<i32> = msgs.iter().map(|m| m.id).collect();
        // COALESCE(seq, id) ordering: A → a.id, C → c.id, B → c.id+1
        assert_eq!(ids, vec![a.id, c.id, b.id], "B must sort after C once its seq > C.id");
    }
}
