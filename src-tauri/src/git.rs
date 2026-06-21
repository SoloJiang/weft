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
pub fn ref_resolves(dir: &Path, r: &str) -> bool {
    !r.is_empty()
        && Command::new("git")
            .args(["rev-parse", "--verify", "--quiet", &format!("{r}^{{commit}}")])
            .current_dir(dir)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
}

/// True when `t` is a FULL commit object id (any hash format — SHA-1 40-hex or SHA-256
/// 64-hex), as opposed to a branch name or an abbreviation. Uses git object identity, not a
/// hard-coded length: an all-hex string that `rev-parse --verify` resolves to a commit whose
/// canonical oid equals `t` itself (a branch/abbrev resolves to a DIFFERENT full oid).
pub fn is_full_commit_oid(repo: &Path, t: &str) -> bool {
    let t = t.trim();
    if t.is_empty() || !t.bytes().all(|c| c.is_ascii_hexdigit()) {
        return false;
    }
    match git(repo, &["rev-parse", "--verify", "-q", &format!("{t}^{{commit}}")]) {
        Ok(out) => out.trim() == t,
        Err(_) => false,
    }
}

/// The bare branch name a user means for the diff target: trimmed, with a
/// leading `origin/` (the remote the UI surfaces) stripped — so typing or
/// pasting `origin/main` behaves like `main` for BOTH the fetch refspec
/// (`git fetch origin main`, not the failing `git fetch origin origin/main`)
/// and ref resolution.
fn normalize_target(target: &str) -> String {
    let t = target.trim();
    // Strip a refs/remotes/origin/ prefix first: the remote base is now carried FULLY
    // QUALIFIED (refs/remotes/origin/<name>) so worktree-add + ancestry resolve the
    // remote-tracking ref unambiguously (a local branch literally named `origin/<name>`
    // can't shadow it), but the RECORDED branched_from must stay the bare branch name.
    let t = t.strip_prefix("refs/remotes/origin/").unwrap_or(t);
    // Strip a refs/heads/ prefix next: the local-branch base is stored QUALIFIED
    // (refs/heads/<name>) so worktree-add + ancestry use an unambiguous ref, but the
    // RECORDED branched_from must stay the bare branch name.
    let t = t.strip_prefix("refs/heads/").unwrap_or(t);
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
    // Conventional integration branch. Qualify the LOCAL check as `refs/heads/<c>` so a
    // same-named TAG (e.g. a `main` tag in a repo whose real default is `master`) can't
    // masquerade as the branch; qualify the REMOTE check as `refs/remotes/origin/<c>` so a
    // local branch literally named `origin/<c>` (refs/heads/origin/<c>) can't shadow the
    // remote-tracking ref and wrongly pin main/master.
    for c in ["main", "master"] {
        if ref_resolves(repo, &format!("refs/heads/{c}"))
            || ref_resolves(repo, &format!("refs/remotes/origin/{c}"))
        {
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

/// Resolve the ref to compare a target branch against. When both `origin/<target>` and
/// a local `<target>` exist and have DIVERGED, pick the one the WORKTREE actually forked
/// from: its merge-base with the worktree's HEAD is the more recent (the fork point),
/// while the diverged ref's merge-base is an older common ancestor. This is correct in
/// BOTH directions — a worktree branched off a fresh `origin/<target>` (with a diverged
/// local) compares against origin, and one branched off a local `<target>` (with a stale
/// origin) compares against local. On ties (no divergence) prefer the fetched remote.
/// Else fall back through the default-branch chain (origin/HEAD → main → master → HEAD).
fn resolve_target_ref(worktree: &Path, target: &str) -> String {
    let t = normalize_target(target);
    // "HEAD" is not a real target branch (see default_target_branch); falling
    // through to the default chain avoids merge-base(HEAD, HEAD) hiding commits.
    if !t.is_empty() && t != "HEAD" {
        let remote = format!("origin/{t}");
        let remote_ok = ref_resolves(worktree, &remote);
        let local_ok = ref_resolves(worktree, &t);
        if remote_ok && local_ok {
            // Compare each ref's fork point with the worktree: the ref the worktree
            // descends from has the more recent merge-base; prefer it.
            let mb_remote = git(worktree, &["merge-base", "HEAD", &remote]).ok();
            let mb_local = git(worktree, &["merge-base", "HEAD", &t]).ok();
            if let (Some(mr), Some(ml)) = (mb_remote, mb_local) {
                // Forked from origin when local's merge-base is an ancestor of origin's
                // (origin's fork point is at least as recent); covers the no-divergence
                // tie too.
                let forked_from_origin =
                    git(worktree, &["merge-base", "--is-ancestor", &ml, &mr]).is_ok();
                return if forked_from_origin { remote } else { t };
            }
        }
        if remote_ok {
            return remote;
        }
        if local_ok {
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
/// if its REMOTE-tracking ref `origin/<name>` still resolves — validated against the
/// remote ref, NOT a coincidentally same-named local branch, so a stale origin/HEAD
/// can't pin an old local feature branch as the "remote default"; (2) the conventional
/// integration branch main/master (local or origin/) — a repo added on a feature branch
/// without origin/HEAD must not default to that feature branch; (3) the recorded
/// `base_ref` if it resolves (non-standard repos, e.g. a "trunk"/"develop" default with
/// no origin/HEAD and no main/master); (4) "main". Returns a bare branch name.
pub fn default_base_branch(repo: &Path, base_ref: &str) -> String {
    if let Some(name) = remote_default_branch(repo) {
        // Validate the cached origin/HEAD against its REMOTE-tracking ref only: a local
        // branch that happens to share the name must not vouch for a stale origin/HEAD
        // whose upstream branch is gone. Qualify as `refs/remotes/origin/<name>` so a local
        // branch literally named `origin/<name>` can't masquerade as the remote ref.
        if ref_resolves(repo, &format!("refs/remotes/origin/{name}")) {
            return name;
        }
    }
    for c in ["main", "master"] {
        if ref_resolves(repo, &format!("refs/heads/{c}"))
            || ref_resolves(repo, &format!("refs/remotes/origin/{c}"))
        {
            return c.to_string();
        }
    }
    let b = base_ref.trim();
    let b = b.strip_prefix("origin/").unwrap_or(b);
    if !b.is_empty()
        && b != "HEAD"
        && (ref_resolves(repo, &format!("refs/heads/{b}"))
            || ref_resolves(repo, &format!("refs/remotes/origin/{b}")))
    {
        return b.to_string();
    }
    "main".to_string()
}

/// The default base for the OFFLINE fallback (when `live_default_branch` is
/// unavailable): prefer the recorded `base_ref` when it was captured as the repo's
/// real DEFAULT branch (`is_default`) AND it still resolves (local `<base_ref>` or
/// remote `origin/<base_ref>`) — a genuinely-captured default is more trustworthy
/// than a possibly stale cached `origin/HEAD`. `is_default` is the load-bearing
/// signal: a LEGACY base_ref (the pre-marker current-branch capture on an upgraded
/// DB) is NOT trusted even when it resolves, because a pushed legacy feature branch
/// whose `origin/<base_ref>` exists is indistinguishable BY VALUE from a real
/// non-standard default — trusting it would pin new work to that legacy branch.
/// Otherwise fall through to `default_base_branch` (cached origin/HEAD → main/master
/// → a resolvable base_ref → main). Returns a bare branch name (no `origin/`).
pub fn recorded_base_or_default(repo: &Path, base_ref: &str, is_default: bool) -> String {
    let b = base_ref.trim();
    let b = b.strip_prefix("origin/").unwrap_or(b);
    if is_default
        && !b.is_empty()
        && b != "HEAD"
        && (ref_resolves(repo, &format!("refs/heads/{b}"))
            || ref_resolves(repo, &format!("refs/remotes/origin/{b}")))
    {
        return b.to_string();
    }
    default_base_branch(repo, base_ref)
}

/// The remote's CURRENT default branch name via `git ls-remote --symref origin HEAD`
/// — queries the remote directly, so it works even for narrowed/single-branch clones
/// where the default moved to an unfetched branch (unlike `git remote set-head --auto`,
/// which needs the ref already fetched). None offline / no remote / parse miss.
/// GIT_TERMINAL_PROMPT=0 fails fast instead of prompting.
pub fn live_default_branch(repo: &Path) -> Option<String> {
    let out = Command::new("git")
        .args(["ls-remote", "--symref", "origin", "HEAD"])
        .current_dir(repo)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout);
    // First line looks like: "ref: refs/heads/<branch>\tHEAD"
    for line in s.lines() {
        if let Some(rest) = line.strip_prefix("ref: ") {
            if let Some(refname) = rest.split_whitespace().next() {
                let name = refname.strip_prefix("refs/heads/").unwrap_or(refname);
                if !name.is_empty() {
                    return Some(name.to_string());
                }
            }
        }
    }
    None
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
    // Prune stale registrations (a dir removed out-of-band) so `worktree add` recreates
    // instead of failing on the leftover registration; safe no-op when none are stale.
    git(repo, &["worktree", "prune"]).ok();
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
    // Source side is branch-namespaced (refs/heads/<b>) so ONLY a branch named `b`
    // on origin lands in refs/remotes/origin/<b>; a same-named TAG no longer leaks
    // into the remote-tracking branch namespace and can't be mistaken for a base.
    let refspec = format!("+refs/heads/{b}:refs/remotes/origin/{b}");
    Command::new("git")
        .args(["fetch", "--quiet", "origin", &refspec])
        .current_dir(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Outcome of `add_worktree_synced`. `created_checkout`/`created_branch` drive
/// rollback: remove the checkout when WE created it; delete the branch only when WE
/// created it (a pre-existing branch reused by the fallback must survive). `synced`
/// is true only when it branched off a freshly-fetched `origin/<base>` (false = a
/// local/stale ref, an existing-branch checkout, or a reused path).
/// `branched_from` is the normalized ref we actually branched off (non-empty only
/// on the `-b` success path where we created a new branch); empty for the
/// existing-branch fallback and the path-reuse path.
pub struct WorktreeAdd {
    pub path: PathBuf,
    pub created_checkout: bool,
    pub created_branch: bool,
    pub synced: bool,
    pub branched_from: String,
}

/// True when `worktree_path` is a worktree REGISTERED TO `repo` and checked out on
/// `branch` — i.e. `list_worktrees(repo)` contains an entry whose canonicalized path
/// equals `canonicalize(worktree_path)` AND whose branch equals `branch`. Checking only
/// for a `.git` + a matching HEAD name is not enough: a stale plain dir, a checkout for a
/// DIFFERENT branch, OR a checkout belonging to a DIFFERENT repository could sit at a
/// deterministic path; `list_worktrees` reads THIS repo's own registry, so anything not
/// registered here is refused. Path comparison is by canonicalized form so a
/// symlinked/`..` spelling still matches.
pub fn is_registered_worktree(repo: &Path, worktree_path: &Path, branch: &str) -> bool {
    let want = std::fs::canonicalize(worktree_path).ok();
    list_worktrees(repo)
        .unwrap_or_default()
        .into_iter()
        .any(|(p, b)| b == branch && std::fs::canonicalize(&p).ok() == want)
}

/// True when `branch` DESCENDS FROM `base` in `repo` — i.e.
/// `git merge-base --is-ancestor <base> <branch>` succeeds (base is an ancestor of
/// branch). Used to verify a reused branch was actually forked from a requested
/// explicit base before recording that base.
pub fn branch_descends_from(repo: &Path, branch: &str, base: &str) -> bool {
    // Qualify the DESCENDANT (work-branch) side as `refs/heads/<branch>` so a TAG sharing
    // the branch name can't shadow the real branch (bare `<branch>` resolves a same-named
    // tag first), which would let a reset branch be accepted as based on `base`.
    git(repo, &["merge-base", "--is-ancestor", base, &format!("refs/heads/{branch}")]).is_ok()
}

/// Branch-qualified existence check: true only when `refs/heads/<branch>` resolves — a bare
/// `ref_resolves(repo, branch)` would also accept a same-named TAG, so use this where the
/// question is specifically "does the local BRANCH exist?".
pub fn local_branch_exists(repo: &Path, branch: &str) -> bool {
    ref_resolves(repo, &format!("refs/heads/{branch}"))
}

/// Does `branch` descend from the explicit base NAME, resolved to the ref the create path
/// would (or did) fork from? PREFERS the FULLY-QUALIFIED `refs/remotes/origin/<base>` when it
/// resolves, falling back to the LOCAL `refs/heads/<base>` only when the remote is absent
/// (single-branch / local-only). Returns Some(true)/Some(false) for the preferred form, and
/// None when neither resolves (base gone → caller skips).
pub fn branch_descends_from_base(repo: &Path, branch: &str, base: &str) -> Option<bool> {
    let t = normalize_target(base);
    if t.is_empty() || t == "HEAD" {
        return None;
    }
    // Mirror the create path's base preference so reuse validates against the SAME ref the lane
    // was (or would be) forked from. Prefer the FULLY-QUALIFIED refs/remotes/origin/<t> when it
    // resolves — (R43-2) a local branch literally named `origin/<t>` would shadow a short
    // `origin/<t>`, and (R43-1) when origin/<t> is ahead of a stale local <t>, accepting a branch
    // that only descends from the stale local ref dispatches it as <t>-based while MISSING origin's
    // commits. Fall back to local refs/heads/<t> only when the remote is absent (single-branch /
    // local-only). None when neither resolves (base gone → caller skips).
    let remote = format!("refs/remotes/origin/{t}");
    if ref_resolves(repo, &remote) {
        return Some(branch_descends_from(repo, branch, &remote));
    }
    let local = format!("refs/heads/{t}");
    if ref_resolves(repo, &local) {
        return Some(branch_descends_from(repo, branch, &local));
    }
    None
}

/// Like `add_worktree`, but first best-effort fetches `base_name` from origin and
/// branches the new worktree off the FRESH `origin/<base_name>` when possible (see
/// `WorktreeAdd` for what it reports). `require_resolvable` = the base was an explicit
/// user/lead choice: if it resolves to neither `origin/<base>` nor local `<base>`
/// (even after fetch), return an error rather than silently using the repo default.
/// When false (empty/default base), fall back through the default-branch chain so the
/// worktree is still created.
pub fn add_worktree_synced(
    repo: &Path,
    branch: &str,
    worktree_path: &Path,
    base_name: &str,
    require_resolvable: bool,
) -> Result<WorktreeAdd> {
    let fetched = fetch_origin_branch(repo, base_name);
    let t = normalize_target(base_name);
    // FULLY-QUALIFIED remote-tracking ref (refs/remotes/origin/<t>), not the short `origin/<t>`:
    // a LOCAL branch literally named `origin/<t>` (refs/heads/origin/<t>) would otherwise shadow
    // the short form and be branched off instead of the freshly-fetched remote tip (R43-2). The
    // qualified ref is unambiguous for the start-point, the ancestry base, and the `synced` check.
    let remote = format!("refs/remotes/origin/{t}");
    let nonempty = !t.is_empty() && t != "HEAD";
    let resolved_opt = if nonempty && fetched && ref_resolves(repo, &remote) {
        // A freshly-fetched remote ref — trustworthy.
        Some(remote.clone())
    } else if nonempty && ref_resolves(repo, &format!("refs/heads/{t}")) {
        // A local BRANCH the user already has. Qualified as refs/heads/<t> so a local
        // TAG named `t` is NOT accepted as a base (bare `ref_resolves(t)` is true for a
        // tag too). Store the QUALIFIED ref (not bare `t`): it's the `worktree add -b … <ref>`
        // start-point and the ancestry base, so a repo with BOTH a branch and a tag named `t`
        // resolves unambiguously to the branch instead of failing on an ambiguous short ref.
        // The remote-tracking ref is carried as refs/remotes/origin/<t> for the same reason.
        Some(format!("refs/heads/{t}"))
    } else if nonempty && !require_resolvable && ref_resolves(repo, &remote) {
        // A STALE remote-tracking ref (the fetch failed — offline, or the branch was
        // deleted upstream). Trusted only for the lenient default/empty path; an
        // EXPLICIT base must be currently confirmable (fresh fetch or a local branch),
        // so a stale-only `origin/<base>` is rejected below rather than silently
        // branching off a possibly-deleted branch.
        Some(remote.clone())
    } else {
        None
    };
    // An explicit base must resolve to a CURRENT ref — even when we're about to reuse
    // an existing checkout — so a misspelled OR stale (deleted-upstream) explicit base
    // can't be silently accepted via an orphan path or a stale remote-tracking ref.
    if require_resolvable && resolved_opt.is_none() {
        bail!("base branch {base_name:?} not found locally or on origin (after fetch)");
    }
    if worktree_path.exists() {
        // Validate the existing path is a worktree REGISTERED TO THIS REPO on `branch`
        // before reusing it (a stale plain dir, a different-branch checkout, or a foreign
        // repo's checkout could sit at this deterministic path and be handed to the worker,
        // running the agent in the wrong tree/repo — dispatch only checks the dir exists).
        if !is_registered_worktree(repo, worktree_path, branch) {
            bail!(
                "worktree path {} exists but is not a worktree of this repo on {branch:?}",
                worktree_path.display()
            );
        }
        // The registered branch must also DESCEND from the resolved base — mirroring the
        // `-b` fallback's ancestry check. A checkout registered for this branch but forked
        // elsewhere (e.g. `feat/x` off main, now reused for a `release` lane) would otherwise
        // record base=release while the worktree sits on main's line. Validate WHENEVER the
        // resolved base RESOLVES as a ref, regardless of require_resolvable: a BLANK-base lane
        // (require_resolvable=false) still records the CURRENT resolved default as its base, so
        // a reused branch not descending from it is just as wrong. `resolved_opt` is None for a
        // blank base, so fall back to resolve_base_ref the same way the create path does. Skip
        // when the base is HEAD or doesn't resolve (e.g. base gone): a surviving registered
        // checkout can still be reused when its base is gone.
        let resolved_base = resolved_opt
            .clone()
            .unwrap_or_else(|| resolve_base_ref(repo, &t));
        if resolved_base != "HEAD"
            && ref_resolves(repo, &resolved_base)
            && !branch_descends_from(repo, branch, &resolved_base)
        {
            bail!(
                "branch {branch:?} already checked out but is not based on {resolved_base:?}; \
                 refusing to record a mismatched base"
            );
        }
        return Ok(WorktreeAdd {
            path: worktree_path.to_path_buf(),
            created_checkout: false,
            created_branch: false,
            synced: false,
            branched_from: String::new(),
        });
    }
    let resolved = resolved_opt.unwrap_or_else(|| resolve_base_ref(repo, &t));
    // Fresh only if we branched off the remote-tracking ref AND the fetch actually succeeded.
    let synced = resolved.starts_with("refs/remotes/origin/") && fetched;
    if let Some(parent) = worktree_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let path_str = worktree_path.to_string_lossy().to_string();
    // A worktree dir removed out-of-band leaves a prunable registration; without this
    // the branch still looks checked out and `worktree add` fails ("missing but already
    // registered") instead of recreating. Prune stale registrations first — a no-op when
    // none are stale, and it never touches a live worktree whose dir still exists.
    git(repo, &["worktree", "prune"]).ok();
    let res = git(repo, &["worktree", "add", "-b", branch, &path_str, &resolved]);
    if res.is_err() {
        // Branch likely already exists (the name appeared after weft chose it). Verify the
        // pre-existing branch actually descends from the resolved base before reusing it —
        // otherwise the lane would be recorded as based on `resolved` while its worktree sits
        // on a branch forked elsewhere. Validate WHENEVER the resolved base RESOLVES as a ref,
        // regardless of require_resolvable: a BLANK base whose resolved default resolves (the
        // common case) must still reject a branch forked off a DIFFERENT base. Skip only when
        // the base is HEAD or doesn't resolve (e.g. detached / base gone), where a surviving
        // pre-existing branch can still be checked out.
        if resolved != "HEAD"
            && ref_resolves(repo, &resolved)
            && !branch_descends_from(repo, branch, &resolved)
        {
            bail!(
                "branch {branch:?} already exists but is not based on {resolved:?}; \
                 refusing to record a mismatched base"
            );
        }
        // Check the existing branch out into a new worktree dir.
        git(repo, &["worktree", "add", &path_str, branch])
            .context("worktree add (existing branch)")?;
        return Ok(WorktreeAdd {
            path: worktree_path.to_path_buf(),
            created_checkout: true,
            created_branch: false,
            synced: false,
            branched_from: String::new(),
        });
    }
    Ok(WorktreeAdd {
        path: worktree_path.to_path_buf(),
        created_checkout: true,
        created_branch: true,
        synced,
        branched_from: normalize_target(&resolved),
    })
}

/// Remove a worktree and prune. (Used by M2 worktree lifecycle management.)
pub fn remove_worktree(repo: &Path, worktree_path: &Path) -> Result<()> {
    let path_str = worktree_path.to_string_lossy().to_string();
    git(repo, &["worktree", "remove", "--force", &path_str]).ok();
    git(repo, &["worktree", "prune"]).ok();
    Ok(())
}

/// Mark `worktree_path` as a LOCKED git worktree of `repo` (`git worktree lock`).
/// Used to DURABLY preserve a reused (non-weft-created) checkout: it stays a real,
/// usable git worktree (its `.git` pointer is kept), and the lock protects it from
/// the orphan-worktree GC — `gc::sweep_repo` skips locked entries so the preserved
/// checkout survives even after its weft DB row is dropped (and across a repo re-add,
/// since the lock lives in the repo's git metadata, not weft's DB). Idempotent: an
/// "already locked" error is treated as success. Best-effort otherwise.
pub fn lock_worktree(repo: &Path, worktree_path: &Path) -> Result<()> {
    let path_str = worktree_path.to_string_lossy().to_string();
    // `git worktree lock` errors if the worktree is already locked — that's the
    // desired end state, so swallow it (and any other error: locking is a
    // best-effort hardening, never a hard failure of teardown).
    git(repo, &["worktree", "lock", &path_str]).ok();
    Ok(())
}

/// Whether git considers `worktree_path` a LOCKED worktree of `repo`. Parses the
/// `locked` line (optionally with a reason) that `git worktree list --porcelain`
/// emits for a locked entry, following its `worktree`/`HEAD`/`branch` block — the
/// same position `prunable` occupies in [`list_worktrees`]. Path comparison is by
/// canonicalized form so a symlinked/`..` spelling still matches. Best-effort:
/// false on any git error.
pub fn is_worktree_locked(repo: &Path, worktree_path: &Path) -> bool {
    let want =
        std::fs::canonicalize(worktree_path).unwrap_or_else(|_| worktree_path.to_path_buf());
    let Ok(out) = git(repo, &["worktree", "list", "--porcelain"]) else {
        return false;
    };
    let mut cur_match = false;
    for line in out.lines() {
        if let Some(p) = line.strip_prefix("worktree ") {
            let entry =
                std::fs::canonicalize(p.trim()).unwrap_or_else(|_| PathBuf::from(p.trim()));
            cur_match = entry == want;
        } else if cur_match && (line == "locked" || line.starts_with("locked ")) {
            return true;
        }
    }
    false
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
    let mut branch: Option<String> = None;
    let mut prunable = false;
    // Each porcelain entry is `worktree <p>` / `HEAD <sha>` / `branch <ref>` (or
    // `detached`) / optional `prunable <reason>`. The `prunable` line FOLLOWS `branch`, so
    // flush an entry only at the NEXT `worktree` (or EOF) — and SKIP prunable ones: a
    // registration left behind by an out-of-band `rm -rf` still lists here, but its path
    // is no longer a real worktree and must not be matched/reused.
    for line in out.lines() {
        if let Some(p) = line.strip_prefix("worktree ") {
            if let (Some(pp), Some(bb)) = (path.take(), branch.take()) {
                if !prunable {
                    res.push((pp, bb));
                }
            }
            path = Some(p.to_string());
            branch = None;
            prunable = false;
        } else if let Some(b) = line.strip_prefix("branch ") {
            branch = Some(b.strip_prefix("refs/heads/").unwrap_or(b).to_string());
        } else if line == "prunable" || line.starts_with("prunable ") {
            prunable = true;
        }
    }
    if let (Some(pp), Some(bb)) = (path.take(), branch.take()) {
        if !prunable {
            res.push((pp, bb));
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

/// FULL 40-char HEAD commit sha. Used where the value is PERSISTED as a stable
/// branch-off point (e.g. a detached-HEAD lane's diff target): a short sha can grow
/// ambiguous as the repo accumulates commits, breaking later ref resolution, so the
/// stored target must be the unabbreviated sha. None on an empty repo (no HEAD yet).
pub fn head_commit_full(repo: &Path) -> Option<String> {
    let out = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repo)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty()).then_some(s)
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
    fn is_full_commit_oid_distinguishes_oid_from_branch_and_abbrev() {
        // R40-2: a FULL commit oid is identified by git object identity, not a length.
        let repo = tmp("oid-sha1");
        init_repo(&repo).unwrap();
        let base = current_branch(&repo).unwrap();
        let full = git(&repo, &["rev-parse", "HEAD"]).unwrap();
        assert_eq!(full.len(), 40, "precondition: SHA-1 repo → 40-hex oid");
        // A full HEAD oid → true.
        assert!(is_full_commit_oid(&repo, &full), "full HEAD oid must be recognized");
        // A branch name → false (it resolves to a commit, but to a DIFFERENT canonical oid).
        assert!(!is_full_commit_oid(&repo, &base), "a branch name is not a full oid");
        // A 7-char abbreviation of HEAD → false (rev-parse returns the full oid ≠ the abbrev).
        let abbrev = &full[..7];
        assert!(!is_full_commit_oid(&repo, abbrev), "an abbreviation is not a full oid");
        // A non-hex string → false (never even reaches rev-parse).
        assert!(!is_full_commit_oid(&repo, "develop"), "a non-hex name is not a full oid");
        // Empty / whitespace → false.
        assert!(!is_full_commit_oid(&repo, "   "), "empty is not a full oid");
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn is_full_commit_oid_matches_sha256_repo() {
        // R40-2: in a SHA-256 repo the oid is 64-hex; the helper must recognize it (the old
        // len()==40 predicate never would). Guard: skip if this git can't init a sha256 repo.
        let repo = tmp("oid-sha256");
        let _ = std::fs::create_dir_all(&repo);
        if git(&repo, &["init", "-q", "--object-format=sha256"]).is_err() {
            eprintln!("SKIP is_full_commit_oid_matches_sha256_repo: git lacks --object-format=sha256");
            let _ = std::fs::remove_dir_all(&repo);
            return;
        }
        git(&repo, &["config", "user.email", "t@t.t"]).unwrap();
        git(&repo, &["config", "user.name", "t"]).unwrap();
        git(&repo, &["commit", "-q", "--allow-empty", "-m", "init"]).unwrap();
        let full = git(&repo, &["rev-parse", "HEAD"]).unwrap();
        assert_eq!(full.len(), 64, "precondition: SHA-256 repo → 64-hex oid");
        assert!(is_full_commit_oid(&repo, &full), "a full 64-hex SHA-256 oid must be recognized");
        // A branch name is still not an oid even in a sha256 repo.
        let base = current_branch(&repo).unwrap();
        assert!(!is_full_commit_oid(&repo, &base), "a branch name is not a full oid (sha256)");
        let _ = std::fs::remove_dir_all(&repo);
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
    fn default_base_branch_ignores_local_branch_for_stale_origin_head() {
        // origin/HEAD points at a branch whose REMOTE-tracking ref is gone, but a LOCAL
        // branch shares the name — the local branch must NOT vouch for the stale
        // origin/HEAD (else new work branches off an old local feature branch).
        let origin = tmp("dbb-localvouch-origin");
        init_repo(&origin).unwrap();
        let main = current_branch(&origin).unwrap();
        let clone = tmp("dbb-localvouch-clone");
        git(
            &std::env::temp_dir(),
            &["clone", "-q", &origin.to_string_lossy(), &clone.to_string_lossy()],
        )
        .unwrap();
        git(&clone, &["branch", "feature-x"]).unwrap(); // local branch only
        git(&clone, &["symbolic-ref", "refs/remotes/origin/HEAD", "refs/remotes/origin/feature-x"]).unwrap();
        assert!(ref_resolves(&clone, "feature-x"), "precondition: local feature-x exists");
        assert!(!ref_resolves(&clone, "origin/feature-x"), "precondition: remote feature-x gone");
        assert_eq!(
            default_base_branch(&clone, ""),
            main,
            "a same-named LOCAL branch must not validate a stale origin/HEAD"
        );
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
    fn default_base_branch_ignores_tag_named_main_prefers_real_master() {
        // R41-2: a repo whose real integration branch is `master`, with a TAG named `main`
        // but NO `main` BRANCH and no origin/HEAD. The bare `ref_resolves(repo, "main")`
        // resolves the TAG, so the old code wrongly selects "main"; qualifying the local
        // check to `refs/heads/main` ignores the tag and selects the real "master".
        let repo = tmp("dbb-tag-main");
        init_repo(&repo).unwrap();
        // Ensure the integration branch is `master` (rename whatever init produced), so there
        // is definitely NO `main` branch present.
        git(&repo, &["branch", "-m", "master"]).unwrap();
        assert!(ref_resolves(&repo, "refs/heads/master"), "precondition: master branch exists");
        assert!(!ref_resolves(&repo, "refs/heads/main"), "precondition: no main branch");
        // A TAG named `main` (points at master's tip) — bare `main` now resolves to it.
        git(&repo, &["tag", "main"]).unwrap();
        assert!(ref_resolves(&repo, "main"), "precondition: bare `main` resolves (to the tag)");
        assert!(ref_resolves(&repo, "refs/tags/main"), "precondition: refs/tags/main exists");
        assert_eq!(
            default_base_branch(&repo, ""),
            "master",
            "a TAG named main must not shadow the real master integration branch"
        );
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn default_base_branch_ignores_local_branch_named_origin_main() {
        // R45-1: the remote check in the main/master fallback must be qualified to
        // `refs/remotes/origin/<c>`. A repo whose real default is `develop`, with NO
        // `main`/`master` branch and NO origin/HEAD, but WITH a LOCAL branch literally
        // named `origin/main` (refs/heads/origin/main). The old short `origin/main` check
        // resolves that local branch and wrongly records "main"; the qualified remote check
        // does not, so it falls through to the resolvable recorded base `develop`.
        let repo = tmp("dbb-localoriginmain");
        init_repo(&repo).unwrap();
        // Make the real integration branch `develop`; ensure no main/master branch exists.
        git(&repo, &["branch", "-m", "develop"]).unwrap();
        assert!(ref_resolves(&repo, "refs/heads/develop"), "precondition: develop branch exists");
        assert!(!ref_resolves(&repo, "refs/heads/main"), "precondition: no main branch");
        assert!(!ref_resolves(&repo, "refs/heads/master"), "precondition: no master branch");
        // A LOCAL branch literally named `origin/main` — the short `origin/main` resolves it.
        git(&repo, &["branch", "origin/main", "develop"]).unwrap();
        assert!(
            ref_resolves(&repo, "origin/main"),
            "precondition: short `origin/main` resolves (to the local branch)"
        );
        assert!(
            !ref_resolves(&repo, "refs/remotes/origin/main"),
            "precondition: no remote-tracking origin/main"
        );
        let got = default_base_branch(&repo, "develop");
        assert_ne!(got, "main", "a LOCAL branch named origin/main must not pin main as the default");
        assert_eq!(got, "develop", "must fall through to the resolvable recorded base develop");
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn resolve_base_ref_ignores_tag_named_main_prefers_real_master() {
        // Same tag-shadow class as default_base_branch (preemptive): resolve_base_ref's
        // conventional main/master fallback must qualify the LOCAL check as refs/heads/<c>
        // so a TAG `main` (no `main` branch; real default `master`) can't masquerade as main.
        let repo = tmp("rbr-tag-main");
        init_repo(&repo).unwrap();
        git(&repo, &["branch", "-m", "master"]).unwrap();
        assert!(ref_resolves(&repo, "refs/heads/master"), "precondition: master branch exists");
        assert!(!ref_resolves(&repo, "refs/heads/main"), "precondition: no main branch");
        git(&repo, &["tag", "main"]).unwrap();
        assert!(ref_resolves(&repo, "main"), "precondition: bare `main` resolves (to the tag)");
        // Empty recorded base_ref + no origin/HEAD → falls through to the main/master loop.
        assert_eq!(
            resolve_base_ref(&repo, ""),
            "master",
            "a TAG named main must not shadow the real master in resolve_base_ref"
        );
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
    fn add_worktree_synced_explicit_stale_deleted_upstream_base_errors() {
        // An EXPLICIT base whose origin/<base> is stale (the branch was deleted
        // upstream, so the fetch fails) must error rather than silently branching off
        // the stale remote-tracking ref. require_resolvable=true.
        let origin = tmp("stale-explicit-origin");
        init_repo(&origin).unwrap();
        git(&origin, &["branch", "develop"]).unwrap();
        // Full clone → origin/develop present as a remote-tracking ref; no LOCAL develop.
        let clone = tmp("stale-explicit-clone");
        git(
            &std::env::temp_dir(),
            &["clone", "-q", &origin.to_string_lossy(), &clone.to_string_lossy()],
        )
        .unwrap();
        assert!(ref_resolves(&clone, "origin/develop"), "precondition: origin ref present");
        assert!(!ref_resolves(&clone, "develop"), "precondition: no local develop");
        // Delete develop upstream so a fetch of it fails (origin/develop goes stale).
        git(&origin, &["branch", "-D", "develop"]).unwrap();
        let wt = tmp("stale-explicit-wt");
        let res = add_worktree_synced(&clone, "feat/s", &wt, "develop", true);
        assert!(res.is_err(), "explicit base deleted upstream (stale origin) must error");
        assert!(!wt.exists(), "no worktree created on error");
        let _ = std::fs::remove_dir_all(&origin);
        let _ = std::fs::remove_dir_all(&clone);
        let _ = std::fs::remove_dir_all(&wt);
    }

    #[test]
    fn resolve_target_ref_picks_the_ref_the_worktree_forked_from() {
        // resolve_target_ref runs against a WORKTREE's HEAD; when local & origin <target>
        // diverge, it must pick whichever the worktree ACTUALLY forked from.
        let origin = tmp("rtr-origin");
        init_repo(&origin).unwrap();
        let main = current_branch(&origin).unwrap();
        git(&origin, &["checkout", "-q", "-b", "develop"]).unwrap();
        git(&origin, &["commit", "-q", "--allow-empty", "-m", "d0"]).unwrap();
        git(&origin, &["checkout", "-q", &main]).unwrap();

        // (A) Worktree forked from LOCAL develop; origin/develop is stale-behind → LOCAL.
        let c1 = tmp("rtr-c1");
        git(&std::env::temp_dir(), &["clone", "-q", &origin.to_string_lossy(), &c1.to_string_lossy()]).unwrap();
        // A clone does not inherit local user config, and CI has no global identity.
        git(&c1, &["config", "user.email", "t@t.t"]).unwrap();
        git(&c1, &["config", "user.name", "t"]).unwrap();
        git(&c1, &["checkout", "-q", "-b", "develop", "origin/develop"]).unwrap();
        git(&c1, &["commit", "-q", "--allow-empty", "-m", "local-ahead"]).unwrap(); // local past stale origin
        git(&c1, &["checkout", "-q", &main]).unwrap();
        let wt1 = tmp("rtr-wt1");
        git(&c1, &["worktree", "add", "-q", "-b", "feat/a", &wt1.to_string_lossy(), "develop"]).unwrap();
        git(&wt1, &["commit", "-q", "--allow-empty", "-m", "agent-a"]).unwrap();
        assert_eq!(
            resolve_target_ref(&wt1, "develop"),
            "develop",
            "worktree forked off local develop (stale origin) → prefer local"
        );

        // (B) Worktree forked from fresh ORIGIN/develop; local develop diverged → ORIGIN.
        git(&origin, &["checkout", "-q", "develop"]).unwrap();
        git(&origin, &["commit", "-q", "--allow-empty", "-m", "d1"]).unwrap();
        git(&origin, &["checkout", "-q", &main]).unwrap();
        let c2 = tmp("rtr-c2");
        git(&std::env::temp_dir(), &["clone", "-q", &origin.to_string_lossy(), &c2.to_string_lossy()]).unwrap();
        git(&c2, &["config", "user.email", "t@t.t"]).unwrap();
        git(&c2, &["config", "user.name", "t"]).unwrap();
        git(&c2, &["checkout", "-q", "-b", "develop", "origin/develop~1"]).unwrap(); // diverge off d0
        git(&c2, &["commit", "-q", "--allow-empty", "-m", "local-diverge"]).unwrap();
        git(&c2, &["checkout", "-q", &main]).unwrap();
        let wt2 = tmp("rtr-wt2");
        git(&c2, &["worktree", "add", "-q", "-b", "feat/b", &wt2.to_string_lossy(), "origin/develop"]).unwrap();
        git(&wt2, &["commit", "-q", "--allow-empty", "-m", "agent-b"]).unwrap();
        assert_eq!(
            resolve_target_ref(&wt2, "develop"),
            "origin/develop",
            "worktree forked off fresh origin/develop (diverged local) → prefer origin"
        );

        let _ = remove_worktree(&c1, &wt1);
        let _ = remove_worktree(&c2, &wt2);
        let _ = std::fs::remove_dir_all(&origin);
        let _ = std::fs::remove_dir_all(&c1);
        let _ = std::fs::remove_dir_all(&c2);
        let _ = std::fs::remove_dir_all(&wt1);
        let _ = std::fs::remove_dir_all(&wt2);
    }

    #[test]
    fn list_worktrees_skips_prunable_entries() {
        // A worktree removed out-of-band (rm -rf) leaves a PRUNABLE registration that git
        // still lists — list_worktrees must skip it so a stale path isn't treated as a live
        // worktree of this repo (which would let a plain dir there be reused).
        let repo = tmp("lw-prunable");
        init_repo(&repo).unwrap();
        let base = current_branch(&repo).unwrap();
        let wt = tmp("lw-prunable-wt");
        add_worktree_synced(&repo, "feat/p", &wt, &base, false).unwrap();
        assert!(
            list_worktrees(&repo).unwrap().iter().any(|(_, b)| b == "feat/p"),
            "a live worktree is listed"
        );
        // Out-of-band rm -rf → the registration becomes prunable (dir gone).
        std::fs::remove_dir_all(&wt).unwrap();
        assert!(
            !list_worktrees(&repo).unwrap().iter().any(|(_, b)| b == "feat/p"),
            "a prunable (rm -rf'd) worktree must not be listed"
        );
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn add_worktree_synced_rejects_invalid_existing_path() {
        // The path-exists reuse must validate the path is a clean worktree for `branch`,
        // not blindly hand a stale/plain dir or a different-branch checkout to the worker.
        let repo = tmp("aws-invalid");
        init_repo(&repo).unwrap();
        let base = current_branch(&repo).unwrap();

        // (A) A plain directory at the path → rejected, not reused.
        let wt_a = tmp("aws-invalid-plain");
        std::fs::create_dir_all(&wt_a).unwrap();
        std::fs::write(wt_a.join("stuff.txt"), "not a worktree\n").unwrap();
        assert!(
            add_worktree_synced(&repo, "feat/a", &wt_a, &base, false).is_err(),
            "a plain dir at the path must be rejected"
        );

        // (B) An existing worktree for a DIFFERENT branch → rejected.
        let wt_b = tmp("aws-invalid-otherbranch");
        add_worktree_synced(&repo, "feat/other", &wt_b, &base, false).unwrap();
        assert!(
            add_worktree_synced(&repo, "feat/b", &wt_b, &base, false).is_err(),
            "an existing worktree on a different branch must be rejected"
        );

        // (C) A valid worktree on the SAME branch → idempotent reuse.
        let wt_c = tmp("aws-invalid-same");
        add_worktree_synced(&repo, "feat/c", &wt_c, &base, false).unwrap();
        let reuse = add_worktree_synced(&repo, "feat/c", &wt_c, &base, false)
            .expect("reusing a valid worktree on the same branch must succeed");
        assert!(!reuse.created_checkout, "reuse reports created_checkout=false");

        // (D) A worktree belonging to a DIFFERENT repo at the path, same branch name →
        // rejected (it is not registered in THIS repo's worktree list).
        let repo2 = tmp("aws-invalid-repo2");
        init_repo(&repo2).unwrap();
        let base2 = current_branch(&repo2).unwrap();
        let wt_d = tmp("aws-invalid-foreign");
        add_worktree_synced(&repo2, "feat/d", &wt_d, &base2, false).unwrap();
        assert!(
            add_worktree_synced(&repo, "feat/d", &wt_d, &base, false).is_err(),
            "a worktree belonging to a DIFFERENT repo must be rejected"
        );

        let _ = remove_worktree(&repo, &wt_b);
        let _ = remove_worktree(&repo, &wt_c);
        let _ = remove_worktree(&repo2, &wt_d);
        let _ = std::fs::remove_dir_all(&repo);
        let _ = std::fs::remove_dir_all(&repo2);
        let _ = std::fs::remove_dir_all(&wt_a);
        let _ = std::fs::remove_dir_all(&wt_b);
        let _ = std::fs::remove_dir_all(&wt_c);
        let _ = std::fs::remove_dir_all(&wt_d);
    }

    #[test]
    fn add_worktree_synced_rejects_existing_branch_not_from_explicit_base() {
        // The `-b` fallback (branch name already exists) must, for an EXPLICIT base, only
        // reuse the branch if it actually descends from that base — else the lane would
        // record a base its worktree wasn't forked from.
        let repo = tmp("aws-mismatch");
        init_repo(&repo).unwrap();
        let main = current_branch(&repo).unwrap();
        git(&repo, &["checkout", "-q", "-b", "release"]).unwrap();
        git(&repo, &["commit", "-q", "--allow-empty", "-m", "release work"]).unwrap();
        git(&repo, &["checkout", "-q", &main]).unwrap();

        // (A) An existing branch based on MAIN, not the explicit base "release" → reject.
        git(&repo, &["branch", "feat/x", &main]).unwrap();
        let wt_a = tmp("aws-mismatch-a");
        assert!(
            add_worktree_synced(&repo, "feat/x", &wt_a, "release", true).is_err(),
            "existing branch not based on the explicit base must be rejected"
        );

        // (B) An existing branch based on "release" → reused.
        git(&repo, &["branch", "feat/y", "release"]).unwrap();
        let wt_b = tmp("aws-mismatch-b");
        assert!(
            add_worktree_synced(&repo, "feat/y", &wt_b, "release", true).is_ok(),
            "existing branch based on the explicit base is reused"
        );

        let _ = remove_worktree(&repo, &wt_b);
        let _ = std::fs::remove_dir_all(&repo);
        let _ = std::fs::remove_dir_all(&wt_a);
        let _ = std::fs::remove_dir_all(&wt_b);
    }

    #[test]
    fn add_worktree_synced_rejects_tag_named_like_explicit_base() {
        // R40-1: an explicit base `release` that exists ONLY as a TAG on origin (no
        // branch) must be REJECTED — the fetch refspec is branch-namespaced
        // (refs/heads/<base>), so a same-named tag never lands in origin/<base> and
        // the base is treated as missing. A worktree must NOT be forked off the tag.
        let origin = tmp("aws-tag-origin");
        init_repo(&origin).unwrap();
        let main = current_branch(&origin).unwrap();
        // Tag a commit that is NOT reachable from main: make it on a side branch, tag
        // it `release`, then delete the side branch. The tag keeps the commit alive on
        // origin, but a `--single-branch --branch main` clone won't auto-fetch a tag
        // pointing outside main's history — so the only way `release` could land in
        // origin/release is via the (buggy) unqualified fetch refspec.
        git(&origin, &["checkout", "-q", "-b", "side"]).unwrap();
        git(&origin, &["commit", "-q", "--allow-empty", "-m", "side"]).unwrap();
        let tag_commit = git(&origin, &["rev-parse", "HEAD"]).unwrap();
        git(&origin, &["tag", "release", &tag_commit]).unwrap();
        git(&origin, &["checkout", "-q", &main]).unwrap();
        git(&origin, &["branch", "-D", "side"]).unwrap();

        // single-branch, no-tags clone of main only → no tags auto-fetched, so the only
        // way `release` could leak in is via the (buggy) unqualified fetch refspec.
        let clone = tmp("aws-tag-clone");
        git(
            &std::env::temp_dir(),
            &["clone", "-q", "--single-branch", "--no-tags", "--branch", &main,
              &origin.to_string_lossy(), &clone.to_string_lossy()],
        )
        .unwrap();
        assert!(!ref_resolves(&clone, "release"), "precondition: no local `release`");
        assert!(!ref_resolves(&clone, "origin/release"), "precondition: no origin/release");

        // (i) tag-only base → Err, and NO worktree created off the tag.
        let wt = tmp("aws-tag-wt");
        let res = add_worktree_synced(&clone, "feat/x", &wt, "release", true);
        assert!(res.is_err(), "a TAG named like the base must not be accepted as a branch");
        assert!(!wt.exists(), "no worktree created off the tag");
        assert!(
            !ref_resolves(&clone, "feat/x"),
            "the worker branch must not be created off the tag's commit"
        );

        // (ii) now make `release` a REAL branch on origin with its own distinct tip →
        // Ok, and the worktree must fork off the BRANCH tip (not the same-named tag).
        git(&origin, &["checkout", "-q", "-b", "release", &main]).unwrap();
        git(&origin, &["commit", "-q", "--allow-empty", "-m", "release work"]).unwrap();
        // Disambiguate: origin now has BOTH refs/tags/release and refs/heads/release, so a
        // bare `rev-parse release` is ambiguous — read the BRANCH ref explicitly.
        let branch_commit = git(&origin, &["rev-parse", "refs/heads/release"]).unwrap();
        git(&origin, &["checkout", "-q", &main]).unwrap();
        assert_ne!(branch_commit, tag_commit, "the branch tip differs from the tag commit");
        let wt2 = tmp("aws-tag-wt2");
        let add = add_worktree_synced(&clone, "feat/y", &wt2, "release", true)
            .expect("a real branch named `release` must be accepted");
        assert!(add.synced, "branched off the freshly-fetched origin/release branch");
        assert_eq!(
            git(&wt2, &["rev-parse", "HEAD"]).unwrap(),
            branch_commit,
            "forked off the BRANCH tip, not the tag commit"
        );

        let _ = remove_worktree(&clone, &wt2);
        let _ = std::fs::remove_dir_all(&origin);
        let _ = std::fs::remove_dir_all(&clone);
        let _ = std::fs::remove_dir_all(&wt);
        let _ = std::fs::remove_dir_all(&wt2);
    }

    #[test]
    fn add_worktree_synced_rejects_local_tag_named_like_explicit_base() {
        // R40-1 (b): the local-branch fallback must reject a LOCAL tag `release` when
        // no local BRANCH `release` exists — `ref_resolves` alone is true for a tag, so
        // the fallback is tightened to refs/heads/<base>. No remote here, so resolution
        // can only succeed via the local fallback.
        let repo = tmp("aws-local-tag");
        init_repo(&repo).unwrap();
        // Tag the initial commit `release`; never create a branch of that name.
        git(&repo, &["tag", "release"]).unwrap();
        assert!(ref_resolves(&repo, "release"), "precondition: tag `release` resolves");
        assert!(
            !ref_resolves(&repo, "refs/heads/release"),
            "precondition: no local BRANCH `release`"
        );
        let wt = tmp("aws-local-tag-wt");
        let res = add_worktree_synced(&repo, "feat/x", &wt, "release", true);
        assert!(res.is_err(), "a LOCAL tag named like the base must be rejected");
        assert!(!wt.exists(), "no worktree created off the local tag");
        let _ = std::fs::remove_dir_all(&repo);
        let _ = std::fs::remove_dir_all(&wt);
    }

    #[test]
    fn add_worktree_synced_handles_branch_and_tag_same_name() {
        // R42-3: a LOCAL repo (no origin) with BOTH a branch `develop` AND a tag `develop`
        // at DIFFERENT commits. The local-branch fallback resolves via refs/heads/develop,
        // but it must PASS the QUALIFIED ref to `worktree add -b`: a bare `develop` start-point
        // is ambiguous (branch vs tag) and `worktree add` FAILS. The worktree must fork off
        // the BRANCH tip, and branched_from must record the bare `develop` (not refs/heads/...).
        let repo = tmp("aws-branch-and-tag");
        init_repo(&repo).unwrap();
        let main = current_branch(&repo).unwrap();
        // Create branch `develop` with its own distinct tip.
        git(&repo, &["checkout", "-q", "-b", "develop"]).unwrap();
        git(&repo, &["commit", "-q", "--allow-empty", "-m", "develop work"]).unwrap();
        let branch_commit = git(&repo, &["rev-parse", "refs/heads/develop"]).unwrap();
        // A tag `develop` on a DIFFERENT commit (back on main's initial commit).
        git(&repo, &["checkout", "-q", &main]).unwrap();
        let tag_commit = git(&repo, &["rev-parse", "HEAD"]).unwrap();
        git(&repo, &["tag", "develop"]).unwrap();
        assert_ne!(branch_commit, tag_commit, "precondition: branch tip differs from the tag commit");

        let wt = tmp("aws-branch-and-tag-wt");
        let add = add_worktree_synced(&repo, "feat/x", &wt, "develop", true)
            .expect("a usable refs/heads/develop must be accepted even when a tag shadows it");
        assert_eq!(
            git(&wt, &["rev-parse", "HEAD"]).unwrap(),
            branch_commit,
            "worktree must fork off the BRANCH develop's tip, not the tag commit"
        );
        assert_eq!(add.branched_from, "develop", "branched_from records the bare branch name");
        let _ = remove_worktree(&repo, &wt);
        let _ = std::fs::remove_dir_all(&repo);
        let _ = std::fs::remove_dir_all(&wt);
    }

    #[test]
    fn add_worktree_synced_recreates_after_out_of_band_dir_removal() {
        // A worktree dir removed out-of-band (rm -rf, not `git worktree remove`) leaves
        // a prunable registration, so the branch still looks checked out. A fresh
        // add_worktree_synced for the same branch/path must PRUNE the stale registration
        // and recreate the checkout, not fail with "already checked out".
        let repo = tmp("aws-oob");
        init_repo(&repo).unwrap();
        let base = current_branch(&repo).unwrap();
        let wt = tmp("aws-oob-wt");
        add_worktree_synced(&repo, "feat/oob", &wt, &base, false).unwrap();
        assert!(wt.join(".git").exists(), "first add created the worktree");
        // Remove the dir OUT-OF-BAND — git keeps the (now-stale) registration.
        std::fs::remove_dir_all(&wt).unwrap();
        assert!(!wt.exists(), "precondition: dir gone, registration stale");
        // Re-add: without the prune this fails ("feat/oob is already checked out").
        add_worktree_synced(&repo, "feat/oob", &wt, &base, false)
            .expect("re-add must prune the stale registration and recreate");
        assert!(wt.join(".git").exists(), "worktree recreated after out-of-band removal");
        let _ = remove_worktree(&repo, &wt);
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
        // existing-branch fallback: branched_from must be empty (no new branch created from a base)
        assert!(add.branched_from.is_empty(), "fallback to existing branch must have empty branched_from");
        assert!(wt.join(".git").exists());
        let _ = remove_worktree(&repo, &wt);
        let _ = std::fs::remove_dir_all(&repo);
        let _ = std::fs::remove_dir_all(&wt);
    }

    #[test]
    fn add_worktree_synced_sets_branched_from_on_new_branch() {
        // Normal create: branched_from is set to the base branch (normalized).
        let repo = tmp("aws-branched-from");
        init_repo(&repo).unwrap();
        let base = current_branch(&repo).unwrap();
        let wt = tmp("aws-branched-from-wt");
        let add = add_worktree_synced(&repo, "feat/new", &wt, &base, false).unwrap();
        assert!(add.created_branch, "new branch created");
        // branched_from must be the base name (no origin/ prefix for local-only repo).
        assert_eq!(add.branched_from, base, "branched_from equals the base we branched off");
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

    #[test]
    fn live_default_branch_reads_remote_head_even_when_unfetched() {
        // origin default moved to `develop`; a single-branch clone of main hasn't
        // fetched develop, but ls-remote --symref still reports the live default.
        let origin = tmp("ldb-origin");
        init_repo(&origin).unwrap();
        let main = current_branch(&origin).unwrap();
        git(&origin, &["checkout", "-q", "-b", "develop"]).unwrap();
        git(&origin, &["commit", "-q", "--allow-empty", "-m", "d"]).unwrap();
        git(&origin, &["symbolic-ref", "HEAD", "refs/heads/develop"]).unwrap();
        let clone = tmp("ldb-clone");
        git(&std::env::temp_dir(), &["clone", "-q", "--single-branch", "--branch", &main,
            &origin.to_string_lossy(), &clone.to_string_lossy()]).unwrap();
        assert!(!ref_resolves(&clone, "origin/develop"), "precondition: develop not fetched");
        assert_eq!(live_default_branch(&clone).as_deref(), Some("develop"),
            "ls-remote --symref reports the live remote default without a local ref");
        let _ = std::fs::remove_dir_all(&origin);
        let _ = std::fs::remove_dir_all(&clone);
    }

    #[test]
    fn recorded_base_or_default_prefers_resolvable_base_ref_over_cached_origin_head() {
        // A clone whose cached origin/HEAD points at `main`, but the recorded base_ref
        // is `develop` CAPTURED AS THE DEFAULT (is_default=true) at register. Offline, we
        // must prefer the recorded develop, not the stale cached main.
        let origin = tmp("rbod-origin");
        init_repo(&origin).unwrap();
        let main = current_branch(&origin).unwrap();
        git(&origin, &["checkout", "-q", "-b", "develop"]).unwrap();
        git(&origin, &["commit", "-q", "--allow-empty", "-m", "d"]).unwrap();
        git(&origin, &["checkout", "-q", &main]).unwrap();
        let clone = tmp("rbod-clone");
        git(&std::env::temp_dir(), &["clone", "-q", &origin.to_string_lossy(), &clone.to_string_lossy()]).unwrap();
        // clone's origin/HEAD → origin/main (the origin's default). develop is fetched.
        assert!(ref_resolves(&clone, "origin/develop"));
        // default_base_branch prefers cached origin/HEAD (main); recorded_base_or_default
        // prefers the resolvable CAPTURED-DEFAULT base_ref (develop).
        assert_eq!(default_base_branch(&clone, "develop"), main, "default_base_branch is origin/HEAD-first");
        assert_eq!(recorded_base_or_default(&clone, "develop", true), "develop", "captured-default base_ref preferred when it resolves");
        // A non-resolving recorded base falls back to the default chain.
        assert_eq!(recorded_base_or_default(&clone, "no-such-xyz", true), main);
        let _ = std::fs::remove_dir_all(&origin);
        let _ = std::fs::remove_dir_all(&clone);
    }

    #[test]
    fn recorded_base_or_default_ignores_local_only_legacy_base_ref() {
        // On an upgraded DB, base_ref was the repo's CURRENT branch at add time — a LOCAL
        // feature branch (no origin/<base_ref>), NOT the live default (is_default=false).
        // Offline, that legacy base_ref must NOT be trusted (else new work branches off the
        // legacy feature branch); the main-family default must win instead.
        let repo = tmp("rbod-local-legacy");
        init_repo(&repo).unwrap();
        let def = current_branch(&repo).unwrap(); // main or master
        // A local-only feature branch, with NO remote-tracking origin/feature/foo.
        git(&repo, &["branch", "feature/foo"]).unwrap();
        assert!(ref_resolves(&repo, "feature/foo"), "precondition: local feature/foo exists");
        assert!(!ref_resolves(&repo, "origin/feature/foo"), "precondition: no remote feature/foo");
        let got = recorded_base_or_default(&repo, "feature/foo", false);
        assert_eq!(got, def, "a legacy base_ref must fall through to the main-family default");
        assert_ne!(got, "feature/foo", "must not trust the legacy local feature branch");
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn recorded_base_or_default_ignores_legacy_base_ref() {
        // R36-1: a PUSHED legacy base_ref — `feature/foo` whose remote-tracking
        // `origin/feature/foo` EXISTS — is indistinguishable BY VALUE from a real
        // non-standard default. The is_default marker is the only signal: with
        // is_default=false (legacy current-branch capture on an upgraded DB), the
        // main-family default (cached origin/HEAD=main) must win, NOT feature/foo.
        let origin = tmp("rbod-pushed-legacy-origin");
        init_repo(&origin).unwrap();
        let main = current_branch(&origin).unwrap();
        // origin also has a feature/foo branch (so the clone fetches origin/feature/foo).
        git(&origin, &["checkout", "-q", "-b", "feature/foo"]).unwrap();
        git(&origin, &["commit", "-q", "--allow-empty", "-m", "f"]).unwrap();
        git(&origin, &["checkout", "-q", &main]).unwrap();
        let clone = tmp("rbod-pushed-legacy-clone");
        git(&std::env::temp_dir(), &["clone", "-q", &origin.to_string_lossy(), &clone.to_string_lossy()]).unwrap();
        // Precondition: the pushed legacy branch DOES resolve via its remote-tracking ref,
        // and the clone's cached origin/HEAD points at main.
        assert!(ref_resolves(&clone, "origin/feature/foo"), "precondition: origin/feature/foo exists");
        assert_eq!(remote_default_branch(&clone).as_deref(), Some(main.as_str()), "precondition: origin/HEAD → main");
        // With is_default=false the pushed legacy base_ref must NOT win, even though it resolves.
        let got = recorded_base_or_default(&clone, "feature/foo", false);
        assert_eq!(got, main, "a pushed legacy base_ref must fall through to the real default");
        assert_ne!(got, "feature/foo", "must not trust the legacy pushed feature branch");
        // Sanity: flipping the marker to true (a genuine non-standard default) DOES trust it.
        assert_eq!(recorded_base_or_default(&clone, "feature/foo", true), "feature/foo", "a captured-default base_ref is trusted when it resolves");
        let _ = std::fs::remove_dir_all(&origin);
        let _ = std::fs::remove_dir_all(&clone);
    }

    #[test]
    fn add_worktree_synced_validates_explicit_base_even_when_path_exists() {
        let repo = tmp("aws-orphan");
        init_repo(&repo).unwrap();
        let wt = tmp("aws-orphan-wt");
        std::fs::create_dir_all(&wt).unwrap(); // simulate an orphan worktree dir (no DB row)
        // Explicit (require_resolvable=true) misspelled base must error even though the
        // path already exists — not silently reuse the orphan.
        let res = add_worktree_synced(&repo, "feat/x", &wt, "no-such-xyz", true);
        assert!(res.is_err(), "explicit unresolvable base must error before reusing an existing path");
        let _ = std::fs::remove_dir_all(&repo);
        let _ = std::fs::remove_dir_all(&wt);
    }

    #[test]
    fn add_worktree_synced_rejects_existing_path_branch_not_from_explicit_base() {
        // The path-exists fast-path (a REGISTERED checkout for `branch`) must, for an
        // EXPLICIT base, still verify the branch descends from that base before reusing —
        // else a checkout forked off main, reused for an explicit `release` lane, records
        // base=release while the worktree sits on main's line.
        let repo = tmp("aws-path-mismatch");
        init_repo(&repo).unwrap();
        let main = current_branch(&repo).unwrap();
        // A distinct base "release", one commit AHEAD of main, so main is NOT a descendant.
        git(&repo, &["checkout", "-q", "-b", "release"]).unwrap();
        git(&repo, &["commit", "-q", "--allow-empty", "-m", "release work"]).unwrap();
        git(&repo, &["checkout", "-q", &main]).unwrap();

        // Register a real worktree for `feat/x` forked from MAIN at the deterministic path.
        let wt = tmp("aws-path-mismatch-wt");
        let add = add_worktree_synced(&repo, "feat/x", &wt, &main, true).unwrap();
        assert!(add.created_branch, "precondition: feat/x created off main");
        assert!(wt.exists() && wt.join(".git").exists(), "precondition: registered worktree on feat/x");

        // (A) Reuse the EXISTING path for an explicit base "release" → reject (feat/x is
        // on main's line, not a descendant of release).
        assert!(
            add_worktree_synced(&repo, "feat/x", &wt, "release", true).is_err(),
            "registered checkout not based on the explicit base must be rejected on the path-exists fast-path"
        );

        // (B) Reuse the EXISTING path for the branch's REAL base (main) → ok.
        assert!(
            add_worktree_synced(&repo, "feat/x", &wt, &main, true).is_ok(),
            "registered checkout based on the explicit base is reused"
        );

        let _ = remove_worktree(&repo, &wt);
        let _ = std::fs::remove_dir_all(&repo);
        let _ = std::fs::remove_dir_all(&wt);
    }

    #[test]
    fn add_worktree_synced_rejects_existing_branch_not_from_resolved_default_for_blank_base() {
        // R38-1: even for a BLANK base (require_resolvable=false), the `-b` fallback's
        // ancestry check must fire when the RESOLVED default base RESOLVES as a ref. If the
        // deterministic branch name pre-exists forked off a DIFFERENT base than the resolved
        // default (an old default, a release line, …), reusing it would dispatch a worker on
        // a branch not descending from the base materialize records (the current default).
        // Validate whenever the resolved base resolves, regardless of the flag.
        let repo = tmp("aws-blank-mismatch");
        init_repo(&repo).unwrap();
        let main = current_branch(&repo).unwrap();

        // (A) A pre-existing branch with a DISJOINT history (an orphan, modelling a branch
        // forked off a different/old base), so it does NOT descend from the resolved default
        // (main). Reusing it via the -b fallback for a BLANK base must be rejected, even
        // though require_resolvable=false.
        git(&repo, &["checkout", "-q", "--orphan", "feat/x"]).unwrap();
        git(&repo, &["commit", "-q", "--allow-empty", "-m", "orphan work"]).unwrap();
        git(&repo, &["checkout", "-q", &main]).unwrap();
        assert!(
            !branch_descends_from(&repo, "feat/x", &main),
            "precondition: orphan feat/x does not descend from the resolved default"
        );
        let wt_a = tmp("aws-blank-mismatch-a");
        assert!(
            add_worktree_synced(&repo, "feat/x", &wt_a, "", false).is_err(),
            "blank base: existing branch not descending from the resolved default must be rejected"
        );

        // (B) A pre-existing branch based on the resolved default (main) → reused, Ok.
        git(&repo, &["branch", "feat/y", &main]).unwrap();
        let wt_b = tmp("aws-blank-mismatch-b");
        assert!(
            add_worktree_synced(&repo, "feat/y", &wt_b, "", false).is_ok(),
            "blank base: existing branch descending from the resolved default is reused"
        );

        let _ = remove_worktree(&repo, &wt_b);
        let _ = std::fs::remove_dir_all(&repo);
        let _ = std::fs::remove_dir_all(&wt_a);
        let _ = std::fs::remove_dir_all(&wt_b);
    }

    #[test]
    fn branch_descends_from_base_checks_local_and_origin_forms() {
        // origin: main + develop one commit AHEAD of main.
        let origin = tmp("bdfb-origin");
        init_repo(&origin).unwrap();
        let main = current_branch(&origin).unwrap();
        git(&origin, &["checkout", "-q", "-b", "develop"]).unwrap();
        git(&origin, &["commit", "-q", "--allow-empty", "-m", "develop work"]).unwrap();
        git(&origin, &["checkout", "-q", &main]).unwrap();

        // (1) LOCAL-only base: a develop-based branch → Some(true); a main-based → Some(false).
        let local = tmp("bdfb-local");
        git(&std::env::temp_dir(), &["clone", "-q", &origin.to_string_lossy(), &local.to_string_lossy()]).unwrap();
        git(&local, &["config", "user.email", "t@t.t"]).unwrap();
        git(&local, &["config", "user.name", "t"]).unwrap();
        // A local `develop` and NO remote-tracking origin/develop (drop it).
        git(&local, &["checkout", "-q", "-b", "develop", "origin/develop"]).unwrap();
        git(&local, &["checkout", "-q", &main]).unwrap();
        let _ = git(&local, &["update-ref", "-d", "refs/remotes/origin/develop"]);
        assert!(ref_resolves(&local, "develop"), "precondition: local develop");
        assert!(!ref_resolves(&local, "origin/develop"), "precondition: no origin/develop");
        git(&local, &["branch", "feat/on-develop", "develop"]).unwrap();
        git(&local, &["branch", "feat/on-main", &main]).unwrap();
        assert_eq!(
            branch_descends_from_base(&local, "feat/on-develop", "develop"),
            Some(true),
            "local-only base: a develop-based branch descends"
        );
        assert_eq!(
            branch_descends_from_base(&local, "feat/on-main", "develop"),
            Some(false),
            "local-only base: a main-based branch does NOT descend"
        );

        // (2) ORIGIN-only base (single-branch clone — no local develop, keep origin/develop):
        // a develop-based branch → Some(true), a main-based branch → Some(false).
        let oc = tmp("bdfb-origin-only");
        git(&std::env::temp_dir(), &["clone", "-q", &origin.to_string_lossy(), &oc.to_string_lossy()]).unwrap();
        git(&oc, &["config", "user.email", "t@t.t"]).unwrap();
        git(&oc, &["config", "user.name", "t"]).unwrap();
        assert!(ref_resolves(&oc, "origin/develop"), "precondition: origin/develop present");
        assert!(!ref_resolves(&oc, "develop"), "precondition: NO local develop");
        git(&oc, &["branch", "feat/on-develop", "origin/develop"]).unwrap();
        git(&oc, &["branch", "feat/on-main", &main]).unwrap();
        assert_eq!(
            branch_descends_from_base(&oc, "feat/on-develop", "develop"),
            Some(true),
            "origin-only base: a develop(origin)-based branch descends"
        );
        assert_eq!(
            branch_descends_from_base(&oc, "feat/on-main", "develop"),
            Some(false),
            "origin-only base: a main-based branch does NOT descend (the bare-local check would skip)"
        );

        // (3) DIVERGED local vs origin: a branch based on origin/develop while local develop
        // diverged off an OLDER point → must still be Some(true) (checks BOTH forms).
        let dv = tmp("bdfb-diverged");
        git(&std::env::temp_dir(), &["clone", "-q", &origin.to_string_lossy(), &dv.to_string_lossy()]).unwrap();
        git(&dv, &["config", "user.email", "t@t.t"]).unwrap();
        git(&dv, &["config", "user.name", "t"]).unwrap();
        // Local develop diverges from origin/develop (a different commit on develop's name).
        git(&dv, &["checkout", "-q", "-b", "develop", "origin/develop"]).unwrap();
        git(&dv, &["commit", "-q", "--allow-empty", "-m", "local-diverge"]).unwrap();
        git(&dv, &["checkout", "-q", &main]).unwrap();
        // A branch forked off the fresh origin/develop (NOT the diverged local develop).
        git(&dv, &["branch", "feat/on-origin-develop", "origin/develop"]).unwrap();
        assert!(
            !branch_descends_from(&dv, "feat/on-origin-develop", "develop"),
            "precondition: it does NOT descend from the diverged LOCAL develop"
        );
        assert_eq!(
            branch_descends_from_base(&dv, "feat/on-origin-develop", "develop"),
            Some(true),
            "diverged local: a branch off origin/develop descends via the origin form, not rejected"
        );

        // (4) Base GONE in BOTH forms → None (caller skips the check).
        assert_eq!(
            branch_descends_from_base(&local, "feat/on-develop", "no-such-base-xyz"),
            None,
            "a base that resolves in neither form returns None"
        );
        // HEAD / empty base → None.
        assert_eq!(branch_descends_from_base(&local, "feat/on-develop", "HEAD"), None);
        assert_eq!(branch_descends_from_base(&local, "feat/on-develop", ""), None);

        let _ = std::fs::remove_dir_all(&origin);
        let _ = std::fs::remove_dir_all(&local);
        let _ = std::fs::remove_dir_all(&oc);
        let _ = std::fs::remove_dir_all(&dv);
    }

    #[test]
    fn branch_descends_from_base_prefers_remote_over_stale_local() {
        // R43-1: when origin/develop is AHEAD of a stale LOCAL develop, the create path forks
        // off the FRESH origin ref. The reuse guard must PREFER the remote form — a branch that
        // only descends from the STALE LOCAL develop would otherwise be accepted as develop-based
        // while MISSING origin's commits. Prefer-remote rejects it (Some(false)); a branch off
        // origin/develop is accepted (Some(true)).
        let origin = tmp("bdfb-prefer-origin");
        init_repo(&origin).unwrap();
        let main = current_branch(&origin).unwrap();
        git(&origin, &["checkout", "-q", "-b", "develop"]).unwrap();
        git(&origin, &["commit", "-q", "--allow-empty", "-m", "d0 (the stale-local fork point)"]).unwrap();
        git(&origin, &["checkout", "-q", &main]).unwrap();

        let clone = tmp("bdfb-prefer-clone");
        git(&std::env::temp_dir(), &["clone", "-q", &origin.to_string_lossy(), &clone.to_string_lossy()]).unwrap();
        git(&clone, &["config", "user.email", "t@t.t"]).unwrap();
        git(&clone, &["config", "user.name", "t"]).unwrap();
        // A LOCAL develop pinned at the OLD origin tip (d0); a work branch forks off it here.
        git(&clone, &["checkout", "-q", "-b", "develop", "origin/develop"]).unwrap();
        git(&clone, &["branch", "feat/off-stale-local", "develop"]).unwrap();
        git(&clone, &["checkout", "-q", &main]).unwrap();
        // origin/develop now ADVANCES past the local develop (a commit present on origin only).
        git(&origin, &["checkout", "-q", "develop"]).unwrap();
        git(&origin, &["commit", "-q", "--allow-empty", "-m", "d1 (origin-only, ahead of local)"]).unwrap();
        git(&origin, &["checkout", "-q", &main]).unwrap();
        git(&clone, &["fetch", "-q", "origin"]).unwrap();
        // A work branch forked off the FRESH origin/develop (carrying d1).
        git(&clone, &["branch", "feat/off-origin", "refs/remotes/origin/develop"]).unwrap();

        // Preconditions: the two develop forms have diverged; the stale-local branch does NOT
        // descend from origin/develop, and the origin branch does NOT descend from local develop.
        assert!(ref_resolves(&clone, "refs/heads/develop"), "precondition: local develop exists");
        assert!(ref_resolves(&clone, "refs/remotes/origin/develop"), "precondition: origin/develop exists");
        assert!(
            !branch_descends_from(&clone, "feat/off-stale-local", "refs/remotes/origin/develop"),
            "precondition: the stale-local branch lacks origin's d1"
        );
        assert!(
            branch_descends_from(&clone, "feat/off-stale-local", "refs/heads/develop"),
            "precondition: the stale-local branch descends from the local develop"
        );

        // PREFER REMOTE: a branch that only descends from the stale LOCAL develop is rejected
        // (Some(false)), not accepted via the local form.
        assert_eq!(
            branch_descends_from_base(&clone, "feat/off-stale-local", "develop"),
            Some(false),
            "R43-1: prefer the qualified remote — a stale-local-only descent must NOT be accepted"
        );
        // A branch forked from refs/remotes/origin/develop descends from the preferred remote.
        assert_eq!(
            branch_descends_from_base(&clone, "feat/off-origin", "develop"),
            Some(true),
            "R43-1: a branch off origin/develop descends from the preferred remote form"
        );

        let _ = std::fs::remove_dir_all(&origin);
        let _ = std::fs::remove_dir_all(&clone);
    }

    #[test]
    fn add_worktree_synced_prefers_remote_tracking_over_local_origin_branch() {
        // R43-2: a LOCAL branch literally named `origin/develop` (refs/heads/origin/develop)
        // would shadow a short `origin/develop`. add_worktree_synced must carry the
        // FULLY-QUALIFIED refs/remotes/origin/develop, so the worktree forks off the
        // remote-tracking tip — NOT the misleading local branch — and branched_from == "develop".
        let origin = tmp("aws-shadow-origin");
        init_repo(&origin).unwrap();
        let main = current_branch(&origin).unwrap();
        git(&origin, &["checkout", "-q", "-b", "develop"]).unwrap();
        git(&origin, &["commit", "-q", "--allow-empty", "-m", "develop work"]).unwrap();
        let remote_dev_tip = git(&origin, &["rev-parse", "HEAD"]).unwrap();
        git(&origin, &["checkout", "-q", &main]).unwrap();

        let clone = tmp("aws-shadow-clone");
        git(&std::env::temp_dir(), &["clone", "-q", &origin.to_string_lossy(), &clone.to_string_lossy()]).unwrap();
        git(&clone, &["config", "user.email", "t@t.t"]).unwrap();
        git(&clone, &["config", "user.name", "t"]).unwrap();
        assert!(ref_resolves(&clone, "refs/remotes/origin/develop"), "precondition: origin/develop fetched");
        // A LOCAL branch literally named `origin/develop`, pointing at main's initial commit
        // (a DIFFERENT commit than the remote-tracking develop tip).
        let local_shadow_tip = git(&clone, &["rev-parse", &main]).unwrap();
        assert_ne!(local_shadow_tip, remote_dev_tip, "precondition: the shadow points elsewhere");
        git(&clone, &["branch", "origin/develop", &local_shadow_tip]).unwrap();
        assert!(ref_resolves(&clone, "refs/heads/origin/develop"), "precondition: local origin/develop branch exists");

        let wt = tmp("aws-shadow-wt");
        let add = add_worktree_synced(&clone, "feat/x", &wt, "develop", true)
            .expect("an explicit base `develop` resolvable via origin must be accepted");
        // The worktree must fork off the REMOTE-tracking tip, not the local `origin/develop` branch.
        assert_eq!(
            git(&wt, &["rev-parse", "HEAD"]).unwrap(),
            remote_dev_tip,
            "R43-2: forked off refs/remotes/origin/develop, not the local origin/develop branch"
        );
        assert_eq!(add.branched_from, "develop", "R43-2: branched_from records the bare branch name");
        assert!(add.synced, "branched off the freshly-fetched remote-tracking develop");

        let _ = remove_worktree(&clone, &wt);
        let _ = std::fs::remove_dir_all(&origin);
        let _ = std::fs::remove_dir_all(&clone);
        let _ = std::fs::remove_dir_all(&wt);
    }

}
