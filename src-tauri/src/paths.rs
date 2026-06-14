//! Canonical weft home + derived paths. Everything persistent lives under
//! ~/.weft so worktree cwds stay stable across restarts (resume depends on it).

use std::path::PathBuf;

/// weft home. Honors the WEFT_HOME env override (used for test isolation and to
/// let users relocate weft's data); otherwise `~/<home_dir_name>` — `.weft-dev`
/// in dev builds, `.weft` in release (see [`home_dir_name`]). Created if missing.
pub fn weft_home() -> std::io::Result<PathBuf> {
    let dir = match std::env::var("WEFT_HOME") {
        Ok(v) if !v.trim().is_empty() => PathBuf::from(v),
        _ => {
            let home = dirs::home_dir()
                .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "no home dir"))?;
            home.join(home_dir_name(cfg!(debug_assertions)))
        }
    };
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Base directory name for weft's data under $HOME. Debug builds (`tauri dev`,
/// `cargo test`) isolate to `.weft-dev` so local iteration never reads or writes
/// the installed app's real `.weft` data; release builds (`tauri build`) use
/// `.weft`. The WEFT_HOME override bypasses this entirely, regardless of profile.
fn home_dir_name(debug_build: bool) -> &'static str {
    if debug_build {
        ".weft-dev"
    } else {
        ".weft"
    }
}

/// The default (non-`WEFT_HOME`) data home for a build profile: `~/.weft` for
/// release, `~/.weft-dev` for debug. `None` if the home dir can't be resolved.
/// Callers that must distinguish the canonical homes from a relocated `WEFT_HOME`
/// (e.g. credential / worktree namespacing) compare the resolved [`weft_home`]
/// against these.
pub fn default_home(debug_build: bool) -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(home_dir_name(debug_build)))
}

/// ~/.weft/weft.db
pub fn db_path() -> std::io::Result<PathBuf> {
    Ok(weft_home()?.join("weft.db"))
}

/// ~/.weft/worktrees
pub fn worktree_home() -> std::io::Result<PathBuf> {
    let dir = weft_home()?.join("worktrees");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// ~/.weft/skills/sources — git-cloned skill source caches, one dir per source.
pub fn skills_home() -> std::io::Result<PathBuf> {
    let dir = weft_home()?.join("skills").join("sources");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Process-global lock guarding the shared `WEFT_HOME` env var across lib
/// tests. The lib-test binary runs tests on parallel threads sharing one
/// process env, so a test that *sets* WEFT_HOME (e.g. materialize tests) and a
/// test that *reads* the default (`paths_are_under_weft_home`) must not overlap.
/// Every test that touches WEFT_HOME acquires this for the duration it relies on
/// a particular env state. Panic-tolerant: a poisoned guard is recovered so one
/// failing test doesn't cascade into the rest.
#[cfg(test)]
pub static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_are_under_weft_home() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // Assert against the default home, so a WEFT_HOME another test set (and
        // may not have cleared yet on its own thread) can't leak in here.
        std::env::remove_var("WEFT_HOME");
        let home = weft_home().unwrap();
        // Default home follows the build profile (`.weft-dev` in debug/test runs,
        // `.weft` in release), so assert against the active-profile name.
        assert!(home.ends_with(home_dir_name(cfg!(debug_assertions))));
        assert!(db_path().unwrap().ends_with("weft.db"));
        assert!(worktree_home().unwrap().ends_with("worktrees"));
        assert!(skills_home().unwrap().ends_with("skills/sources"));
    }

    #[test]
    fn home_dir_name_isolates_dev_from_prod() {
        // Dev builds get their own home so local iteration never touches the
        // installed app's data; release keeps `.weft` so existing users on the
        // installed app need no migration.
        assert_eq!(home_dir_name(true), ".weft-dev");
        assert_eq!(home_dir_name(false), ".weft");
    }
}
