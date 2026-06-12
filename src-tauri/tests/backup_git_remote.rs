//! End-to-end git wrapper test: init → commit → push → clone-back, using a
//! local `git init --bare` repo as the "remote".

use std::process::Command;
use weft_app_lib::backup::git_remote;

fn make_bare_remote(parent: &std::path::Path) -> std::path::PathBuf {
    let bare = parent.join("remote.git");
    Command::new("git")
        .arg("init")
        .arg("--bare")
        .arg(&bare)
        .status()
        .expect("git init --bare");
    bare
}

fn remote_url(p: &std::path::Path) -> String {
    format!("file://{}", p.to_string_lossy())
}

#[test]
fn ensure_git_available_works() {
    git_remote::ensure_git_available().expect("git must be installed for tests");
}

#[test]
fn ls_remote_against_local_bare() {
    let tmp = tempfile::tempdir().unwrap();
    let bare = make_bare_remote(tmp.path());
    git_remote::ls_remote(&remote_url(&bare)).unwrap();
}

#[test]
fn ensure_clone_commit_push_then_clone_back() {
    let tmp = tempfile::tempdir().unwrap();
    let bare = make_bare_remote(tmp.path());
    let url = remote_url(&bare);

    let staging = tmp.path().join("staging");
    git_remote::ensure_clone(&staging, &url).unwrap();

    std::fs::write(staging.join("weft.db"), b"\x00\xffSOMEBYTES\x00").unwrap();
    std::fs::write(staging.join(".weft-backup-meta.json"), b"{}").unwrap();

    let r = git_remote::commit_and_push(&staging, "snapshot test").unwrap();
    assert!(!r.commit_sha.is_empty());
    assert!(r.bytes_pushed > 0);

    let restore = tmp.path().join("restore");
    git_remote::clone_to(&restore, &url).unwrap();
    let db = std::fs::read(restore.join("weft.db")).unwrap();
    assert_eq!(db, b"\x00\xffSOMEBYTES\x00");
}

#[test]
fn ensure_clone_rebuilds_when_origin_changes() {
    let tmp = tempfile::tempdir().unwrap();
    let bare1 = make_bare_remote(&tmp.path().join("a"));
    let bare2 = make_bare_remote(&tmp.path().join("b"));
    let staging = tmp.path().join("staging");

    git_remote::ensure_clone(&staging, &remote_url(&bare1)).unwrap();
    git_remote::ensure_clone(&staging, &remote_url(&bare2)).unwrap();

    let out = Command::new("git")
        .current_dir(&staging)
        .arg("remote")
        .arg("get-url")
        .arg("origin")
        .output()
        .unwrap();
    let url = String::from_utf8(out.stdout).unwrap();
    assert!(url.trim().ends_with("b/remote.git"));
}
