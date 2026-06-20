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
        // Idempotent: the worktree dir still exists — nothing to do.
        if std::path::Path::new(&existing.path).exists() {
            return Ok(vec![existing]);
        }
        // The dir was reclaimed (remove_direction_worktree), but the row (and
        // usually the branch) survives. Recreate the on-disk worktree for the
        // stored branch so the lane's worker can resume. The branch typically
        // still exists, so add_worktree_synced takes the -b fallback and sets
        // created_branch=false — the branch is preserved on future cleanup.
        let repo_path = std::path::Path::new(&repo_ref.local_git_path);
        let wt_path = std::path::Path::new(&existing.path);
        // Recreate FROM the direction's stored branch-off base, NOT the work branch
        // itself. When the work branch still exists, add_worktree_synced's -b fallback
        // checks it out and the base is unused (work preserved). But if the work branch
        // no longer resolves (deleted externally), the base must be the recorded
        // branch-off point so the recreated branch starts there rather than off an
        // arbitrary fallback ref. Mirror the normal path's base resolution.
        let explicit = !dir.base_branch.trim().is_empty();
        let recreate_base = if explicit {
            dir.base_branch.trim().to_string()
        } else {
            git::live_default_branch(repo_path)
                .unwrap_or_else(|| git::recorded_base_or_default(repo_path, &repo_ref.base_ref))
        };
        // Require the stored base to resolve ONLY when we must branch off it — i.e. the
        // work branch is gone. If the work branch still exists we just check it out (the
        // base is unused), so a gone explicit base shouldn't block resuming the work. But
        // when the work branch is gone AND the base is explicit, a missing base must ERROR
        // rather than silently branching off an arbitrary fallback ref.
        let require = explicit && !git::ref_resolves(repo_path, &existing.branch);
        let add = git::add_worktree_synced(repo_path, &existing.branch, wt_path, &recreate_base, require)
            .with_context(|| {
                format!(
                    "re-materialize reclaimed worktree for branch {}",
                    existing.branch
                )
            })?;
        // If the recreate CREATED a fresh branch/checkout (the original was deleted),
        // weft now owns it — OR the new ownership into the row. Never CLEAR a flag: a
        // branch/checkout weft made on the first materialize stays owned even when this
        // pass only re-checked-out an existing one. Otherwise a branch/checkout weft
        // just created would escape cleanup (stale created_*=false).
        let cb = existing.created_branch || add.created_branch;
        let cc = existing.created_checkout || add.created_checkout;
        if cb != existing.created_branch || cc != existing.created_checkout {
            repo::set_worktree_ownership(db, existing.id, cb, cc).await?;
            if let Some(updated) = repo::worktree_for(db, direction_id, repo_ref.id).await? {
                return Ok(vec![updated]);
            }
        }
        return Ok(vec![existing]);
    }
    let repo_path = std::path::Path::new(&repo_ref.local_git_path);
    let path = worktree_path(repo_path, &dir.branch);
    git::git_exclude(repo_path, ".worktrees/");
    // Branch off the chosen base (or the repo's live default branch), syncing
    // origin first so the worktree starts from the latest remote state.
    let explicit = !dir.base_branch.trim().is_empty();
    // For the blank-base path, capture the live default separately so we can
    // detect when it differs from the recorded base_ref and persist the update.
    let live_default: Option<String> = if explicit {
        None
    } else {
        git::live_default_branch(repo_path)
    };
    let base = if explicit {
        dir.base_branch.trim().to_string()
    } else {
        // Live remote default (authoritative). Offline, prefer the recorded base_ref
        // (the live default captured at register) over a possibly-stale cached origin/HEAD.
        live_default
            .clone()
            .unwrap_or_else(|| git::recorded_base_or_default(repo_path, &repo_ref.base_ref))
    };
    let add = git::add_worktree_synced(repo_path, &dir.branch, &path, &base, explicit)
        .with_context(|| format!("worktree for repo {}", repo_ref.name))?;
    if add.created_branch && !add.synced {
        eprintln!(
            "[weft] materialize {}: origin sync unavailable — branched off local {base}",
            dir.branch
        );
    }
    // The ref we actually branched off: use add.branched_from when available
    // (the actual resolved ref, e.g. "origin/develop" → "develop"), otherwise
    // fall back to the requested base (for existing-branch and path-reuse paths
    // where no new branch was created from a known base).
    let recorded_base = if add.branched_from.is_empty() {
        base.clone()
    } else {
        add.branched_from.clone()
    };
    let finish = async {
        if !explicit {
            // Record the ACTUAL branched-off ref as the immutable branch-off base, so
            // a later re-approval compares against what we actually branched off, not a
            // re-resolved (possibly changed) default. Use recorded_base (which may differ
            // from the requested `base` when the fallback chain picked a different ref).
            repo::set_direction_base_branch(db, direction_id, &recorded_base).await?;
            // R17-5: if we learned a live default that differs from the recorded
            // base_ref, persist it so future offline fallbacks use the current value.
            // Only when live_default returned Some (never the offline fallback), and
            // only when it actually differs — best-effort (write hiccup must not fail
            // materialize, but we propagate DB errors from set_direction_base_branch
            // above; an error here would be equally unexpected, so we let it surface).
            if let Some(ref live) = live_default {
                if live != &repo_ref.base_ref {
                    // Best-effort: ignore errors so a transient write hiccup never
                    // fails the materialize call.
                    let _ = repo::set_repo_base_ref(db, repo_ref.id, live).await;
                }
            }
        }
        // Keep the diff "vs target" consistent with the branch we actually based off:
        // when the direction has no explicit target yet, pin it to the actual branched-from
        // ref (otherwise an empty target would resolve via a possibly-stale repo base_ref).
        if dir.target_branch.trim().is_empty() {
            repo::set_direction_target_branch(db, direction_id, &recorded_base).await?;
        }
        let rec = repo::record_worktree(
            db,
            repo_ref.id,
            direction_id,
            &dir.branch,
            &path.to_string_lossy(),
            add.created_branch,
            add.created_checkout,
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
/// cascade delete). `removed` is the (repo_id, path, branch, created_branch,
/// created_checkout) list returned by `repo::delete_thread_cascade`. Per the
/// zero-accumulation principle the worktree's namespaced branch is torn down
/// too — but ONLY when weft created it (`created_branch`); a pre-existing branch
/// the worktree merely checked out (the `-b` fallback) is preserved. Similarly,
/// `git worktree remove` is only called when `created_checkout` is true — a
/// reused pre-existing path must survive cascade cleanup.
pub async fn cleanup_worktrees(db: &Db, removed: &[(i32, String, String, bool, bool)]) -> Result<()> {
    use sea_orm::EntityTrait;
    for (repo_id, path, branch, created_branch, created_checkout) in removed {
        if let Some(r) = entities::repo_ref::Entity::find_by_id(*repo_id)
            .one(&db.0)
            .await?
        {
            let repo_path = std::path::Path::new(&r.local_git_path);
            if *created_checkout {
                if let Err(e) = git::remove_worktree(repo_path, std::path::Path::new(path)) {
                    eprintln!("[weft] worktree remove failed for {path}: {e}");
                }
            }
            if *created_branch {
                if let Err(e) = git::delete_branch(repo_path, branch) {
                    eprintln!("[weft] branch delete failed for {branch}: {e}");
                }
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
///
/// Gates on `created_checkout`: if the worktree reused a pre-existing path
/// (`created_checkout=false`) the directory is NOT removed — only Weft-created
/// checkouts are torn down. Similarly, the branch is only deleted when
/// `created_branch=true`.
pub async fn rollback_direction(db: &Db, direction_id: i32) -> Result<()> {
    use sea_orm::EntityTrait;
    for w in repo::list_worktrees(db, Some(direction_id)).await? {
        if let Some(r) = entities::repo_ref::Entity::find_by_id(w.repo_id)
            .one(&db.0)
            .await?
        {
            let repo_path = std::path::Path::new(&r.local_git_path);
            // Only remove the checkout if WE created it. A reused pre-existing path
            // must survive rollback.
            if w.created_checkout {
                if let Err(e) = git::remove_worktree(repo_path, std::path::Path::new(&w.path)) {
                    eprintln!("[weft] rollback worktree remove failed for {}: {e}", w.path);
                }
            }
            // Only delete the branch if WE created it. A pre-existing branch reused
            // by the fallback path must survive rollback (the user's own branch).
            if w.created_branch {
                if let Err(e) = git::delete_branch(repo_path, &w.branch) {
                    eprintln!("[weft] rollback branch delete failed for {}: {e}", w.branch);
                }
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

    /// R13-1: a worktree row with created_branch=false must survive rollback with its
    /// branch intact (the branch was pre-existing and must not be deleted).
    #[tokio::test]
    async fn cleanup_worktrees_preserves_preexisting_branch() {
        use crate::store::repo;
        use std::process::Command as Cmd;
        let _env = crate::paths::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tag = format!("weft-cleanup-preex-{}", std::process::id());
        let root = std::env::temp_dir().join(format!("{tag}-root"));
        let weft_home = std::env::temp_dir().join(format!("{tag}-home"));
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&weft_home);
        std::fs::create_dir_all(&root).unwrap();
        std::env::set_var("WEFT_HOME", weft_home.to_str().unwrap());

        let repo_path = root.join("repo");
        crate::git::init_repo(&repo_path).unwrap();
        let g = |args: &[&str]| {
            Cmd::new("git").args(args).current_dir(&repo_path).status().unwrap();
        };
        g(&["checkout", "-q", "-b", "feat/keep"]);
        g(&["commit", "-q", "--allow-empty", "-m", "keep"]);
        let main = crate::git::current_branch(&repo_path).unwrap();
        g(&["checkout", "-q", "-"]);

        let db = crate::store::Db::connect("sqlite::memory:").await.unwrap();
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        let r = repo::add_repo_ref(&db, ws.id, "repo", repo_path.to_str().unwrap(), &main, "")
            .await.unwrap();
        let t = repo::create_thread(&db, ws.id, "t", "feature", "claude").await.unwrap();
        let dir = repo::create_direction(&db, t.id, "d", "claude", r.id, "reason", "plan+impl", "")
            .await.unwrap();
        let wt_path = worktree_path(&repo_path, "feat/keep");
        std::fs::create_dir_all(&wt_path).unwrap();
        // created_branch=false → the branch is pre-existing (the -b fallback reused it).
        repo::record_worktree(&db, r.id, dir.id, "feat/keep", &wt_path.to_string_lossy(), false, true)
            .await.unwrap();

        // Deleting the thread removes the worktree but must PRESERVE the pre-existing branch.
        let removed = repo::delete_thread_cascade(&db, t.id).await.unwrap();
        cleanup_worktrees(&db, &removed).await.unwrap();
        let branch_check = Cmd::new("git")
            .args(["rev-parse", "--verify", "feat/keep"])
            .current_dir(&repo_path)
            .output().unwrap();
        assert!(branch_check.status.success(), "pre-existing branch must survive delete cleanup");

        std::env::remove_var("WEFT_HOME");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&weft_home);
    }

    /// R15-2: materialize_direction must recreate the on-disk worktree when the
    /// row exists but the directory was reclaimed (remove_direction_worktree path).
    /// After re-materialization the directory must exist again.
    #[tokio::test]
    async fn materialize_recreate_requires_explicit_base_only_when_work_branch_gone() {
        use crate::store::repo;
        use std::process::Command as Cmd;
        let _env = crate::paths::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tag = format!("weft-remat-req-{}", std::process::id());
        let root = std::env::temp_dir().join(format!("{tag}-root"));
        let weft_home = std::env::temp_dir().join(format!("{tag}-home"));
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&weft_home);
        std::fs::create_dir_all(&root).unwrap();
        std::env::set_var("WEFT_HOME", weft_home.to_str().unwrap());

        let repo_path = root.join("repo");
        crate::git::init_repo(&repo_path).unwrap();
        let main = crate::git::current_branch(&repo_path).unwrap();
        let g = |args: &[&str]| {
            Cmd::new("git").args(args).current_dir(&repo_path).status().unwrap();
        };
        g(&["branch", "develop"]);
        g(&["branch", "release"]);

        let db = crate::store::Db::connect("sqlite::memory:").await.unwrap();
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        let r = repo::add_repo_ref(&db, ws.id, "repo", repo_path.to_str().unwrap(), &main, "")
            .await.unwrap();
        let t = repo::create_thread(&db, ws.id, "t", "feature", "claude").await.unwrap();

        // Scenario A: work branch GONE + explicit base GONE → re-materialize ERRORS
        // (must not silently branch off an arbitrary fallback).
        let dir_a = repo::create_direction(&db, t.id, "a", "claude", r.id, "reason", "plan+impl", "develop")
            .await.unwrap();
        let wts_a = materialize_direction(&db, dir_a.id).await.unwrap();
        let wt_a = std::path::Path::new(&wts_a[0].path).to_path_buf();
        let branch_a = wts_a[0].branch.clone();
        let _ = crate::git::remove_worktree(&repo_path, &wt_a);
        let _ = std::fs::remove_dir_all(&wt_a);
        g(&["worktree", "prune"]);
        g(&["branch", "-D", &branch_a]);
        g(&["branch", "-D", "develop"]);
        assert!(
            materialize_direction(&db, dir_a.id).await.is_err(),
            "explicit base + work branch BOTH gone must error, not fall back to an arbitrary base"
        );

        // Scenario B: work branch EXISTS + explicit base GONE → re-materialize SUCCEEDS
        // (we check out the surviving work branch; the base is unused).
        let dir_b = repo::create_direction(&db, t.id, "b", "claude", r.id, "reason", "plan+impl", "release")
            .await.unwrap();
        let wts_b = materialize_direction(&db, dir_b.id).await.unwrap();
        let wt_b = std::path::Path::new(&wts_b[0].path).to_path_buf();
        let _ = crate::git::remove_worktree(&repo_path, &wt_b);
        let _ = std::fs::remove_dir_all(&wt_b);
        g(&["worktree", "prune"]);
        g(&["branch", "-D", "release"]); // base gone, but the work branch survives
        assert!(
            materialize_direction(&db, dir_b.id).await.is_ok(),
            "a surviving work branch must check out even when the explicit base is gone"
        );
        assert!(wt_b.exists(), "worktree recreated by checking out the surviving work branch");

        std::env::remove_var("WEFT_HOME");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&weft_home);
    }

    #[tokio::test]
    async fn materialize_recreate_updates_ownership_flags_when_branch_created() {
        use crate::store::repo;
        let _env = crate::paths::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tag = format!("weft-remat-own-{}", std::process::id());
        let root = std::env::temp_dir().join(format!("{tag}-root"));
        let weft_home = std::env::temp_dir().join(format!("{tag}-home"));
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&weft_home);
        std::fs::create_dir_all(&root).unwrap();
        std::env::set_var("WEFT_HOME", weft_home.to_str().unwrap());

        let repo_path = root.join("repo");
        crate::git::init_repo(&repo_path).unwrap();
        let main = crate::git::current_branch(&repo_path).unwrap();

        let db = crate::store::Db::connect("sqlite::memory:").await.unwrap();
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        let r = repo::add_repo_ref(&db, ws.id, "repo", repo_path.to_str().unwrap(), &main, "")
            .await.unwrap();
        let t = repo::create_thread(&db, ws.id, "t", "feature", "claude").await.unwrap();
        let dir = repo::create_direction(&db, t.id, "d", "claude", r.id, "reason", "plan+impl", &main)
            .await.unwrap();

        // A reclaimed row recorded as NOT weft-owned (created_branch=false,
        // created_checkout=false), for a branch + path that do not exist on disk →
        // re-materialize must CREATE both and FLIP the ownership flags to true.
        let branch = "feat/recreate-owns";
        let wt_path = worktree_path(&repo_path, branch);
        repo::record_worktree(&db, r.id, dir.id, branch, &wt_path.to_string_lossy(), false, false)
            .await.unwrap();
        assert!(!wt_path.exists(), "precondition: dir absent");

        let wts = materialize_direction(&db, dir.id).await.unwrap();
        assert_eq!(wts.len(), 1);
        assert!(wt_path.exists(), "worktree recreated");
        assert!(wts[0].created_branch, "created_branch must flip true after weft created the branch");
        assert!(wts[0].created_checkout, "created_checkout must flip true after weft created the checkout");
        let row = repo::worktree_for(&db, dir.id, r.id).await.unwrap().unwrap();
        assert!(row.created_branch && row.created_checkout, "ownership persisted on the row");

        std::env::remove_var("WEFT_HOME");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&weft_home);
    }

    #[tokio::test]
    async fn materialize_direction_recreates_off_stored_base_when_work_branch_gone() {
        use crate::store::repo;
        use std::process::Command as Cmd;
        let _env = crate::paths::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tag = format!("weft-remat-gone-{}", std::process::id());
        let root = std::env::temp_dir().join(format!("{tag}-root"));
        let weft_home = std::env::temp_dir().join(format!("{tag}-home"));
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&weft_home);
        std::fs::create_dir_all(&root).unwrap();
        std::env::set_var("WEFT_HOME", weft_home.to_str().unwrap());

        let repo_path = root.join("repo");
        crate::git::init_repo(&repo_path).unwrap();
        let main = crate::git::current_branch(&repo_path).unwrap();
        let g = |args: &[&str]| {
            Cmd::new("git").args(args).current_dir(&repo_path).status().unwrap();
        };
        let sha = |rev: &str| -> String {
            String::from_utf8(
                Cmd::new("git").args(["rev-parse", rev]).current_dir(&repo_path).output().unwrap().stdout,
            ).unwrap().trim().to_string()
        };
        // A distinct base branch "release", one commit AHEAD of main (so a fallback to
        // main/HEAD would yield a DIFFERENT commit than the stored base).
        g(&["checkout", "-q", "-b", "release"]);
        g(&["commit", "-q", "--allow-empty", "-m", "release work"]);
        let release_sha = sha("release");
        g(&["checkout", "-q", &main]);
        assert_ne!(release_sha, sha(&main), "release must be ahead of main");

        let db = crate::store::Db::connect("sqlite::memory:").await.unwrap();
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        let r = repo::add_repo_ref(&db, ws.id, "repo", repo_path.to_str().unwrap(), &main, "")
            .await.unwrap();
        let t = repo::create_thread(&db, ws.id, "t", "feature", "claude").await.unwrap();
        // EXPLICIT base_branch = release.
        let dir = repo::create_direction(&db, t.id, "d", "claude", r.id, "reason", "plan+impl", "release")
            .await.unwrap();

        // ① materialize off release, then reclaim the dir, then DELETE the work branch.
        let wts = materialize_direction(&db, dir.id).await.unwrap();
        let wt_path = std::path::Path::new(&wts[0].path).to_path_buf();
        let work_branch = wts[0].branch.clone();
        assert!(wt_path.exists());
        let _ = crate::git::remove_worktree(&repo_path, &wt_path);
        let _ = std::fs::remove_dir_all(&wt_path);
        g(&["worktree", "prune"]);
        g(&["branch", "-D", &work_branch]);
        assert!(
            !Cmd::new("git").args(["rev-parse", "--verify", "--quiet", &work_branch])
                .current_dir(&repo_path).status().unwrap().success(),
            "precondition: work branch deleted"
        );

        // ② re-materialize: the gone work branch must be recreated off the STORED base
        // (release), NOT an arbitrary fallback (main/HEAD).
        let wts2 = materialize_direction(&db, dir.id).await.unwrap();
        assert_eq!(wts2.len(), 1);
        assert!(wt_path.exists(), "worktree dir recreated");
        assert_eq!(
            sha(&work_branch), release_sha,
            "recreated work branch must branch off the stored base (release), not a fallback"
        );

        std::env::remove_var("WEFT_HOME");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&weft_home);
    }

    #[tokio::test]
    async fn materialize_direction_recreates_reclaimed_worktree() {
        use crate::store::repo;
        let _env = crate::paths::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tag = format!("weft-remat-reclaim-{}", std::process::id());
        let root = std::env::temp_dir().join(format!("{tag}-root"));
        let weft_home = std::env::temp_dir().join(format!("{tag}-home"));
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&weft_home);
        std::fs::create_dir_all(&root).unwrap();
        std::env::set_var("WEFT_HOME", weft_home.to_str().unwrap());

        // Stand up a minimal git repo (no remote needed; branch already exists).
        let repo_path = root.join("repo");
        crate::git::init_repo(&repo_path).unwrap();
        let main = crate::git::current_branch(&repo_path).unwrap();

        let db = crate::store::Db::connect("sqlite::memory:").await.unwrap();
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        let r = repo::add_repo_ref(
            &db,
            ws.id,
            "repo",
            repo_path.to_str().unwrap(),
            &main,
            "",
        )
        .await
        .unwrap();
        let t = repo::create_thread(&db, ws.id, "t", "feature", "claude")
            .await
            .unwrap();
        let dir =
            repo::create_direction(&db, t.id, "d", "claude", r.id, "reason", "plan+impl", "")
                .await
                .unwrap();

        // ① First materialize: creates the worktree dir and row.
        let wts = materialize_direction(&db, dir.id).await.unwrap();
        assert_eq!(wts.len(), 1, "first materialize must succeed");
        let wt_path = std::path::Path::new(&wts[0].path);
        assert!(wt_path.exists(), "worktree dir must exist after first materialize");

        // ② Simulate reclaim: remove the on-disk directory (but keep the DB row).
        // Use git worktree remove so git's metadata is clean.
        let _ = crate::git::remove_worktree(&repo_path, wt_path);
        // Even if the git prune fails (the dir is already gone), make sure the dir is absent.
        let _ = std::fs::remove_dir_all(wt_path);
        assert!(!wt_path.exists(), "precondition: dir gone after simulated reclaim");

        // The row must still be there.
        let existing = repo::worktree_for(&db, dir.id, r.id).await.unwrap();
        assert!(existing.is_some(), "worktree row must survive reclaim");

        // ③ Re-materialize: must recreate the directory.
        let wts2 = materialize_direction(&db, dir.id).await.unwrap();
        assert_eq!(wts2.len(), 1, "re-materialize must return one worktree");
        assert!(
            std::path::Path::new(&wts2[0].path).exists(),
            "worktree dir must be recreated after re-materialize"
        );
        // Row count must not have grown (no duplicate insert).
        assert_eq!(
            repo::list_worktrees(&db, Some(dir.id)).await.unwrap().len(),
            1,
            "exactly one worktree row after re-materialize"
        );

        // Cleanup.
        let removed = repo::delete_thread_cascade(&db, t.id).await.unwrap();
        let _ = cleanup_worktrees(&db, &removed).await;
        std::env::remove_var("WEFT_HOME");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&weft_home);
    }

    #[tokio::test]
    async fn rollback_direction_preserves_preexisting_branch() {
        use crate::store::repo;
        use std::process::Command as Cmd;
        let _env = crate::paths::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tag = format!("weft-rollback-preex-{}", std::process::id());
        let root = std::env::temp_dir().join(format!("{tag}-root"));
        let weft_home = std::env::temp_dir().join(format!("{tag}-home"));
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&weft_home);
        std::fs::create_dir_all(&root).unwrap();
        std::env::set_var("WEFT_HOME", weft_home.to_str().unwrap());

        // Set up a bare git repo with an initial commit.
        let repo_path = root.join("repo");
        crate::git::init_repo(&repo_path).unwrap();
        let g = |args: &[&str]| {
            Cmd::new("git").args(args).current_dir(&repo_path).status().unwrap();
        };

        // Create feat/keep as a pre-existing branch.
        g(&["checkout", "-q", "-b", "feat/keep"]);
        g(&["commit", "-q", "--allow-empty", "-m", "keep"]);
        let main = crate::git::current_branch(&repo_path).unwrap();
        // Switch away so we can check out feat/keep in a worktree.
        g(&["checkout", "-q", "-"]);

        let db = crate::store::Db::connect("sqlite::memory:").await.unwrap();
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        let r = repo::add_repo_ref(&db, ws.id, "repo", repo_path.to_str().unwrap(), &main, "")
            .await.unwrap();
        let t = repo::create_thread(&db, ws.id, "t", "feature", "claude").await.unwrap();
        let dir = repo::create_direction(&db, t.id, "d", "claude", r.id, "reason", "plan+impl", "")
            .await.unwrap();

        // Simulate: worktree dir where feat/keep would be checked out.
        let wt_path = worktree_path(&repo_path, "feat/keep");
        std::fs::create_dir_all(&wt_path).unwrap();

        // Record a worktree row with created_branch=false (branch is pre-existing).
        repo::record_worktree(&db, r.id, dir.id, "feat/keep", &wt_path.to_string_lossy(), false, true)
            .await.unwrap();

        // Rollback must NOT delete feat/keep.
        rollback_direction(&db, dir.id).await.unwrap();

        // The branch must still exist.
        let branch_check = Cmd::new("git")
            .args(["rev-parse", "--verify", "feat/keep"])
            .current_dir(&repo_path)
            .output().unwrap();
        assert!(branch_check.status.success(), "pre-existing branch must survive rollback");

        // The direction and its worktree rows must be gone.
        assert!(repo::get_direction(&db, dir.id).await.unwrap().is_none(),
            "direction row must be deleted after rollback");

        std::env::remove_var("WEFT_HOME");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&weft_home);
    }

    /// R13-1 positive control: a worktree row with created_branch=true must have its
    /// branch deleted on rollback (the normal weft-created branch case).
    #[tokio::test]
    async fn rollback_direction_deletes_created_branch() {
        use crate::store::repo;
        use std::process::Command as Cmd;
        let _env = crate::paths::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tag = format!("weft-rollback-created-{}", std::process::id());
        let root = std::env::temp_dir().join(format!("{tag}-root"));
        let weft_home = std::env::temp_dir().join(format!("{tag}-home"));
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&weft_home);
        std::fs::create_dir_all(&root).unwrap();
        std::env::set_var("WEFT_HOME", weft_home.to_str().unwrap());

        let repo_path = root.join("repo");
        crate::git::init_repo(&repo_path).unwrap();
        let main = crate::git::current_branch(&repo_path).unwrap();

        // Create feat/weft-branch using add_worktree_synced so the branch actually exists.
        let wt_path = worktree_path(&repo_path, "feat/weft-branch");
        let add = crate::git::add_worktree_synced(&repo_path, "feat/weft-branch", &wt_path, &main, false).unwrap();
        assert!(add.created_branch, "precondition: branch was created");

        let db = crate::store::Db::connect("sqlite::memory:").await.unwrap();
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        let r = repo::add_repo_ref(&db, ws.id, "repo", repo_path.to_str().unwrap(), &main, "")
            .await.unwrap();
        let t = repo::create_thread(&db, ws.id, "t", "feature", "claude").await.unwrap();
        let dir = repo::create_direction(&db, t.id, "d", "claude", r.id, "reason", "plan+impl", "")
            .await.unwrap();

        // Record the worktree row with created_branch=true.
        repo::record_worktree(&db, r.id, dir.id, "feat/weft-branch", &wt_path.to_string_lossy(), true, true)
            .await.unwrap();

        // Rollback MUST delete the branch.
        rollback_direction(&db, dir.id).await.unwrap();

        let branch_check = Cmd::new("git")
            .args(["rev-parse", "--verify", "feat/weft-branch"])
            .current_dir(&repo_path)
            .output().unwrap();
        assert!(!branch_check.status.success(), "weft-created branch must be deleted on rollback");

        assert!(repo::get_direction(&db, dir.id).await.unwrap().is_none(),
            "direction row must be deleted after rollback");

        std::env::remove_var("WEFT_HOME");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&weft_home);
    }

    /// R17-5: blank-base materialize where live_default_branch resolves a default
    /// that DIFFERS from the registered base_ref → the repo's base_ref must be
    /// updated to the live default afterward (so future offline fallbacks are current).
    #[tokio::test]
    async fn materialize_persists_live_default_when_it_differs_from_registered_base_ref() {
        use crate::store::repo;
        use std::process::Command as Cmd;
        let _env = crate::paths::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tag = format!("weft-r17-5-{}", std::process::id());
        let root = std::env::temp_dir().join(format!("{tag}-root"));
        let weft_home = std::env::temp_dir().join(format!("{tag}-home"));
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&weft_home);
        std::fs::create_dir_all(&root).unwrap();
        std::env::set_var("WEFT_HOME", weft_home.to_str().unwrap());

        // Set up an origin with a commit so `git ls-remote --symref` returns a HEAD.
        let origin = root.join("origin");
        std::fs::create_dir_all(&origin).unwrap();
        let g = |a: &[&str]| { Cmd::new("git").args(a).current_dir(&origin).status().unwrap(); };
        g(&["init", "-q"]);
        g(&["config", "user.email", "t@t.t"]);
        g(&["config", "user.name", "t"]);
        std::fs::write(origin.join("README.md"), "# x\n").unwrap();
        g(&["add", "-A"]);
        g(&["commit", "-q", "-m", "init"]);
        // Discover the actual default branch name (could be "main" or "master").
        let live_default = crate::git::current_branch(&origin).unwrap();

        // Clone so the clone has an `origin` remote that live_default_branch can query.
        let clone = root.join("clone");
        Cmd::new("git")
            .args(["clone", "-q", &origin.to_string_lossy(), &clone.to_string_lossy()])
            .current_dir(&root)
            .status()
            .unwrap();

        let db = crate::store::Db::connect("sqlite::memory:").await.unwrap();
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        // Register the clone with a STALE base_ref that differs from the live default.
        let stale_base = "stale-old-default";
        let r = repo::add_repo_ref(&db, ws.id, "api", clone.to_str().unwrap(), stale_base, "")
            .await
            .unwrap();
        assert_eq!(r.base_ref, stale_base, "precondition: registered with stale base_ref");

        let t = repo::create_thread(&db, ws.id, "t1", "feature", "claude").await.unwrap();
        // Blank base → will call live_default_branch, which should return `live_default`.
        let dir = repo::create_direction(&db, t.id, "x", "claude", r.id, "r", "plan+impl", "")
            .await
            .unwrap();

        materialize_direction(&db, dir.id).await.unwrap();

        // The repo row's base_ref must now be the live default, not the stale value.
        let updated = repo::get_repo(&db, r.id).await.unwrap().unwrap();
        assert_eq!(
            updated.base_ref, live_default,
            "R17-5: repo base_ref must be updated to the live remote default (was stale)"
        );

        let removed = repo::delete_thread_cascade(&db, t.id).await.unwrap();
        let _ = cleanup_worktrees(&db, &removed).await;
        std::env::remove_var("WEFT_HOME");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&weft_home);
    }

    /// R18-2a: rollback_direction must NOT remove a checkout directory when
    /// `created_checkout=false` (the path was pre-existing; weft only reused it).
    #[tokio::test]
    async fn rollback_direction_preserves_reused_checkout() {
        use crate::store::repo;
        let _env = crate::paths::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tag = format!("weft-rollback-reused-co-{}", std::process::id());
        let root = std::env::temp_dir().join(format!("{tag}-root"));
        let weft_home = std::env::temp_dir().join(format!("{tag}-home"));
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&weft_home);
        std::fs::create_dir_all(&root).unwrap();
        std::env::set_var("WEFT_HOME", weft_home.to_str().unwrap());

        let repo_path = root.join("repo");
        crate::git::init_repo(&repo_path).unwrap();
        let main = crate::git::current_branch(&repo_path).unwrap();

        let db = crate::store::Db::connect("sqlite::memory:").await.unwrap();
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        let r = repo::add_repo_ref(&db, ws.id, "repo", repo_path.to_str().unwrap(), &main, "")
            .await.unwrap();
        let t = repo::create_thread(&db, ws.id, "t", "feature", "claude").await.unwrap();
        let dir = repo::create_direction(&db, t.id, "d", "claude", r.id, "reason", "plan+impl", "")
            .await.unwrap();

        // Simulate a pre-existing path that weft did not create.
        let wt_path = worktree_path(&repo_path, "feat/preexist-co");
        std::fs::create_dir_all(&wt_path).unwrap();
        assert!(wt_path.exists(), "precondition: dir exists before rollback");

        // Record with created_checkout=false (weft reused this path, did not create it).
        repo::record_worktree(&db, r.id, dir.id, "feat/preexist-co", &wt_path.to_string_lossy(), false, false)
            .await.unwrap();

        rollback_direction(&db, dir.id).await.unwrap();

        // The directory must SURVIVE — weft did not create it.
        assert!(wt_path.exists(), "R18-2: reused checkout dir must survive rollback");

        // The direction row must be gone.
        assert!(repo::get_direction(&db, dir.id).await.unwrap().is_none(),
            "direction row must be deleted after rollback");

        std::env::remove_var("WEFT_HOME");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&weft_home);
    }

    /// R18-2b: cleanup_worktrees must NOT remove a checkout directory when
    /// `created_checkout=false`. Mirrors the rollback test but via the cascade path.
    #[tokio::test]
    async fn cleanup_worktrees_preserves_reused_checkout() {
        use crate::store::repo;
        let _env = crate::paths::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tag = format!("weft-cleanup-reused-co-{}", std::process::id());
        let root = std::env::temp_dir().join(format!("{tag}-root"));
        let weft_home = std::env::temp_dir().join(format!("{tag}-home"));
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&weft_home);
        std::fs::create_dir_all(&root).unwrap();
        std::env::set_var("WEFT_HOME", weft_home.to_str().unwrap());

        let repo_path = root.join("repo");
        crate::git::init_repo(&repo_path).unwrap();
        let main = crate::git::current_branch(&repo_path).unwrap();

        let db = crate::store::Db::connect("sqlite::memory:").await.unwrap();
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        let r = repo::add_repo_ref(&db, ws.id, "repo", repo_path.to_str().unwrap(), &main, "")
            .await.unwrap();
        let t = repo::create_thread(&db, ws.id, "t", "feature", "claude").await.unwrap();
        let dir = repo::create_direction(&db, t.id, "d", "claude", r.id, "reason", "plan+impl", "")
            .await.unwrap();

        // A pre-existing directory that weft did not create.
        let wt_path = worktree_path(&repo_path, "feat/preexist-cascade");
        std::fs::create_dir_all(&wt_path).unwrap();
        assert!(wt_path.exists(), "precondition: dir exists before cleanup");

        // Record with created_checkout=false.
        repo::record_worktree(&db, r.id, dir.id, "feat/preexist-cascade", &wt_path.to_string_lossy(), false, false)
            .await.unwrap();

        // Thread cascade returns the 5-tuple; cleanup must not remove the dir.
        let removed = repo::delete_thread_cascade(&db, t.id).await.unwrap();
        cleanup_worktrees(&db, &removed).await.unwrap();

        assert!(wt_path.exists(), "R18-2: reused checkout dir must survive cascade cleanup");

        std::env::remove_var("WEFT_HOME");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&weft_home);
    }
}
