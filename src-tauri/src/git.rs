//! Minimal git worktree helpers for M1. Branch names are namespaced with the
//! thread dimension (`ws/<workspace>/<thread>/<direction>`) so the same branch
//! is never checked out in two worktrees at once.

use anyhow::{bail, Context, Result};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::process::Command;

fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .with_context(|| format!("spawn git {:?}", args))?;
    if !out.status.success() {
        bail!(
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// True if `path` is inside a git work tree.
pub fn is_git_repo(path: &Path) -> bool {
    Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .current_dir(path)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Create a worktree for `repo` on a fresh `branch` at `worktree_path`.
/// Idempotent-ish: if the worktree path already exists it is reused.
pub fn add_worktree(repo: &Path, branch: &str, worktree_path: &Path) -> Result<PathBuf> {
    if worktree_path.exists() {
        return Ok(worktree_path.to_path_buf());
    }
    if let Some(parent) = worktree_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    // -b creates the branch; if it already exists, fall back to plain add.
    let path_str = worktree_path.to_string_lossy().to_string();
    let res = git(repo, &["worktree", "add", "-b", branch, &path_str]);
    if res.is_err() {
        git(repo, &["worktree", "add", &path_str, branch])
            .context("worktree add (existing branch)")?;
    }
    Ok(worktree_path.to_path_buf())
}

/// Remove a worktree and prune. (Used by M2 worktree lifecycle management.)
pub fn remove_worktree(repo: &Path, worktree_path: &Path) -> Result<()> {
    let path_str = worktree_path.to_string_lossy().to_string();
    git(repo, &["worktree", "remove", "--force", &path_str]).ok();
    git(repo, &["worktree", "prune"]).ok();
    Ok(())
}

/// Delete a (weft-namespaced) branch from `repo`, ignoring "not found".
pub fn delete_branch(repo: &Path, branch: &str) -> Result<()> {
    // -D force-deletes; weft worktree branches are throwaway WIP and the caller
    // is explicitly tearing the direction down (zero-accumulation principle).
    git(repo, &["branch", "-D", branch]).map(|_| ()).or(Ok(()))
}

/// Create a throwaway demo repo (for trying the app without a real repo).
pub fn init_demo_repo(at: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(at)?;
    git(at, &["init", "-q"])?;
    git(at, &["config", "user.email", "demo@weft.local"])?;
    git(at, &["config", "user.name", "weft demo"])?;
    std::fs::write(at.join("README.md"), "# weft demo repo\n")?;
    git(at, &["add", "-A"])?;
    git(at, &["commit", "-q", "-m", "init"])?;
    Ok(at.to_path_buf())
}

/// One file's diff stat in a worktree.
#[derive(Serialize, Debug, PartialEq)]
pub struct FileDiff {
    pub path: String,
    pub added: u32,
    pub removed: u32,
}

/// Per-repo working-tree diff stat (staged + unstaged + untracked-as-added).
#[derive(Serialize, Debug, Default)]
pub struct DiffSummary {
    pub files: Vec<FileDiff>,
}

/// `git worktree list --porcelain` parsed into (path, branch) pairs.
pub fn list_worktrees(repo: &Path) -> Result<Vec<(String, String)>> {
    let out = git(repo, &["worktree", "list", "--porcelain"])?;
    let mut res = Vec::new();
    let mut path: Option<String> = None;
    for line in out.lines() {
        if let Some(p) = line.strip_prefix("worktree ") {
            path = Some(p.to_string());
        } else if let Some(b) = line.strip_prefix("branch ") {
            if let Some(p) = path.take() {
                let branch = b.strip_prefix("refs/heads/").unwrap_or(b).to_string();
                res.push((p, branch));
            }
        }
    }
    Ok(res)
}

/// Diff stat for a worktree: tracked changes via `git diff --numstat HEAD`
/// plus untracked files counted as fully-added.
pub fn repo_diff(worktree_path: &Path) -> Result<DiffSummary> {
    let mut files = Vec::new();
    let numstat = git(worktree_path, &["diff", "--numstat", "HEAD"])?;
    for line in numstat.lines() {
        let mut parts = line.split('\t');
        let added = parts.next().unwrap_or("0").parse().unwrap_or(0);
        let removed = parts.next().unwrap_or("0").parse().unwrap_or(0);
        if let Some(path) = parts.next() {
            files.push(FileDiff { path: path.to_string(), added, removed });
        }
    }
    let untracked = git(
        worktree_path,
        &["ls-files", "--others", "--exclude-standard"],
    )?;
    for path in untracked.lines().filter(|l| !l.is_empty()) {
        let full = worktree_path.join(path);
        let added = std::fs::read_to_string(&full)
            .map(|c| c.lines().count() as u32)
            .unwrap_or(0);
        files.push(FileDiff { path: path.to_string(), added, removed: 0 });
    }
    Ok(DiffSummary { files })
}

/// Current branch name of a repo (e.g. "main").
pub fn current_branch(repo: &Path) -> Result<String> {
    git(repo, &["rev-parse", "--abbrev-ref", "HEAD"])
}

/// Short HEAD commit sha; used to stamp a repo profile and detect staleness.
pub fn head_commit(repo: &Path) -> Result<String> {
    git(repo, &["rev-parse", "--short", "HEAD"])
}
