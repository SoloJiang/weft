//! Turn a direction's single bound write-repo into a git worktree under that
//! repo's local `.worktrees/weft/` directory, and record it. Reads are unmanaged
//! (agents read real repos directly). Weft injection files stay untracked.

use crate::git;
use crate::store::{entities, repo, Db};
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// The deterministic worktree path for a direction branch inside its target repo:
/// `<repo>/.worktrees/<weft|weft-dev>/<branch>`. The branch suffix keeps the path
/// user-visible and aligned with the repo's own naming style while `.worktrees/weft`
/// keeps Weft checkouts separate from manually-created worktrees.
pub fn worktree_path(repo_path: &Path, branch: &str) -> PathBuf {
    worktree_root(repo_path).join(branch)
}

pub fn worktree_root(repo_path: &Path) -> PathBuf {
    repo_path
        .join(".worktrees")
        .join(worktree_dirname(cfg!(debug_assertions)))
}

/// The `.worktrees/<this>` subdir for the active build profile. Debug builds
/// (`tauri dev`) use `weft-dev` so dev checkouts never collide with — or get
/// orphaned by — the installed release app's `weft` worktrees in the same repo
/// (the two profiles run separate DBs that can't see each other's worktree rows).
/// Release keeps `weft` so existing checkouts are unaffected.
fn worktree_dirname(debug_build: bool) -> &'static str {
    if debug_build {
        "weft-dev"
    } else {
        "weft"
    }
}

/// Create the one worktree for `direction_id`'s bound repo at
/// `<repo>/.worktrees/weft/<branch>` on the direction's branch.
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
    let _thread = entities::thread::Entity::find_by_id(dir.thread_id)
        .one(&db.0)
        .await?
        .context("thread not found")?;

    let Some(repo_ref) = repo::direction_repo_of(db, direction_id).await? else {
        return Ok(Vec::new());
    };
    if let Some(existing) = repo::worktree_for(db, direction_id, repo_ref.id).await? {
        return Ok(vec![existing]);
    }
    let repo_path = std::path::Path::new(&repo_ref.local_git_path);
    let path = worktree_path(repo_path, &dir.branch);
    git::git_exclude(repo_path, ".worktrees/");
    git::add_worktree(repo_path, &dir.branch, &path, &repo_ref.base_ref)
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
    fn same_repo_in_two_threads_does_not_collide() {
        // §5 M2 acceptance: a repo opened by two threads must land on distinct
        // paths (and thus distinct branches) — the thread segment guarantees it.
        let repo = Path::new("/repo");
        let a = worktree_path(repo, "feat/thread-a");
        let b = worktree_path(repo, "feat/thread-b");
        assert_ne!(a, b);
    }

    #[test]
    fn two_directions_in_one_thread_do_not_collide() {
        let repo = Path::new("/repo");
        let a = worktree_path(repo, "feat/dir-a");
        let b = worktree_path(repo, "feat/dir-b");
        assert_ne!(a, b);
    }

    #[test]
    fn same_scope_is_deterministic() {
        // Idempotent re-materialize must resolve to the identical path.
        let repo = Path::new("/repo");
        let a = worktree_path(repo, "feat/d1");
        let b = worktree_path(repo, "feat/d1");
        assert_eq!(a, b);
    }

    #[test]
    fn worktree_path_is_repo_local_and_branch_shaped() {
        let p = worktree_path(Path::new("/repo"), "feat/add-login");
        // Profile-aware: `.worktrees/weft-dev` in debug/test runs, `.worktrees/weft`
        // in release.
        let expected = Path::new("/repo/.worktrees")
            .join(worktree_dirname(cfg!(debug_assertions)))
            .join("feat/add-login");
        assert_eq!(p, expected);
    }

    #[test]
    fn worktree_dirname_isolates_dev_from_prod() {
        // Dev checkouts get their own repo-local subdir so they never collide with
        // or orphan the installed app's worktrees; release keeps `weft`.
        assert_eq!(worktree_dirname(true), "weft-dev");
        assert_eq!(worktree_dirname(false), "weft");
    }
}
