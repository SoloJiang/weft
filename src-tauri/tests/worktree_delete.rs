//! A finished (Done) task can have its worktree deleted on its own: the
//! working-copy directory and the DB row go away, but the branch and the task
//! (direction) record survive. This is distinct from `delete_thread`'s cascade
//! teardown, which also force-deletes the branch (zero-accumulation).
//!
//! Lives in its own test binary so the `WEFT_HOME` env it sets can't race the
//! other worktree tests (integration tests in one file run on parallel threads;
//! separate files are separate processes). Everything is asserted in one test
//! for the same reason — a second env-mutating test in this file would race it.
use std::path::{Path, PathBuf};
use std::process::Command;
use weft::materialize::{cleanup_worktrees, materialize_direction, remove_direction_worktree};
use weft::store::{repo, Db};

fn sh(dir: &Path, args: &[&str]) {
    let st = Command::new(args[0])
        .args(&args[1..])
        .current_dir(dir)
        .status()
        .unwrap();
    assert!(st.success(), "cmd {:?} failed", args);
}

fn make_repo(root: &Path, name: &str) -> PathBuf {
    let p = root.join(name);
    std::fs::create_dir_all(&p).unwrap();
    sh(&p, &["git", "init", "-q"]);
    sh(&p, &["git", "config", "user.email", "t@t.t"]);
    sh(&p, &["git", "config", "user.name", "t"]);
    std::fs::write(p.join("README.md"), "# x\n").unwrap();
    sh(&p, &["git", "add", "-A"]);
    sh(&p, &["git", "commit", "-q", "-m", "init"]);
    p
}

fn branch_exists(repo: &Path, branch: &str) -> bool {
    Command::new("git")
        .args([
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ])
        .current_dir(repo)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[tokio::test]
async fn delete_worktree_keeps_branch_and_task() {
    let root = std::env::temp_dir().join(format!("weft-wtdel-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let weft_home = std::env::temp_dir().join(format!("weft-wtdel-home-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&weft_home);
    std::env::set_var("WEFT_HOME", weft_home.to_str().unwrap());

    let repo_a = make_repo(&root, "repo-a");
    let db = Db::connect("sqlite::memory:").await.unwrap();
    let ws = repo::create_workspace(&db, "ws").await.unwrap();
    let ra = repo::add_repo_ref(&db, ws.id, "repo-a", repo_a.to_str().unwrap(), "main")
        .await
        .unwrap();
    let t1 = repo::create_thread(&db, ws.id, "t1", "feature", "claude")
        .await
        .unwrap();
    let d1 = repo::create_direction(&db, t1.id, "da", "claude", ra.id, "modify repo-a", "plan+impl")
        .await
        .unwrap();

    let w = materialize_direction(&db, d1.id).await.unwrap();
    assert_eq!(w.len(), 1);
    let (wt_id, path, branch) = (w[0].id, w[0].path.clone(), w[0].branch.clone());
    assert!(Path::new(&path).exists(), "worktree materialized on disk");
    assert!(branch_exists(&repo_a, &branch), "branch created");

    // Done-only guard: a non-done task (created status defaults to `queued`)
    // must NOT have its worktree reclaimed — guards a stale confirm dialog after
    // the task was moved back to working/review.
    assert!(
        remove_direction_worktree(&db, wt_id).await.is_err(),
        "non-done worktree deletion is rejected"
    );
    assert!(Path::new(&path).exists(), "worktree preserved on rejection");
    assert_eq!(
        repo::list_worktrees(&db, None).await.unwrap().len(),
        1,
        "row preserved on rejection"
    );

    // Mark it done, then the Done-card "delete worktree" action removes just this
    // worktree.
    repo::set_direction_status(&db, d1.id, "done").await.unwrap();
    remove_direction_worktree(&db, wt_id).await.unwrap();

    // Directory and DB row are gone...
    assert!(!Path::new(&path).exists(), "worktree dir removed from disk");
    assert!(
        repo::list_worktrees(&db, None).await.unwrap().is_empty(),
        "worktree row removed"
    );
    // ...but the branch is kept (the distinguishing behavior vs cascade delete)...
    assert!(branch_exists(&repo_a, &branch), "branch is kept");
    // ...and the task (direction) record survives so the Done card stays.
    let dirs = repo::list_directions(&db, t1.id).await.unwrap();
    assert!(dirs.iter().any(|d| d.id == d1.id), "task card survives");

    // Idempotent: deleting an already-removed worktree is a no-op, not an error.
    remove_direction_worktree(&db, wt_id).await.unwrap();

    // Zero-accumulation: deleting the whole issue later still tears down the
    // kept branch, even though no worktree row points at it anymore.
    let removed = repo::delete_thread_cascade(&db, t1.id).await.unwrap();
    cleanup_worktrees(&db, &removed).await.unwrap();
    assert!(
        !branch_exists(&repo_a, &branch),
        "kept branch is cleaned when the issue is deleted"
    );

    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&weft_home);
}
