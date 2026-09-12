//! Idempotent git worktree create. Path layout is a trait so Weft
//! (`<repo>/.worktrees/<home-token>/<branch>`) and weft-codex
//! (`<home>/worktrees/<issue>/<dir>`) keep separate homes.
#![cfg_attr(
    not(test),
    deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)
)]

use anyhow::Context;
use std::path::{Path, PathBuf};

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

/// Weft layout: `<repo>/.worktrees/<dirname>/<branch>`.
///
/// `dirname` is adapter-chosen (`weft` / `weft-dev` / `weft-<hash>`) so two
/// Weft homes never share a root. This crate does not read `$HOME`.
pub struct WeftRepoLayout<'a> {
    pub repo: &'a Path,
    pub dirname: &'a str,
}

impl WorktreeLayout for WeftRepoLayout<'_> {
    fn worktree_path(&self, _issue_slug: &str, branch: &str) -> PathBuf {
        weft_repo_worktree_path(self.repo, self.dirname, branch)
    }
}

/// `<repo>/.worktrees/<dirname>`.
pub fn weft_repo_worktree_root(repo: &Path, dirname: &str) -> PathBuf {
    repo.join(".worktrees").join(dirname)
}

/// `<repo>/.worktrees/<dirname>/<branch>`.
pub fn weft_repo_worktree_path(repo: &Path, dirname: &str, branch: &str) -> PathBuf {
    weft_repo_worktree_root(repo, dirname).join(branch)
}

fn git_blocking(repo: &Path, args: &[&str]) -> anyhow::Result<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
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
pub fn ensure_worktree_blocking(
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
    match git_blocking(repo, &["worktree", "add", &wt, "-b", branch, base_arg]) {
        Ok(_) => Ok(WorktreeInfo {
            path: wt_path.to_path_buf(),
            branch: branch.to_string(),
            created: true,
        }),
        Err(first) => {
            git_blocking(repo, &["worktree", "add", &wt, branch])
                .with_context(|| format!("attach existing branch after: {first:#}"))?;
            Ok(WorktreeInfo {
                path: wt_path.to_path_buf(),
                branch: branch.to_string(),
                created: false,
            })
        }
    }
}

/// Async wrapper around [`ensure_worktree_blocking`] for Codex `Orchestrator`.
pub async fn ensure_worktree(
    repo: &Path,
    wt_path: &Path,
    branch: &str,
    base: &str,
) -> anyhow::Result<WorktreeInfo> {
    let repo = repo.to_path_buf();
    let wt_path = wt_path.to_path_buf();
    let branch = branch.to_string();
    let base = base.to_string();
    match tokio::task::spawn_blocking(move || {
        ensure_worktree_blocking(&repo, &wt_path, &branch, &base)
    })
    .await
    {
        Ok(result) => result,
        Err(error) => Err(anyhow::anyhow!("ensure_worktree join: {error}")),
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

    fn run(dir: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .expect("git");
        assert!(out.status.success(), "git {args:?} failed");
    }

    fn init_repo(dir: &Path) {
        run(dir, &["init", "-q", "-b", "main"]);
        run(dir, &["config", "user.email", "test@example.com"]);
        run(dir, &["config", "user.name", "test"]);
        std::fs::write(dir.join("README"), "x").expect("write");
        run(dir, &["add", "."]);
        run(dir, &["commit", "-q", "-m", "init"]);
    }

    #[test]
    fn ensure_creates_and_resumes() {
        let tmp = tempfile::tempdir().expect("tmp");
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir");
        init_repo(&repo);
        let wt = tmp.path().join("wts").join("i1").join("d1");
        let info = ensure_worktree_blocking(&repo, &wt, "weft/i1/d1", "").expect("create");
        assert!(info.created);
        assert!(wt.join("README").exists());
        let again = ensure_worktree_blocking(&repo, &wt, "weft/i1/d1", "").expect("resume");
        assert!(!again.created);
    }

    #[test]
    fn layout_trait_keeps_homes_apart() {
        let home = Path::new("/tmp/weft-codex-home");
        let repo = Path::new("/repo");
        let codex = CodexHomeLayout { home };
        let weft = WeftRepoLayout {
            repo,
            dirname: "weft",
        };
        let codex_path = codex.worktree_path("iss", "dir");
        let weft_path = weft.worktree_path("", "feat/dir");
        assert_eq!(codex_path, worktree_path(home, "iss", "dir"));
        assert_eq!(weft_path, weft_repo_worktree_path(repo, "weft", "feat/dir"));
        assert!(weft_path.starts_with(repo.join(".worktrees")));
        assert!(!codex_path.starts_with(repo.join(".worktrees")));
        assert_eq!(branch_name("iss", "dir"), "weft/iss/dir");
    }
}
