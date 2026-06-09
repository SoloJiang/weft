//! Background GC: reclaim leaked worktrees — ones git still has registered but
//! Weft's DB no longer tracks (crash / partial-cleanup residue). Safety-first:
//! only ever touches paths UNDER worktree_home that are NOT DB-tracked and are
//! older than a TTL. Never the canonical repo, never user dirs, never a tracked
//! worktree. done-direction / artifact cleanup is intentionally out of scope.

use crate::store::{repo, Db};
use crate::{git, paths};
use std::collections::HashSet;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn env_secs(key: &str, default: u64) -> u64 {
    std::env::var(key).ok().and_then(|v| v.trim().parse().ok()).unwrap_or(default)
}

/// Canonicalize for robust path comparison (resolves macOS /var→/private/var
/// symlinks). Falls back to the lossy string if canonicalize fails.
fn canon_str(p: &Path) -> String {
    std::fs::canonicalize(p)
        .unwrap_or_else(|_| p.to_path_buf())
        .to_string_lossy()
        .to_string()
}

/// Dir mtime in unix secs, None if unreadable (→ "unknown age" = never swept).
fn dir_mtime_secs(p: &Path) -> Option<u64> {
    std::fs::metadata(p).ok()?.modified().ok()?
        .duration_since(UNIX_EPOCH).ok().map(|d| d.as_secs())
}

/// True iff `path_canon` is at or under `home_canon` with a real path boundary
/// (so `/h/worktrees-evil` is NOT under `/h/worktrees`).
fn is_under(path_canon: &str, home_canon: &str) -> bool {
    path_canon == home_canon || path_canon.starts_with(&format!("{home_canon}/"))
}

/// PURE safety decision. Sweep iff under worktree_home AND not DB-tracked AND old
/// enough. Unknown mtime → keep. Safety-critical predicate.
fn should_sweep(
    path_canon: &str,
    home_canon: &str,
    tracked: &HashSet<String>,
    ttl: u64,
    now: u64,
    mtime: Option<u64>,
) -> bool {
    if !is_under(path_canon, home_canon) {
        return false;
    }
    if tracked.contains(path_canon) {
        return false;
    }
    match mtime {
        Some(m) => now.saturating_sub(m) >= ttl,
        None => false,
    }
}

/// Sweep one canonical repo's registered worktrees. Returns count removed.
fn sweep_repo(
    canonical_repo: &Path,
    home_canon: &str,
    tracked: &HashSet<String>,
    ttl: u64,
    now: u64,
) -> usize {
    let mut removed = 0;
    for path in git::list_registered_worktrees(canonical_repo) {
        let pc = canon_str(&path);
        let mtime = dir_mtime_secs(&path);
        if should_sweep(&pc, home_canon, tracked, ttl, now, mtime) {
            let _ = git::remove_worktree(canonical_repo, &path);
            if !path.exists() {
                eprintln!("[weft] gc: reclaimed orphan worktree {}", path.display());
                removed += 1;
            }
        }
    }
    removed
}

/// Reclaim orphan worktrees across all repos. `ttl_secs == 0` disables (no-op).
pub async fn sweep_orphan_worktrees(db: &Db, ttl_secs: u64) -> anyhow::Result<usize> {
    if ttl_secs == 0 {
        return Ok(0);
    }
    let home_canon = canon_str(&paths::worktree_home()?);
    let tracked: HashSet<String> = repo::list_worktrees(db, None)
        .await?
        .into_iter()
        .map(|w| canon_str(Path::new(&w.path)))
        .collect();
    let now = now_secs();
    let mut removed = 0;
    for ws in repo::list_workspaces(db).await? {
        for r in repo::list_repos(db, ws.id).await? {
            removed += sweep_repo(Path::new(&r.local_git_path), &home_canon, &tracked, ttl_secs, now);
        }
    }
    Ok(removed)
}

/// Spawn the periodic GC loop: sweep at startup, then every 6h. Best-effort.
pub fn spawn_periodic(app: tauri::AppHandle) {
    use tauri::Manager;
    std::thread::spawn(move || {
        let ttl = env_secs("WEFT_GC_ORPHAN_TTL_SECS", 259_200); // 72h; 0 disables
        loop {
            if let Some(db) = app.try_state::<Db>() {
                let db = Db(db.0.clone());
                tauri::async_runtime::spawn(async move {
                    let _ = sweep_orphan_worktrees(&db, ttl).await;
                });
            }
            std::thread::sleep(Duration::from_secs(6 * 3600));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(items: &[&str]) -> HashSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn sweeps_untracked_old_under_home() {
        assert!(should_sweep("/h/worktrees/ws/t/d/repo", "/h/worktrees", &set(&[]), 100, 1000, Some(800)));
    }
    #[test]
    fn never_sweeps_tracked() {
        let t = set(&["/h/worktrees/ws/t/d/repo"]);
        assert!(!should_sweep("/h/worktrees/ws/t/d/repo", "/h/worktrees", &t, 100, 10_000, Some(0)));
    }
    #[test]
    fn never_sweeps_outside_home() {
        assert!(!should_sweep("/h/some-real-repo", "/h/worktrees", &set(&[]), 100, 10_000, Some(0)));
        assert!(!should_sweep("/h/worktrees-evil/x", "/h/worktrees", &set(&[]), 100, 10_000, Some(0)));
    }
    #[test]
    fn never_sweeps_too_new() {
        assert!(!should_sweep("/h/worktrees/x", "/h/worktrees", &set(&[]), 100, 1000, Some(950)));
    }
    #[test]
    fn unknown_mtime_is_kept() {
        assert!(!should_sweep("/h/worktrees/x", "/h/worktrees", &set(&[]), 100, 10_000, None));
    }
    #[test]
    fn is_under_equality_and_boundary() {
        assert!(is_under("/h/worktrees", "/h/worktrees"));
        assert!(is_under("/h/worktrees/a", "/h/worktrees"));
        assert!(!is_under("/h/worktrees-evil/x", "/h/worktrees"));
    }
    #[test]
    fn sweep_repo_removes_orphan_keeps_tracked() {
        let base = std::env::temp_dir().join(format!("weft-gc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let repo = base.join("repo");
        let home = base.join("worktrees");
        std::fs::create_dir_all(&home).unwrap();
        git::init_repo(&repo).unwrap();
        let br = git::current_branch(&repo).unwrap();
        let tracked_wt = home.join("keep");
        let orphan_wt = home.join("drop");
        git::add_worktree(&repo, "ws/keep", &tracked_wt, &br).unwrap();
        git::add_worktree(&repo, "ws/drop", &orphan_wt, &br).unwrap();
        let home_canon = canon_str(&home);
        let tracked = set(&[&canon_str(&tracked_wt)]);
        let n = sweep_repo(&repo, &home_canon, &tracked, 0, now_secs());
        assert_eq!(n, 1, "exactly the orphan removed");
        assert!(tracked_wt.join(".git").exists(), "tracked worktree kept");
        assert!(!orphan_wt.exists(), "orphan worktree removed");
        let _ = std::fs::remove_dir_all(&base);
    }
}
