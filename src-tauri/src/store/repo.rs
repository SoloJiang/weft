//! All DB reads/writes go through here. Keeps SeaORM specifics out of commands.

use super::entities::{
    app_setting, direction, lead_message, plan, repo_profile, repo_ref, session, skill_enable,
    skill_source, thread, worktree, workspace,
};
use super::Db;
use crate::slug::unique_slug;
use anyhow::Result;
use sea_orm::{ActiveModelTrait, ColumnTrait, EntityTrait, QueryFilter, QueryOrder, Set, TryIntoModel};

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

/// The most-recently created workspace (highest id), if any. Used as the
/// default-workspace bootstrap target for first-run onboarding.
pub async fn latest_workspace(db: &Db) -> Result<Option<workspace::Model>> {
    Ok(workspace::Entity::find()
        .order_by_desc(workspace::Column::Id)
        .one(&db.0)
        .await?)
}

pub async fn add_skill_source(db: &Db, git_url: &str, git_ref: Option<&str>) -> Result<skill_source::Model> {
    let m = skill_source::ActiveModel {
        git_url: Set(git_url.to_string()),
        git_ref: Set(git_ref.unwrap_or("").to_string()),
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

pub async fn set_skill_source_status(db: &Db, id: i32, status: &str, synced: Option<&str>) -> Result<()> {
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

pub async fn set_skill_enable(db: &Db, source_id: i32, skill_name: &str, scope: &str, on: bool) -> Result<()> {
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

pub async fn add_repo_ref(
    db: &Db,
    workspace_id: i32,
    name: &str,
    local_git_path: &str,
    base_ref: &str,
) -> Result<repo_ref::Model> {
    let existing: Vec<String> = repo_ref::Entity::find()
        .filter(repo_ref::Column::WorkspaceId.eq(workspace_id))
        .all(&db.0)
        .await?
        .into_iter()
        .map(|r| r.slug)
        .collect();
    let m = repo_ref::ActiveModel {
        workspace_id: Set(workspace_id),
        name: Set(name.to_string()),
        slug: Set(unique_slug(name, &existing)),
        local_git_path: Set(local_git_path.to_string()),
        base_ref: Set(base_ref.to_string()),
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
        None => repo_profile::ActiveModel {
            repo_id: Set(repo_id),
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
    let ws = workspace::Entity::find_by_id(t.workspace_id)
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
    let slug = unique_slug(name, &existing);
    let branch = format!("ws/{}/{}/{}", ws.slug, t.slug, slug);
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
    if m == "impl-only" { "impl-only" } else { "plan+impl" }
}

pub async fn get_direction(db: &Db, direction_id: i32) -> Result<Option<direction::Model>> {
    Ok(direction::Entity::find_by_id(direction_id).one(&db.0).await?)
}

/// Set a direction's lifecycle status (agent- or human-driven). No-op if gone.
pub async fn set_direction_status(db: &Db, direction_id: i32, status: &str) -> Result<()> {
    if let Some(d) = direction::Entity::find_by_id(direction_id).one(&db.0).await? {
        let mut a: direction::ActiveModel = d.into();
        a.status = Set(status.to_string());
        a.update(&db.0).await?;
    }
    Ok(())
}

/// The single write repo bound to a direction (scope rework). None if the
/// direction has no repo set (repo_id = 0) or the repo row is gone.
pub async fn direction_repo_of(db: &Db, direction_id: i32) -> Result<Option<repo_ref::Model>> {
    let Some(d) = direction::Entity::find_by_id(direction_id).one(&db.0).await? else {
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
        a.status = Set("running".to_string());
        a.update(&db.0).await?;
    }
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

/// Flip the OLDEST queued user message to complete — called when the engine
/// flushes the front of its FIFO into the process; queue order equals row
/// insertion order, so position (not content) is the identity. `_text` kept
/// for telemetry/debug call sites.
pub async fn complete_queued(db: &Db, thread_id: i32, _text: &str) -> Result<()> {
    if let Some(m) = lead_message::Entity::find()
        .filter(lead_message::Column::ThreadId.eq(thread_id))
        .filter(lead_message::Column::Status.eq("queued"))
        .order_by_asc(lead_message::Column::Id)
        .one(&db.0)
        .await?
    {
        let mut a: lead_message::ActiveModel = m.into();
        a.status = Set("complete".to_string());
        a.update(&db.0).await?;
    }
    Ok(())
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
    let content = serde_json::json!({ "native_id": native_id }).to_string();
    let existing = lead_message::Entity::find()
        .filter(lead_message::Column::ThreadId.eq(thread_id))
        .filter(lead_message::Column::Kind.eq("meta"))
        .one(&db.0)
        .await?;
    match existing {
        Some(m) => {
            let mut a: lead_message::ActiveModel = m.into();
            a.content = Set(content);
            a.update(&db.0).await?;
        }
        None => {
            insert_lead_message(db, thread_id, None, 0, "system", "meta", &content, "complete")
                .await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Db;

    async fn mem() -> Db {
        Db::connect("sqlite::memory:").await.unwrap()
    }

    #[tokio::test]
    async fn lead_message_roundtrip() {
        let db = mem().await;
        let m = insert_lead_message(&db, 1, None, 1, "user", "text", r#"{"text":"hi"}"#, "complete")
            .await
            .unwrap();
        assert_eq!(m.thread_id, 1);
        update_lead_message(&db, m.id, r#"{"text":"hi!"}"#, "complete").await.unwrap();
        let all = list_lead_messages(&db, 1).await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].content, r#"{"text":"hi!"}"#);
        assert_eq!(next_turn_id(&db, 1).await.unwrap(), 2);
    }

    #[tokio::test]
    async fn queued_flips_to_complete() {
        let db = mem().await;
        insert_lead_message(&db, 2, None, 2, "user", "text", r#"{"text":"later"}"#, "queued")
            .await
            .unwrap();
        complete_queued(&db, 2, "later").await.unwrap();
        let all = list_lead_messages(&db, 2).await.unwrap();
        assert_eq!(all[0].status, "complete");
    }

    #[tokio::test]
    async fn lead_native_id_upserts() {
        let db = mem().await;
        assert!(lead_native_id(&db, 3).await.unwrap().is_none());
        set_lead_native_id(&db, 3, "abc").await.unwrap();
        set_lead_native_id(&db, 3, "def").await.unwrap();
        assert_eq!(lead_native_id(&db, 3).await.unwrap().as_deref(), Some("def"));
        // meta row stays single + out of turn numbering
        assert_eq!(list_lead_messages(&db, 3).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn create_and_cascade_delete() {
        let db = mem().await;
        let ws = create_workspace(&db, "Demo WS").await.unwrap();
        assert_eq!(ws.slug, "demo-ws");
        let repo = add_repo_ref(&db, ws.id, "web-app", "/tmp/x", "main")
            .await
            .unwrap();
        let t = create_thread(&db, ws.id, "Add login", "feature", "claude")
            .await
            .unwrap();
        let dir = create_direction(&db, t.id, "main", "claude", repo.id, "build the feature", "plan+impl")
            .await
            .unwrap();
        assert_eq!(dir.branch, "ws/demo-ws/add-login/main");
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
        assert_eq!(removed, vec![(repo.id, "/tmp/wt".to_string(), "ws/demo-ws/add-login/main".to_string())]);
        assert_eq!(list_workspaces(&db).await.unwrap().len(), 1); // ws survives
        assert_eq!(list_threads(&db, ws.id).await.unwrap().len(), 0);
        assert_eq!(list_worktrees(&db, None).await.unwrap().len(), 0);
    }

    #[tokio::test]
    async fn latest_session_for_returns_newest_with_native() {
        let db = mem().await;
        let ws = create_workspace(&db, "Demo WS").await.unwrap();
        let repo = add_repo_ref(&db, ws.id, "web-app", "/tmp/x", "main")
            .await
            .unwrap();
        let thread = create_thread(&db, ws.id, "T", "feature", "claude").await.unwrap();
        let dir = create_direction(&db, thread.id, "D", "claude", repo.id, "r", "impl-only")
            .await
            .unwrap();
        // older session (no native), then newer (native captured)
        let _s1 = create_session(&db, dir.id, repo.id, "claude", "/tmp/x").await.unwrap();
        let s2 = create_session(&db, dir.id, repo.id, "claude", "/tmp/x").await.unwrap();
        set_session_native_id(&db, s2.id, "abc-123").await.unwrap();

        let latest = latest_session_for(&db, dir.id, repo.id).await.unwrap().unwrap();
        assert_eq!(latest.id, s2.id);
        assert_eq!(latest.native_session_id.as_deref(), Some("abc-123"));
        assert!(latest_session_for(&db, dir.id, 99999).await.unwrap().is_none());
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
            branch: Set("ws/demo-ws/add-login/main".to_string()),
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
    async fn skill_source_and_enable_roundtrip() {
        let db = mem().await;
        let s = add_skill_source(&db, "https://example.com/skills.git", None).await.unwrap();
        assert_eq!(s.git_url, "https://example.com/skills.git");
        assert_eq!(s.last_status, "never");
        // update status
        set_skill_source_status(&db, s.id, "ok", Some("123")).await.unwrap();
        let got = list_skill_sources(&db).await.unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].last_status, "ok");
        assert_eq!(got[0].last_synced, "123");
        // enable a skill globally, then list
        set_skill_enable(&db, s.id, "deploy", "global", true).await.unwrap();
        let en = list_skill_enable(&db).await.unwrap();
        assert_eq!(en.len(), 1);
        assert_eq!((en[0].skill_name.as_str(), en[0].scope.as_str()), ("deploy", "global"));
        // toggling off removes it
        set_skill_enable(&db, s.id, "deploy", "global", false).await.unwrap();
        assert!(list_skill_enable(&db).await.unwrap().is_empty());
        // remove source cascades its enables
        set_skill_enable(&db, s.id, "x", "ws:1", true).await.unwrap();
        remove_skill_source(&db, s.id).await.unwrap();
        assert!(list_skill_sources(&db).await.unwrap().is_empty());
        assert!(list_skill_enable(&db).await.unwrap().is_empty());
    }
}
