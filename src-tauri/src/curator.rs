//! The deterministic half of the workspace Curator (ARCHITECTURE §4.9, §4.11):
//! profile each repo from its manifests and reconcile the cross-repo dependency
//! graph. No agent here — this is the cheap, always-available floor. The
//! semantic one-liner from an agent curator layers on top later; a user edit
//! (source = "user") always outranks re-inference.

use crate::git;
use crate::profile::{self, Edge, RepoFacts};
use crate::store::entities::{repo_profile, repo_ref};
use crate::store::{repo, Db};
use anyhow::Result;
use serde::Serialize;
use std::path::Path;

/// A profile as the UI sees it: decoded arrays + repo name + live staleness.
#[derive(Clone, Debug, Serialize)]
pub struct ProfileView {
    pub repo_id: i32,
    pub repo_name: String,
    pub role: String,
    pub stack: Vec<String>,
    pub summary: String,
    pub published: Vec<String>,
    pub deps: Vec<String>,
    pub source: String,
    pub profiled_commit: String,
    pub stale: bool,
}

/// The workspace dependency graph: profiled repos + the edges between them.
#[derive(Clone, Debug, Serialize)]
pub struct Graph {
    pub nodes: Vec<ProfileView>,
    pub edges: Vec<Edge>,
}

fn json(v: &[String]) -> String {
    serde_json::to_string(v).unwrap_or_else(|_| "[]".into())
}

fn arr(s: &str) -> Vec<String> {
    serde_json::from_str(s).unwrap_or_default()
}

fn facts_of(m: &repo_profile::Model) -> RepoFacts {
    RepoFacts {
        role: m.role.clone(),
        stack: arr(&m.stack),
        summary: m.summary.clone(),
        published: arr(&m.published),
        deps: arr(&m.deps),
    }
}

/// Re-infer a repo's facts from disk and persist. Factual fields
/// (stack/published/deps) always refresh; the opinion fields (summary/role) are
/// preserved when the user has edited them (source = "user").
pub async fn profile_repo(db: &Db, repo: &repo_ref::Model) -> Result<repo_profile::Model> {
    let path = Path::new(&repo.local_git_path);
    let facts = profile::infer_repo_facts(path);
    let commit = git::head_commit(path).unwrap_or_default();

    let prior = repo::get_repo_profile(db, repo.id).await?;
    let user_owned = prior.as_ref().map(|p| p.source == "user").unwrap_or(false);
    let (role, summary, source) = match &prior {
        Some(p) if user_owned => (p.role.clone(), p.summary.clone(), "user"),
        _ => (facts.role.clone(), facts.summary.clone(), "inferred"),
    };

    repo::upsert_repo_profile(
        db,
        repo.id,
        &role,
        &json(&facts.stack),
        &summary,
        &json(&facts.published),
        &json(&facts.deps),
        source,
        &commit,
    )
    .await
}

/// Apply a user edit to the opinion fields; marks the profile user-owned so
/// future re-profiling won't clobber it.
pub async fn edit_profile(
    db: &Db,
    repo_id: i32,
    summary: &str,
    role: &str,
) -> Result<repo_profile::Model> {
    let existing = repo::get_repo_profile(db, repo_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("no profile for repo {repo_id} yet"))?;
    repo::upsert_repo_profile(
        db,
        repo_id,
        role,
        &existing.stack,
        summary,
        &existing.published,
        &existing.deps,
        "user",
        &existing.profiled_commit,
    )
    .await
}

fn view_of(repo: &repo_ref::Model, profile: &repo_profile::Model) -> ProfileView {
    let live = git::head_commit(Path::new(&repo.local_git_path)).ok();
    let stale = match (&live, profile.profiled_commit.as_str()) {
        (Some(_), "") => true,
        (Some(head), at) => head != at,
        (None, _) => false, // can't tell (not a git repo / no commits)
    };
    ProfileView {
        repo_id: repo.id,
        repo_name: repo.name.clone(),
        role: profile.role.clone(),
        stack: arr(&profile.stack),
        summary: profile.summary.clone(),
        published: arr(&profile.published),
        deps: arr(&profile.deps),
        source: profile.source.clone(),
        profiled_commit: profile.profiled_commit.clone(),
        stale,
    }
}

/// All profiled repos in a workspace as the UI sees them (unprofiled repos are
/// omitted; profiling is eager on add, so this is normally every repo).
pub async fn list(db: &Db, workspace_id: i32) -> Result<Vec<ProfileView>> {
    let repos = repo::list_repos(db, workspace_id).await?;
    let mut out = Vec::new();
    for r in &repos {
        if let Some(p) = repo::get_repo_profile(db, r.id).await? {
            out.push(view_of(r, &p));
        }
    }
    Ok(out)
}

/// The workspace dependency graph: nodes + consumer→producer edges, computed
/// from stored profiles (no disk read).
pub async fn graph(db: &Db, workspace_id: i32) -> Result<Graph> {
    let repos = repo::list_repos(db, workspace_id).await?;
    let mut nodes = Vec::new();
    let mut facts: Vec<(i32, RepoFacts)> = Vec::new();
    for r in &repos {
        if let Some(p) = repo::get_repo_profile(db, r.id).await? {
            facts.push((r.id, facts_of(&p)));
            nodes.push(view_of(r, &p));
        }
    }
    let edges = profile::compute_edges(&facts);
    Ok(Graph { nodes, edges })
}
