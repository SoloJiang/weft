//! Thin wrapper around the system `git` CLI.
//!
//! Every invocation sets `GIT_TERMINAL_PROMPT=0` so missing creds fail fast
//! instead of blocking on a hidden TTY prompt. Commits are stamped with
//! `-c user.email=… -c user.name=…` so we don't read or mutate the user's
//! global git identity.

use anyhow::{Result, anyhow};
use std::path::Path;
use std::process::Command;

const COMMITTER_NAME: &str = "Weft";
const COMMITTER_EMAIL: &str = "weft@local";

fn git() -> Command {
    let mut c = Command::new("git");
    c.env("GIT_TERMINAL_PROMPT", "0");
    c
}

fn run(mut cmd: Command, ctx: &str) -> Result<String> {
    let out = cmd
        .output()
        .map_err(|e| anyhow!("spawn git for {ctx}: {e}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let truncated = if stderr.len() > 4096 {
            format!("{}...(truncated)", &stderr[..4096])
        } else {
            stderr.to_string()
        };
        return Err(anyhow!(
            "git {ctx} failed (status {:?}): {}",
            out.status.code(),
            truncated
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// Verify `git` is on PATH; surfaces a clear error if it isn't.
pub fn ensure_git_available() -> Result<()> {
    let mut c = git();
    c.arg("--version");
    let _ = run(c, "version").map_err(|e| anyhow!("git CLI not found: {e}"))?;
    Ok(())
}

/// Probe a remote with `git ls-remote --heads <url>`. Fast reachability check.
pub fn ls_remote(remote_url: &str) -> Result<()> {
    let mut c = git();
    c.arg("ls-remote").arg("--heads").arg(remote_url);
    let _ = run(c, "ls-remote")?;
    Ok(())
}

/// Ensure `staging_dir` is a git work-tree pointed at `remote_url`. If a
/// `.git` already exists with a different origin, the work-tree is rebuilt
/// from scratch so the next push doesn't land in the wrong remote.
pub fn ensure_clone(staging_dir: &Path, remote_url: &str) -> Result<()> {
    let git_dir = staging_dir.join(".git");
    if git_dir.is_dir() {
        let mut c = git();
        c.current_dir(staging_dir)
            .arg("remote")
            .arg("get-url")
            .arg("origin");
        match run(c, "remote get-url") {
            Ok(out) if out.trim() == remote_url => return Ok(()),
            _ => {
                std::fs::remove_dir_all(&git_dir)?;
            }
        }
    }

    std::fs::create_dir_all(staging_dir)?;

    let mut c = git();
    c.current_dir(staging_dir).arg("init").arg("--quiet");
    run(c, "init")?;

    let mut c = git();
    c.current_dir(staging_dir)
        .arg("remote")
        .arg("add")
        .arg("origin")
        .arg(remote_url);
    run(c, "remote add")?;

    // Name the branch `main` to match the modern convention. If `init`
    // already created `main` (newer git), the checkout fails harmlessly.
    let mut c = git();
    c.current_dir(staging_dir)
        .arg("checkout")
        .arg("-b")
        .arg("main");
    let _ = c.output();

    Ok(())
}

pub struct PushReport {
    pub commit_sha: String,
    pub bytes_pushed: i64,
}

/// `git add -A` + commit (if anything changed) + push to `main`. Returns the
/// short HEAD sha and a coarse byte count of files in the work-tree.
pub fn commit_and_push(staging_dir: &Path, message: &str) -> Result<PushReport> {
    let mut c = git();
    c.current_dir(staging_dir).arg("add").arg("-A");
    run(c, "add")?;

    let mut c = git();
    c.current_dir(staging_dir)
        .arg("diff")
        .arg("--cached")
        .arg("--quiet");
    let has_changes = !c.status().map(|s| s.success()).unwrap_or(true);

    if has_changes {
        let mut c = git();
        c.current_dir(staging_dir)
            .arg("-c")
            .arg(format!("user.email={COMMITTER_EMAIL}"))
            .arg("-c")
            .arg(format!("user.name={COMMITTER_NAME}"))
            .arg("commit")
            .arg("-m")
            .arg(message);
        run(c, "commit")?;
    }

    let mut c = git();
    c.current_dir(staging_dir)
        .arg("push")
        .arg("--porcelain")
        .arg("origin")
        .arg("HEAD:refs/heads/main");
    let _ = run(c, "push")?;

    let mut c = git();
    c.current_dir(staging_dir)
        .arg("rev-parse")
        .arg("--short")
        .arg("HEAD");
    let sha = run(c, "rev-parse")?.trim().to_string();

    let bytes = file_size_sum(staging_dir).unwrap_or(0);

    Ok(PushReport {
        commit_sha: sha,
        bytes_pushed: bytes,
    })
}

/// Shallow-clone `remote_url`'s `main` branch into `temp_dir` (must not exist).
pub fn clone_to(temp_dir: &Path, remote_url: &str) -> Result<()> {
    if temp_dir.exists() {
        return Err(anyhow!(
            "clone target must not exist: {}",
            temp_dir.display()
        ));
    }
    let mut c = git();
    c.arg("clone")
        .arg("--depth")
        .arg("1")
        .arg("--branch")
        .arg("main")
        .arg(remote_url)
        .arg(temp_dir);
    run(c, "clone")?;
    Ok(())
}

fn file_size_sum(dir: &Path) -> std::io::Result<i64> {
    let mut total: i64 = 0;
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_name() == ".git" {
            continue;
        }
        let meta = entry.metadata()?;
        if meta.is_file() {
            total = total.saturating_add(meta.len() as i64);
        }
    }
    Ok(total)
}
