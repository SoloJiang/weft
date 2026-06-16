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
        .join(worktree_dirname(crate::paths::weft_home().ok().as_deref()))
}

/// The `.worktrees/<this>` subdir for the active weft home. Keyed on the resolved
/// HOME, not just the build profile, because `WEFT_HOME` can point a same-profile
/// build at a different data set: two homes must never share a worktree root, or
/// `gc::sweep_orphan_worktrees` — which tracks only the active DB's rows — would
/// prune the other home's live worktrees after the TTL. The two default homes keep
/// readable names (`weft` / `weft-dev`); a relocated `WEFT_HOME` gets a stable
/// hash suffix so its root is unique.
///
/// This isolates worktree *directories*. Branch *names* are repo-global, and
/// `choose_branch_name` dedupes against the repo's real git refs (shared across
/// homes), so once one home's branch ref exists the others avoid it. Known
/// limitation: if two homes reserve the same branch name before either
/// materializes its ref, the second `git worktree add` fails loudly (recoverable
/// by renaming) — the cross-DB form of the single-DB race `reserved` already guards.
fn worktree_dirname(home: Option<&Path>) -> String {
    // Canonicalize so the same physical home via a symlink or `..` resolves to one
    // root — an aliased release home must not get a second `weft-<hash>` root that
    // gc::sweep_orphan_worktrees would treat as a foreign home's.
    let home = home.map(crate::paths::canonical);
    let release = crate::paths::default_home(false).map(|p| crate::paths::canonical(&p));
    let dev = crate::paths::default_home(true).map(|p| crate::paths::canonical(&p));
    worktree_dirname_for(home.as_deref(), release.as_deref(), dev.as_deref())
}

fn worktree_dirname_for(
    home: Option<&Path>,
    release_default: Option<&Path>,
    dev_default: Option<&Path>,
) -> String {
    match home {
        Some(h) if release_default == Some(h) => "weft".to_string(),
        Some(h) if dev_default == Some(h) => "weft-dev".to_string(),
        Some(h) => format!("weft-{}", home_token(h)),
        // Home unresolvable (no $HOME): fall back to the release root.
        None => "weft".to_string(),
    }
}

/// Stable, dependency-free FNV-1a hex of the home path. Deterministic across runs
/// and platforms, so a relocated home's worktree root name never drifts (drift
/// would orphan its existing worktrees and let the GC reclaim them).
fn home_token(home: &Path) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in home.to_string_lossy().as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")
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

/// Reclaim one direction's worktree on its own: remove the working-copy directory
/// but KEEP the branch, the worktree row, and the direction. Used by the Done-card
/// "delete worktree" action — the user is freeing disk for a finished task, not
/// tearing the direction down. The row is deliberately retained as the record that
/// Weft created this branch here: it lets `delete_thread`'s cascade still clean the
/// branch later (zero-accumulation) and drives the `exists=false` state the board
/// uses to disable the now-defunct worktree's actions. Idempotent: a missing row,
/// or an already-removed directory, is a no-op.
pub async fn remove_direction_worktree(db: &Db, worktree_id: i32) -> Result<()> {
    use sea_orm::EntityTrait;
    let Some(wt) = entities::worktree::Entity::find_by_id(worktree_id)
        .one(&db.0)
        .await?
    else {
        return Ok(()); // already gone — idempotent
    };
    // Done-only: re-read the owning direction so a stale confirm dialog can't
    // reclaim the worktree of a task that was moved back to working/review (by a
    // human or the bus) after the dialog opened — that would delete an active
    // agent's working copy.
    let dir = entities::direction::Entity::find_by_id(wt.direction_id)
        .one(&db.0)
        .await?
        .context("direction not found")?;
    if dir.status != "done" {
        anyhow::bail!("worktree can only be deleted for a done task");
    }
    // Don't yank the cwd out from under a live agent: a human can mark a still-
    // running task done, but if its worker is mid-turn the persisted session
    // status is running/starting (set when a turn begins). Force-removing the
    // worktree then would discard in-flight work — refuse while a turn is active.
    if let Some(sess) = repo::latest_session_for(db, wt.direction_id, wt.repo_id).await? {
        if matches!(sess.status.as_str(), "running" | "starting") {
            anyhow::bail!("cannot delete the worktree while its worker is running");
        }
    }
    let path = std::path::Path::new(&wt.path);
    if let Some(r) = entities::repo_ref::Entity::find_by_id(wt.repo_id)
        .one(&db.0)
        .await?
    {
        let repo_path = std::path::Path::new(&r.local_git_path);
        // remove_worktree drops the working tree and prunes; it leaves the branch.
        if let Err(e) = git::remove_worktree(repo_path, path) {
            eprintln!("[weft] worktree remove failed for {}: {e}", wt.path);
        }
    }
    // Surface a failed removal instead of silently "succeeding": if the directory
    // survives (repo path missing, locked, …) the row's `exists` stays true, so the
    // card keeps offering a retry rather than showing a phantom reclaim.
    if path.exists() {
        anyhow::bail!("worktree directory could not be removed: {}", wt.path);
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
        // Idempotent re-materialize must resolve to the identical path. Hold the
        // env lock so a concurrent test can't change WEFT_HOME between the calls.
        let _g = crate::paths::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let repo = Path::new("/repo");
        let a = worktree_path(repo, "feat/d1");
        let b = worktree_path(repo, "feat/d1");
        assert_eq!(a, b);
    }

    #[test]
    fn worktree_path_is_repo_local_and_branch_shaped() {
        // The middle component is the active home's namespace (env-dependent), so
        // assert the repo-local shape rather than a fixed name.
        let p = worktree_path(Path::new("/repo"), "feat/add-login");
        assert!(p.starts_with("/repo/.worktrees"));
        assert!(p.ends_with("feat/add-login"));
    }

    #[test]
    fn worktree_dirname_keys_on_home() {
        let release = Path::new("/u/.weft");
        let dev = Path::new("/u/.weft-dev");
        let (r, d) = (Some(release), Some(dev));
        // Default homes keep readable names...
        assert_eq!(worktree_dirname_for(Some(release), r, d), "weft");
        assert_eq!(worktree_dirname_for(Some(dev), r, d), "weft-dev");
        // ...a relocated WEFT_HOME gets a distinct, stable suffix (never the
        // default roots), so two homes never share a worktree root.
        let reloc = worktree_dirname_for(Some(Path::new("/custom/home")), r, d);
        assert!(reloc.starts_with("weft-") && reloc != "weft-dev");
        assert_eq!(reloc, worktree_dirname_for(Some(Path::new("/custom/home")), r, d));
        assert_ne!(reloc, worktree_dirname_for(Some(Path::new("/custom/other")), r, d));
        // An unresolvable home falls back to the release root.
        assert_eq!(worktree_dirname_for(None, r, d), "weft");
    }
}
