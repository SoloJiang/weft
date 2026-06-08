//! All DB reads/writes go through here. Keeps SeaORM specifics out of commands.

use super::entities::{
    direction, direction_repo, plan, repo_profile, repo_ref, session, thread, worktree, workspace,
};
use super::Db;
use crate::slug::unique_slug;
use anyhow::Result;
use sea_orm::{ActiveModelTrait, ColumnTrait, EntityTrait, QueryFilter, Set, TryIntoModel};

fn now() -> String {
    // RFC3339 without pulling chrono: seconds since epoch is enough for ordering.
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{secs}")
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

pub async fn add_repo_ref(
    db: &Db,
    workspace_id: i32,
    name: &str,
    local_git_path: &str,
    base_ref: &str,
    default_tool: &str,
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
        default_tool: Set(default_tool.to_string()),
        ..Default::default()
    };
    Ok(m.insert(&db.0).await?)
}

pub async fn create_thread(
    db: &Db,
    workspace_id: i32,
    title: &str,
    kind: &str,
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
        status: Set("active".to_string()),
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

// NOTE: dormant — returns empty until direction_repo is removed in sub-plan 1d.
pub async fn list_direction_repos(
    db: &Db,
    direction_id: i32,
) -> Result<Vec<direction_repo::Model>> {
    Ok(direction_repo::Entity::find()
        .filter(direction_repo::Column::DirectionId.eq(direction_id))
        .all(&db.0)
        .await?)
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
        created_at: Set(now()),
        ..Default::default()
    }
    .insert(&db.0)
    .await?;
    Ok(dir)
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
        direction_repo::Entity::delete_many()
            .filter(direction_repo::Column::DirectionId.eq(d.id))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Db;

    async fn mem() -> Db {
        Db::connect("sqlite::memory:").await.unwrap()
    }

    #[tokio::test]
    async fn create_and_cascade_delete() {
        let db = mem().await;
        let ws = create_workspace(&db, "Demo WS").await.unwrap();
        assert_eq!(ws.slug, "demo-ws");
        let repo = add_repo_ref(&db, ws.id, "web-app", "/tmp/x", "main", "claude")
            .await
            .unwrap();
        let t = create_thread(&db, ws.id, "Add login", "feature")
            .await
            .unwrap();
        let dir = create_direction(&db, t.id, "main", "claude", repo.id, "build the feature")
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
    async fn direction_repo_of_none_when_unset() {
        let db = mem().await;
        let ws = create_workspace(&db, "Demo WS").await.unwrap();
        let t = create_thread(&db, ws.id, "Add login", "feature")
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
}
