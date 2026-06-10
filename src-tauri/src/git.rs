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

/// Resolve a usable base commit-ish for a NEW worktree branch: prefer the repo's
/// recorded base_ref; if it no longer resolves, fall back through origin/HEAD →
/// main → master → HEAD so worktree creation never silently branches off whatever
/// happens to be checked out in the canonical repo.
fn resolve_base_ref(repo: &Path, recorded: &str) -> String {
    let resolves = |r: &str| {
        !r.is_empty()
            && Command::new("git")
                .args(["rev-parse", "--verify", "--quiet", &format!("{r}^{{commit}}")])
                .current_dir(repo)
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
    };
    if resolves(recorded) {
        return recorded.to_string();
    }
    if let Ok(out) = Command::new("git")
        .args(["symbolic-ref", "--short", "refs/remotes/origin/HEAD"])
        .current_dir(repo)
        .output()
    {
        if out.status.success() {
            let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if resolves(&s) {
                return s;
            }
        }
    }
    for c in ["main", "master", "origin/main", "origin/master"] {
        if resolves(c) {
            return c.to_string();
        }
    }
    "HEAD".to_string()
}

/// Create a worktree for `repo` on a fresh `branch` at `worktree_path`, branched
/// off `base_ref` (resolved defensively; see resolve_base_ref). Idempotent: an
/// existing path is reused, and an existing branch is checked out rather than
/// recreated.
pub fn add_worktree(repo: &Path, branch: &str, worktree_path: &Path, base_ref: &str) -> Result<PathBuf> {
    if worktree_path.exists() {
        return Ok(worktree_path.to_path_buf());
    }
    if let Some(parent) = worktree_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let path_str = worktree_path.to_string_lossy().to_string();
    let base = resolve_base_ref(repo, base_ref);
    let res = git(repo, &["worktree", "add", "-b", branch, &path_str, &base]);
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

/// Create a brand-new git repo at `at` with an empty initial commit, so worktrees
/// (which need a commit-ish) work immediately. Fails if `at` is a non-empty dir.
/// Relies on the user's global git identity for the commit.
pub fn init_repo(at: &Path) -> Result<()> {
    if at.exists() && std::fs::read_dir(at).map(|mut d| d.next().is_some()).unwrap_or(false) {
        bail!("a folder already exists at {} and isn't empty", at.display());
    }
    std::fs::create_dir_all(at)?;
    git(at, &["init", "-q"])?;
    git(at, &["commit", "-q", "--allow-empty", "-m", "Initial commit"])?;
    Ok(())
}

/// Clone `url` into `dest` (which must not be an existing non-empty dir). Uses the
/// system git credentials / SSH agent; weft never prompts for secrets, so a
/// private repo without configured credentials fails with git's own error.
pub fn clone_repo(url: &str, dest: &Path) -> Result<()> {
    if dest.exists() && std::fs::read_dir(dest).map(|mut d| d.next().is_some()).unwrap_or(false) {
        bail!("a folder already exists at {} and isn't empty", dest.display());
    }
    let parent = dest.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    git(parent, &["clone", url, &dest.to_string_lossy()])?;
    Ok(())
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

/// File stats + the unified patch for a worktree (the worker observe Diff tab).
#[derive(Serialize, Debug, Default)]
pub struct WorktreeDiff {
    pub files: Vec<FileDiff>,
    pub patch: String,
}

/// Unified patch of a worktree's changes: tracked via `git diff HEAD`, plus
/// untracked files synthesized as add-patches (workers building from scratch
/// create new files, which `git diff HEAD` omits). Skips unreadable (binary) and
/// very large files.
pub fn repo_patch(worktree_path: &Path) -> Result<String> {
    let mut out = git(worktree_path, &["diff", "HEAD"])?;
    let untracked = git(worktree_path, &["ls-files", "--others", "--exclude-standard"])?;
    for rel in untracked.lines().filter(|l| !l.is_empty()) {
        let Ok(content) = std::fs::read_to_string(worktree_path.join(rel)) else {
            continue; // binary / unreadable
        };
        let lines: Vec<&str> = content.lines().collect();
        if lines.len() > 2000 {
            continue; // don't flood the view with a huge generated file
        }
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&format!(
            "diff --git a/{rel} b/{rel}\nnew file mode 100644\n--- /dev/null\n+++ b/{rel}\n@@ -0,0 +1,{} @@\n",
            lines.len()
        ));
        for l in &lines {
            out.push('+');
            out.push_str(l);
            out.push('\n');
        }
    }
    Ok(out)
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

/// Absolute paths of every worktree git has registered for `repo` (including the
/// main checkout, which is first). Best-effort: empty on error.
pub fn list_registered_worktrees(repo: &Path) -> Vec<PathBuf> {
    match git(repo, &["worktree", "list", "--porcelain"]) {
        Ok(s) => s
            .lines()
            .filter_map(|l| l.strip_prefix("worktree "))
            .map(|p| PathBuf::from(p.trim()))
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// Current branch name of a repo (e.g. "main").
pub fn current_branch(repo: &Path) -> Result<String> {
    git(repo, &["rev-parse", "--abbrev-ref", "HEAD"])
}

/// Short HEAD commit sha; used to stamp a repo profile and detect staleness.
pub fn head_commit(repo: &Path) -> Result<String> {
    git(repo, &["rev-parse", "--short", "HEAD"])
}

/// Append `name` to a worktree's git exclude (info/exclude) so weft's injected,
/// untracked files never show in `git status` / diffs / accidental commits.
/// Resolves the real exclude path via git (worktrees use a separate gitdir).
/// Best-effort: silently does nothing if git isn't available.
pub fn git_exclude(cwd: &std::path::Path, name: &str) {
    let out = std::process::Command::new("git")
        .args(["-C", &cwd.to_string_lossy(), "rev-parse", "--git-path", "info/exclude"])
        .output();
    let Ok(out) = out else { return };
    if !out.status.success() {
        return;
    }
    let rel = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if rel.is_empty() {
        return;
    }
    let p = std::path::Path::new(&rel);
    let exclude_path = if p.is_absolute() { p.to_path_buf() } else { cwd.join(p) };
    if let Some(parent) = exclude_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let existing = std::fs::read_to_string(&exclude_path).unwrap_or_default();
    if existing.lines().any(|l| l.trim() == name) {
        return;
    }
    let mut content = existing;
    if !content.is_empty() && !content.ends_with('\n') {
        content.push('\n');
    }
    content.push_str(name);
    content.push('\n');
    let _ = std::fs::write(&exclude_path, content);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("weft-git-{}-{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&p);
        p
    }

    #[test]
    fn worktree_branches_from_recorded_base_not_current_head() {
        let repo = tmp("base");
        init_repo(&repo).unwrap();
        let base = current_branch(&repo).unwrap();
        let base_commit = git(&repo, &["rev-parse", &base]).unwrap();
        git(&repo, &["checkout", "-q", "-b", "other"]).unwrap();
        git(&repo, &["commit", "-q", "--allow-empty", "-m", "other"]).unwrap();
        let other_commit = git(&repo, &["rev-parse", "HEAD"]).unwrap();
        assert_ne!(base_commit, other_commit);

        let wt = tmp("base-wt");
        add_worktree(&repo, "ws/x/t/d", &wt, &base).unwrap();
        let wt_head = git(&wt, &["rev-parse", "HEAD"]).unwrap();
        assert_eq!(wt_head, base_commit, "must branch from recorded base, not current HEAD");
        assert_ne!(wt_head, other_commit);

        let _ = remove_worktree(&repo, &wt);
        let _ = std::fs::remove_dir_all(&repo);
        let _ = std::fs::remove_dir_all(&wt);
    }

    #[test]
    fn bogus_base_ref_falls_back_and_still_creates() {
        let repo = tmp("bogus");
        init_repo(&repo).unwrap();
        let wt = tmp("bogus-wt");
        add_worktree(&repo, "ws/x/t/d2", &wt, "no-such-branch-xyz").unwrap();
        assert!(wt.join(".git").exists());
        let _ = remove_worktree(&repo, &wt);
        let _ = std::fs::remove_dir_all(&repo);
        let _ = std::fs::remove_dir_all(&wt);
    }

    #[test]
    fn resolve_prefers_recorded_then_falls_back() {
        let repo = tmp("resolve");
        init_repo(&repo).unwrap();
        let base = current_branch(&repo).unwrap();
        assert_eq!(resolve_base_ref(&repo, &base), base);
        let fb = resolve_base_ref(&repo, "nope-xyz");
        assert!(git(&repo, &["rev-parse", "--verify", &fb]).is_ok());
        let _ = std::fs::remove_dir_all(&repo);
    }
}
