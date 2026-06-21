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
        let repo_path = std::path::Path::new(&repo_ref.local_git_path);
        let wt_path = std::path::Path::new(&existing.path);
        // Idempotent ONLY when the recorded path is a worktree REGISTERED to this repo on
        // the lane's branch — not merely that the path exists(). If it was replaced
        // out-of-band by a plain dir or a checkout for another repo/branch, trusting
        // `exists` would dispatch the worker into the wrong tree (the frontend dispatch
        // only filters on `exists`). When it is NOT a valid registered worktree, fall
        // through to the recreate path below, whose add_worktree_synced fast-path then
        // bails on the non-registered/mismatched dir — surfacing the problem instead of
        // dispatching into it.
        //
        // R38-3: registration alone is not enough. For an EXPLICIT base (`base_branch`
        // non-empty) that STILL RESOLVES, the registered work branch must also DESCEND from
        // it. A still-registered branch externally reset/recreated off a DIFFERENT base (e.g.
        // a `release` lane reset onto `main`) would otherwise be returned idempotently and
        // dispatched as release-based though it no longer descends from release. When it
        // doesn't descend, fall through to the recreate path, whose R37-2 guard then bails
        // with a clear error. Don't validate for a blank base or a base that no longer
        // resolves (a gone base must not block resuming a surviving branch).
        //
        // R39-3: resolve the explicit base through branch_descends_from_base (LOCAL `<base>`
        // OR `origin/<base>`), not a BARE LOCAL `<base>`. In a single-branch clone the base
        // exists only as `origin/<base>`, so a bare-local check would skip entirely and let a
        // reset-off-main branch slip through; a diverged local `<base>` would wrongly reject
        // an origin-based checkout. Some(false) = base resolves in some form but the branch
        // descends from NONE → mismatch; None (base gone) → don't block the surviving branch.
        let explicit_base = dir.base_branch.trim();
        let base_mismatch = !explicit_base.is_empty()
            && git::branch_descends_from_base(repo_path, &existing.branch, explicit_base) == Some(false);
        if git::is_registered_worktree(repo_path, wt_path, &existing.branch) && !base_mismatch {
            return Ok(vec![existing]);
        }
        // The dir was reclaimed (remove_direction_worktree) or replaced, but the row (and
        // usually the branch) survives. Recreate the on-disk worktree for the
        // stored branch so the lane's worker can resume. The branch typically
        // still exists, so add_worktree_synced takes the -b fallback and sets
        // created_branch=false — the branch is preserved on future cleanup.
        // Recreate FROM the direction's stored branch-off base, NOT the work branch
        // itself. When the work branch still exists, add_worktree_synced's -b fallback
        // checks it out and the base is unused (work preserved). But if the work branch
        // no longer resolves (deleted externally), the base must be the recorded
        // branch-off point so the recreated branch starts there rather than off an
        // arbitrary fallback ref. Mirror the normal path's base resolution.
        let explicit = !dir.base_branch.trim().is_empty();
        // A blank base paired with a stored target is the detached-HEAD fallback ONLY when
        // that target is the branch-off COMMIT — reuse it so the recreate starts from the
        // SAME point, not a re-resolved live default that may have moved. On an UPGRADED
        // direction `base_branch==""` may instead pair with a USER-EDITED diff target that
        // is a BRANCH NAME (`develop`, `origin/develop`, …); recreating off that would start
        // the task from the wrong point. The detached fallback stores a FULL commit oid
        // (head_commit_full), and a branch name is never a full oid — so discriminate on
        // that: reuse the target as the recreate base ONLY when it is a full commit oid that
        // still resolves; otherwise fall through to live/default re-resolution. Hash-agnostic
        // (SHA-1 40-hex OR SHA-256 64-hex) via git object identity, not a hard-coded length.
        let t = dir.target_branch.trim();
        let target_is_commit_sha = git::is_full_commit_oid(repo_path, t);
        let recreate_base = if explicit {
            dir.base_branch.trim().to_string()
        } else if target_is_commit_sha {
            t.to_string()
        } else {
            git::live_default_branch(repo_path).unwrap_or_else(|| {
                git::recorded_base_or_default(
                    repo_path,
                    &repo_ref.base_ref,
                    repo_ref.base_ref_is_default,
                )
            })
        };
        // Require the stored base to resolve ONLY when we must branch off it — i.e. the
        // work branch is gone. If the work branch still exists we just check it out (the
        // base is unused), so a gone explicit base shouldn't block resuming the work. But
        // when the work branch is gone AND the base is explicit, a missing base must ERROR
        // rather than silently branching off an arbitrary fallback ref.
        let require = explicit && !git::ref_resolves(repo_path, &existing.branch);
        // R37-2: when require=false (the work branch survives) the -b fallback checks it out
        // WITHOUT add_worktree_synced's explicit-base ancestry check. A surviving branch that
        // was externally reset/recreated off a DIFFERENT base (e.g. main) while this lane's
        // explicit base is `release` would then be recorded+dispatched as based on release
        // while it sits on main's line. Guard it here: for an EXPLICIT base, when the work
        // branch resolves AND the explicit base STILL RESOLVES, the branch must descend from
        // that base — else bail. Only validate when the base resolves: a GONE explicit base
        // must NOT block resuming a surviving branch (the prior round's behavior).
        //
        // R39-3: resolve the base through branch_descends_from_base (LOCAL `<base>` OR
        // `origin/<base>`) so a single-branch clone (origin-only base) is still checked and a
        // diverged local base doesn't wrongly reject an origin-based branch. Some(false) =
        // base resolves in some form but the branch descends from none; None (base gone) does
        // not block resuming the surviving branch.
        if explicit
            && git::ref_resolves(repo_path, &existing.branch)
            && git::branch_descends_from_base(repo_path, &existing.branch, &recreate_base) == Some(false)
        {
            anyhow::bail!(
                "work branch {:?} no longer descends from its explicit base {:?}; \
                 delete the sub-task to recreate it from {:?}",
                existing.branch, recreate_base, recreate_base
            );
        }
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
        live_default.clone().unwrap_or_else(|| {
            git::recorded_base_or_default(repo_path, &repo_ref.base_ref, repo_ref.base_ref_is_default)
        })
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
    let raw_recorded = if add.branched_from.is_empty() {
        base.clone()
    } else {
        add.branched_from.clone()
    };
    // A bare "HEAD" is the detached / no-default fallback. The BASE and the diff TARGET
    // diverge here:
    //   - base_branch stores EMPTY, because "HEAD" is not a usable named ref — reconcile
    //     would otherwise compare against a moving ref; empty means "the default".
    //   - target_branch stores the actual branch-off COMMIT (a stable SHA). An empty or
    //     "HEAD" target would later resolve to the worktree's OWN HEAD, making the diff
    //     merge-base(HEAD, HEAD) and HIDING all committed worker changes.
    let was_head = raw_recorded == "HEAD";
    let recorded_base = if was_head { String::new() } else { raw_recorded };
    let recorded_target = if was_head {
        // FULL sha (not the --short head_commit): this is PERSISTED as the diff target
        // and re-resolved later by worktree_diff_target/rematerialization, where a short
        // sha could become ambiguous in a large repo.
        git::head_commit_full(repo_path).unwrap_or_default()
    } else {
        recorded_base.clone()
    };
    // R39-1: add_worktree_synced's path-exists fast-path can't tell a user-reused path
    // from OUR OWN crash orphan (weft created the worktree+branch, then crashed between
    // `worktree add` and record_worktree), so it returns ownership=false. But `path` is
    // always under our managed, weft-reserved root, and we only reach here (the create
    // path) with no DB row — so an existing registered checkout here is ours. Adopt it
    // as weft-created, else reclaim/cascade treats it as user-owned and never cleans it.
    // Computed here (above `finish`) so BOTH the record call and the error arm — which
    // runs after the async block — see the adopted flags. Depends only on add/path/repo_path.
    let crash_orphan = !add.created_checkout && path.starts_with(worktree_root(repo_path));
    let owns_branch = add.created_branch || crash_orphan;
    let owns_checkout = add.created_checkout || crash_orphan;
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
                // Persist when the value changed OR the row isn't yet marked default (an
                // upgraded/legacy row whose base_ref already equals the live default but
                // whose base_ref_is_default was never set). set_repo_base_ref also sets the
                // marker, so this backfills it; without it a later offline materialize treats
                // this verified default as legacy and can fall through to main/master.
                if live != &repo_ref.base_ref || !repo_ref.base_ref_is_default {
                    // Best-effort: ignore errors so a transient write hiccup never
                    // fails the materialize call.
                    let _ = repo::set_repo_base_ref(db, repo_ref.id, live).await;
                }
            }
        }
        // Keep the diff "vs target" consistent with what we actually based off: when the
        // direction has no explicit target yet, pin it to the branched-from ref — or, for
        // the detached HEAD fallback, the branch-off COMMIT (see recorded_target above) so
        // the diff has a stable ref instead of resolving to the worktree's own HEAD.
        if dir.target_branch.trim().is_empty() {
            repo::set_direction_target_branch(db, direction_id, &recorded_target).await?;
        }
        let rec = repo::record_worktree(
            db,
            repo_ref.id,
            direction_id,
            &dir.branch,
            &path.to_string_lossy(),
            owns_branch,
            owns_checkout,
        )
        .await?;
        Ok::<entities::worktree::Model, anyhow::Error>(rec)
    }
    .await;
    match finish {
        Ok(rec) => Ok(vec![rec]),
        Err(err) => {
            // Remove the checkout we own (created OR adopted as a crash orphan); delete the
            // branch ONLY if we own it — a pre-existing branch reused by the fallback must
            // survive. Using the adopted flags so a failed adoption cleans up its own orphan.
            if owns_checkout {
                let _ = git::remove_worktree(repo_path, &path);
            }
            if owns_branch {
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
            } else {
                // A reused (non-weft) checkout: keep the directory + contents AND its
                // git-worktree registration (so it stays a usable worktree), but LOCK it
                // so the orphan-worktree GC — which reclaims registered, no-longer-DB-tracked
                // worktrees under weft's root — skips it after the TTL once this row is
                // dropped. The lock lives in the repo's git metadata, so it also survives a
                // later repo re-add (which would otherwise re-orphan the checkout).
                let _ = git::lock_worktree(repo_path, std::path::Path::new(path));
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
            } else {
                // Reused (non-weft) checkout: keep it as a usable worktree, but LOCK it so
                // the orphan GC skips it after the row is deleted below (see cleanup_worktrees).
                let _ = git::lock_worktree(repo_path, std::path::Path::new(&w.path));
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
    // Honor created_checkout: weft must not delete a worktree it did not create. A lane
    // that reused a pre-existing checkout (created_checkout=false, never recreated)
    // keeps it — consistent with rollback/cascade cleanup — so reclaim refuses rather
    // than removing a checkout that isn't weft's to delete.
    if !wt.created_checkout {
        anyhow::bail!(
            "cannot reclaim a worktree Weft did not create (reused pre-existing checkout): {}",
            wt.path
        );
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
        let r = repo::add_repo_ref(&db, ws.id, "api", clone.to_str().unwrap(), "stale-feature", "", true)
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
        let r = repo::add_repo_ref(&db, ws.id, "api", clone.to_str().unwrap(), &main, "", true).await.unwrap();
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
        let r = repo::add_repo_ref(&db, ws.id, "api", clone.to_str().unwrap(), &main, "", true)
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
        let r = repo::add_repo_ref(&db, ws.id, "repo", repo_path.to_str().unwrap(), &main, "", true)
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
    async fn materialize_recreates_detached_head_lane_from_stored_target_commit() {
        use crate::store::repo;
        use std::process::Command as Cmd;
        let _env = crate::paths::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tag = format!("weft-remat-head-target-{}", std::process::id());
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
        let sha = |rev: &str| {
            String::from_utf8(
                Cmd::new("git").args(["rev-parse", rev]).current_dir(&repo_path).output().unwrap().stdout,
            ).unwrap().trim().to_string()
        };
        // Detached, no named branch → blank base falls back to HEAD (commit X).
        g(&["checkout", "-q", "--detach"]);
        g(&["branch", "-D", &main]);
        let x = sha("HEAD");

        let db = crate::store::Db::connect("sqlite::memory:").await.unwrap();
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        let r = repo::add_repo_ref(&db, ws.id, "repo", repo_path.to_str().unwrap(), "main", "", true)
            .await.unwrap();
        let t = repo::create_thread(&db, ws.id, "t", "feature", "claude").await.unwrap();
        let dir = repo::create_direction(&db, t.id, "d", "claude", r.id, "reason", "plan+impl", "")
            .await.unwrap();

        // ① materialize off HEAD (X); reclaim the dir + delete the work branch.
        let wts = materialize_direction(&db, dir.id).await.unwrap();
        let wt_path = std::path::Path::new(&wts[0].path).to_path_buf();
        let work_branch = wts[0].branch.clone();
        let _ = crate::git::remove_worktree(&repo_path, &wt_path);
        let _ = std::fs::remove_dir_all(&wt_path);
        g(&["worktree", "prune"]);
        g(&["branch", "-D", &work_branch]);

        // ② Advance the detached HEAD to a NEW commit Y — re-resolving the "live default"
        // would now yield Y, not the original branch-off X.
        g(&["commit", "-q", "--allow-empty", "-m", "advance-to-Y"]);
        assert_ne!(x, sha("HEAD"), "HEAD advanced to Y");

        // ③ Re-materialize: must recreate off the STORED target (X), not the advanced HEAD.
        let wts2 = materialize_direction(&db, dir.id).await.unwrap();
        assert_eq!(wts2.len(), 1);
        assert!(wt_path.exists(), "worktree recreated");
        assert_eq!(
            sha(&work_branch), x,
            "recreated off the stored branch-off commit X, not the advanced live HEAD"
        );

        std::env::remove_var("WEFT_HOME");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&weft_home);
    }

    /// R33-5: a reclaimed lane with `base_branch==""` may, on an UPGRADED direction, pair
    /// with a USER-EDITED diff `target_branch` that is a BRANCH NAME (not the detached-HEAD
    /// SHA). The recreate must NOT branch off that user-edited branch verbatim — only a bare
    /// COMMIT target is reused; a branch-ref target is re-resolved to the live/default base.
    #[tokio::test]
    async fn materialize_recreate_ignores_branch_name_target_uses_default_base() {
        use crate::store::repo;
        use std::process::Command as Cmd;
        let _env = crate::paths::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tag = format!("weft-remat-branchtarget-{}", std::process::id());
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
        // A branch "develop" one commit AHEAD of main. If the recreate blindly used the
        // user-edited target "develop" as the base, the work branch would land on develop's
        // commit; the correct re-resolution to the default (main) lands on main's commit.
        g(&["checkout", "-q", "-b", "develop"]);
        g(&["commit", "-q", "--allow-empty", "-m", "develop work"]);
        let develop_sha = sha("develop");
        g(&["checkout", "-q", &main]);
        let main_sha = sha(&main);
        assert_ne!(develop_sha, main_sha, "develop must be ahead of main");

        let db = crate::store::Db::connect("sqlite::memory:").await.unwrap();
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        // repo base_ref = main (the default). No remote, so live_default is None and the
        // offline fallback resolves to local main.
        let r = repo::add_repo_ref(&db, ws.id, "repo", repo_path.to_str().unwrap(), &main, "", true)
            .await.unwrap();
        let t = repo::create_thread(&db, ws.id, "t", "feature", "claude").await.unwrap();
        // BLANK base → would normally re-resolve to the default at materialize.
        let dir = repo::create_direction(&db, t.id, "d", "claude", r.id, "reason", "plan+impl", "")
            .await.unwrap();

        // ① materialize (off the default main), then reclaim the dir + delete the work branch.
        let wts = materialize_direction(&db, dir.id).await.unwrap();
        let wt_path = std::path::Path::new(&wts[0].path).to_path_buf();
        let work_branch = wts[0].branch.clone();
        let _ = crate::git::remove_worktree(&repo_path, &wt_path);
        let _ = std::fs::remove_dir_all(&wt_path);
        g(&["worktree", "prune"]);
        g(&["branch", "-D", &work_branch]);

        // ② Simulate an UPGRADED, user-edited diff target: a BRANCH NAME ("develop"), with
        // base_branch still blank. This is NOT a detached-HEAD SHA.
        repo::set_direction_base_branch(&db, dir.id, "").await.unwrap();
        repo::set_direction_target_branch(&db, dir.id, "develop").await.unwrap();
        assert!(crate::git::ref_resolves(&repo_path, "refs/heads/develop"), "precondition: develop is a real branch");

        // ③ Re-materialize: the work branch is gone, so the base is used. It must NOT be the
        // user-edited branch "develop" verbatim — it re-resolves to the default (main).
        let wts2 = materialize_direction(&db, dir.id).await.unwrap();
        assert_eq!(wts2.len(), 1);
        assert!(wt_path.exists(), "worktree recreated");
        assert_eq!(
            sha(&work_branch), main_sha,
            "recreate must re-resolve a branch-name target to the default (main), not branch off 'develop' verbatim"
        );
        assert_ne!(
            sha(&work_branch), develop_sha,
            "a user-edited branch-name target must NOT be reused as the recreate base"
        );

        std::env::remove_var("WEFT_HOME");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&weft_home);
    }

    #[tokio::test]
    async fn materialize_detached_head_stores_empty_base_and_target_not_head() {
        use crate::store::repo;
        use sea_orm::EntityTrait;
        use std::process::Command as Cmd;
        let _env = crate::paths::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tag = format!("weft-remat-head-{}", std::process::id());
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
        // Detach HEAD and delete the only branch → no main/master/any named branch, so
        // the blank-base resolution falls all the way back to HEAD.
        g(&["checkout", "-q", "--detach"]);
        g(&["branch", "-D", &main]);
        // The commit HEAD points at — what the worktree branches off, and what the diff
        // target must be pinned to (a stable FULL sha, not the moving "HEAD" nor a --short
        // sha that could grow ambiguous later).
        let head_sha = String::from_utf8(
            Cmd::new("git").args(["rev-parse", "HEAD"]).current_dir(&repo_path).output().unwrap().stdout,
        ).unwrap().trim().to_string();
        assert_eq!(head_sha.len(), 40, "precondition: captured the FULL 40-char sha");

        let db = crate::store::Db::connect("sqlite::memory:").await.unwrap();
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        let r = repo::add_repo_ref(&db, ws.id, "repo", repo_path.to_str().unwrap(), "main", "", true)
            .await.unwrap();
        let t = repo::create_thread(&db, ws.id, "t", "feature", "claude").await.unwrap();
        // BLANK base → falls back to HEAD during materialize.
        let dir = repo::create_direction(&db, t.id, "d", "claude", r.id, "reason", "plan+impl", "")
            .await.unwrap();

        let wts = materialize_direction(&db, dir.id).await.unwrap();
        assert_eq!(wts.len(), 1, "materialize must still create the worktree");

        // The HEAD fallback stores an EMPTY base (reconcile default-equiv) but pins the
        // diff target to the branch-off COMMIT — an empty/"HEAD" target would resolve to
        // the worktree's own HEAD and hide committed work.
        let d2 = entities::direction::Entity::find_by_id(dir.id)
            .one(&db.0).await.unwrap().unwrap();
        assert_eq!(d2.base_branch, "", "HEAD fallback must store empty base, not 'HEAD'");
        assert_eq!(
            d2.target_branch, head_sha,
            "HEAD fallback must store the branch-off COMMIT as target, not empty/'HEAD'"
        );

        std::env::remove_var("WEFT_HOME");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&weft_home);
    }

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
        let r = repo::add_repo_ref(&db, ws.id, "repo", repo_path.to_str().unwrap(), &main, "", true)
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
        let r = repo::add_repo_ref(&db, ws.id, "repo", repo_path.to_str().unwrap(), &main, "", true)
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
        let r = repo::add_repo_ref(&db, ws.id, "repo", repo_path.to_str().unwrap(), &main, "", true)
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
            true,
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

    /// R37-1: the idempotent early-return must GIT-VALIDATE the recorded path, not trust
    /// `path.exists()` alone. If the recorded worktree dir was replaced out-of-band by a
    /// PLAIN directory (not a registered worktree of this repo on the branch), materialize
    /// must NOT return the stale row as-valid — it falls through to the recreate path, whose
    /// add_worktree_synced fast-path then bails on the non-registered dir, surfacing the
    /// problem rather than dispatching the worker into the wrong tree. A real registered
    /// worktree at the path still returns Ok (the normal idempotent case).
    #[tokio::test]
    async fn materialize_idempotent_rejects_stale_unregistered_path() {
        use crate::store::repo;
        use std::process::Command as Cmd;
        let _env = crate::paths::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tag = format!("weft-remat-staledir-{}", std::process::id());
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

        let db = crate::store::Db::connect("sqlite::memory:").await.unwrap();
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        let r = repo::add_repo_ref(&db, ws.id, "repo", repo_path.to_str().unwrap(), &main, "", true)
            .await.unwrap();
        let t = repo::create_thread(&db, ws.id, "t", "feature", "claude").await.unwrap();
        let dir = repo::create_direction(&db, t.id, "d", "claude", r.id, "reason", "plan+impl", "")
            .await.unwrap();

        // ① First materialize: a REAL registered worktree at the deterministic path.
        let wts = materialize_direction(&db, dir.id).await.unwrap();
        let wt_path = std::path::Path::new(&wts[0].path).to_path_buf();
        let work_branch = wts[0].branch.clone();
        assert!(wt_path.exists(), "precondition: worktree dir exists");

        // The NORMAL idempotent case: a real registered worktree still at the path → Ok,
        // returns the existing row without recreating.
        let again = materialize_direction(&db, dir.id).await.unwrap();
        assert_eq!(again.len(), 1, "a valid registered worktree returns the row idempotently");

        // ② Replace the dir OUT-OF-BAND with a PLAIN directory: drop the real worktree
        // (so git's registration is gone) then recreate a bare dir at the same path. The
        // path still exists(), but it is NOT a registered worktree of this repo on the branch.
        let _ = crate::git::remove_worktree(&repo_path, &wt_path);
        let _ = std::fs::remove_dir_all(&wt_path);
        g(&["branch", "-D", &work_branch]); // also drop the work branch so recreate can't reuse it
        std::fs::create_dir_all(&wt_path).unwrap();
        std::fs::write(wt_path.join("stuff.txt"), "not a worktree\n").unwrap();
        assert!(wt_path.exists(), "precondition: a plain dir now sits at the recorded path");
        assert!(
            !crate::git::is_registered_worktree(&repo_path, &wt_path, &work_branch),
            "precondition: the plain dir is NOT a registered worktree"
        );

        // ③ Re-materialize: must NOT return the stale row as-valid. The trusting early-return
        // is bypassed (path not git-validated), it falls to recreate, and add_worktree_synced
        // bails on the non-registered dir → an error, not a bad path handed to the worker.
        let res = materialize_direction(&db, dir.id).await;
        assert!(
            res.is_err(),
            "a stale plain dir at the recorded path must surface an error, not be returned as a valid worktree"
        );

        std::env::remove_var("WEFT_HOME");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&weft_home);
    }

    /// R37-2: when a reclaimed explicit-base lane's WORK BRANCH still exists, the recreate
    /// reuses it (require=false, the -b fallback bypasses the ancestry check). If that branch
    /// was externally reset/recreated off a DIFFERENT base (e.g. main) while the recorded
    /// `base_branch` is `release`, rematerialization must NOT record+dispatch a branch that no
    /// longer descends from its explicit base — it bails. (Only validated when the explicit
    /// base still resolves; a GONE base keeps the surviving-branch checkout working — see
    /// materialize_recreate_requires_explicit_base_only_when_work_branch_gone Scenario B.)
    #[tokio::test]
    async fn materialize_recreate_rejects_work_branch_reset_off_other_base() {
        use crate::store::repo;
        use std::process::Command as Cmd;
        let _env = crate::paths::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tag = format!("weft-remat-reset-{}", std::process::id());
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
        // release: one commit AHEAD of main (so main does NOT descend from release).
        g(&["checkout", "-q", "-b", "release"]);
        g(&["commit", "-q", "--allow-empty", "-m", "release work"]);
        let release_sha = sha("release");
        g(&["checkout", "-q", &main]);
        let main_sha = sha(&main);
        assert_ne!(release_sha, main_sha, "release must be ahead of main");

        let db = crate::store::Db::connect("sqlite::memory:").await.unwrap();
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        let r = repo::add_repo_ref(&db, ws.id, "repo", repo_path.to_str().unwrap(), &main, "", true)
            .await.unwrap();
        let t = repo::create_thread(&db, ws.id, "t", "feature", "claude").await.unwrap();

        // ---- Case A: work branch reset OFF main (not descending from release) → Err. ----
        let dir_a = repo::create_direction(&db, t.id, "a", "claude", r.id, "reason", "plan+impl", "release")
            .await.unwrap();
        let wts_a = materialize_direction(&db, dir_a.id).await.unwrap();
        let wt_a = std::path::Path::new(&wts_a[0].path).to_path_buf();
        let branch_a = wts_a[0].branch.clone();
        assert_eq!(sha(&branch_a), release_sha, "precondition: work branch forked off release");
        // Reclaim the dir but KEEP the work branch.
        let _ = crate::git::remove_worktree(&repo_path, &wt_a);
        let _ = std::fs::remove_dir_all(&wt_a);
        g(&["worktree", "prune"]);
        // Externally RESET the surviving work branch to main (a commit NOT descending from release).
        g(&["branch", "-f", &branch_a, &main]);
        assert_eq!(sha(&branch_a), main_sha, "precondition: work branch reset to main");
        assert!(crate::git::ref_resolves(&repo_path, "release"), "precondition: explicit base still resolves");
        // Re-materialize: must bail — the work branch no longer descends from its explicit base.
        assert!(
            materialize_direction(&db, dir_a.id).await.is_err(),
            "a surviving work branch reset off a different base than its explicit base must error"
        );

        // ---- Case B: work branch still descends from release → Ok. ----
        let dir_b = repo::create_direction(&db, t.id, "b", "claude", r.id, "reason", "plan+impl", "release")
            .await.unwrap();
        let wts_b = materialize_direction(&db, dir_b.id).await.unwrap();
        let wt_b = std::path::Path::new(&wts_b[0].path).to_path_buf();
        let branch_b = wts_b[0].branch.clone();
        // Reclaim the dir; the work branch (still off release) survives untouched.
        let _ = crate::git::remove_worktree(&repo_path, &wt_b);
        let _ = std::fs::remove_dir_all(&wt_b);
        g(&["worktree", "prune"]);
        assert!(crate::git::branch_descends_from(&repo_path, &branch_b, "release"),
            "precondition: work branch still descends from release");
        // Re-materialize: succeeds (checks out the surviving, still-valid work branch).
        assert!(
            materialize_direction(&db, dir_b.id).await.is_ok(),
            "a surviving work branch that still descends from its explicit base must re-materialize"
        );
        assert!(wt_b.exists(), "worktree recreated by checking out the surviving work branch");

        std::env::remove_var("WEFT_HOME");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&weft_home);
    }

    /// R38-3: the idempotent early-return must also validate explicit-base ancestry, not just
    /// `is_registered_worktree`. A direction with `base_branch="release"` whose STILL-REGISTERED
    /// work branch was externally reset/recreated off `main` (no longer descending from release)
    /// must NOT be returned idempotently as release-based on a confirm/approve retry — it falls
    /// through to the recreate path, whose R37-2 guard then bails. When the branch still descends
    /// from release the early-return is idempotent Ok (the normal reuse). (Validated only for an
    /// EXPLICIT base that still resolves; a blank or gone base does not validate.)
    #[tokio::test]
    async fn materialize_idempotent_rejects_registered_branch_reset_off_other_base() {
        use crate::store::repo;
        use std::process::Command as Cmd;
        let _env = crate::paths::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tag = format!("weft-remat-idem-reset-{}", std::process::id());
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
        // release: one commit AHEAD of main (so main does NOT descend from release).
        g(&["checkout", "-q", "-b", "release"]);
        g(&["commit", "-q", "--allow-empty", "-m", "release work"]);
        let release_sha = sha("release");
        g(&["checkout", "-q", &main]);
        let main_sha = sha(&main);
        assert_ne!(release_sha, main_sha, "release must be ahead of main");

        let db = crate::store::Db::connect("sqlite::memory:").await.unwrap();
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        let r = repo::add_repo_ref(&db, ws.id, "repo", repo_path.to_str().unwrap(), &main, "", true)
            .await.unwrap();
        let t = repo::create_thread(&db, ws.id, "t", "feature", "claude").await.unwrap();

        // ---- Case A: registered work branch reset OFF main (not descending from release) → Err. ----
        let dir_a = repo::create_direction(&db, t.id, "a", "claude", r.id, "reason", "plan+impl", "release")
            .await.unwrap();
        let wts_a = materialize_direction(&db, dir_a.id).await.unwrap();
        let wt_a = std::path::Path::new(&wts_a[0].path).to_path_buf();
        let branch_a = wts_a[0].branch.clone();
        assert_eq!(sha(&branch_a), release_sha, "precondition: work branch forked off release");
        // The worktree STAYS REGISTERED (this is the idempotent early-return path). Externally
        // reset the checked-out work branch to main FROM WITHIN ITS WORKTREE (git refuses to
        // force-update a branch checked out elsewhere).
        Cmd::new("git").args(["reset", "--hard", &main_sha]).current_dir(&wt_a).status().unwrap();
        assert_eq!(sha(&branch_a), main_sha, "precondition: registered work branch reset to main");
        assert!(crate::git::is_registered_worktree(&repo_path, &wt_a, &branch_a),
            "precondition: the worktree is still registered on the work branch");
        assert!(crate::git::ref_resolves(&repo_path, "release"), "precondition: explicit base still resolves");
        // Re-materialize: must NOT idempotently return the registered-but-mismatched worktree.
        assert!(
            materialize_direction(&db, dir_a.id).await.is_err(),
            "a registered work branch reset off a different base than its explicit base must not be returned idempotently"
        );

        // ---- Case B: registered work branch still descends from release → idempotent Ok. ----
        let dir_b = repo::create_direction(&db, t.id, "b", "claude", r.id, "reason", "plan+impl", "release")
            .await.unwrap();
        let wts_b = materialize_direction(&db, dir_b.id).await.unwrap();
        let wt_b = std::path::Path::new(&wts_b[0].path).to_path_buf();
        let branch_b = wts_b[0].branch.clone();
        assert!(crate::git::branch_descends_from(&repo_path, &branch_b, "release"),
            "precondition: work branch still descends from release");
        // The worktree stays registered + valid → the early-return is idempotent Ok.
        assert!(
            materialize_direction(&db, dir_b.id).await.is_ok(),
            "a registered work branch still descending from its explicit base must return idempotently"
        );
        assert!(wt_b.exists(), "the still-valid worktree survives the idempotent reuse");

        std::env::remove_var("WEFT_HOME");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&weft_home);
    }

    /// R39-3: the idempotent fast-path's explicit-base ancestry check must resolve the base
    /// through `branch_descends_from_base` (LOCAL `<base>` OR `origin/<base>`), not a BARE
    /// LOCAL `<base>`. In a SINGLE-BRANCH clone the explicit base `develop` exists ONLY as
    /// `origin/develop` (no local), so the bare-local `ref_resolves(repo, "develop")` is false
    /// and the old check is SKIPPED — a registered work branch reset onto a `main` commit
    /// (not descending from develop) would slip through as idempotent Ok. With the fix it must
    /// fall through to recreate and bail. A develop(origin)-based work branch still returns Ok.
    #[tokio::test]
    async fn materialize_idempotent_rejects_registered_branch_reset_off_other_base_single_branch_clone() {
        use crate::store::repo;
        use std::process::Command as Cmd;
        let _env = crate::paths::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tag = format!("weft-remat-idem-sbc-{}", std::process::id());
        let root = std::env::temp_dir().join(format!("{tag}-root"));
        let weft_home = std::env::temp_dir().join(format!("{tag}-home"));
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&weft_home);
        std::fs::create_dir_all(&root).unwrap();
        std::env::set_var("WEFT_HOME", weft_home.to_str().unwrap());

        // origin: main + develop one commit AHEAD (so main does NOT descend from develop).
        let origin = root.join("origin");
        std::fs::create_dir_all(&origin).unwrap();
        let og = |a: &[&str]| { Cmd::new("git").args(a).current_dir(&origin).status().unwrap(); };
        og(&["init", "-q"]); og(&["config", "user.email", "t@t.t"]); og(&["config", "user.name", "t"]);
        std::fs::write(origin.join("README.md"), "# x\n").unwrap();
        og(&["add", "-A"]); og(&["commit", "-q", "-m", "init"]);
        let main = crate::git::current_branch(&origin).unwrap();
        og(&["checkout", "-q", "-b", "develop"]);
        og(&["commit", "-q", "--allow-empty", "-m", "develop work"]);
        og(&["checkout", "-q", &main]);

        // SINGLE-BRANCH clone of main only → no local develop, origin/develop unfetched.
        let clone = root.join("clone");
        Cmd::new("git").args([
            "clone", "-q", "--single-branch", "--branch", &main,
            &origin.to_string_lossy(), &clone.to_string_lossy(),
        ]).current_dir(&root).status().unwrap();
        assert!(!crate::git::ref_resolves(&clone, "develop"), "precondition: no local develop");
        assert!(!crate::git::ref_resolves(&clone, "origin/develop"), "precondition: origin/develop not fetched yet");

        let g = |args: &[&str]| { Cmd::new("git").args(args).current_dir(&clone).status().unwrap(); };
        let sha = |rev: &str| -> String {
            String::from_utf8(
                Cmd::new("git").args(["rev-parse", rev]).current_dir(&clone).output().unwrap().stdout,
            ).unwrap().trim().to_string()
        };

        let db = crate::store::Db::connect("sqlite::memory:").await.unwrap();
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        let r = repo::add_repo_ref(&db, ws.id, "api", clone.to_str().unwrap(), &main, "", true)
            .await.unwrap();
        let t = repo::create_thread(&db, ws.id, "t", "feature", "claude").await.unwrap();

        // ---- Case A: registered work branch reset off main (not descending from develop) → Err. ----
        let dir_a = repo::create_direction(&db, t.id, "a", "claude", r.id, "reason", "plan+impl", "develop")
            .await.unwrap();
        let wts_a = materialize_direction(&db, dir_a.id).await.unwrap();
        let wt_a = std::path::Path::new(&wts_a[0].path).to_path_buf();
        let branch_a = wts_a[0].branch.clone();
        // The materialize fetched origin/develop and branched off it; develop exists ONLY as origin/develop.
        assert!(crate::git::ref_resolves(&clone, "origin/develop"), "origin/develop fetched by materialize");
        assert!(!crate::git::ref_resolves(&clone, "develop"), "still no LOCAL develop (the bare-local check would skip)");
        assert!(crate::git::branch_descends_from(&clone, &branch_a, "origin/develop"),
            "precondition: work branch forked off origin/develop");
        let main_sha = sha(&main);
        // Reset the still-registered work branch onto a main commit (NOT descending from develop),
        // from within its worktree (git refuses to force-update a branch checked out elsewhere).
        Cmd::new("git").args(["reset", "--hard", &main_sha]).current_dir(&wt_a).status().unwrap();
        assert_eq!(sha(&branch_a), main_sha, "precondition: work branch reset to main");
        assert!(crate::git::is_registered_worktree(&clone, &wt_a, &branch_a),
            "precondition: worktree still registered on the work branch");
        assert!(!crate::git::branch_descends_from(&clone, &branch_a, "origin/develop"),
            "precondition: reset branch no longer descends from origin/develop");
        // Re-materialize: must NOT idempotently return the registered-but-mismatched worktree.
        assert!(
            materialize_direction(&db, dir_a.id).await.is_err(),
            "single-branch clone: a registered work branch reset off main (origin-only develop base) must not slip through idempotently"
        );

        // ---- Case B: registered work branch still descends from origin/develop → idempotent Ok. ----
        // Drop the reset Case-A worktree so a fresh develop-based lane can materialize.
        let _ = crate::git::remove_worktree(&clone, &wt_a);
        let _ = std::fs::remove_dir_all(&wt_a);
        g(&["worktree", "prune"]);
        g(&["branch", "-D", &branch_a]);
        let dir_b = repo::create_direction(&db, t.id, "b", "claude", r.id, "reason", "plan+impl", "develop")
            .await.unwrap();
        let wts_b = materialize_direction(&db, dir_b.id).await.unwrap();
        let wt_b = std::path::Path::new(&wts_b[0].path).to_path_buf();
        let branch_b = wts_b[0].branch.clone();
        assert!(crate::git::branch_descends_from(&clone, &branch_b, "origin/develop"),
            "precondition: work branch descends from origin/develop");
        assert!(
            materialize_direction(&db, dir_b.id).await.is_ok(),
            "single-branch clone: a registered work branch still descending from origin/develop returns idempotently"
        );
        assert!(wt_b.exists(), "the still-valid worktree survives the idempotent reuse");

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
        let r = repo::add_repo_ref(&db, ws.id, "repo", repo_path.to_str().unwrap(), &main, "", true)
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
        let r = repo::add_repo_ref(&db, ws.id, "repo", repo_path.to_str().unwrap(), &main, "", true)
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
        let r = repo::add_repo_ref(&db, ws.id, "api", clone.to_str().unwrap(), stale_base, "", true)
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

    /// R39-2: when an upgraded repo's `base_ref` ALREADY EQUALS the live default but
    /// `base_ref_is_default` is still false, materialize must still backfill the marker. The
    /// old `live != base_ref` guard skipped set_repo_base_ref (which also sets the marker), so
    /// the verified default stayed marked legacy — and a later OFFLINE materialize (where
    /// recorded_base_or_default only trusts a base_ref with is_default=true) would treat it as
    /// legacy and fall through to main/master. After a blank-base materialize the row's
    /// base_ref_is_default must be true.
    #[tokio::test]
    async fn materialize_backfills_default_marker_when_base_ref_already_equals_live_default() {
        use crate::store::repo;
        use std::process::Command as Cmd;
        let _env = crate::paths::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tag = format!("weft-r39-2-{}", std::process::id());
        let root = std::env::temp_dir().join(format!("{tag}-root"));
        let weft_home = std::env::temp_dir().join(format!("{tag}-home"));
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&weft_home);
        std::fs::create_dir_all(&root).unwrap();
        std::env::set_var("WEFT_HOME", weft_home.to_str().unwrap());

        // origin with a commit so `git ls-remote --symref` returns a HEAD.
        let origin = root.join("origin");
        std::fs::create_dir_all(&origin).unwrap();
        let g = |a: &[&str]| { Cmd::new("git").args(a).current_dir(&origin).status().unwrap(); };
        g(&["init", "-q"]);
        g(&["config", "user.email", "t@t.t"]);
        g(&["config", "user.name", "t"]);
        std::fs::write(origin.join("README.md"), "# x\n").unwrap();
        g(&["add", "-A"]);
        g(&["commit", "-q", "-m", "init"]);
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
        // base_ref ALREADY EQUALS the live default, but base_ref_is_default=false (the
        // upgraded/legacy row whose marker was never set).
        let r = repo::add_repo_ref(&db, ws.id, "api", clone.to_str().unwrap(), &live_default, "", false)
            .await.unwrap();
        assert_eq!(r.base_ref, live_default, "precondition: base_ref equals the live default");
        assert!(!r.base_ref_is_default, "precondition: marker not yet set");

        let t = repo::create_thread(&db, ws.id, "t1", "feature", "claude").await.unwrap();
        // Blank base → materialize calls live_default_branch (returns live_default == base_ref).
        let dir = repo::create_direction(&db, t.id, "x", "claude", r.id, "r", "plan+impl", "")
            .await.unwrap();

        materialize_direction(&db, dir.id).await.unwrap();

        // The marker must now be backfilled to true even though base_ref didn't change.
        let updated = repo::get_repo(&db, r.id).await.unwrap().unwrap();
        assert_eq!(updated.base_ref, live_default, "base_ref unchanged (already correct)");
        assert!(
            updated.base_ref_is_default,
            "R39-2: the default marker must be backfilled when base_ref already equals the live default"
        );

        let removed = repo::delete_thread_cascade(&db, t.id).await.unwrap();
        let _ = cleanup_worktrees(&db, &removed).await;
        std::env::remove_var("WEFT_HOME");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&weft_home);
    }

    /// R39-1: if weft crashes AFTER `git worktree add -b` but BEFORE record_worktree, the
    /// retry's add_worktree_synced takes the path-exists fast-path and reports
    /// created_branch=false/created_checkout=false — so the crash-orphan checkout+branch
    /// would be recorded as USER-owned and reclaim/cascade would refuse to clean them
    /// (zero-accumulation never reaches zero). Because lane worktrees ALWAYS live under the
    /// managed, weft-reserved root, a registered checkout at our `path` with no DB row (we're
    /// in the create path) is OUR orphan and must be adopted as weft-created: the new row's
    /// created_branch AND created_checkout must both be true.
    #[tokio::test]
    async fn materialize_adopts_crash_orphan_worktree_as_weft_created() {
        use crate::store::repo;
        use sea_orm::EntityTrait;
        let _env = crate::paths::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tag = format!("weft-r39-1-{}", std::process::id());
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
        let r = repo::add_repo_ref(&db, ws.id, "repo", repo_path.to_str().unwrap(), &main, "", true)
            .await.unwrap();
        let t = repo::create_thread(&db, ws.id, "t", "feature", "claude").await.unwrap();
        let dir = repo::create_direction(&db, t.id, "d", "claude", r.id, "reason", "plan+impl", "")
            .await.unwrap();

        // ① Materialize: weft creates the worktree (branch + checkout) under the managed root
        // and records the row owning both.
        let wts = materialize_direction(&db, dir.id).await.unwrap();
        assert_eq!(wts.len(), 1);
        let wt_path = std::path::Path::new(&wts[0].path).to_path_buf();
        assert!(wt_path.exists(), "precondition: on-disk worktree created");
        assert!(wt_path.starts_with(worktree_root(&repo_path)), "precondition: under the managed root");
        assert!(wts[0].created_branch && wts[0].created_checkout, "precondition: weft owns both initially");

        // ② Simulate the crash: DELETE only the DB worktree row, leaving the on-disk worktree
        // + git registration in place (as if weft died between `worktree add` and record).
        entities::worktree::Entity::delete_by_id(wts[0].id).exec(&db.0).await.unwrap();
        assert!(repo::worktree_for(&db, dir.id, r.id).await.unwrap().is_none(),
            "precondition: DB row gone (crash orphan), checkout still registered");
        assert!(crate::git::is_registered_worktree(&repo_path, &wt_path, &wts[0].branch),
            "precondition: the on-disk checkout is still a registered worktree of this repo");

        // ③ Re-materialize the SAME direction: the create path runs (no row), add_worktree_synced
        // hits the path-exists fast-path (ownership=false). The orphan must be ADOPTED so the new
        // row owns BOTH — else reclaim/cascade would refuse to clean weft's own leftover.
        let wts2 = materialize_direction(&db, dir.id).await.unwrap();
        assert_eq!(wts2.len(), 1);
        assert!(
            wts2[0].created_branch,
            "R39-1: a crash-orphan checkout under the managed root must be adopted with created_branch=true"
        );
        assert!(
            wts2[0].created_checkout,
            "R39-1: a crash-orphan checkout under the managed root must be adopted with created_checkout=true"
        );
        // And it persisted on the row.
        let row = repo::worktree_for(&db, dir.id, r.id).await.unwrap().unwrap();
        assert!(row.created_branch && row.created_checkout, "adopted ownership persisted on the row");

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
        let r = repo::add_repo_ref(&db, ws.id, "repo", repo_path.to_str().unwrap(), &main, "", true)
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

    /// R36-3: cleanup_worktrees must KEEP a `created_checkout=false` reused checkout as a
    /// real, USABLE git worktree — it stays registered (`list_worktrees`) and `git status`
    /// works inside it — while LOCKING it so the orphan GC can't reclaim it after the row
    /// is dropped. (The previous behavior unregistered it, which degraded it to a plain
    /// directory; locking preserves usability AND survives a repo re-add.)
    #[tokio::test]
    async fn cleanup_locks_reused_checkout_kept_as_usable_worktree() {
        use crate::store::repo;
        let _env = crate::paths::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tag = format!("weft-cleanup-lock-{}", std::process::id());
        let root = std::env::temp_dir().join(format!("{tag}-root"));
        let weft_home = std::env::temp_dir().join(format!("{tag}-home"));
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&weft_home);
        std::fs::create_dir_all(&root).unwrap();
        std::env::set_var("WEFT_HOME", weft_home.to_str().unwrap());

        let repo_path = root.join("repo");
        crate::git::init_repo(&repo_path).unwrap();
        let base = crate::git::current_branch(&repo_path).unwrap();
        // A REAL registered worktree, treated below as a reused (created_checkout=false)
        // checkout with user content.
        let wt = root.join("reused-wt");
        crate::git::add_worktree_synced(&repo_path, "feat/reused", &wt, &base, false).unwrap();
        std::fs::write(wt.join("user.txt"), "user content\n").unwrap();
        assert!(
            crate::git::list_worktrees(&repo_path).unwrap().iter().any(|(_, b)| b == "feat/reused"),
            "registered before cleanup"
        );
        assert!(!crate::git::is_worktree_locked(&repo_path, &wt), "not locked before cleanup");

        let db = crate::store::Db::connect("sqlite::memory:").await.unwrap();
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        let r = repo::add_repo_ref(&db, ws.id, "repo", repo_path.to_str().unwrap(), &base, "", true)
            .await.unwrap();

        // Cascade cleanup of a reused (created_checkout=false, created_branch=false) entry.
        let removed = vec![(r.id, wt.to_string_lossy().to_string(), "feat/reused".to_string(), false, false)];
        cleanup_worktrees(&db, &removed).await.unwrap();

        // Dir + contents survive...
        assert!(wt.join("user.txt").exists(), "reused checkout contents preserved");
        // ...it stays a REAL, usable git worktree (still registered)...
        assert!(
            crate::git::list_worktrees(&repo_path).unwrap().iter().any(|(_, b)| b == "feat/reused"),
            "reused checkout stays a registered git worktree"
        );
        // ...`git status` inside it still works (it's a valid worktree, not a bare dir)...
        let status_ok = std::process::Command::new("git")
            .args(["-C", &wt.to_string_lossy(), "status", "--porcelain"])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(status_ok, "git status works inside the preserved worktree");
        // ...and it is LOCKED, so the orphan GC will skip it after the row is gone.
        assert!(
            crate::git::is_worktree_locked(&repo_path, &wt),
            "reused checkout locked so the orphan GC can't sweep it"
        );

        std::env::remove_var("WEFT_HOME");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&weft_home);
    }

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
        let r = repo::add_repo_ref(&db, ws.id, "repo", repo_path.to_str().unwrap(), &main, "", true)
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
