//! Idempotent git worktree create. Path layout is a trait so Weft
//! (`<repo>/.worktrees/weft/<branch>`) and weft-codex
//! (`<home>/worktrees/<issue>/<dir>`) keep separate homes.
#![cfg_attr(
    not(test),
    deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)
)]

use anyhow::Context;
use std::path::{Path, PathBuf};
use tokio::process::Command;

pub struct WorktreeInfo {
    pub path: PathBuf,
    pub branch: String,
    pub created: bool,
}

/// Where a product stores direction worktrees. Implementors must not share
/// `~/.weft` with `~/.weft-codex`.
pub trait WorktreeLayout {
    fn worktree_path(&self, issue_slug: &str, direction_slug: &str) -> PathBuf;
}

/// weft-codex layout: `<home>/worktrees/<issue-slug>/<dir-slug>`.
pub struct CodexHomeLayout<'a> {
    pub home: &'a Path,
}

impl WorktreeLayout for CodexHomeLayout<'_> {
    fn worktree_path(&self, issue_slug: &str, direction_slug: &str) -> PathBuf {
        worktree_path(self.home, issue_slug, direction_slug)
    }
}

async fn git(repo: &Path, args: &[&str]) -> anyhow::Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .await
        .context("spawn git")?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!("git {:?} failed: {}", args, stderr.trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Ensure the direction's worktree exists at `wt_path`, on `branch`, created
/// off `base` (empty = repo HEAD). Idempotent: an existing directory is
/// returned as-is (resume path).
pub async fn ensure_worktree(
    repo: &Path,
    wt_path: &Path,
    branch: &str,
    base: &str,
) -> anyhow::Result<WorktreeInfo> {
    if wt_path.exists() {
        return Ok(WorktreeInfo {
            path: wt_path.to_path_buf(),
            branch: branch.to_string(),
            created: false,
        });
    }
    if let Some(parent) = wt_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create worktrees dir {}", parent.display()))?;
    }
    let wt = wt_path.to_string_lossy().to_string();
    let base_arg = if base.is_empty() { "HEAD" } else { base };
    let created = git(repo, &["worktree", "add", &wt, "-b", branch, base_arg]).await;
    match created {
        Ok(_) => Ok(WorktreeInfo {
            path: wt_path.to_path_buf(),
            branch: branch.to_string(),
            created: true,
        }),
        Err(first) => {
            git(repo, &["worktree", "add", &wt, branch])
                .await
                .with_context(|| format!("attach existing branch after: {first:#}"))?;
            Ok(WorktreeInfo {
                path: wt_path.to_path_buf(),
                branch: branch.to_string(),
                created: false,
            })
        }
    }
}

/// Conventional weft-codex location: `<home>/worktrees/<issue-slug>/<dir-slug>`.
pub fn worktree_path(home: &Path, issue_slug: &str, direction_slug: &str) -> PathBuf {
    home.join("worktrees").join(issue_slug).join(direction_slug)
}

/// Conventional branch name: `weft/<issue-slug>/<direction-slug>`.
pub fn branch_name(issue_slug: &str, direction_slug: &str) -> String {
    format!("weft/{issue_slug}/{direction_slug}")
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn run(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .await
            .expect("git");
        assert!(out.status.success(), "git {args:?} failed");
    }

    async fn init_repo(dir: &Path) {
        run(dir, &["init", "-q", "-b", "main"]).await;
        run(dir, &["config", "user.email", "test@example.com"]).await;
        run(dir, &["config", "user.name", "test"]).await;
        std::fs::write(dir.join("README"), "x").expect("write");
        run(dir, &["add", "."]).await;
        run(dir, &["commit", "-q", "-m", "init"]).await;
    }

    #[tokio::test]
    async fn ensure_creates_and_resumes() {
        let tmp = tempfile::tempdir().expect("tmp");
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir");
        init_repo(&repo).await;
        let wt = tmp.path().join("wts").join("i1").join("d1");
        let info = ensure_worktree(&repo, &wt, "weft/i1/d1", "")
            .await
            .expect("create");
        assert!(info.created);
        assert!(wt.join("README").exists());
        let again = ensure_worktree(&repo, &wt, "weft/i1/d1", "")
            .await
            .expect("resume");
        assert!(!again.created);
    }

    #[test]
    fn layout_trait_matches_codex_home_convention() {
        let home = Path::new("/tmp/weft-codex-home");
        let layout = CodexHomeLayout { home };
        assert_eq!(
            layout.worktree_path("iss", "dir"),
            worktree_path(home, "iss", "dir")
        );
        assert_eq!(branch_name("iss", "dir"), "weft/iss/dir");
    }
}
