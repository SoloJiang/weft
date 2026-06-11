//! Turn a direction's single bound write-repo into a git worktree under the
//! persistent worktree home, and record it. Reads are unmanaged (agents read
//! real repos directly). Nothing is written into the canonical repo (§2.1).

use crate::store::{entities, repo, Db};
use crate::{git, paths};
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// The deterministic worktree path for a direction's bound repo, namespaced
/// `<home>/<ws>/<thread>/<direction>/<repo>`. Pure (no DB / FS) so the layout —
/// the core of "scope→物化映射" (§6) — is unit-testable. The thread + direction
/// segments are what keep the same repo, opened by two threads, from colliding
/// on one path/branch (§5 M2 acceptance, §7 known-issue).
pub fn worktree_path(
    home: &Path,
    ws_slug: &str,
    thread_slug: &str,
    dir_slug: &str,
    repo_slug: &str,
) -> PathBuf {
    home.join(ws_slug)
        .join(thread_slug)
        .join(dir_slug)
        .join(repo_slug)
}

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
    let path = worktree_path(&home, &ws.slug, &thread.slug, &dir.slug, &repo_ref.slug);
    git::add_worktree(
        std::path::Path::new(&repo_ref.local_git_path),
        &dir.branch,
        &path,
        &repo_ref.base_ref,
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
        if let Some(r) = entities::repo_ref::Entity::find_by_id(*repo_id)
            .one(&db.0)
            .await?
        {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worktree_path_is_namespaced_ws_thread_dir_repo() {
        let p = worktree_path(
            Path::new("/home/wt"),
            "acme",
            "checkout-promo",
            "api",
            "billing",
        );
        assert_eq!(p, Path::new("/home/wt/acme/checkout-promo/api/billing"));
    }

    #[test]
    fn same_repo_in_two_threads_does_not_collide() {
        // §5 M2 acceptance: a repo opened by two threads must land on distinct
        // paths (and thus distinct branches) — the thread segment guarantees it.
        let home = Path::new("/wt");
        let a = worktree_path(home, "acme", "thread-a", "d1", "billing");
        let b = worktree_path(home, "acme", "thread-b", "d1", "billing");
        assert_ne!(a, b);
    }

    #[test]
    fn two_directions_in_one_thread_do_not_collide() {
        let home = Path::new("/wt");
        let a = worktree_path(home, "acme", "t1", "dir-a", "billing");
        let b = worktree_path(home, "acme", "t1", "dir-b", "billing");
        assert_ne!(a, b);
    }

    #[test]
    fn same_scope_is_deterministic() {
        // Idempotent re-materialize must resolve to the identical path.
        let home = Path::new("/wt");
        let a = worktree_path(home, "acme", "t1", "d1", "billing");
        let b = worktree_path(home, "acme", "t1", "d1", "billing");
        assert_eq!(a, b);
    }
}
