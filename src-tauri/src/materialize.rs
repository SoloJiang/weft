//! Turn a direction's single bound write-repo into a git worktree under the
//! persistent worktree home, and record it. Reads are unmanaged (agents read
//! real repos directly). Nothing is written into the canonical repo (§2.1).

use crate::store::{entities, repo, Db};
use crate::{git, paths};
use anyhow::{Context, Result};
use std::path::PathBuf;

/// Create the one worktree for `direction_id`'s bound repo at
/// `<worktree_home>/<ws>/<thread>/<direction>/<repo>` on the direction's branch.
/// Idempotent: an existing worktree row/path is reused. Returns empty if the
/// direction has no repo bound (shouldn't happen for a confirmed write direction).
pub async fn materialize_direction(
    db: &Db,
    direction_id: i32,
) -> Result<Vec<entities::worktree::Model>> {
    use sea_orm::EntityTrait;
    let dir = entities::direction::Entity::find_by_id(direction_id)
        .one(&db.0)
        .await?
        .context("direction not found")?;
    let thread = entities::thread::Entity::find_by_id(dir.thread_id)
        .one(&db.0)
        .await?
        .context("thread not found")?;
    let ws = entities::workspace::Entity::find_by_id(thread.workspace_id)
        .one(&db.0)
        .await?
        .context("workspace not found")?;

    let Some(repo_ref) = repo::direction_repo_of(db, direction_id).await? else {
        return Ok(Vec::new());
    };
    if let Some(existing) = repo::worktree_for(db, direction_id, repo_ref.id).await? {
        return Ok(vec![existing]);
    }
    let home = paths::worktree_home()?;
    let path: PathBuf = home
        .join(&ws.slug)
        .join(&thread.slug)
        .join(&dir.slug)
        .join(&repo_ref.slug);
    git::add_worktree(
        std::path::Path::new(&repo_ref.local_git_path),
        &dir.branch,
        &path,
    )
    .with_context(|| format!("worktree for repo {}", repo_ref.name))?;
    let rec = repo::record_worktree(
        db,
        repo_ref.id,
        direction_id,
        &dir.branch,
        &path.to_string_lossy(),
    )
    .await?;
    Ok(vec![rec])
}

/// Physically remove worktrees and their namespaced branches (called during
/// cascade delete). `removed` is the (repo_id, path, branch) list returned by
/// `repo::delete_thread_cascade`. Per the zero-accumulation principle, the
/// branch is torn down too so deleted threads leave nothing in the canonical repo.
pub async fn cleanup_worktrees(db: &Db, removed: &[(i32, String, String)]) -> Result<()> {
    use sea_orm::EntityTrait;
    for (repo_id, path, branch) in removed {
        if let Some(r) = entities::repo_ref::Entity::find_by_id(*repo_id).one(&db.0).await? {
            let repo_path = std::path::Path::new(&r.local_git_path);
            if let Err(e) = git::remove_worktree(repo_path, std::path::Path::new(path)) {
                eprintln!("[weft] worktree remove failed for {path}: {e}");
            }
            if let Err(e) = git::delete_branch(repo_path, branch) {
                eprintln!("[weft] branch delete failed for {branch}: {e}");
            }
        }
    }
    Ok(())
}
