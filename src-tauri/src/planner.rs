//! The planner: capturing the lead's proposed decomposition of a Task into
//! directions + per-repo scope (ARCHITECTURE §4.10, §5.1), and confirming it
//! into real directions. The lead (a native CLI session) calls the planner MCP
//! to read the repo map and `propose_directions`; the human reviews/edits in the
//! scope-confirm step, then confirms — which materializes worktrees.
//!
//! Repos are addressed by NAME across the MCP boundary (the lead reasons over
//! names from the repo map); resolution to ids happens here against the
//! workspace, so an unknown name is surfaced, never silently dropped.

use crate::materialize;
use crate::store::{repo, Db};
use anyhow::Result;
use serde::{Deserialize, Serialize};

/// One proposed work line: a tool plus the repos it WRITES, by name. Reads are
/// unmanaged (the change note "scope simplification"): an agent may read any
/// file; only the write set is scoped, materialized, and confirmed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProposedDirection {
    pub name: String,
    pub tool: String,
    #[serde(default)]
    pub writes: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Proposal {
    #[serde(default)]
    pub rationale: String,
    #[serde(default)]
    pub directions: Vec<ProposedDirection>,
}

/// A write repo in a resolved direction: id (-1 if the name is unknown), the
/// name as written, and whether it matched a workspace repo.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ScopeEntry {
    pub repo_id: i32,
    pub repo_name: String,
    pub known: bool,
}

/// A direction resolved against the workspace's repos, ready for the UI / confirm.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ResolvedDirection {
    pub name: String,
    pub tool: String,
    /// The write set, resolved to workspace repos.
    pub writes: Vec<ScopeEntry>,
}

/// Resolve one proposed direction's write-repo names to workspace repo ids.
/// `repos` is (id, name) for the workspace; unknown names are kept with
/// `known = false`, never silently dropped. Duplicates are de-duped.
pub fn resolve(dir: &ProposedDirection, repos: &[(i32, String)]) -> ResolvedDirection {
    let id_of = |name: &str| repos.iter().find(|(_, n)| n == name).map(|(id, _)| *id);
    let mut writes = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for name in &dir.writes {
        if !seen.insert(name.clone()) {
            continue;
        }
        let id = id_of(name);
        writes.push(ScopeEntry {
            repo_id: id.unwrap_or(-1),
            repo_name: name.clone(),
            known: id.is_some(),
        });
    }
    ResolvedDirection {
        name: dir.name.clone(),
        tool: dir.tool.clone(),
        writes,
    }
}

// ---- DB orchestration ----

fn now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{secs}")
}

/// Store (replace) the proposal for a thread, status = "proposed".
pub async fn save_proposal(db: &Db, thread_id: i32, proposal: &Proposal) -> Result<()> {
    let json = serde_json::to_string(proposal)?;
    repo::upsert_plan(db, thread_id, &json, "proposed", &now()).await?;
    Ok(())
}

/// The stored proposal for a thread, resolved against its workspace repos.
pub async fn get_resolved(db: &Db, thread_id: i32) -> Result<Option<ResolvedProposal>> {
    let Some(p) = repo::get_plan(db, thread_id).await? else {
        return Ok(None);
    };
    let proposal: Proposal = serde_json::from_str(&p.proposal).unwrap_or_default();
    let repos = workspace_repos(db, thread_id).await?;
    let directions = proposal.directions.iter().map(|d| resolve(d, &repos)).collect();
    Ok(Some(ResolvedProposal {
        thread_id,
        rationale: proposal.rationale,
        status: p.status,
        directions,
    }))
}

#[derive(Clone, Debug, Serialize)]
pub struct ResolvedProposal {
    pub thread_id: i32,
    pub rationale: String,
    pub status: String,
    pub directions: Vec<ResolvedDirection>,
}

/// Confirm the stored proposal: create each direction with its known-repo scope
/// and materialize its worktrees. Marks the plan confirmed. Unknown repo names
/// are skipped (they never resolved to a worktree-able repo).
pub async fn confirm(db: &Db, thread_id: i32) -> Result<Vec<i32>> {
    let resolved = get_resolved(db, thread_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("no proposal to confirm for thread {thread_id}"))?;
    let mut created = Vec::new();
    for d in &resolved.directions {
        let scope: Vec<(i32, String)> = d
            .writes
            .iter()
            .filter(|s| s.known)
            .map(|s| (s.repo_id, "write".to_string()))
            .collect();
        let dir = repo::create_direction(db, thread_id, &d.name, &d.tool, &scope).await?;
        materialize::materialize_direction(db, dir.id).await?;
        created.push(dir.id);
    }
    if let Some(p) = repo::get_plan(db, thread_id).await? {
        repo::upsert_plan(db, thread_id, &p.proposal, "confirmed", &p.created_at).await?;
    }
    Ok(created)
}

async fn workspace_repos(db: &Db, thread_id: i32) -> Result<Vec<(i32, String)>> {
    let t = repo::get_thread(db, thread_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("thread {thread_id} not found"))?;
    let repos = repo::list_repos(db, t.workspace_id).await?;
    Ok(repos.into_iter().map(|r| (r.id, r.name)).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repos() -> Vec<(i32, String)> {
        vec![
            (1, "web-app".into()),
            (2, "api".into()),
            (3, "shared-lib".into()),
        ]
    }

    #[test]
    fn resolves_write_names_to_ids() {
        let d = ProposedDirection {
            name: "Payments".into(),
            tool: "claude".into(),
            writes: vec!["web-app".into(), "api".into()],
        };
        let r = resolve(&d, &repos());
        assert_eq!(r.name, "Payments");
        assert_eq!(r.writes.len(), 2);
        assert_eq!(r.writes[0], ScopeEntry { repo_id: 1, repo_name: "web-app".into(), known: true });
        assert_eq!(r.writes[1], ScopeEntry { repo_id: 2, repo_name: "api".into(), known: true });
    }

    #[test]
    fn unknown_repo_name_is_flagged_not_dropped() {
        let d = ProposedDirection {
            name: "X".into(),
            tool: "codex".into(),
            writes: vec!["ghost-repo".into()],
        };
        let r = resolve(&d, &repos());
        assert_eq!(r.writes.len(), 1);
        assert!(!r.writes[0].known);
        assert_eq!(r.writes[0].repo_id, -1);
    }

    #[test]
    fn duplicate_write_is_deduped() {
        let d = ProposedDirection {
            name: "X".into(),
            tool: "claude".into(),
            writes: vec!["api".into(), "api".into()],
        };
        let r = resolve(&d, &repos());
        assert_eq!(r.writes.len(), 1);
    }

    #[test]
    fn proposal_parses_with_missing_optional_fields() {
        let p: Proposal = serde_json::from_str(
            r#"{ "directions": [ { "name": "no writes yet", "tool": "claude" } ] }"#,
        )
        .unwrap();
        assert_eq!(p.rationale, "");
        assert_eq!(p.directions.len(), 1);
        assert!(p.directions[0].writes.is_empty());
    }
}
