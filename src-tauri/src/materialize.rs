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
    // Branch off the chosen base (or the repo's live default branch), syncing
    // origin first so the worktree starts from the latest remote state.
    let explicit = !dir.base_branch.trim().is_empty();
    let base = if explicit {
        dir.base_branch.trim().to_string()
    } else {
        // Authoritative live default (works for narrowed clones); cached fallback offline.
        git::live_default_branch(repo_path)
            .unwrap_or_else(|| git::default_base_branch(repo_path, &repo_ref.base_ref))
    };
    let add = git::add_worktree_synced(repo_path, &dir.branch, &path, &base, explicit)
        .with_context(|| format!("worktree for repo {}", repo_ref.name))?;
    if add.created_branch && !add.synced {
        eprintln!(
            "[weft] materialize {}: origin sync unavailable — branched off local {base}",
            dir.branch
        );
    }
    let finish = async {
        if !explicit {
            // Record the resolved default as the immutable branch-off base, so a later
            // re-approval compares against what we actually branched off, not a
            // re-resolved (possibly changed) default.
            repo::set_direction_base_branch(db, direction_id, &base).await?;
        }
        // Keep the diff "vs target" consistent with the branch we actually based off:
        // when the direction has no explicit target yet, pin it to the resolved base
        // (otherwise an empty target would resolve via a possibly-stale repo base_ref).
        if dir.target_branch.trim().is_empty() {
            repo::set_direction_target_branch(db, direction_id, &base).await?;
        }
        let rec = repo::record_worktree(
            db,
            repo_ref.id,
            direction_id,
            &dir.branch,
            &path.to_string_lossy(),
        )
        .await?;
        Ok::<entities::worktree::Model, anyhow::Error>(rec)
    }
    .await;
    match finish {
        Ok(rec) => Ok(vec![rec]),
        Err(err) => {
            // Remove the checkout we created (if any); delete the branch ONLY if we
            // created it — a pre-existing branch reused by the fallback must survive.
            if add.created_checkout {
                let _ = git::remove_worktree(repo_path, &path);
            }
            if add.created_branch {
                let _ = git::delete_branch(repo_path, &dir.branch);
            }
            Err(err)
        }
    }
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

/// Fully tear down a direction created during a confirm that then failed:
/// remove each of its worktrees (working copy + namespaced branch) and delete the
/// direction + worktree rows. Used to keep confirm atomic — a failed batch leaves
/// no partial worktrees/branches behind. Best-effort on the git side (a missing
/// path/branch is fine); the row delete is authoritative.
pub async fn rollback_direction(db: &Db, direction_id: i32) -> Result<()> {
    use sea_orm::EntityTrait;
    for w in repo::list_worktrees(db, Some(direction_id)).await? {
        if let Some(r) = entities::repo_ref::Entity::find_by_id(w.repo_id)
            .one(&db.0)
            .await?
        {
            let repo_path = std::path::Path::new(&r.local_git_path);
            if let Err(e) = git::remove_worktree(repo_path, std::path::Path::new(&w.path)) {
                eprintln!("[weft] rollback worktree remove failed for {}: {e}", w.path);
            }
            if let Err(e) = git::delete_branch(repo_path, &w.branch) {
                eprintln!("[weft] rollback branch delete failed for {}: {e}", w.branch);
            }
        }
    }
    repo::delete_direction(db, direction_id).await?;
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
    // Don't yank the cwd out from under a session that still owns it. A human can
    // mark a task done while its worker is mid-turn (running/starting) or while it
    // has been taken over in their own terminal (stopped — the takeover/runaway
    // state per lead_chat::engine, where a human may still be driving it).
    // Force-removing the worktree then would discard in-flight work or break the
    // live terminal session, so refuse.
    if let Some(sess) = repo::latest_session_for(db, wt.direction_id, wt.repo_id).await? {
        if matches!(sess.status.as_str(), "running" | "starting" | "stopped") {
            anyhow::bail!("cannot delete the worktree while its worker is active");
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

    #[tokio::test]
    async fn materialize_pins_empty_target_to_resolved_base() {
        use crate::store::repo;
        use std::process::Command as Cmd;
        let _env = crate::paths::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tag = format!("weft-mat-tgt-{}", std::process::id());
        let root = std::env::temp_dir().join(format!("{tag}-root"));
        let weft_home = std::env::temp_dir().join(format!("{tag}-home"));
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&weft_home);
        std::fs::create_dir_all(&root).unwrap();
        std::env::set_var("WEFT_HOME", weft_home.to_str().unwrap());

        let origin = root.join("origin");
        std::fs::create_dir_all(&origin).unwrap();
        let g = |a: &[&str]| { Cmd::new("git").args(a).current_dir(&origin).status().unwrap(); };
        g(&["init", "-q"]); g(&["config","user.email","t@t.t"]); g(&["config","user.name","t"]);
        std::fs::write(origin.join("README.md"), "# x\n").unwrap();
        g(&["add","-A"]); g(&["commit","-q","-m","init"]);
        let main = crate::git::current_branch(&origin).unwrap();

        let clone = root.join("clone");
        Cmd::new("git").args(["clone","-q",&origin.to_string_lossy(),&clone.to_string_lossy()])
            .current_dir(&root).status().unwrap();

        let db = crate::store::Db::connect("sqlite::memory:").await.unwrap();
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        // Register with a STALE base_ref (a feature branch that isn't the remote default).
        let r = repo::add_repo_ref(&db, ws.id, "api", clone.to_str().unwrap(), "stale-feature", "")
            .await.unwrap();
        let t = repo::create_thread(&db, ws.id, "t1", "feature", "claude").await.unwrap();
        // Empty base_branch → both base_branch and target_branch start empty.
        let dir = repo::create_direction(&db, t.id, "x", "claude", r.id, "r", "plan+impl", "")
            .await.unwrap();
        assert_eq!(dir.target_branch, "");

        materialize_direction(&db, dir.id).await.unwrap();
        let after = repo::get_direction(&db, dir.id).await.unwrap().unwrap();
        assert_eq!(after.target_branch, main,
            "empty target pinned to the resolved remote default, not the stale base_ref");

        let removed = repo::delete_thread_cascade(&db, t.id).await.unwrap();
        let _ = cleanup_worktrees(&db, &removed).await;
        std::env::remove_var("WEFT_HOME");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&weft_home);
    }

    #[tokio::test]
    async fn materialize_persists_resolved_base_for_empty_base() {
        use crate::store::repo;
        use std::process::Command as Cmd;
        let _env = crate::paths::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tag = format!("weft-mat-persistbase-{}", std::process::id());
        let root = std::env::temp_dir().join(format!("{tag}-root"));
        let weft_home = std::env::temp_dir().join(format!("{tag}-home"));
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&weft_home);
        std::fs::create_dir_all(&root).unwrap();
        std::env::set_var("WEFT_HOME", weft_home.to_str().unwrap());
        let origin = root.join("origin");
        std::fs::create_dir_all(&origin).unwrap();
        let g = |a: &[&str]| { Cmd::new("git").args(a).current_dir(&origin).status().unwrap(); };
        g(&["init","-q"]); g(&["config","user.email","t@t.t"]); g(&["config","user.name","t"]);
        std::fs::write(origin.join("README.md"), "# x\n").unwrap();
        g(&["add","-A"]); g(&["commit","-q","-m","init"]);
        let main = crate::git::current_branch(&origin).unwrap();
        let clone = root.join("clone");
        Cmd::new("git").args(["clone","-q",&origin.to_string_lossy(),&clone.to_string_lossy()]).current_dir(&root).status().unwrap();
        let db = crate::store::Db::connect("sqlite::memory:").await.unwrap();
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        let r = repo::add_repo_ref(&db, ws.id, "api", clone.to_str().unwrap(), &main, "").await.unwrap();
        let t = repo::create_thread(&db, ws.id, "t1", "feature", "claude").await.unwrap();
        let dir = repo::create_direction(&db, t.id, "x", "claude", r.id, "r", "plan+impl", "").await.unwrap();
        assert_eq!(dir.base_branch, "");
        materialize_direction(&db, dir.id).await.unwrap();
        let after = repo::get_direction(&db, dir.id).await.unwrap().unwrap();
        assert_eq!(after.base_branch, main, "empty base persisted to the resolved default");
        let removed = repo::delete_thread_cascade(&db, t.id).await.unwrap();
        let _ = cleanup_worktrees(&db, &removed).await;
        std::env::remove_var("WEFT_HOME");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&weft_home);
    }

    #[tokio::test]
    async fn materialize_branches_off_chosen_base_from_origin() {
        use crate::store::repo;
        use std::process::Command as Cmd;
        let _env = crate::paths::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tag = format!("weft-mat-base-{}", std::process::id());
        let root = std::env::temp_dir().join(format!("{tag}-root"));
        let weft_home = std::env::temp_dir().join(format!("{tag}-home"));
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&weft_home);
        std::fs::create_dir_all(&root).unwrap();
        std::env::set_var("WEFT_HOME", weft_home.to_str().unwrap());

        // origin: main + develop (develop one commit ahead).
        let origin = root.join("origin");
        std::fs::create_dir_all(&origin).unwrap();
        let g = |args: &[&str]| { Cmd::new("git").args(args).current_dir(&origin).status().unwrap(); };
        g(&["init", "-q"]);
        g(&["config", "user.email", "t@t.t"]);
        g(&["config", "user.name", "t"]);
        std::fs::write(origin.join("README.md"), "# x\n").unwrap();
        g(&["add", "-A"]);
        g(&["commit", "-q", "-m", "init"]);
        let main = crate::git::current_branch(&origin).unwrap();
        g(&["checkout", "-q", "-b", "develop"]);
        std::fs::write(origin.join("d.txt"), "d\n").unwrap();
        g(&["add", "-A"]);
        g(&["commit", "-q", "-m", "develop work"]);
        let dev_commit = Cmd::new("git").args(["rev-parse", "HEAD"]).current_dir(&origin)
            .output().unwrap();
        let dev_commit = String::from_utf8_lossy(&dev_commit.stdout).trim().to_string();
        g(&["checkout", "-q", &main]);

        // clone weft refs (a full clone; origin/develop is available to fetch).
        let clone = root.join("clone");
        Cmd::new("git").args(["clone", "-q", &origin.to_string_lossy(), &clone.to_string_lossy()])
            .current_dir(&root).status().unwrap();

        let db = crate::store::Db::connect("sqlite::memory:").await.unwrap();
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        let r = repo::add_repo_ref(&db, ws.id, "api", clone.to_str().unwrap(), &main, "")
            .await.unwrap();
        let t = repo::create_thread(&db, ws.id, "t1", "feature", "claude").await.unwrap();
        let dir = repo::create_direction(&db, t.id, "x", "claude", r.id, "r", "plan+impl", "develop")
            .await.unwrap();

        let wts = materialize_direction(&db, dir.id).await.unwrap();
        assert_eq!(wts.len(), 1);
        let wt_head = Cmd::new("git").args(["rev-parse", "HEAD"])
            .current_dir(&wts[0].path).output().unwrap();
        let wt_head = String::from_utf8_lossy(&wt_head.stdout).trim().to_string();
        assert_eq!(wt_head, dev_commit, "branched off the fresh origin/develop");

        let removed = repo::delete_thread_cascade(&db, t.id).await.unwrap();
        let _ = cleanup_worktrees(&db, &removed).await;
        std::env::remove_var("WEFT_HOME");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&weft_home);
    }
}
