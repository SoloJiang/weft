//! git worktree + diff helpers against a real throwaway repo.
use std::path::PathBuf;
use std::process::Command;

fn sh(dir: &PathBuf, args: &[&str]) {
    let st = Command::new(args[0])
        .args(&args[1..])
        .current_dir(dir)
        .status()
        .unwrap();
    assert!(st.success(), "cmd {:?} failed", args);
}

#[test]
fn worktree_list_and_diff() {
    let root = std::env::temp_dir().join(format!("weft-m2-git-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let repo = root.join("repo");
    let wt = root.join("wt");
    std::fs::create_dir_all(&repo).unwrap();
    sh(&repo, &["git", "init", "-q"]);
    sh(&repo, &["git", "config", "user.email", "t@t.t"]);
    sh(&repo, &["git", "config", "user.name", "t"]);
    std::fs::write(repo.join("README.md"), "# x\n").unwrap();
    sh(&repo, &["git", "add", "-A"]);
    sh(&repo, &["git", "commit", "-q", "-m", "init"]);
    sh(
        &repo,
        &[
            "git",
            "worktree",
            "add",
            "-q",
            "-b",
            "ws/d/t/m",
            wt.to_str().unwrap(),
        ],
    );

    // new untracked file in the worktree
    std::fs::write(wt.join("hello.txt"), "a\nb\n").unwrap();

    let wts = weft::git::list_worktrees(&repo).unwrap();
    assert!(wts.iter().any(|(_, b)| b == "ws/d/t/m"));

    let diff = weft::git::repo_diff(&wt).unwrap();
    let hello = diff.files.iter().find(|f| f.path == "hello.txt").unwrap();
    assert_eq!(hello.added, 2);

    let _ = std::fs::remove_dir_all(&root);
}
