//! Sync a git skill source into its cache dir: clone on first run, hard-reset to
//! the upstream ref on subsequent runs (idempotent, no local drift). The caller
//! records last_status/last_synced; this module just does the git work.

use anyhow::{anyhow, Result};
use std::path::Path;
use std::process::Command;

fn run(dir: Option<&Path>, args: &[&str]) -> Result<()> {
    let mut cmd = Command::new("git");
    if let Some(d) = dir {
        cmd.current_dir(d);
    }
    let out = cmd
        .args(args)
        .output()
        .map_err(|e| anyhow!("git spawn: {e}"))?;
    if !out.status.success() {
        return Err(anyhow!(
            "git {:?}: {}",
            args,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
}

/// Bring `cache` to the latest of `url` (optional `git_ref`, empty = default
/// branch). Clones if absent, else fetch + hard reset. Errors bubble to caller.
pub fn sync_to(url: &str, git_ref: &str, cache: &Path) -> Result<()> {
    let is_repo = cache.join(".git").exists();
    if !is_repo {
        if let Some(parent) = cache.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let _ = std::fs::remove_dir_all(cache); // stale non-repo dir
        let mut args = vec!["clone", "--depth", "1"];
        if !git_ref.is_empty() {
            args.push("-b");
            args.push(git_ref);
        }
        let cache_s = cache.to_string_lossy().to_string();
        args.push("--");
        args.push(url);
        args.push(&cache_s);
        run(None, &args)?;
        return Ok(());
    }
    run(Some(cache), &["fetch", "--depth", "1", "origin"])?;
    let target = if git_ref.is_empty() {
        // resolve remote HEAD's default branch
        "FETCH_HEAD".to_string()
    } else {
        format!("origin/{git_ref}")
    };
    run(Some(cache), &["reset", "--hard", &target])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn sh(dir: &std::path::Path, args: &[&str]) {
        assert!(
            Command::new(args[0])
                .args(&args[1..])
                .current_dir(dir)
                .status()
                .unwrap()
                .success(),
            "cmd {:?}",
            args
        );
    }

    #[test]
    fn sync_clones_then_pulls() {
        let base = std::env::temp_dir().join(format!("weft-sksync-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let origin = base.join("origin");
        let cache = base.join("cache");
        std::fs::create_dir_all(&origin).unwrap();
        sh(&origin, &["git", "init", "-q"]);
        sh(&origin, &["git", "config", "user.email", "t@t.t"]);
        sh(&origin, &["git", "config", "user.name", "t"]);
        std::fs::create_dir_all(origin.join("skills/deploy")).unwrap();
        std::fs::write(
            origin.join("skills/deploy/SKILL.md"),
            "---\nname: deploy\n---\n",
        )
        .unwrap();
        sh(&origin, &["git", "add", "-A"]);
        sh(&origin, &["git", "commit", "-q", "-m", "init"]);

        let url = origin.to_string_lossy().to_string();
        // first sync clones
        sync_to(&url, "", &cache).unwrap();
        assert!(cache.join("skills/deploy/SKILL.md").exists());
        // add a second commit upstream, re-sync pulls it
        std::fs::write(
            origin.join("skills/deploy/SKILL.md"),
            "---\nname: deploy\ndescription: v2\n---\n",
        )
        .unwrap();
        sh(&origin, &["git", "add", "-A"]);
        sh(&origin, &["git", "commit", "-q", "-m", "v2"]);
        sync_to(&url, "", &cache).unwrap();
        let body = std::fs::read_to_string(cache.join("skills/deploy/SKILL.md")).unwrap();
        assert!(body.contains("v2"), "pull picked up upstream change");
        let _ = std::fs::remove_dir_all(&base);
    }
}
