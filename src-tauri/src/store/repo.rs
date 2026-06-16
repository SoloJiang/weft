//! All DB reads/writes go through here. Keeps SeaORM specifics out of commands.

use super::entities::{
    app_setting, direction, im_route, lead_message, plan, repo_profile, repo_ref, session,
    skill_enable, skill_source, thread, workspace, worktree,
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
    let dup = workspace::Entity::find()
        .filter(workspace::Column::Name.eq(name))
        .filter(workspace::Column::Id.ne(workspace_id))
        .one(&db.0)
        .await?;
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

/// Workspace container used by per-IM-conversation Concierge threads.
pub const K_CONCIERGE_WORKSPACE: &str = "concierge.workspace_id";

/// app_setting key holding a workspace's hidden curator-chat thread id.
fn curator_thread_key(workspace_id: i32) -> String {
    format!("curator.thread.{workspace_id}")
}

/// Get-or-create the hidden curator-chat thread for a workspace (mirrors the
/// Concierge get-or-create). The id is stable (persisted in app_setting); the
/// thread is `kind="curator"` so board views can filter it out.
pub async fn ensure_curator_thread(db: &Db, workspace_id: i32) -> Result<i32> {
    let key = curator_thread_key(workspace_id);
    if let Some(id) = get_setting(db, &key).await?.and_then(|s| s.parse::<i32>().ok()) {
        if let Some(t) = get_thread(db, id).await? {
            if t.kind == "curator" {
                return Ok(id);
            }
        }
    }
    let t = create_thread(db, workspace_id, "Dependency curator", "curator", "claude").await?;
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
) -> Result<repo_ref::Model> {
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
        return Ok(dup.clone());
    }
    let slugs: Vec<String> = existing.into_iter().map(|r| r.slug).collect();
    let m = repo_ref::ActiveModel {
        workspace_id: Set(workspace_id),
        name: Set(name.to_string()),
        slug: Set(unique_slug(name, &slugs)),
        local_git_path: Set(local_git_path.to_string()),
        base_ref: Set(base_ref.to_string()),
        remote_url: Set(remote_url.to_string()),
        ..Default::default()
    };
    Ok(m.insert(&db.0).await?)
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
    let existing: Vec<String> = thread::Entity::find()
        .filter(thread::Column::WorkspaceId.eq(workspace_id))
        .all(&db.0)
        .await?
        .into_iter()
        .map(|t| t.slug)
        .collect();
    let m = thread::ActiveModel {
        workspace_id: Set(workspace_id),
        title: Set(title.to_string()),
        slug: Set(unique_slug(title, &existing)),
        kind: Set(kind.to_string()),
        lead_tool: Set(lead_tool.to_string()),
        created_at: Set(now()),
        ..Default::default()
    };
    Ok(m.insert(&db.0).await?)
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

/// Insert or update a thread's plan/proposal.
pub async fn upsert_plan(
    db: &Db,
    thread_id: i32,
    proposal: &str,
    status: &str,
    created_at: &str,
) -> Result<plan::Model> {
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
    Ok(a.save(&db.0).await?.try_into_model()?)
}

pub async fn get_repo_profile(db: &Db, repo_id: i32) -> Result<Option<repo_profile::Model>> {
    Ok(repo_profile::Entity::find()
        .filter(repo_profile::Column::RepoId.eq(repo_id))
        .one(&db.0)
        .await?)
}

/// Insert or update a repo's profile. `stack`/`published`/`deps` are JSON arrays.
#[allow(clippy::too_many_arguments)]
pub async fn upsert_repo_profile(
    db: &Db,
    repo_id: i32,
    role: &str,
    stack: &str,
    summary: &str,
    published: &str,
    deps: &str,
    source: &str,
    profiled_commit: &str,
) -> Result<repo_profile::Model> {
    let mut a = match get_repo_profile(db, repo_id).await? {
        Some(m) => m.into(),
        // New row: agent relations start empty. (On update we leave `relations`
        // untouched, so deterministic re-profiling never wipes agent findings.)
        None => repo_profile::ActiveModel {
            repo_id: Set(repo_id),
            relations: Set("[]".to_string()),
            ..Default::default()
        },
    };
    a.role = Set(role.to_string());
    a.stack = Set(stack.to_string());
    a.summary = Set(summary.to_string());
    a.published = Set(published.to_string());
    a.deps = Set(deps.to_string());
    a.source = Set(source.to_string());
    a.profiled_commit = Set(profiled_commit.to_string());
    Ok(a.save(&db.0).await?.try_into_model()?)
}

/// Persist the agent curator's inferred relations (JSON array of
/// `profile::AgentRelation`) for a repo, leaving its deterministic facts intact.
/// No-op if the repo has no profile row yet (profiling is eager on add).
pub async fn set_repo_relations(db: &Db, repo_id: i32, relations: &str) -> Result<()> {
    if let Some(m) = get_repo_profile(db, repo_id).await? {
        let mut a: repo_profile::ActiveModel = m.into();
        a.relations = Set(relations.to_string());
        a.update(&db.0).await?;
    }
    Ok(())
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

/// Apply one human calibration to a producer repo's relations. `action="add"`
/// upserts a user-sourced relation for `(to, kind)`; `action="remove"` writes a
/// user `rejected` tombstone for that pair so the edge disappears and the auto
/// pass won't resurrect it. Replaces any prior entry for the same `(to, kind)`.
/// No-op if the repo has no profile row yet.
pub async fn calibrate_repo_relation(
    db: &Db,
    from_id: i32,
    to_id: i32,
    kind: &str,
    via: &str,
    action: &str,
) -> Result<()> {
    let Some(p) = get_repo_profile(db, from_id).await? else {
        return Ok(());
    };
    let mut rels: Vec<crate::profile::AgentRelation> =
        serde_json::from_str(&p.relations).unwrap_or_default();
    // One entry per (to, kind): drop any prior one, then add the calibration.
    rels.retain(|r| !(r.to == to_id && r.kind == kind));
    rels.push(crate::profile::AgentRelation {
        to: to_id,
        kind: kind.to_string(),
        via: via.to_string(),
        confidence: 100,
        source: "user".to_string(),
        rejected: action == "remove",
    });
    let json = serde_json::to_string(&rels).unwrap_or_else(|_| "[]".into());
    set_repo_relations(db, from_id, &json).await
}

pub async fn list_directions(db: &Db, thread_id: i32) -> Result<Vec<direction::Model>> {
    Ok(direction::Entity::find()
        .filter(direction::Column::ThreadId.eq(thread_id))
        .all(&db.0)
        .await?)
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
) -> Result<direction::Model> {
    let t = thread::Entity::find_by_id(thread_id)
        .one(&db.0)
        .await?
        .ok_or_else(|| anyhow::anyhow!("thread {thread_id} not found"))?;
    let _ws = workspace::Entity::find_by_id(t.workspace_id)
        .one(&db.0)
        .await?
        .ok_or_else(|| anyhow::anyhow!("workspace missing"))?;
    let existing: Vec<String> = direction::Entity::find()
        .filter(direction::Column::ThreadId.eq(thread_id))
        .all(&db.0)
        .await?
        .into_iter()
        .map(|d| d.slug)
        .collect();
    let repo_ref = repo_ref::Entity::find_by_id(repo_id)
        .one(&db.0)
        .await?
        .ok_or_else(|| anyhow::anyhow!("repo {repo_id} not found"))?;
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
        created_at: Set(now()),
        ..Default::default()
    }
    .insert(&db.0)
    .await?;
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
) -> Result<worktree::Model> {
    Ok(worktree::ActiveModel {
        repo_id: Set(repo_id),
        direction_id: Set(direction_id),
        branch: Set(branch.to_string()),
        path: Set(path.to_string()),
        created_at: Set(now()),
        ..Default::default()
    }
    .insert(&db.0)
    .await?)
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
/// their sessions, and its worktree rows. Returns the worktrees (repo_id, path,
/// branch) the caller must physically `git worktree remove` — DB rows are gone
/// after this. NEVER touches the user's actual repo directory at `local_git_path`.
pub async fn delete_repo_cascade(db: &Db, repo_id: i32) -> Result<Vec<(i32, String, String)>> {
    // Worktrees registered for this repo (each direction's worktree is keyed to
    // its write repo, so this covers the bound directions' worktrees too).
    let removed: Vec<(i32, String, String)> = worktree::Entity::find()
        .filter(worktree::Column::RepoId.eq(repo_id))
        .all(&db.0)
        .await?
        .into_iter()
        .map(|w| (w.repo_id, w.path, w.branch))
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
    Ok(removed)
}

/// Delete a thread and everything under it. Returns the worktree paths that the
/// caller must physically remove via git (DB rows are gone after this).
pub async fn delete_thread_cascade(db: &Db, thread_id: i32) -> Result<Vec<(i32, String, String)>> {
    let dirs = direction::Entity::find()
        .filter(direction::Column::ThreadId.eq(thread_id))
        .all(&db.0)
        .await?;
    let mut removed: Vec<(i32, String, String)> = Vec::new(); // (repo_id, worktree path, branch)
    for d in &dirs {
        let wts = worktree::Entity::find()
            .filter(worktree::Column::DirectionId.eq(d.id))
            .all(&db.0)
            .await?;
        for w in wts {
            removed.push((w.repo_id, w.path.clone(), w.branch.clone()));
            worktree::Entity::delete_by_id(w.id).exec(&db.0).await?;
        }
        session::Entity::delete_many()
            .filter(session::Column::DirectionId.eq(d.id))
            .exec(&db.0)
            .await?;
        direction::Entity::delete_by_id(d.id).exec(&db.0).await?;
    }
    thread::Entity::delete_by_id(thread_id).exec(&db.0).await?;
    Ok(removed)
}

pub async fn create_session(
    db: &Db,
    direction_id: i32,
    repo_id: i32,
    tool: &str,
    cwd: &str,
) -> Result<session::Model> {
    Ok(session::ActiveModel {
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
    .await?)
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
    Ok(lead_message::ActiveModel {
        thread_id: Set(thread_id),
        session_id: Set(session_id),
        turn_id: Set(turn_id),
        role: Set(role.to_string()),
        kind: Set(kind.to_string()),
        content: Set(content.to_string()),
        status: Set(status.to_string()),
        created_at: Set(now()),
        ..Default::default()
    }
    .insert(&db.0)
    .await?)
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
    // A crash leaves both a half-streamed assistant row and any user message the
    // user queued behind it (the in-memory FIFO is gone, so a `queued` row has no
    // live processor and would otherwise show as pending forever). Close both as
    // interrupted; full queued-message replay is the backend-queue feature (later).
    let mut q = lead_message::Entity::find()
        .filter(lead_message::Column::ThreadId.eq(thread_id))
        .filter(lead_message::Column::Status.is_in(["streaming", "queued"]));
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
    Ok(lead_message::Entity::find()
        .filter(lead_message::Column::ThreadId.eq(thread_id))
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

/// Bind an issue (thread) to an IM thread. Upserts on `thread_id`: re-binding the
/// same issue replaces its target. Returns the resulting row.
pub async fn bind_im_route(
    db: &Db,
    thread_id: i32,
    channel: &str,
    chat_id: &str,
    im_thread_ref: &str,
) -> Result<im_route::Model> {
    if let Some(existing) = im_route::Entity::find()
        .filter(im_route::Column::ThreadId.eq(thread_id))
        .one(&db.0)
        .await?
    {
        let mut a: im_route::ActiveModel = existing.into();
        a.channel = Set(channel.to_string());
        a.chat_id = Set(chat_id.to_string());
        a.im_thread_ref = Set(im_thread_ref.to_string());
        let m = a.update(&db.0).await?;
        return Ok(m);
    }
    let now = now();
    let am = im_route::ActiveModel {
        channel: Set(channel.to_string()),
        chat_id: Set(chat_id.to_string()),
        im_thread_ref: Set(im_thread_ref.to_string()),
        thread_id: Set(thread_id),
        created_at: Set(now),
        ..Default::default()
    };
    let m = am.insert(&db.0).await?.try_into_model()?;
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
        )
        .await
        .unwrap();

        // Same local path (any name/remote) → returns the existing row, no insert.
        let same_path = add_repo_ref(&db, ws.id, "renamed", "/code/web", "main", "")
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
        )
        .await
        .unwrap();
        assert_ne!(other.id, a.id);

        // Two local repos with NO remote and different paths both exist — an empty
        // remote key must never collapse distinct repos.
        let l1 = add_repo_ref(&db, ws.id, "local-1", "/code/l1", "main", "")
            .await
            .unwrap();
        let l2 = add_repo_ref(&db, ws.id, "local-2", "/code/l2", "main", "")
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

    #[tokio::test]
    async fn delete_repo_cascade_removes_repo_and_its_deps_only() {
        let db = mem().await;
        let ws = create_workspace(&db, "ws").await.unwrap();
        let a = add_repo_ref(&db, ws.id, "a", "/tmp/a", "main", "")
            .await
            .unwrap();
        let b = add_repo_ref(&db, ws.id, "b", "/tmp/b", "main", "")
            .await
            .unwrap();
        upsert_repo_profile(&db, a.id, "service", "[]", "", "[]", "[]", "inferred", "")
            .await
            .unwrap();
        upsert_repo_profile(&db, b.id, "service", "[]", "", "[]", "[]", "inferred", "")
            .await
            .unwrap();
        let t = create_thread(&db, ws.id, "T", "feature", "claude")
            .await
            .unwrap();
        // a direction bound to repo `a`, with a session + worktree
        let dir = create_direction(&db, t.id, "d", "claude", a.id, "reason", "plan+impl")
            .await
            .unwrap();
        let sess = create_session(&db, dir.id, a.id, "claude", "/tmp/a-wt")
            .await
            .unwrap();
        record_worktree(&db, a.id, dir.id, &dir.branch, "/tmp/a-wt")
            .await
            .unwrap();
        // a direction bound to repo `b` — must SURVIVE the delete of `a`
        let dir_b = create_direction(&db, t.id, "db", "claude", b.id, "reason", "plan+impl")
            .await
            .unwrap();

        let removed = delete_repo_cascade(&db, a.id).await.unwrap();
        // returns repo `a`'s worktree(s) for the caller to physically remove
        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0].1, "/tmp/a-wt");

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
    }

    #[tokio::test]
    async fn calibrate_repo_relation_adds_user_edge_then_tombstones_removal() {
        let db = mem().await;
        let ws = create_workspace(&db, "ws").await.unwrap();
        let web = add_repo_ref(&db, ws.id, "web", "/tmp/web", "main", "")
            .await
            .unwrap();
        let api = add_repo_ref(&db, ws.id, "api", "/tmp/api", "main", "")
            .await
            .unwrap();
        upsert_repo_profile(&db, web.id, "app", "[]", "", "[]", "[]", "inferred", "")
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

        // remove the same pair+kind → a single user tombstone (not two rows)
        calibrate_repo_relation(&db, web.id, api.id, "grpc", "", "remove")
            .await
            .unwrap();
        let rels = read(&db, web.id).await;
        assert_eq!(rels.len(), 1);
        assert!(rels[0].rejected, "removal writes a tombstone");
        assert_eq!(rels[0].source, "user");

        // a repo with no profile is a no-op (must not panic)
        calibrate_repo_relation(&db, 9999, api.id, "http", "x", "add")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn ensure_curator_thread_is_idempotent_and_kinded() {
        let db = mem().await;
        let ws = create_workspace(&db, "ws").await.unwrap();
        let a = ensure_curator_thread(&db, ws.id).await.unwrap();
        let b = ensure_curator_thread(&db, ws.id).await.unwrap();
        assert_eq!(a, b, "the same curator thread is reused");
        assert_eq!(get_thread(&db, a).await.unwrap().unwrap().kind, "curator");
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
        let m = insert_lead_message(
            &db,
            1,
            None,
            1,
            "user",
            "text",
            r#"{"text":"hi"}"#,
            "complete",
        )
        .await
        .unwrap();
        assert_eq!(m.thread_id, 1);
        update_lead_message(&db, m.id, r#"{"text":"hi!"}"#, "complete")
            .await
            .unwrap();
        let all = list_lead_messages(&db, 1).await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].content, r#"{"text":"hi!"}"#);
        assert_eq!(next_turn_id(&db, 1).await.unwrap(), 2);
    }

    #[tokio::test]
    async fn resolve_action_card_persists_resolved_marker() {
        let db = mem().await;
        let card = insert_lead_message(
            &db,
            1,
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
        let all = list_lead_messages(&db, 1).await.unwrap();
        assert_eq!(all[0].content, updated.content);
        // a missing row is a no-op
        assert!(resolve_action_card(&db, 9999, "x").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn stale_streaming_messages_mark_interrupted_on_reopen() {
        let db = mem().await;
        let streaming = insert_lead_message(
            &db,
            4,
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
            4,
            Some(9),
            2,
            "user",
            "text",
            r#"{"text":"next"}"#,
            "queued",
        )
        .await
        .unwrap();

        mark_incomplete_turns_interrupted(&db, 4, Some(9))
            .await
            .unwrap();

        let all = list_lead_messages(&db, 4).await.unwrap();
        assert_eq!(
            all.iter().find(|m| m.id == streaming.id).unwrap().status,
            "interrupted"
        );
        // A queued user message orphaned by the crash (no live FIFO to deliver it)
        // is closed as interrupted too, so it doesn't show as pending forever.
        assert_eq!(
            all.iter().find(|m| m.id == queued.id).unwrap().status,
            "interrupted"
        );
    }

    #[tokio::test]
    async fn reset_stale_running_sessions_idles_legacy_rows() {
        let db = mem().await;
        // Pre-fix rows: status was a write-once high-water-mark, so an idle worker
        // reads "running" (or "starting" before it ever attached).
        let running = create_session(&db, 1, 1, "codex", "/tmp/a").await.unwrap();
        set_session_status(&db, running.id, "running")
            .await
            .unwrap();
        let starting = create_session(&db, 2, 1, "codex", "/tmp/b").await.unwrap();
        set_session_status(&db, starting.id, "starting")
            .await
            .unwrap();
        let idle = create_session(&db, 3, 1, "codex", "/tmp/c").await.unwrap();
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
        insert_lead_message(
            &db,
            2,
            None,
            2,
            "user",
            "text",
            r#"{"text":"later"}"#,
            "queued",
        )
        .await
        .unwrap();
        let updated = complete_queued(&db, 2, None).await.unwrap().unwrap();
        assert_eq!(updated.status, "complete");
        let all = list_lead_messages(&db, 2).await.unwrap();
        assert_eq!(all[0].status, "complete");
    }

    #[tokio::test]
    async fn queued_status_updates_are_session_scoped() {
        let db = mem().await;
        let lead = insert_lead_message(
            &db,
            7,
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
            7,
            Some(3),
            1,
            "user",
            "text",
            r#"{"text":"worker"}"#,
            "queued",
        )
        .await
        .unwrap();

        let completed = complete_queued(&db, 7, Some(3)).await.unwrap().unwrap();
        assert_eq!(completed.id, worker.id);
        let failed = set_queued_status(&db, 7, None, "interrupted")
            .await
            .unwrap();

        assert_eq!(
            failed.iter().map(|m| m.id).collect::<Vec<_>>(),
            vec![lead.id]
        );
        let all = list_lead_messages(&db, 7).await.unwrap();
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
    async fn lead_native_id_upserts() {
        let db = mem().await;
        assert!(lead_native_id(&db, 3).await.unwrap().is_none());
        set_lead_native_id(&db, 3, "abc").await.unwrap();
        set_lead_native_id(&db, 3, "def").await.unwrap();
        assert_eq!(
            lead_native_id(&db, 3).await.unwrap().as_deref(),
            Some("def")
        );
        // meta row stays single + out of turn numbering
        assert_eq!(list_lead_messages(&db, 3).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn session_status_round_trips() {
        let db = mem().await;
        let s = create_session(&db, 1, 1, "codex", "/tmp/wt").await.unwrap();
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
        set_lead_native_id(&db, 7, "nat-xyz").await.unwrap();
        set_lead_status(&db, 7, "running").await.unwrap();
        assert_eq!(
            lead_status(&db, 7).await.unwrap().as_deref(),
            Some("running")
        );
        assert_eq!(
            lead_native_id(&db, 7).await.unwrap().as_deref(),
            Some("nat-xyz")
        );
        // opposite write order must also coexist (status first, native id second)
        set_lead_status(&db, 8, "idle").await.unwrap();
        set_lead_native_id(&db, 8, "nat-8").await.unwrap();
        assert_eq!(lead_status(&db, 8).await.unwrap().as_deref(), Some("idle"));
        assert_eq!(
            lead_native_id(&db, 8).await.unwrap().as_deref(),
            Some("nat-8")
        );
    }

    #[tokio::test]
    async fn im_route_bind_and_lookup() {
        let db = mem().await;
        let r = bind_im_route(&db, 7, "feishu", "oc_chat", "th_1")
            .await
            .unwrap();
        assert_eq!(r.thread_id, 7);
        // forward lookup by thread_id
        let got = im_route_of_thread(&db, 7).await.unwrap().unwrap();
        assert_eq!(got.im_thread_ref, "th_1");
        // reverse lookup by (channel, chat_id, im_thread_ref)
        let got = im_route_of_thread_ref(&db, "feishu", "oc_chat", "th_1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(got.thread_id, 7);
        // re-bind same issue: row count stays 1, target replaced
        bind_im_route(&db, 7, "feishu", "oc_chat", "th_2")
            .await
            .unwrap();
        assert_eq!(list_im_routes(&db).await.unwrap().len(), 1);
        assert!(im_route_of_thread_ref(&db, "feishu", "oc_chat", "th_1")
            .await
            .unwrap()
            .is_none());
        // unbind
        unbind_im_route(&db, 7).await.unwrap();
        assert!(im_route_of_thread(&db, 7).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn im_route_thread_ref_is_unique_across_issues() {
        // Same (channel, chat_id, im_thread_ref) cannot bind to two different issues.
        let db = mem().await;
        bind_im_route(&db, 1, "feishu", "oc_chat", "th_1")
            .await
            .unwrap();
        let err = bind_im_route(&db, 2, "feishu", "oc_chat", "th_1").await;
        assert!(err.is_err(), "second bind should violate unique index");
    }

    #[tokio::test]
    async fn create_and_cascade_delete() {
        let db = mem().await;
        let ws = create_workspace(&db, "Demo WS").await.unwrap();
        assert_eq!(ws.slug, "demo-ws");
        let repo = add_repo_ref(&db, ws.id, "web-app", "/tmp/x", "main", "")
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
        )
        .await
        .unwrap();
        assert_eq!(dir.branch, "feature/add-login");
        assert_eq!(dir.repo_id, repo.id);
        assert_eq!(dir.reason, "build the feature");

        // pretend it was materialized
        record_worktree(&db, repo.id, dir.id, &dir.branch, "/tmp/wt")
            .await
            .unwrap();
        assert_eq!(list_worktrees(&db, Some(dir.id)).await.unwrap().len(), 1);
        assert!(direction_repo_of(&db, dir.id).await.unwrap().is_some());

        // cascade delete returns the path to clean and empties the rows
        let removed = delete_thread_cascade(&db, t.id).await.unwrap();
        assert_eq!(
            removed,
            vec![(
                repo.id,
                "/tmp/wt".to_string(),
                "feature/add-login".to_string()
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
    async fn latest_session_for_returns_newest_with_native() {
        let db = mem().await;
        let ws = create_workspace(&db, "Demo WS").await.unwrap();
        let repo = add_repo_ref(&db, ws.id, "web-app", "/tmp/x", "main", "")
            .await
            .unwrap();
        let thread = create_thread(&db, ws.id, "T", "feature", "claude")
            .await
            .unwrap();
        let dir = create_direction(&db, thread.id, "D", "claude", repo.id, "r", "impl-only")
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
        let repo = add_repo_ref(&db, ws.id, "r", "/tmp/x", "main", "").await.unwrap();
        // An existing claude lead + worker, created before any alias.
        let old_thread = create_thread(&db, ws.id, "old", "feature", "claude")
            .await
            .unwrap();
        let dir = create_direction(&db, old_thread.id, "d", "claude", repo.id, "why", "impl-only")
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
        let repo = add_repo_ref(&db, ws.id, "web-app", "/tmp/x", "main", "")
            .await
            .unwrap();
        let t = create_thread(&db, ws.id, "Add login", "feature", "claude")
            .await
            .unwrap();
        let d = create_direction(&db, t.id, "main", "claude", repo.id, "r", "plan+impl")
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

        let repo = add_repo_ref(&db, ws_a.id, "web-app", "/tmp/x", "main", "")
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

        let d1 = create_direction(&db, t1.id, "api", "claude", repo.id, "r", "plan+impl")
            .await
            .unwrap();
        let d2 = create_direction(&db, t1.id, "ui", "claude", repo.id, "r", "plan+impl")
            .await
            .unwrap();
        assert!(rename_direction(&db, d2.id, "api").await.is_err());
        // same direction name under a DIFFERENT thread is fine
        let d3 = create_direction(&db, t2.id, "main", "claude", repo.id, "r", "plan+impl")
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
}
