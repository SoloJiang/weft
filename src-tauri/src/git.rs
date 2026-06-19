//! Minimal git worktree helpers. Branch names follow the target repo's observed
//! style (`feat/*` vs `feature/*`, `fix/*` vs `bugfix/*`) and worktrees are
//! materialized under that repo's `.worktrees/weft/` root.

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

/// True if `r` resolves to a commit in `dir` (non-empty + `rev-parse` verifies).
fn ref_resolves(dir: &Path, r: &str) -> bool {
    !r.is_empty()
        && Command::new("git")
            .args(["rev-parse", "--verify", "--quiet", &format!("{r}^{{commit}}")])
            .current_dir(dir)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
}

/// The bare branch name a user means for the diff target: trimmed, with a
/// leading `origin/` (the remote the UI surfaces) stripped — so typing or
/// pasting `origin/main` behaves like `main` for BOTH the fetch refspec
/// (`git fetch origin main`, not the failing `git fetch origin origin/main`)
/// and ref resolution.
fn normalize_target(target: &str) -> String {
    let t = target.trim();
    t.strip_prefix("origin/").unwrap_or(t).to_string()
}

/// Resolve a usable base commit-ish for a NEW worktree branch: prefer the repo's
/// recorded base_ref; if it no longer resolves, fall back through origin/HEAD →
/// main → master → HEAD so worktree creation never silently branches off whatever
/// happens to be checked out in the canonical repo.
fn resolve_base_ref(repo: &Path, recorded: &str) -> String {
    if ref_resolves(repo, recorded) {
        return recorded.to_string();
    }
    if let Ok(out) = Command::new("git")
        .args(["symbolic-ref", "--short", "refs/remotes/origin/HEAD"])
        .current_dir(repo)
        .output()
    {
        if out.status.success() {
            let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if ref_resolves(repo, &s) {
                return s;
            }
        }
    }
    for c in ["main", "master", "origin/main", "origin/master"] {
        if ref_resolves(repo, c) {
            return c.to_string();
        }
    }
    "HEAD".to_string()
}

/// The default target-branch *name* for a worktree's diff "vs target" mode:
/// the repo's recorded base branch (stripped of any `origin/` prefix) if set,
/// else detected via origin/HEAD → main → master. Used as the placeholder /
/// fallback when a direction has no explicit target_branch.
pub fn default_target_branch(worktree: &Path, base_ref: &str) -> String {
    let strip = |s: &str| s.strip_prefix("origin/").unwrap_or(s).to_string();
    let b = base_ref.trim();
    // A repo registered while detached records base_ref = "HEAD" — that's not a
    // real branch, so treat it like "unset" and detect, rather than letting the
    // default target become "HEAD" (which would resolve to the worker's own HEAD
    // and hide all committed task changes).
    if !b.is_empty() && b != "HEAD" {
        return strip(b);
    }
    let detected = resolve_base_ref(worktree, "");
    if detected == "HEAD" {
        "main".to_string()
    } else {
        strip(&detected)
    }
}

/// Resolve the ref to compare a target branch against: prefer the (freshly
/// fetched) remote `origin/<target>`, else a local `<target>`, else fall back
/// through the repo's default-branch chain (origin/HEAD → main → master → HEAD).
fn resolve_target_ref(worktree: &Path, target: &str) -> String {
    let t = normalize_target(target);
    // "HEAD" is not a real target branch (see default_target_branch); falling
    // through to the default chain avoids merge-base(HEAD, HEAD) hiding commits.
    if !t.is_empty() && t != "HEAD" {
        let remote = format!("origin/{t}");
        if ref_resolves(worktree, &remote) {
            return remote;
        }
        if ref_resolves(worktree, &t) {
            return t;
        }
    }
    resolve_base_ref(worktree, "")
}

/// The remote's default branch *name* (origin/HEAD's target, `origin/` stripped),
/// or None when no remote-tracking HEAD is set locally (no remote, or a repo added
/// by path that was never cloned / `git remote set-head`). Local-only; no network.
fn remote_default_branch(repo: &Path) -> Option<String> {
    let out = Command::new("git")
        .args(["symbolic-ref", "--short", "refs/remotes/origin/HEAD"])
        .current_dir(repo)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let name = s.strip_prefix("origin/").unwrap_or(&s).to_string();
    (!name.is_empty()).then_some(name)
}

/// The default BASE branch name for a NEW worktree (and the value captured as a
/// repo's base_ref at add time). Precedence: (1) the remote's default (origin/HEAD)
/// if it still resolves; (2) the conventional integration branch main/master
/// (local or origin/) — a repo added on a feature branch without origin/HEAD must
/// not default to that feature branch; (3) the recorded `base_ref` if it resolves
/// (non-standard repos, e.g. a "trunk"/"develop" default with no origin/HEAD and no
/// main/master); (4) "main". Returns a bare branch name (no `origin/`).
pub fn default_base_branch(repo: &Path, base_ref: &str) -> String {
    if let Some(name) = remote_default_branch(repo) {
        if ref_resolves(repo, &name) || ref_resolves(repo, &format!("origin/{name}")) {
            return name;
        }
    }
    for c in ["main", "master"] {
        if ref_resolves(repo, c) || ref_resolves(repo, &format!("origin/{c}")) {
            return c.to_string();
        }
    }
    let b = base_ref.trim();
    let b = b.strip_prefix("origin/").unwrap_or(b);
    if !b.is_empty()
        && b != "HEAD"
        && (ref_resolves(repo, b) || ref_resolves(repo, &format!("origin/{b}")))
    {
        return b.to_string();
    }
    "main".to_string()
}

/// PR-style diff (files + patch + the ref compared against) of a worktree
/// against a target branch: the task's own changes relative to where it
/// branched off the target's latest *remote* state (merge-base with
/// `origin/<target>`), **including uncommitted edits**. The target's own newer
/// commits don't appear as noise. With `fetch`, refreshes `origin/<target>`
/// first ("对齐远端最新") — best-effort, so offline/no-remote never breaks the diff.
pub fn target_diff(worktree_path: &Path, target: &str, fetch: bool) -> Result<TargetDiff> {
    // Normalize so a remote-prefixed input (e.g. the `origin/main` the UI shows)
    // fetches as `main` rather than failing on `git fetch origin origin/main`.
    let target = normalize_target(target);
    if fetch {
        fetch_origin_branch(worktree_path, &target);
    }
    let resolved = resolve_target_ref(worktree_path, &target);
    // PR-style base = merge-base(resolved, HEAD). If it fails (unrelated
    // histories / missing ref), fall back to diffing against the resolved ref
    // directly, then HEAD — always produce *some* diff rather than erroring.
    let base = git(worktree_path, &["merge-base", &resolved, "HEAD"])
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| resolved.clone());
    let files = repo_diff_from(worktree_path, &base)?.files;
    let patch = repo_patch_from(worktree_path, &base).unwrap_or_default();
    Ok(TargetDiff {
        files,
        patch,
        resolved,
    })
}

/// Create a worktree for `repo` on a fresh `branch` at `worktree_path`, branched
/// off `base_ref` (resolved defensively; see resolve_base_ref). Idempotent: an
/// existing path is reused, and an existing branch is checked out rather than
/// recreated.
pub fn add_worktree(
    repo: &Path,
    branch: &str,
    worktree_path: &Path,
    base_ref: &str,
) -> Result<PathBuf> {
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

/// Best-effort fetch of one branch from origin into refs/remotes/origin/<branch>.
/// Explicit destination refspec so the branch lands in remote-tracking refs even
/// under a `--single-branch` clone's narrowed remote.origin.fetch; GIT_TERMINAL_PROMPT=0
/// fails fast instead of hanging on a credential prompt. Returns true when the fetch
/// actually succeeded (the remote was reachable and returned data), false otherwise —
/// callers use this to distinguish "freshly synced" from "stale ref already present".
/// `dir` may be the canonical repo or any of its worktrees.
pub fn fetch_origin_branch(dir: &Path, branch: &str) -> bool {
    let b = normalize_target(branch);
    if b.is_empty() || b == "HEAD" {
        return false;
    }
    let refspec = format!("+{b}:refs/remotes/origin/{b}");
    Command::new("git")
        .args(["fetch", "--quiet", "origin", &refspec])
        .current_dir(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Like `add_worktree`, but first best-effort fetches `base_name` from origin and
/// branches the new worktree off the FRESH `origin/<base_name>`. Returns the path and
/// Some(true) only when it branched off origin AND the fetch that refreshed it
/// succeeded (truly fresh); Some(false) when it fell back to a stale-origin/local ref
/// (a "couldn't sync" signal); None when an existing path/branch was reused.
/// Outcome of `add_worktree_synced`. `created_checkout`/`created_branch` drive
/// rollback: remove the checkout when WE created it; delete the branch only when WE
/// created it (a pre-existing branch reused by the fallback must survive).
pub struct WorktreeAdd {
    pub path: PathBuf,
    pub created_checkout: bool,
    pub created_branch: bool,
    pub synced: bool,
}

/// `require_resolvable` = the base was an explicit user/lead choice: if it resolves to
/// neither `origin/<base>` nor local `<base>` (even after fetch), return an error
/// rather than silently using the repo default. When false (empty/default base), fall
/// back through the default-branch chain so the worktree is still created.
pub fn add_worktree_synced(
    repo: &Path,
    branch: &str,
    worktree_path: &Path,
    base_name: &str,
    require_resolvable: bool,
) -> Result<WorktreeAdd> {
    if worktree_path.exists() {
        return Ok(WorktreeAdd {
            path: worktree_path.to_path_buf(),
            created_checkout: false,
            created_branch: false,
            synced: false,
        });
    }
    let fetched = fetch_origin_branch(repo, base_name);
    let t = normalize_target(base_name);
    let remote = format!("origin/{t}");
    let resolved = if !t.is_empty() && t != "HEAD" && ref_resolves(repo, &remote) {
        remote.clone()
    } else if !t.is_empty() && t != "HEAD" && ref_resolves(repo, &t) {
        t.clone()
    } else if require_resolvable {
        bail!("base branch {base_name:?} not found locally or on origin (after fetch)");
    } else {
        resolve_base_ref(repo, &t)
    };
    // Fresh only if we branched off the remote ref AND the fetch actually succeeded.
    let synced = resolved.starts_with("origin/") && fetched;
    if let Some(parent) = worktree_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let path_str = worktree_path.to_string_lossy().to_string();
    let res = git(repo, &["worktree", "add", "-b", branch, &path_str, &resolved]);
    if res.is_err() {
        // Branch likely already exists: check it out into a new worktree dir.
        git(repo, &["worktree", "add", &path_str, branch])
            .context("worktree add (existing branch)")?;
        return Ok(WorktreeAdd {
            path: worktree_path.to_path_buf(),
            created_checkout: true,
            created_branch: false,
            synced: false,
        });
    }
    Ok(WorktreeAdd {
        path: worktree_path.to_path_buf(),
        created_checkout: true,
        created_branch: true,
        synced,
    })
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
/// Uses the configured git identity when available, otherwise writes a local
/// fallback identity so first-time git users and CI can commit.
pub fn init_repo(at: &Path) -> Result<()> {
    if at.exists()
        && std::fs::read_dir(at)
            .map(|mut d| d.next().is_some())
            .unwrap_or(false)
    {
        bail!(
            "a folder already exists at {} and isn't empty",
            at.display()
        );
    }
    std::fs::create_dir_all(at)?;
    git(at, &["init", "-q"])?;
    ensure_repo_identity(at)?;
    git(
        at,
        &["commit", "-q", "--allow-empty", "-m", "Initial commit"],
    )?;
    Ok(())
}

fn ensure_repo_identity(at: &Path) -> Result<()> {
    if git(at, &["config", "user.email"]).is_err() {
        git(at, &["config", "user.email", "weft@example.invalid"])?;
    }
    if git(at, &["config", "user.name"]).is_err() {
        git(at, &["config", "user.name", "weft"])?;
    }
    Ok(())
}

/// Clone `url` into `dest` (which must not be an existing non-empty dir). Uses the
/// system git credentials / SSH agent; weft never prompts for secrets, so a
/// private repo without configured credentials fails with git's own error.
pub fn clone_repo(url: &str, dest: &Path) -> Result<()> {
    if dest.exists()
        && std::fs::read_dir(dest)
            .map(|mut d| d.next().is_some())
            .unwrap_or(false)
    {
        bail!(
            "a folder already exists at {} and isn't empty",
            dest.display()
        );
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

/// "vs target" diff: like [`WorktreeDiff`] but relative to a target branch's
/// merge-base, plus the ref actually compared against (e.g. `origin/main`).
#[derive(Serialize, Debug, Default)]
pub struct TargetDiff {
    pub files: Vec<FileDiff>,
    pub patch: String,
    pub resolved: String,
}

/// Unified patch of a worktree's changes against HEAD (the working-tree view).
pub fn repo_patch(worktree_path: &Path) -> Result<String> {
    repo_patch_from(worktree_path, "HEAD")
}

/// Unified patch of a worktree's changes from `base` to the working tree:
/// tracked via `git diff <base>`, plus untracked files synthesized as
/// add-patches (workers building from scratch create new files, which
/// `git diff` omits). Skips unreadable (binary) and very large files. `base`
/// is "HEAD" for the working-tree view, or a merge-base sha for "vs target".
pub fn repo_patch_from(worktree_path: &Path, base: &str) -> Result<String> {
    let mut out = git(worktree_path, &["diff", base])?;
    let untracked = git(
        worktree_path,
        &["ls-files", "--others", "--exclude-standard"],
    )?;
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

/// Diff stat for a worktree against HEAD (the working-tree view).
pub fn repo_diff(worktree_path: &Path) -> Result<DiffSummary> {
    repo_diff_from(worktree_path, "HEAD")
}

/// Diff stat for a worktree from `base` to the working tree: tracked changes
/// via `git diff --numstat <base>` plus untracked files counted as fully-added.
pub fn repo_diff_from(worktree_path: &Path, base: &str) -> Result<DiffSummary> {
    let mut files = Vec::new();
    let numstat = git(worktree_path, &["diff", "--numstat", base])?;
    for line in numstat.lines() {
        let mut parts = line.split('\t');
        let added = parts.next().unwrap_or("0").parse().unwrap_or(0);
        let removed = parts.next().unwrap_or("0").parse().unwrap_or(0);
        if let Some(path) = parts.next() {
            files.push(FileDiff {
                path: path.to_string(),
                added,
                removed,
            });
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
        files.push(FileDiff {
            path: path.to_string(),
            added,
            removed: 0,
        });
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

fn all_branch_refs(repo: &Path) -> Vec<String> {
    git(
        repo,
        &[
            "for-each-ref",
            "--format=%(refname:short)",
            "refs/heads",
            "refs/remotes",
        ],
    )
    .map(|out| {
        out.lines()
            .map(str::trim)
            .filter(|s| !s.is_empty() && !s.ends_with("/HEAD"))
            .map(ToString::to_string)
            .collect()
    })
    .unwrap_or_default()
}

/// `reserved` carries branch names already chosen for this repo but not yet present
/// as git refs — e.g. sibling directions whose worktree hasn't materialized yet.
/// Including them stops two directions on the same issue/repo from reserving the
/// identical branch (and thus the same `.worktrees/weft/<branch>` path) before the
/// first branch exists in git.
pub fn choose_branch_name(repo: &Path, semantic: &str, title: &str, reserved: &[String]) -> String {
    let mut refs = all_branch_refs(repo);
    refs.extend(reserved.iter().cloned());
    choose_branch_name_from_refs(semantic, title, &refs)
}

fn choose_branch_name_from_refs(semantic: &str, title: &str, refs: &[String]) -> String {
    let slug = crate::slug::slugify(title);
    let prefix = branch_prefix_for_semantic(semantic, refs);
    // A ref named exactly `prefix` (e.g. a branch literally `feature`) blocks the
    // whole `prefix/…` namespace — git stores refs as files, so `refs/heads/feature`
    // and `refs/heads/feature/x` can't coexist (a D/F conflict). Join with `-`
    // instead so the branch is actually creatable.
    let prefix_is_leaf = refs.iter().any(|r| {
        let r = r.strip_prefix("origin/").unwrap_or(r);
        r == prefix
    });
    let sep = if prefix_is_leaf { '-' } else { '/' };
    unique_branch(&format!("{prefix}{sep}{slug}"), refs)
}

/// A ref that would collide with `name` if both were created. Beyond an exact
/// match, git's file-backed refs forbid a leaf and a directory at the same path,
/// so `feature` conflicts with `feature/x` and vice versa (a "D/F conflict").
fn df_conflict(name: &str, refs: &[String]) -> bool {
    refs.iter().any(|r| {
        let r = r.strip_prefix("origin/").unwrap_or(r);
        r == name || r.starts_with(&format!("{name}/")) || name.starts_with(&format!("{r}/"))
    })
}

fn branch_prefix_for_semantic(semantic: &str, refs: &[String]) -> &'static str {
    let short = count_prefix(refs, "feat") + count_prefix(refs, "fix");
    let long = count_prefix(refs, "feature") + count_prefix(refs, "bugfix");
    match semantic {
        // UI/IM issues store the kind as `bugfix`; conventional commits use `fix`.
        // Both are fix-style work and must follow the repo's fix branch convention.
        "fix" | "bugfix" => {
            if count_prefix(refs, "bugfix") > count_prefix(refs, "fix") {
                "bugfix"
            } else {
                "fix"
            }
        }
        "docs" => "docs",
        "test" => "test",
        "refactor" => "refactor",
        "chore" => "chore",
        "polish" => "polish",
        _ => {
            if short > long {
                "feat"
            } else {
                "feature"
            }
        }
    }
}

fn count_prefix(refs: &[String], prefix: &str) -> usize {
    refs.iter()
        .filter(|r| {
            let r = r.strip_prefix("origin/").unwrap_or(r);
            r.strip_prefix(prefix)
                .is_some_and(|rest| rest.starts_with('/') || rest.starts_with('-'))
        })
        .count()
}

fn unique_branch(base: &str, refs: &[String]) -> String {
    if !df_conflict(base, refs) {
        return base.to_string();
    }
    let mut n = 2;
    loop {
        let candidate = format!("{base}-{n}");
        if !df_conflict(&candidate, refs) {
            return candidate;
        }
        n += 1;
    }
}

/// Current branch name of a repo (e.g. "main").
pub fn current_branch(repo: &Path) -> Result<String> {
    git(repo, &["rev-parse", "--abbrev-ref", "HEAD"])
}

/// The repo's `origin` remote URL, if one is configured. None for a freshly
/// `git init`-ed local repo (no origin) or any git error — callers treat that
/// as "no remote identity, dedup by path only".
pub fn remote_url(repo: &Path) -> Option<String> {
    git(repo, &["remote", "get-url", "origin"])
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Strip credentials from a remote URL before persisting it. For a scheme URL
/// (`scheme://[user[:pass]@]host/…`) the entire authority userinfo is dropped, so
/// an embedded password/PAT from `.git/config` never lands in Weft's DB/backups.
/// scp-style `[user@]host:path` is left as-is (its user is an SSH login, not a
/// secret). Only the authority is touched — an `@` later in the path survives.
pub fn redact_remote(url: &str) -> String {
    let s = url.trim();
    if let Some(pos) = s.find("://") {
        let scheme = &s[..pos + 3];
        let rest = &s[pos + 3..];
        let authority_end = rest.find('/').unwrap_or(rest.len());
        if let Some(at) = rest[..authority_end].rfind('@') {
            return format!("{scheme}{}", &rest[at + 1..]);
        }
    }
    s.to_string()
}

/// Normalized dedup key for a git remote URL, mirroring the frontend
/// `gitUrlKey` (src/lib/gitUrl.ts) so both sides agree on "same repo": drop a
/// trailing `.git`/slashes, lowercase the scheme + host only — the repo path
/// stays case-sensitive (git hosts treat `Team/App` and `team/App` as distinct).
/// Empty in → empty out.
pub fn git_url_key(url: &str) -> String {
    let url = url.trim();
    if url.is_empty() {
        return String::new();
    }
    // Trim trailing slashes BEFORE `.git` so `repo.git/` and `repo` key the same.
    // The `is_char_boundary` guard keeps the slice char-safe — a Unicode path
    // component like `host:éabc` must never panic on a non-ASCII byte boundary.
    let no_slash = url.trim_end_matches('/');
    let cut = no_slash.len().checked_sub(4);
    let base = match cut {
        Some(i)
            if no_slash.is_char_boundary(i) && no_slash[i..].eq_ignore_ascii_case(".git") =>
        {
            &no_slash[..i]
        }
        _ => no_slash,
    };
    // Lowercase the host; keep any userinfo / ssh user (case-sensitive).
    let lower_host = |authority: &str| match authority.rfind('@') {
        Some(at) => format!("{}{}", &authority[..=at], authority[at + 1..].to_lowercase()),
        None => authority.to_lowercase(),
    };
    // scheme URL: `scheme://authority[/path]`.
    if let Some(pos) = base.find("://") {
        let scheme = &base[..pos + 3];
        let rest = &base[pos + 3..];
        let (authority, path) = match rest.find('/') {
            Some(s) => (&rest[..s], &rest[s..]),
            None => (rest, ""),
        };
        return format!("{}{}{}", scheme.to_lowercase(), lower_host(authority), path);
    }
    // scp-style: `[user@]host:path` (split on the first colon).
    if let Some(colon) = base.find(':') {
        return format!("{}:{}", lower_host(&base[..colon]), &base[colon + 1..]);
    }
    base.to_lowercase()
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
        .args([
            "-C",
            &cwd.to_string_lossy(),
            "rev-parse",
            "--git-path",
            "info/exclude",
        ])
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
    let exclude_path = if p.is_absolute() {
        p.to_path_buf()
    } else {
        cwd.join(p)
    };
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
    fn git_url_key_normalizes_host_and_dotgit() {
        // Host is case-insensitive; trailing `.git`/slashes are dropped — so all
        // these spellings of the same remote collapse to one dedup key.
        let k = git_url_key("https://github.com/acme/app");
        assert_eq!(git_url_key("https://GitHub.com/acme/app.git"), k);
        assert_eq!(git_url_key("https://github.com/acme/app.git/"), k);
        // The repo path stays case-sensitive (git hosts treat these as distinct).
        assert_ne!(git_url_key("https://github.com/Acme/App"), git_url_key("https://github.com/acme/app"));
        // scp form `[user@]host:path`: host lowercased, ssh user + path kept.
        let s = git_url_key("git@github.com:acme/api");
        assert_eq!(git_url_key("git@GitHub.com:acme/api.git"), s);
        assert_ne!(s, k); // scp and scheme spellings don't unify — and shouldn't here
        assert_eq!(git_url_key(""), "");
    }

    #[test]
    fn git_url_key_handles_unicode_suffix_without_panic() {
        // A non-ASCII tail whose last 4 bytes aren't a char boundary must not
        // panic the `.git` strip (regression for byte-slicing).
        assert_eq!(git_url_key("git@host:éabc"), "git@host:éabc");
        // A real .git after Unicode still strips cleanly.
        assert_eq!(git_url_key("https://host/é-repo.git"), "https://host/é-repo");
    }

    #[test]
    fn redact_remote_strips_scheme_credentials_only() {
        // HTTPS userinfo (user[:token]) is dropped — no secret reaches storage.
        assert_eq!(
            redact_remote("https://user:ghp_secret@github.com/acme/app.git"),
            "https://github.com/acme/app.git"
        );
        assert_eq!(
            redact_remote("https://token@github.com/acme/app"),
            "https://github.com/acme/app"
        );
        assert_eq!(redact_remote("ssh://git@host/acme/app"), "ssh://host/acme/app");
        // No credentials → unchanged.
        assert_eq!(
            redact_remote("https://github.com/acme/app.git"),
            "https://github.com/acme/app.git"
        );
        // scp-style: the `git@` user is an SSH login, not a secret — keep it.
        assert_eq!(redact_remote("git@github.com:acme/app.git"), "git@github.com:acme/app.git");
        // An `@` in the PATH (not the authority) is preserved.
        assert_eq!(redact_remote("https://host/acme/app@v2"), "https://host/acme/app@v2");
    }

    #[test]
    fn remote_url_reads_origin_or_none() {
        let repo = tmp("remote");
        init_repo(&repo).unwrap();
        assert_eq!(remote_url(&repo), None, "a fresh repo has no origin");
        git(
            &repo,
            &["remote", "add", "origin", "https://github.com/acme/app.git"],
        )
        .unwrap();
        assert_eq!(
            remote_url(&repo).as_deref(),
            Some("https://github.com/acme/app.git")
        );
        let _ = std::fs::remove_dir_all(&repo);
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
        add_worktree(&repo, "feat/base-test", &wt, &base).unwrap();
        let wt_head = git(&wt, &["rev-parse", "HEAD"]).unwrap();
        assert_eq!(
            wt_head, base_commit,
            "must branch from recorded base, not current HEAD"
        );
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
        add_worktree(&repo, "feat/bogus-base", &wt, "no-such-branch-xyz").unwrap();
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

    #[test]
    fn target_diff_is_pr_style_excludes_target_advance_includes_uncommitted() {
        let repo = tmp("tdiff");
        init_repo(&repo).unwrap();
        let base = current_branch(&repo).unwrap();
        // A tracked file on the base, then branch a worktree off that point.
        std::fs::write(repo.join("keep.txt"), "0\n").unwrap();
        git(&repo, &["add", "-A"]).unwrap();
        git(&repo, &["commit", "-q", "-m", "base file"]).unwrap();

        let wt = tmp("tdiff-wt");
        add_worktree(&repo, "feat/tdiff", &wt, &base).unwrap();

        // Task work: a committed new file, an uncommitted edit, and an untracked file.
        std::fs::write(wt.join("task.txt"), "task\n").unwrap();
        git(&wt, &["add", "task.txt"]).unwrap();
        git(&wt, &["commit", "-q", "-m", "task work"]).unwrap();
        std::fs::write(wt.join("keep.txt"), "0\nmod\n").unwrap(); // uncommitted edit
        std::fs::write(wt.join("untracked.txt"), "u\n").unwrap(); // untracked

        // The TARGET advances after the branch point with an unrelated commit.
        std::fs::write(repo.join("other.txt"), "other\n").unwrap();
        git(&repo, &["add", "-A"]).unwrap();
        git(&repo, &["commit", "-q", "-m", "base advances"]).unwrap();

        let td = target_diff(&wt, &base, false).unwrap();
        let paths: Vec<&str> = td.files.iter().map(|f| f.path.as_str()).collect();
        // The task's own changes — committed, uncommitted, and untracked — are all shown.
        assert!(paths.contains(&"task.txt"), "committed change missing: {paths:?}");
        assert!(paths.contains(&"keep.txt"), "uncommitted edit missing: {paths:?}");
        assert!(paths.contains(&"untracked.txt"), "untracked missing: {paths:?}");
        // The target's later unrelated commit must NOT appear (merge-base excludes it).
        assert!(!paths.contains(&"other.txt"), "target advance leaked: {paths:?}");
        assert!(td.patch.contains("task.txt"));
        assert!(!td.patch.contains("other.txt"));
        // No remote in the test → resolves to the local base branch.
        assert_eq!(td.resolved, base);

        // A remote-prefixed input (origin/<base>) normalizes to the same diff —
        // the `origin/` is stripped, not treated as a literal branch name.
        let td_prefixed = target_diff(&wt, &format!("origin/{base}"), false).unwrap();
        let prefixed_paths: Vec<&str> = td_prefixed.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(prefixed_paths, paths, "origin/<base> must normalize to <base>");

        let _ = remove_worktree(&repo, &wt);
        let _ = std::fs::remove_dir_all(&repo);
        let _ = std::fs::remove_dir_all(&wt);
    }

    #[test]
    fn target_fetch_uses_explicit_refspec_updates_remote_tracking() {
        // origin with main + a develop branch that has an extra commit.
        let origin = tmp("rfs-origin");
        init_repo(&origin).unwrap();
        let main = current_branch(&origin).unwrap();
        git(&origin, &["checkout", "-q", "-b", "develop"]).unwrap();
        std::fs::write(origin.join("d.txt"), "d\n").unwrap();
        git(&origin, &["add", "-A"]).unwrap();
        git(&origin, &["commit", "-q", "-m", "develop work"]).unwrap();
        git(&origin, &["checkout", "-q", &main]).unwrap();

        // Clone, then narrow remote.origin.fetch to main-only and drop
        // origin/develop — simulating a --single-branch clone.
        let clone = tmp("rfs-clone");
        git(
            &std::env::temp_dir(),
            &["clone", "-q", &origin.to_string_lossy(), &clone.to_string_lossy()],
        )
        .unwrap();
        git(
            &clone,
            &[
                "config",
                "remote.origin.fetch",
                &format!("+refs/heads/{main}:refs/remotes/origin/{main}"),
            ],
        )
        .unwrap();
        let _ = git(&clone, &["update-ref", "-d", "refs/remotes/origin/develop"]);
        assert!(
            !ref_resolves(&clone, "origin/develop"),
            "precondition: origin/develop pruned"
        );

        // target_diff(develop, fetch=true) must repopulate origin/develop via the
        // explicit refspec (the plain `git fetch origin develop` would land it in
        // FETCH_HEAD only under the narrowed mapping) and resolve to it.
        let td = target_diff(&clone, "develop", true).unwrap();
        assert_eq!(td.resolved, "origin/develop");

        let _ = std::fs::remove_dir_all(&origin);
        let _ = std::fs::remove_dir_all(&clone);
    }

    #[test]
    fn default_target_branch_prefers_base_ref_strips_origin() {
        let repo = tmp("dtb");
        init_repo(&repo).unwrap();
        assert_eq!(default_target_branch(&repo, "main"), "main");
        assert_eq!(default_target_branch(&repo, "origin/develop"), "develop");
        // Empty base_ref → detect from the repo; init_repo's branch resolves.
        let detected = default_target_branch(&repo, "");
        assert!(!detected.is_empty() && detected != "HEAD");
        // A "HEAD" base_ref (repo registered while detached) is NOT a branch —
        // must fall through to detection, never returning "HEAD".
        assert_ne!(default_target_branch(&repo, "HEAD"), "HEAD");
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn branch_name_follows_repo_short_prefix_style() {
        let existing = vec![
            "main".to_string(),
            "origin/feat/search".to_string(),
            "origin/feat/payments".to_string(),
            "fix/login".to_string(),
        ];
        assert_eq!(
            choose_branch_name_from_refs("feature", "Add Checkout Promo", &existing),
            "feat/add-checkout-promo"
        );
    }

    #[test]
    fn branch_name_follows_repo_long_prefix_style_and_dedups() {
        let existing = vec![
            "feature/add-checkout-promo".to_string(),
            "feature/add-checkout-promo-2".to_string(),
            "bugfix/login".to_string(),
        ];
        assert_eq!(
            choose_branch_name_from_refs("feature", "Add Checkout Promo", &existing),
            "feature/add-checkout-promo-3"
        );
        assert_eq!(
            choose_branch_name_from_refs("fix", "Login Timeout", &existing),
            "bugfix/login-timeout"
        );
    }

    #[test]
    fn bugfix_issue_kind_uses_fix_branch_style() {
        // UI/IM issues store kind = "bugfix"; it must follow the repo's fix-style
        // branch convention, not fall through to the feature/feat path.
        let short = vec![
            "main".to_string(),
            "fix/login".to_string(),
            "feat/search".to_string(),
        ];
        assert_eq!(
            choose_branch_name_from_refs("bugfix", "Crash On Logout", &short),
            "fix/crash-on-logout"
        );
        let long = vec!["bugfix/login".to_string(), "feature/search".to_string()];
        assert_eq!(
            choose_branch_name_from_refs("bugfix", "Crash On Logout", &long),
            "bugfix/crash-on-logout"
        );
    }

    #[test]
    fn reserved_branches_dedup_before_git_has_them() {
        // Two directions for the same issue/repo derive the same branch from the
        // same title with no git refs yet; the second must avoid the first's
        // reserved branch (choose_branch_name merges `reserved` into these refs).
        let no_refs: Vec<String> = vec![];
        let first = choose_branch_name_from_refs("feature", "Add Login", &no_refs);
        assert_eq!(first, "feature/add-login");
        let with_reserved = vec![first.clone()];
        let second = choose_branch_name_from_refs("feature", "Add Login", &with_reserved);
        assert_ne!(second, first);
        assert_eq!(second, "feature/add-login-2");
    }

    #[test]
    fn leaf_prefix_ref_falls_back_to_hyphen_separator() {
        // A repo with a branch literally named `feature` blocks the `feature/`
        // namespace (git D/F conflict), so the branch must not sit below it.
        let refs = vec!["main".to_string(), "feature".to_string()];
        let name = choose_branch_name_from_refs("feature", "Add Login", &refs);
        assert_eq!(name, "feature-add-login");
        assert!(!name.starts_with("feature/"));
    }

    #[test]
    fn branch_avoids_df_conflict_with_existing_nested_ref() {
        // `feat/login` exists as a directory ref → a new `feat/login` leaf can't be
        // created; the dedup must bump past it.
        let refs = vec!["feat/login/sub".to_string()];
        let name = choose_branch_name_from_refs("fix", "login", &refs);
        // prefix resolves to `fix` (no feat/fix refs counted), base `fix/login`, free.
        assert_eq!(name, "fix/login");
        // but a direct collision under an existing dir ref bumps:
        let refs2 = vec!["fix/login/old".to_string()];
        let n2 = choose_branch_name_from_refs("fix", "login", &refs2);
        assert_eq!(n2, "fix/login-2");
    }

    #[test]
    fn default_base_branch_prefers_remote_default_then_base_ref() {
        // origin whose default HEAD is `develop` (not the initial branch).
        let origin = tmp("dbb-origin");
        init_repo(&origin).unwrap();
        git(&origin, &["checkout", "-q", "-b", "develop"]).unwrap();
        git(&origin, &["commit", "-q", "--allow-empty", "-m", "d"]).unwrap();
        git(&origin, &["symbolic-ref", "HEAD", "refs/heads/develop"]).unwrap();
        let clone = tmp("dbb-clone");
        git(
            &std::env::temp_dir(),
            &["clone", "-q", &origin.to_string_lossy(), &clone.to_string_lossy()],
        )
        .unwrap();
        // The clone's origin/HEAD → origin/develop; the live remote default wins
        // over a stale recorded base_ref ("main").
        assert_eq!(default_base_branch(&clone, "main"), "develop");

        // A repo with no remote falls back to the recorded base_ref only if it RESOLVES...
        let local = tmp("dbb-local");
        init_repo(&local).unwrap();
        let cur = current_branch(&local).unwrap();
        assert_eq!(default_base_branch(&local, &cur), cur);
        // ...and falls through to main/master when the recorded base no longer exists.
        let fell = default_base_branch(&local, "deleted-xyz");
        assert!(fell == "main" || fell == "master", "stale base_ref must fall through, got {fell}");
        // ...and an empty/HEAD base_ref detects main/master, never returns "HEAD".
        let det = default_base_branch(&local, "");
        assert!(det == "main" || det == "master", "got {det}");
        assert_ne!(default_base_branch(&local, "HEAD"), "HEAD");

        let _ = std::fs::remove_dir_all(&origin);
        let _ = std::fs::remove_dir_all(&clone);
        let _ = std::fs::remove_dir_all(&local);
    }

    #[test]
    fn default_base_branch_falls_through_stale_origin_head() {
        // origin/HEAD points at a branch that no longer resolves → must NOT be returned.
        let origin = tmp("dbb-stale-origin");
        init_repo(&origin).unwrap();
        let clone = tmp("dbb-stale-clone");
        git(&std::env::temp_dir(), &["clone", "-q", &origin.to_string_lossy(), &clone.to_string_lossy()]).unwrap();
        // Point origin/HEAD at a dangling remote branch.
        git(&clone, &["symbolic-ref", "refs/remotes/origin/HEAD", "refs/remotes/origin/gone"]).unwrap();
        assert!(!ref_resolves(&clone, "origin/gone"), "precondition: dangling origin/HEAD target");
        let got = default_base_branch(&clone, "");
        assert_ne!(got, "gone", "stale origin/HEAD must not be used");
        assert!(ref_resolves(&clone, &got) || got == "main", "falls through to a real default, got {got}");
        let _ = std::fs::remove_dir_all(&origin);
        let _ = std::fs::remove_dir_all(&clone);
    }

    #[test]
    fn default_base_branch_prefers_main_over_recorded_feature_branch() {
        // No remote (no origin/HEAD). On a feature branch with main present, a blank
        // base must default to main, NOT the recorded feature branch.
        let repo = tmp("dbb-feature");
        init_repo(&repo).unwrap();
        let def = current_branch(&repo).unwrap(); // main or master
        git(&repo, &["checkout", "-q", "-b", "feature-x"]).unwrap();
        git(&repo, &["commit", "-q", "--allow-empty", "-m", "f"]).unwrap();
        // base_ref recorded as the feature branch (what register_repo would capture here).
        let got = default_base_branch(&repo, "feature-x");
        assert_eq!(got, def, "must prefer the integration branch over a recorded feature branch");
        assert_ne!(got, "feature-x");
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn add_worktree_synced_branches_off_fresh_origin() {
        // origin with develop ahead of main by one commit.
        let origin = tmp("aws-origin");
        init_repo(&origin).unwrap();
        let main = current_branch(&origin).unwrap();
        git(&origin, &["checkout", "-q", "-b", "develop"]).unwrap();
        std::fs::write(origin.join("d.txt"), "d\n").unwrap();
        git(&origin, &["add", "-A"]).unwrap();
        git(&origin, &["commit", "-q", "-m", "develop work"]).unwrap();
        let dev_commit = git(&origin, &["rev-parse", "HEAD"]).unwrap();
        git(&origin, &["checkout", "-q", &main]).unwrap();

        // single-branch clone (main only) → origin/develop absent until fetched.
        let clone = tmp("aws-clone");
        git(
            &std::env::temp_dir(),
            &["clone", "-q", "--single-branch", "--branch", &main,
              &origin.to_string_lossy(), &clone.to_string_lossy()],
        )
        .unwrap();
        assert!(!ref_resolves(&clone, "origin/develop"), "precondition");

        let wt = tmp("aws-wt");
        let add = add_worktree_synced(&clone, "feat/x", &wt, "develop", false).unwrap();
        assert!(add.synced, "branched off the synced origin/develop");
        assert!(add.created_branch, "a new branch was created");
        assert_eq!(git(&wt, &["rev-parse", "HEAD"]).unwrap(), dev_commit);

        let _ = remove_worktree(&clone, &wt);
        let _ = std::fs::remove_dir_all(&origin);
        let _ = std::fs::remove_dir_all(&clone);
        let _ = std::fs::remove_dir_all(&wt);
    }

    #[test]
    fn add_worktree_synced_falls_back_to_local_when_offline() {
        // No remote → fetch is a no-op, branch off the local base, signal Some(false).
        let repo = tmp("aws-offline");
        init_repo(&repo).unwrap();
        let base = current_branch(&repo).unwrap();
        let wt = tmp("aws-offline-wt");
        let add = add_worktree_synced(&repo, "feat/y", &wt, &base, false).unwrap();
        assert!(!add.synced, "offline → branched off local");
        assert!(wt.join(".git").exists());
        let _ = remove_worktree(&repo, &wt);
        let _ = std::fs::remove_dir_all(&repo);
        let _ = std::fs::remove_dir_all(&wt);
    }

    #[test]
    fn add_worktree_synced_stale_origin_without_fetch_is_not_synced() {
        // Clone has origin/main, then break the remote so fetch fails — the stale
        // origin ref still resolves, but we must NOT report it as freshly synced.
        let origin = tmp("stale-origin");
        init_repo(&origin).unwrap();
        let main = current_branch(&origin).unwrap();
        let clone = tmp("stale-clone");
        git(&std::env::temp_dir(),
            &["clone", "-q", &origin.to_string_lossy(), &clone.to_string_lossy()]).unwrap();
        assert!(ref_resolves(&clone, &format!("origin/{main}")), "precondition: origin ref present");
        // Break the remote so the fetch inside add_worktree_synced fails.
        git(&clone, &["remote", "set-url", "origin", "/nonexistent/repo.git"]).unwrap();
        let wt = tmp("stale-wt");
        let add = add_worktree_synced(&clone, "feat/s", &wt, &main, false).unwrap();
        assert!(!add.synced, "stale origin + failed fetch must not be 'synced'");
        let _ = remove_worktree(&clone, &wt);
        let _ = std::fs::remove_dir_all(&origin);
        let _ = std::fs::remove_dir_all(&clone);
        let _ = std::fs::remove_dir_all(&wt);
    }

    #[test]
    fn add_worktree_synced_explicit_unresolvable_base_errors() {
        // An explicit base that resolves to neither origin/<base> nor local must error
        // (require_resolvable=true), instead of silently using the repo default.
        let repo = tmp("explicit-bad");
        init_repo(&repo).unwrap();
        let wt = tmp("explicit-bad-wt");
        let res = add_worktree_synced(&repo, "feat/e", &wt, "no-such-branch-xyz", true);
        assert!(res.is_err(), "explicit unresolvable base must error");
        assert!(!wt.exists(), "no worktree created on error");
        let _ = std::fs::remove_dir_all(&repo);
        let _ = std::fs::remove_dir_all(&wt);
    }

    #[test]
    fn add_worktree_synced_existing_branch_creates_checkout_not_branch() {
        let repo = tmp("aws-existing-branch");
        init_repo(&repo).unwrap();
        let base = current_branch(&repo).unwrap();
        // Pre-create the branch so `worktree add -b <branch>` fails and falls back.
        git(&repo, &["branch", "feat/exists"]).unwrap();
        let wt = tmp("aws-existing-branch-wt");
        let add = add_worktree_synced(&repo, "feat/exists", &wt, &base, false).unwrap();
        assert!(add.created_checkout, "the fallback created a new checkout dir");
        assert!(!add.created_branch, "the branch pre-existed; we did not create it");
        assert!(wt.join(".git").exists());
        let _ = remove_worktree(&repo, &wt);
        let _ = std::fs::remove_dir_all(&repo);
        let _ = std::fs::remove_dir_all(&wt);
    }

    #[test]
    fn add_worktree_synced_empty_default_base_is_lenient() {
        // The default/empty path (require_resolvable=false) still falls back and creates,
        // even if the passed name doesn't resolve.
        let repo = tmp("lenient");
        init_repo(&repo).unwrap();
        let wt = tmp("lenient-wt");
        let add = add_worktree_synced(&repo, "feat/l", &wt, "no-such-default", false).unwrap();
        assert!(!add.synced);
        assert!(wt.join(".git").exists(), "default path still creates a worktree");
        let _ = remove_worktree(&repo, &wt);
        let _ = std::fs::remove_dir_all(&repo);
        let _ = std::fs::remove_dir_all(&wt);
    }
}
