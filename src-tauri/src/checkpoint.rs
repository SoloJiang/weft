//! Per-worktree code checkpoints in a bare "shadow" git repo
//! (`<weft_home>/checkpoints/<worktree_id>.git`), driven via
//! `git --git-dir=<shadow> --work-tree=<wt>` plumbing so the real branch/index
//! is never touched. One ref per session (`refs/heads/s<session_id>`) holds a
//! chain of pre-turn snapshot commits; a code rewind restores the worktree to
//! the snapshot taken before that turn (after stashing the current state on a
//! `rewind-backup-*` ref so the restore itself is recoverable).

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Directories never indexed into a checkpoint (dependency/build output), on
/// top of the worktree's own .gitignore (git's default). `.git/` is
/// belt-and-braces — git already skips it during untracked discovery.
const EXCLUDES: &[&str] = &[
    ".git/",
    "node_modules/",
    "target/",
    "dist/",
    ".next/",
    ".venv/",
    "__pycache__/",
];

/// `<weft_home>/checkpoints/<worktree_id>.git` — the shadow bare repo for one
/// worktree. The parent dir is created; the repo itself is `git init --bare`'d
/// lazily on the first snapshot.
pub fn shadow_repo_for(worktree_id: i32) -> std::io::Result<PathBuf> {
    let dir = crate::paths::weft_home()?.join("checkpoints");
    std::fs::create_dir_all(&dir)?;
    Ok(dir.join(format!("{worktree_id}.git")))
}

/// Remove a worktree's shadow repo (worktree teardown cascade). Best-effort:
/// a missing dir is fine.
pub fn remove_shadow(worktree_id: i32) {
    match shadow_repo_for(worktree_id) {
        Ok(dir) => {
            if let Err(e) = std::fs::remove_dir_all(&dir) {
                if e.kind() != std::io::ErrorKind::NotFound {
                    eprintln!("[weft] shadow repo remove failed for worktree {worktree_id}: {e}");
                }
            }
        }
        Err(e) => eprintln!("[weft] shadow repo path failed for worktree {worktree_id}: {e}"),
    }
}

/// What one snapshot produced: the shadow commit plus the REAL repo's HEAD at
/// snapshot time (a rewind resets the lane branch back to it when the agent
/// committed after the snapshot).
pub struct Snapshot {
    pub shadow_sha: String,
    pub head_sha: String,
}

/// Snapshot the pre-turn state of worktree `wt` for session `session_id` (ref
/// `refs/heads/s<session_id>`, message `turn <turn_id>`) and read the real
/// repo's HEAD. An unchanged state reuses the parent commit (no-op snapshot).
pub fn snapshot(wt: &Path, shadow: &Path, session_id: i32, turn_id: i32) -> Result<Snapshot> {
    let shadow_sha = snapshot_to_ref(
        wt,
        shadow,
        &session_ref(session_id),
        &format!("turn {turn_id}"),
    )?;
    let head_sha = real_git(wt, &["rev-parse", "HEAD"])?;
    Ok(Snapshot {
        shadow_sha,
        head_sha,
    })
}

/// Restore worktree `wt` to the checkpoint `shadow_sha` (the state BEFORE the
/// turn that checkpoint opened). First snapshots the CURRENT state onto
/// `refs/heads/rewind-backup-s<session_id>` so the restore is recoverable, and
/// refuses outright when that backup fails. When the real branch advanced past
/// the checkpoint's recorded `head_sha` (the agent committed), reset it back —
/// but never across `base_commit` (the lane's fork point; empty disables that
/// guard), and never when `head_sha` is not an ancestor of HEAD (the branch
/// was rewritten externally — then only the working tree is restored).
pub fn restore(
    wt: &Path,
    shadow: &Path,
    session_id: i32,
    shadow_sha: &str,
    head_sha: &str,
    base_commit: &str,
) -> Result<bool> {
    // Validate BEFORE any mutation: if the shadow repo lost the checkpoint
    // object, read-tree would fail AFTER a reset --hard already moved the
    // user's branch — refusing here keeps the worktree byte-identical.
    shadow_git(shadow, wt, &["cat-file", "-e", &format!("{shadow_sha}^{{tree}}")])
        .context("checkpoint object missing from the shadow repo — refusing to touch the worktree")?;
    // Safety snapshot next: restoring without a backup would be unrecoverable.
    snapshot_to_ref(wt, shadow, &backup_ref(session_id), "rewind backup")?;
    let current = real_git(wt, &["rev-parse", "HEAD"])?;
    if current != head_sha {
        if !base_commit.is_empty()
            && !real_git_ok(wt, &["merge-base", "--is-ancestor", base_commit, head_sha])
        {
            bail!(
                "checkpoint HEAD {head_sha} no longer descends from the lane's base commit {base_commit} — refusing to reset"
            );
        }
        if real_git_ok(wt, &["merge-base", "--is-ancestor", head_sha, "HEAD"]) {
            real_git(wt, &["reset", "--hard", head_sha])?;
        }
    }
    // Working-tree restore. `read-tree` replaces the shadow index wholesale
    // (a plain `checkout <sha> -- .` would keep stale index entries from a
    // LATER snapshot, and those files would then survive `clean`), so after
    // `checkout-index` every path absent from the snapshot is untracked and
    // `clean` removes it. Ignored/excluded files are untouched (no -x).
    sync_excludes(shadow, wt)?;
    shadow_git(shadow, wt, &["read-tree", shadow_sha])?;
    shadow_git(shadow, wt, &["checkout-index", "-f", "-a"])?;
    shadow_git(shadow, wt, &["clean", "-fd"])?;
    Ok(true)
}

fn session_ref(session_id: i32) -> String {
    format!("refs/heads/s{session_id}")
}

fn backup_ref(session_id: i32) -> String {
    format!("refs/heads/rewind-backup-s{session_id}")
}

/// Snapshot the worktree state as a commit on `ref_name` in the shadow repo.
/// Identical consecutive states reuse the parent commit. Returns the sha.
fn snapshot_to_ref(wt: &Path, shadow: &Path, ref_name: &str, message: &str) -> Result<String> {
    ensure_shadow(shadow)?;
    sync_excludes(shadow, wt)?;
    shadow_git(shadow, wt, &["add", "-A"])?;
    let tree = shadow_git(shadow, wt, &["write-tree"])?;
    let parent = shadow_git(shadow, wt, &["rev-parse", "--verify", "--quiet", ref_name])
        .ok()
        .filter(|s| !s.is_empty());
    if let Some(p) = &parent {
        let parent_tree = shadow_git(shadow, wt, &["rev-parse", &format!("{p}^{{tree}}")])?;
        if parent_tree == tree {
            return Ok(p.clone());
        }
    }
    let mut args: Vec<&str> = vec![
        "-c",
        "user.name=weft",
        "-c",
        "user.email=weft@localhost",
        "commit-tree",
        &tree,
    ];
    if let Some(p) = &parent {
        args.push("-p");
        args.push(p);
    }
    args.push("-m");
    args.push(message);
    let sha = shadow_git(shadow, wt, &args)?;
    shadow_git(shadow, wt, &["update-ref", ref_name, &sha])?;
    Ok(sha)
}

/// `git init --bare` the shadow repo on first use and write its default
/// exclude list (see [`EXCLUDES`]).
fn ensure_shadow(shadow: &Path) -> Result<()> {
    if shadow.join("HEAD").exists() {
        return Ok(());
    }
    std::fs::create_dir_all(shadow)?;
    let out = Command::new("git")
        .env("PATH", crate::detect::tool_path())
        .args(["init", "--bare", "-q"])
        .arg(shadow)
        .output()
        .context("spawn git init --bare")?;
    if !out.status.success() {
        bail!(
            "git init --bare failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let info = shadow.join("info");
    std::fs::create_dir_all(&info)?;
    std::fs::write(info.join("exclude"), EXCLUDES.join("\n") + "\n")?;
    Ok(())
}

/// Fold the REAL repo's local excludes (`<git-common-dir>/info/exclude`) into
/// the shadow's own exclude file. Shadow git ops read excludes from the
/// shadow's git dir, so a path the real repo ignores only via its local
/// info/exclude (often secrets or machine-local config) would be untracked in
/// the shadow — and `clean` would then DELETE it on restore. Recomputed each
/// call so edits to the real file are picked up. (`.gitignore` files in the
/// work tree and `core.excludesFile` are already honored by git itself.)
fn sync_excludes(shadow: &Path, wt: &Path) -> Result<()> {
    let common = real_git(wt, &["rev-parse", "--git-common-dir"])?;
    let common = {
        let p = PathBuf::from(common);
        if p.is_absolute() {
            p
        } else {
            wt.join(p)
        }
    };
    let mut body = EXCLUDES.join("\n");
    body.push('\n');
    if let Ok(extra) = std::fs::read_to_string(common.join("info").join("exclude")) {
        body.push_str(&extra);
        if !extra.ends_with('\n') {
            body.push('\n');
        }
    }
    let info = shadow.join("info");
    std::fs::create_dir_all(&info)?;
    std::fs::write(info.join("exclude"), body)?;
    Ok(())
}

/// Run git against the shadow repo bound to worktree `wt`; trimmed stdout on
/// success, stderr in the error otherwise.
fn shadow_git(shadow: &Path, wt: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .env("PATH", crate::detect::tool_path())
        .arg(format!("--git-dir={}", shadow.display()))
        .arg(format!("--work-tree={}", wt.display()))
        .args(args)
        .current_dir(wt)
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

/// Run git in the worktree's REAL repo (HEAD read / reset / ancestry checks).
fn real_git(wt: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .env("PATH", crate::detect::tool_path())
        .args(args)
        .current_dir(wt)
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

/// Status-only variant of [`real_git`] (merge-base --is-ancestor): exit code
/// as a bool.
fn real_git_ok(wt: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .env("PATH", crate::detect::tool_path())
        .args(args)
        .current_dir(wt)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git_ok(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .env("PATH", crate::detect::tool_path())
            .args(args)
            .current_dir(dir)
            .output()
            .expect("spawn git");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// A fixture worktree: a real repo (base commit C1) with tracked files
    /// a.txt + sub/b.txt, node_modules gitignored (untracked content lives
    /// there). Returns (tmp, worktree, shadow-path, base sha).
    fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf, String) {
        let dir = tempfile::tempdir().expect("tempdir");
        let wt = dir.path().join("wt");
        std::fs::create_dir_all(&wt).expect("wt dir");
        crate::git::init_repo(&wt).expect("init repo");
        std::fs::write(wt.join(".gitignore"), "node_modules/\n").expect("gitignore");
        std::fs::write(wt.join("a.txt"), "one").expect("a.txt");
        std::fs::create_dir_all(wt.join("sub")).expect("sub dir");
        std::fs::write(wt.join("sub/b.txt"), "two").expect("b.txt");
        std::fs::create_dir_all(wt.join("node_modules")).expect("nm dir");
        std::fs::write(wt.join("node_modules/x.js"), "nm").expect("x.js");
        git_ok(&wt, &["add", "-A"]);
        git_ok(&wt, &["commit", "-qm", "files"]);
        let base = git_ok(&wt, &["rev-parse", "HEAD"]);
        let shadow = dir.path().join("shadow.git");
        (dir, wt, shadow, base)
    }

    /// Snapshot state on disk as (path, content) pairs, .git and node_modules
    /// excluded (the assertion target is the shadow-visible tree).
    fn visible_files(wt: &Path) -> Vec<(String, String)> {
        fn walk(dir: &Path, root: &Path, out: &mut Vec<(String, String)>) {
            for entry in std::fs::read_dir(dir).expect("read dir") {
                let entry = entry.expect("dir entry");
                let name = entry.file_name().to_string_lossy().to_string();
                if name == ".git" || name == "node_modules" {
                    continue;
                }
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, root, out);
                } else if path.is_file() {
                    let rel = path
                        .strip_prefix(root)
                        .expect("strip prefix")
                        .to_string_lossy()
                        .to_string();
                    let content = std::fs::read_to_string(&path).expect("read file");
                    out.push((rel, content));
                }
            }
        }
        let mut out = Vec::new();
        walk(wt, wt, &mut out);
        out.sort();
        out
    }

    #[test]
    fn snapshot_restore_roundtrip_exact_tree() {
        let (_dir, wt, shadow, base) = fixture();
        let first = snapshot(&wt, &shadow, 7, 1).expect("snapshot 1");
        assert_eq!(first.head_sha, base);

        // Mutate: modify tracked, add new, delete tracked, touch excluded dir.
        std::fs::write(wt.join("a.txt"), "MODIFIED").expect("modify");
        std::fs::write(wt.join("added.txt"), "new").expect("add");
        std::fs::remove_file(wt.join("sub/b.txt")).expect("delete");
        std::fs::write(wt.join("node_modules/y.js"), "nm2").expect("nm add");
        let second = snapshot(&wt, &shadow, 7, 2).expect("snapshot 2");
        assert_ne!(
            first.shadow_sha, second.shadow_sha,
            "changed state must commit anew"
        );

        let restored =
            restore(&wt, &shadow, 7, &first.shadow_sha, &first.head_sha, &base).expect("restore");
        assert!(restored);
        assert_eq!(
            visible_files(&wt),
            vec![
                (".gitignore".to_string(), "node_modules/\n".to_string()),
                ("a.txt".to_string(), "one".to_string()),
                ("sub/b.txt".to_string(), "two".to_string()),
            ],
            "modified reverted, added removed, deleted restored"
        );
        // The excluded dir is untouched by snapshot/clean (no -x).
        assert!(wt.join("node_modules/x.js").exists());
        assert!(wt.join("node_modules/y.js").exists());
    }

    #[test]
    fn noop_snapshot_reuses_parent_sha() {
        let (_dir, wt, shadow, _base) = fixture();
        let first = snapshot(&wt, &shadow, 7, 1).expect("snapshot 1");
        let second = snapshot(&wt, &shadow, 7, 2).expect("snapshot 2");
        assert_eq!(
            first.shadow_sha, second.shadow_sha,
            "unchanged tree reuses parent"
        );
    }

    #[test]
    fn restore_writes_rewind_backup_ref() {
        let (_dir, wt, shadow, base) = fixture();
        let first = snapshot(&wt, &shadow, 7, 1).expect("snapshot 1");
        std::fs::write(wt.join("added.txt"), "new").expect("add");
        restore(&wt, &shadow, 7, &first.shadow_sha, &first.head_sha, &base).expect("restore");
        // The backup captured the pre-restore (mutated) state: added.txt is in
        // its tree even though the restore removed it from the worktree.
        let backup = shadow_git(&shadow, &wt, &["rev-parse", "refs/heads/rewind-backup-s7"])
            .expect("backup ref resolves");
        let tree = shadow_git(&shadow, &wt, &["ls-tree", "-r", "--name-only", &backup])
            .expect("ls-tree backup");
        assert!(
            tree.lines().any(|l| l == "added.txt"),
            "backup holds pre-restore state: {tree}"
        );
    }

    #[test]
    fn agent_commit_is_reset_back_to_checkpoint_head() {
        let (_dir, wt, shadow, base) = fixture();
        let first = snapshot(&wt, &shadow, 7, 1).expect("snapshot 1");
        // The agent commits its work mid-turn (HEAD advances past head_sha).
        std::fs::write(wt.join("a.txt"), "agent work").expect("modify");
        git_ok(&wt, &["add", "-A"]);
        git_ok(&wt, &["commit", "-qm", "agent commit"]);
        assert_ne!(git_ok(&wt, &["rev-parse", "HEAD"]), first.head_sha);

        restore(&wt, &shadow, 7, &first.shadow_sha, &first.head_sha, &base).expect("restore");
        assert_eq!(
            git_ok(&wt, &["rev-parse", "HEAD"]),
            first.head_sha,
            "branch reset back"
        );
        assert_eq!(
            std::fs::read_to_string(wt.join("a.txt")).expect("read"),
            "one"
        );
    }

    #[test]
    fn rewritten_branch_is_not_reset_but_tree_is_restored() {
        let (_dir, wt, shadow, base) = fixture();
        let first = snapshot(&wt, &shadow, 7, 1).expect("snapshot 1");
        // The branch was rewritten externally: HEAD now sits on a commit
        // head_sha is NOT an ancestor of (side line forked from the empty
        // initial commit).
        let empty = git_ok(&wt, &["rev-list", "--max-parents=0", "HEAD"]);
        git_ok(&wt, &["checkout", "-qb", "side", &empty]);
        std::fs::write(wt.join("side.txt"), "side").expect("side file");
        git_ok(&wt, &["add", "-A"]);
        git_ok(&wt, &["commit", "-qm", "side commit"]);
        let side_head = git_ok(&wt, &["rev-parse", "HEAD"]);
        assert!(
            !real_git_ok(
                &wt,
                &["merge-base", "--is-ancestor", &first.head_sha, "HEAD"]
            ),
            "head_sha must NOT be an ancestor of the rewritten HEAD"
        );

        restore(&wt, &shadow, 7, &first.shadow_sha, &first.head_sha, &base).expect("restore");
        assert_eq!(
            git_ok(&wt, &["rev-parse", "HEAD"]),
            side_head,
            "no reset across rewritten history"
        );
        assert_eq!(
            visible_files(&wt),
            vec![
                (".gitignore".to_string(), "node_modules/\n".to_string()),
                ("a.txt".to_string(), "one".to_string()),
                ("sub/b.txt".to_string(), "two".to_string()),
            ],
            "working tree still restored to the checkpoint"
        );
    }

    #[test]
    fn restore_refuses_to_cross_base_commit() {
        let (_dir, wt, shadow, _base) = fixture();
        let first = snapshot(&wt, &shadow, 7, 1).expect("snapshot 1");
        std::fs::write(wt.join("a.txt"), "agent work").expect("modify");
        git_ok(&wt, &["add", "-A"]);
        git_ok(&wt, &["commit", "-qm", "agent commit"]);
        // A base the checkpoint's head does NOT descend from (here: the
        // agent's own later commit) must refuse the reset outright.
        let bogus_base = git_ok(&wt, &["rev-parse", "HEAD"]);
        let r = restore(
            &wt,
            &shadow,
            7,
            &first.shadow_sha,
            &first.head_sha,
            &bogus_base,
        );
        assert!(r.is_err(), "crossing base_commit must refuse");
        assert_eq!(
            std::fs::read_to_string(wt.join("a.txt")).expect("read"),
            "agent work"
        );
        assert_eq!(
            git_ok(&wt, &["rev-parse", "HEAD"]),
            bogus_base,
            "HEAD untouched"
        );
    }

    /// Codex-review regression: a missing/corrupt checkpoint object must fail
    /// BEFORE the backup snapshot and any reset — never after the worktree
    /// has been moved.
    #[test]
    fn restore_with_missing_checkpoint_object_changes_nothing() {
        let (_dir, wt, shadow, base) = fixture();
        let head_before = git_ok(&wt, &["rev-parse", "HEAD"]);
        let r = restore(
            &wt,
            &shadow,
            7,
            "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef",
            &head_before,
            &base,
        );
        assert!(r.is_err(), "unknown checkpoint object must refuse");
        assert_eq!(git_ok(&wt, &["rev-parse", "HEAD"]), head_before, "HEAD untouched");
        assert_eq!(
            std::fs::read_to_string(wt.join("a.txt")).expect("read"),
            "one",
            "worktree untouched"
        );
        // The backup ref must not exist either (validation precedes it).
        assert!(
            Command::new("git")
                .arg(format!("--git-dir={}", shadow.display()))
                .args(["rev-parse", "--verify", "--quiet", "refs/heads/rewind-backup-s7"])
                .current_dir(&wt)
                .output()
                .map(|o| !o.status.success())
                .unwrap_or(true),
            "no backup ref written"
        );
    }

    /// Codex-review regression: a path ignored only via the REAL repo's local
    /// `.git/info/exclude` (typical for secrets / machine config) must neither
    /// enter snapshots nor be deleted by `clean` on restore.
    #[test]
    fn restore_preserves_real_repo_local_excludes() {
        let (_dir, wt, shadow, base) = fixture();
        // Ignore secret.local ONLY in the real repo's local exclude file.
        let info = wt.join(".git/info");
        std::fs::create_dir_all(&info).expect("info dir");
        std::fs::write(info.join("exclude"), "secret.local\n").expect("info/exclude");

        let first = snapshot(&wt, &shadow, 7, 1).expect("snapshot 1");
        // After the checkpoint: modify a tracked file and create the ignored file.
        std::fs::write(wt.join("a.txt"), "changed").expect("modify");
        std::fs::write(wt.join("secret.local"), "do-not-lose").expect("secret");
        // A file ignored only by .gitignore would survive `clean` too — but
        // secret.local is ignored only via the real repo's info/exclude.
        let r = restore(&wt, &shadow, 7, &first.shadow_sha, &first.head_sha, &base);
        assert!(r.is_ok(), "restore: {r:?}");
        assert_eq!(
            std::fs::read_to_string(wt.join("a.txt")).expect("read"),
            "one",
            "tracked change restored"
        );
        assert_eq!(
            std::fs::read_to_string(wt.join("secret.local")).expect("secret survives"),
            "do-not-lose",
            "real-repo-local ignored file must survive the restore clean"
        );
        // And it was never tracked into the shadow snapshot either.
        let listed = Command::new("git")
            .arg(format!("--git-dir={}", shadow.display()))
            .args(["ls-tree", "-r", "--name-only", &first.shadow_sha])
            .current_dir(&wt)
            .output()
            .expect("ls-tree");
        assert!(
            !String::from_utf8_lossy(&listed.stdout).contains("secret.local"),
            "secret never snapshot-tracked"
        );
    }
}
