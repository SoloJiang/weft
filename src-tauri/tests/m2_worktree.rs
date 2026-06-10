//! M2 acceptance: two directions don't interfere; delete cleans worktrees;
//! same repo across two threads doesn't collide.
use std::path::{Path, PathBuf};
use std::process::Command;
use weft_app_lib::materialize::{cleanup_worktrees, materialize_direction};
use weft_app_lib::store::{repo, Db};

fn sh(dir: &Path, args: &[&str]) {
    let st = Command::new(args[0]).args(&args[1..]).current_dir(dir).status().unwrap();
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

#[tokio::test]
async fn m2_acceptance() {
    let root = std::env::temp_dir().join(format!("weft-m2-acc-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let weft_home = std::env::temp_dir().join(format!("weft-m2-home-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&weft_home);
    std::env::set_var("WEFT_HOME", weft_home.to_str().unwrap());
    let repo_a = make_repo(&root, "repo-a");
    let repo_b = make_repo(&root, "repo-b");

    let db = Db::connect("sqlite::memory:").await.unwrap();
    let ws = repo::create_workspace(&db, "ws").await.unwrap();
    let ra = repo::add_repo_ref(&db, ws.id, "repo-a", repo_a.to_str().unwrap(), "main", "claude").await.unwrap();
    let rb = repo::add_repo_ref(&db, ws.id, "repo-b", repo_b.to_str().unwrap(), "main", "claude").await.unwrap();

    // ① one thread, two directions on different repos -> independent worktrees
    let t1 = repo::create_thread(&db, ws.id, "t1", "feature").await.unwrap();
    let d1 = repo::create_direction(&db, t1.id, "da", "claude", ra.id, "modify repo-a", "plan+impl").await.unwrap();
    let d2 = repo::create_direction(&db, t1.id, "db", "claude", rb.id, "modify repo-b", "plan+impl").await.unwrap();
    let w1 = materialize_direction(&db, d1.id).await.unwrap();
    let w2 = materialize_direction(&db, d2.id).await.unwrap();
    assert_eq!(w1.len(), 1);
    assert_eq!(w2.len(), 1);
    assert!(Path::new(&w1[0].path).exists());
    assert!(Path::new(&w2[0].path).exists());
    assert_ne!(w1[0].path, w2[0].path);

    // ③ same repo across two threads -> two worktrees, distinct branches/paths
    let t2 = repo::create_thread(&db, ws.id, "t2", "feature").await.unwrap();
    let d3 = repo::create_direction(&db, t2.id, "da", "claude", ra.id, "modify repo-a", "impl-only").await.unwrap();
    let w3 = materialize_direction(&db, d3.id).await.unwrap();
    assert_ne!(w3[0].path, w1[0].path, "same repo, different thread -> different path");
    assert_ne!(w3[0].branch, w1[0].branch, "branches must differ");
    // both worktrees coexist in repo-a
    let listed = weft_app_lib::git::list_worktrees(&repo_a).unwrap();
    assert!(listed.iter().any(|(_, b)| b == &w1[0].branch));
    assert!(listed.iter().any(|(_, b)| b == &w3[0].branch));

    // ② delete a thread -> its worktrees are gone (rows + on disk), others remain
    let removed = repo::delete_thread_cascade(&db, t1.id).await.unwrap();
    cleanup_worktrees(&db, &removed).await.unwrap();
    assert!(!Path::new(&w1[0].path).exists(), "deleted worktree removed from disk");
    assert!(Path::new(&w3[0].path).exists(), "other thread's worktree survives");
    assert_eq!(repo::list_worktrees(&db, None).await.unwrap().len(), 1);

    // the deleted thread's namespaced branch must be gone from the canonical repo
    // (zero-accumulation), while the surviving thread's branch remains.
    let listed_after = weft_app_lib::git::list_worktrees(&repo_a).unwrap();
    assert!(!listed_after.iter().any(|(_, b)| b == &w1[0].branch), "deleted thread's branch must be gone");
    assert!(listed_after.iter().any(|(_, b)| b == &w3[0].branch), "surviving thread's branch remains");

    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&weft_home);
}
