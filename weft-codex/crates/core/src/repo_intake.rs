//! Local repository intake for workspace registration.
//!
//! The UI supplies paths; the daemon owns validation and Git discovery so
//! every surface (standalone today, Desktop later) gets identical behavior.

use std::path::{Path, PathBuf};

use anyhow::Context;
use tokio::process::Command;

use crate::api_error::ApiError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InspectedRepo {
    pub name: String,
    pub path: String,
    pub base_ref: String,
    pub remote_url: String,
    pub base_ref_is_default: bool,
}

async fn git(path: &Path, args: &[&str]) -> anyhow::Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .await
        .with_context(|| format!("run git in {}", path.display()))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git {:?} failed: {}", args, stderr.trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

async fn git_optional(path: &Path, args: &[&str]) -> Option<String> {
    git(path, args).await.ok().filter(|value| !value.is_empty())
}

async fn ref_resolves(path: &Path, reference: &str) -> bool {
    git_optional(
        path,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{reference}^{{commit}}"),
        ],
    )
    .await
    .is_some()
}

async fn discover_base_ref(path: &Path) -> (String, bool) {
    if let Some(remote_head) = git_optional(
        path,
        &[
            "symbolic-ref",
            "--quiet",
            "--short",
            "refs/remotes/origin/HEAD",
        ],
    )
    .await
    {
        let name = remote_head.strip_prefix("origin/").unwrap_or(&remote_head);
        let remote_ref = format!("refs/remotes/origin/{name}");
        if !name.is_empty() && ref_resolves(path, &remote_ref).await {
            return (name.to_string(), true);
        }
    }

    for name in ["main", "master"] {
        let local_ref = format!("refs/heads/{name}");
        let remote_ref = format!("refs/remotes/origin/{name}");
        if ref_resolves(path, &local_ref).await || ref_resolves(path, &remote_ref).await {
            return (name.to_string(), true);
        }
    }

    if let Some(current) = git_optional(path, &["symbolic-ref", "--quiet", "--short", "HEAD"]).await
    {
        let local_ref = format!("refs/heads/{current}");
        if ref_resolves(path, &local_ref).await {
            return (current, false);
        }
    }

    // Detached checkouts are still usable: worktrees can branch from HEAD,
    // but this is deliberately not marked as a detected default branch.
    ("HEAD".to_string(), false)
}

fn derived_name(path: &Path) -> anyhow::Result<String> {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .ok_or_else(|| {
            ApiError::bad_request(
                "invalid_repo",
                format!("invalid repository name for {}", path.display()),
            )
            .into()
        })
}

/// Validate a user-supplied local path and derive its durable registration
/// metadata. Subdirectories are accepted and normalized to the Git root.
pub async fn inspect_local_repo(
    requested_path: &str,
    name_override: Option<&str>,
) -> anyhow::Result<InspectedRepo> {
    let requested_path = requested_path.trim();
    if requested_path.is_empty() {
        return Err(ApiError::bad_request("invalid_repo", "path is required").into());
    }
    if requested_path.chars().count() > 4096 {
        return Err(ApiError::bad_request("invalid_repo", "path is too long").into());
    }

    let input = PathBuf::from(requested_path);
    let canonical_input = match tokio::task::spawn_blocking(move || std::fs::canonicalize(&input))
        .await
        .context("join repository path canonicalization")?
    {
        Ok(path) => path,
        Err(_) => {
            return Err(ApiError::bad_request(
                "invalid_repo",
                format!("invalid repository path {requested_path:?}"),
            )
            .into());
        }
    };
    if !canonical_input.is_dir() {
        return Err(ApiError::bad_request(
            "invalid_repo",
            format!("{} is not a directory", canonical_input.display()),
        )
        .into());
    }

    let root = git(&canonical_input, &["rev-parse", "--show-toplevel"])
        .await
        .map_err(|_| {
            ApiError::bad_request(
                "invalid_repo",
                format!("{requested_path:?} is not a Git repository"),
            )
        })?;
    let root_path = PathBuf::from(root);
    let canonical_root = tokio::task::spawn_blocking(move || std::fs::canonicalize(&root_path))
        .await
        .context("join Git root canonicalization")?
        .context("canonicalize Git root")?;

    git(&canonical_root, &["rev-parse", "--verify", "HEAD^{commit}"])
        .await
        .map_err(|_| {
            ApiError::bad_request(
                "invalid_repo",
                format!("{} has no commits", canonical_root.display()),
            )
        })?;

    let override_name = name_override.map(str::trim).filter(|name| !name.is_empty());
    let name = match override_name {
        Some(name) => name.to_string(),
        None => derived_name(&canonical_root)?,
    };
    if name.chars().count() > 120 {
        return Err(ApiError::bad_request("invalid_repo", "name is too long").into());
    }

    let (base_ref, base_ref_is_default) = discover_base_ref(&canonical_root).await;
    let remote_url = git_optional(&canonical_root, &["config", "--get", "remote.origin.url"])
        .await
        .unwrap_or_default();

    Ok(InspectedRepo {
        name,
        path: canonical_root.to_string_lossy().to_string(),
        base_ref,
        remote_url,
        base_ref_is_default,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn run(path: &Path, args: &[&str]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(path)
            .args(args)
            .output()
            .await
            .expect("run git");
        assert!(output.status.success(), "git {args:?} failed");
    }

    async fn init_repo(path: &Path, branch: &str) {
        std::fs::create_dir_all(path).expect("mkdir");
        run(path, &["init", "-q", "-b", branch]).await;
        run(path, &["config", "user.email", "test@example.com"]).await;
        run(path, &["config", "user.name", "test"]).await;
        std::fs::write(path.join("README.md"), "test").expect("write");
        run(path, &["add", "README.md"]).await;
        run(path, &["commit", "-q", "-m", "initial"]).await;
    }

    #[tokio::test]
    async fn normalizes_subdirectory_and_detects_nonstandard_branch() {
        let temp = tempfile::tempdir().expect("tempdir");
        let repo = temp.path().join("product-api");
        init_repo(&repo, "trunk").await;
        let nested = repo.join("src");
        std::fs::create_dir_all(&nested).expect("nested");

        let inspected = inspect_local_repo(nested.to_str().expect("path"), None)
            .await
            .expect("inspect");
        assert_eq!(inspected.name, "product-api");
        assert_eq!(
            inspected.path,
            std::fs::canonicalize(repo)
                .expect("canonical")
                .to_string_lossy()
        );
        assert_eq!(inspected.base_ref, "trunk");
        assert!(!inspected.base_ref_is_default);
    }

    #[tokio::test]
    async fn main_beats_a_checked_out_feature_branch() {
        let temp = tempfile::tempdir().expect("tempdir");
        let repo = temp.path().join("web");
        init_repo(&repo, "main").await;
        run(&repo, &["checkout", "-q", "-b", "feature/current"]).await;

        let inspected = inspect_local_repo(repo.to_str().expect("path"), Some("Custom web"))
            .await
            .expect("inspect");
        assert_eq!(inspected.name, "Custom web");
        assert_eq!(inspected.base_ref, "main");
        assert!(inspected.base_ref_is_default);
    }

    #[tokio::test]
    async fn rejects_non_git_and_empty_repositories() {
        let temp = tempfile::tempdir().expect("tempdir");
        assert!(
            inspect_local_repo(temp.path().to_str().expect("path"), None)
                .await
                .is_err()
        );

        let empty = temp.path().join("empty");
        std::fs::create_dir_all(&empty).expect("mkdir");
        run(&empty, &["init", "-q", "-b", "main"]).await;
        assert!(inspect_local_repo(empty.to_str().expect("path"), None)
            .await
            .is_err());
    }
}
