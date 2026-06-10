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

/// One proposed work line: the ONE repo it writes (by name), and the required
/// reason it must change. Reads are unmanaged — agents read any repo freely
/// (scope rework, spec Part 1). The tool is no longer part of the proposal;
/// it is chosen by the human at approval time (or picked from the workspace
/// default for batch confirm).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProposedDirection {
    pub name: String,
    #[serde(default)]
    pub repo: String,
    #[serde(default)]
    pub reason: String,
    /// Worker mandate: "plan+impl" (default) | "impl-only".
    #[serde(default)]
    pub mandate: String,
    /// Human decision on this write declaration: "" (pending) | "approved" | "denied".
    #[serde(default)]
    pub decision: String,
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
/// The tool is absent from the resolved form; it is provided by the human on the
/// approval card (approve_direction) or taken from the workspace default (confirm).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ResolvedDirection {
    pub name: String,
    /// The one write repo, resolved to a workspace repo.
    pub repo: ScopeEntry,
    pub reason: String,
    /// Worker mandate: "plan+impl" | "impl-only".
    pub mandate: String,
    pub decision: String,
}

/// Resolve one proposed direction's write-repo name to a workspace repo id.
/// `repos` is (id, name); an unknown name is kept with `known = false`.
pub fn resolve(dir: &ProposedDirection, repos: &[(i32, String)]) -> ResolvedDirection {
    let id = repos.iter().find(|(_, n)| *n == dir.repo).map(|(id, _)| *id);
    ResolvedDirection {
        name: dir.name.clone(),
        repo: ScopeEntry {
            repo_id: id.unwrap_or(-1),
            repo_name: dir.repo.clone(),
            known: id.is_some(),
        },
        reason: dir.reason.clone(),
        mandate: repo::normalize_mandate(&dir.mandate).to_string(),
        decision: dir.decision.clone(),
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
    let tool = crate::tools::default_tool(db).await;
    for d in &resolved.directions {
        if !d.repo.known {
            continue; // unknown repo name never resolved to a worktree-able repo
        }
        if d.decision == "approved" || d.decision == "denied" {
            continue; // already handled via per-card approve/deny
        }
        let dir =
            repo::create_direction(
                db, thread_id, &d.name, &tool, d.repo.repo_id, &d.reason, &d.mandate,
            )
            .await?;
        materialize::materialize_direction(db, dir.id).await?;
        created.push(dir.id);
    }
    if let Some(p) = repo::get_plan(db, thread_id).await? {
        repo::upsert_plan(db, thread_id, &p.proposal, "confirmed", &p.created_at).await?;
    }
    Ok(created)
}

/// Approve one proposed direction (by index): mark it approved in the stored
/// proposal, create the real direction bound to its repo + reason using the
/// human-selected `tool`, and materialize its worktree. Returns the new
/// direction id.
pub async fn approve_direction(db: &Db, thread_id: i32, index: usize, tool: &str) -> Result<i32> {
    let plan = repo::get_plan(db, thread_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("no proposal for thread {thread_id}"))?;
    let mut proposal: Proposal = serde_json::from_str(&plan.proposal).unwrap_or_default();
    let pd = proposal
        .directions
        .get(index)
        .ok_or_else(|| anyhow::anyhow!("write trigger {index} out of range"))?
        .clone();
    let repos = workspace_repos(db, thread_id).await?;
    let resolved = resolve(&pd, &repos);
    if !resolved.repo.known {
        anyhow::bail!("repo {:?} is not a known workspace repo", resolved.repo.repo_name);
    }
    let dirs = repo::list_directions(db, thread_id).await?;
    if let Some(existing) = dirs
        .iter()
        .find(|d| d.name == resolved.name && d.repo_id == resolved.repo.repo_id)
    {
        // Already created (e.g. the lead re-proposed and the decision was reset).
        // Idempotent: don't create a second direction/worktree.
        let id = existing.id;
        proposal.directions[index].decision = "approved".to_string();
        persist_decision(db, thread_id, &proposal, &plan).await?;
        return Ok(id);
    }
    let dir = repo::create_direction(
        db,
        thread_id,
        &resolved.name,
        tool,
        resolved.repo.repo_id,
        &resolved.reason,
        &resolved.mandate,
    )
    .await?;
    materialize::materialize_direction(db, dir.id).await?;
    proposal.directions[index].decision = "approved".to_string();
    persist_decision(db, thread_id, &proposal, &plan).await?;
    Ok(dir.id)
}

/// Deny one proposed direction (by index): mark it denied in the stored
/// proposal. Returns the denied direction's (name, repo_name) for the caller to
/// relay to the lead over the bus.
pub async fn deny_direction(db: &Db, thread_id: i32, index: usize) -> Result<(String, String)> {
    let plan = repo::get_plan(db, thread_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("no proposal for thread {thread_id}"))?;
    let mut proposal: Proposal = serde_json::from_str(&plan.proposal).unwrap_or_default();
    let pd = proposal
        .directions
        .get_mut(index)
        .ok_or_else(|| anyhow::anyhow!("write trigger {index} out of range"))?;
    pd.decision = "denied".to_string();
    let info = (pd.name.clone(), pd.repo.clone());
    persist_decision(db, thread_id, &proposal, &plan).await?;
    Ok(info)
}

async fn persist_decision(
    db: &Db,
    thread_id: i32,
    proposal: &Proposal,
    plan: &crate::store::entities::plan::Model,
) -> Result<()> {
    let json = serde_json::to_string(proposal)?;
    repo::upsert_plan(db, thread_id, &json, &plan.status, &plan.created_at).await?;
    Ok(())
}

/// One pending write declaration: its index into the stored proposal plus the
/// resolved direction fields. Pending = known repo AND decision not yet made.
#[derive(Clone, Debug, Serialize)]
pub struct PendingWrite {
    pub index: usize,
    pub name: String,
    pub repo_name: String,
    pub reason: String,
}

/// The pending write declarations for a thread (known repo + undecided).
pub async fn pending_writes(db: &Db, thread_id: i32) -> Result<Vec<PendingWrite>> {
    let Some(p) = get_resolved(db, thread_id).await? else {
        return Ok(Vec::new());
    };
    // A confirmed plan has no pending writes: confirm() created every still-
    // undecided direction wholesale, so lingering cards would double-create.
    if p.status == "confirmed" {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for (i, d) in p.directions.iter().enumerate() {
        if d.repo.known && d.decision.is_empty() {
            out.push(PendingWrite {
                index: i,
                name: d.name.clone(),
                repo_name: d.repo.repo_name.clone(),
                reason: d.reason.clone(),
            });
        }
    }
    Ok(out)
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
    fn resolves_repo_name_to_id_with_reason() {
        let d = ProposedDirection {
            name: "Payments".into(),
            repo: "api".into(),
            reason: "add the discount endpoint".into(),
            mandate: "".into(),
            decision: "".into(),
        };
        let r = resolve(&d, &repos());
        assert_eq!(r.name, "Payments");
        assert_eq!(r.mandate, "plan+impl"); // empty mandate normalizes to the default
        assert_eq!(r.reason, "add the discount endpoint");
        assert_eq!(r.repo, ScopeEntry { repo_id: 2, repo_name: "api".into(), known: true });
    }

    #[test]
    fn unknown_repo_name_is_flagged_not_dropped() {
        let d = ProposedDirection {
            name: "X".into(),
            repo: "ghost-repo".into(),
            reason: "whatever".into(),
            mandate: "impl-only".into(),
            decision: "".into(),
        };
        let r = resolve(&d, &repos());
        assert!(!r.repo.known);
        assert_eq!(r.mandate, "impl-only");
        assert_eq!(r.repo.repo_id, -1);
    }

    #[test]
    fn proposal_parses_with_missing_and_legacy_fields() {
        // Legacy proposals carried a "tool" per direction; serde must ignore it.
        let p: Proposal = serde_json::from_str(
            r#"{ "directions": [ { "name": "wip", "tool": "claude" } ] }"#,
        )
        .unwrap();
        assert_eq!(p.rationale, "");
        assert_eq!(p.directions.len(), 1);
        assert_eq!(p.directions[0].repo, "");
        assert_eq!(p.directions[0].reason, "");
    }

    #[test]
    fn resolve_carries_decision_through() {
        let d = ProposedDirection {
            name: "X".into(),
            repo: "api".into(),
            reason: "r".into(),
            mandate: "plan+impl".into(),
            decision: "approved".into(),
        };
        let r = resolve(&d, &repos());
        assert_eq!(r.decision, "approved");
    }

    #[test]
    fn pending_filter_skips_decided_and_unknown() {
        let rs = vec![
            resolve(&ProposedDirection { name: "a".into(), repo: "api".into(), reason: "r".into(), mandate: "".into(), decision: "".into() }, &repos()),
            resolve(&ProposedDirection { name: "b".into(), repo: "api".into(), reason: "r".into(), mandate: "".into(), decision: "approved".into() }, &repos()),
            resolve(&ProposedDirection { name: "c".into(), repo: "ghost".into(), reason: "r".into(), mandate: "".into(), decision: "".into() }, &repos()),
        ];
        let pending: Vec<_> = rs.iter().enumerate()
            .filter(|(_, d)| d.repo.known && d.decision.is_empty())
            .map(|(i, _)| i)
            .collect();
        assert_eq!(pending, vec![0]);
    }

    // ---- DB-backed: approve/deny/pending against a real repo + worktree ----

    fn sh(dir: &std::path::Path, args: &[&str]) {
        let st = std::process::Command::new(args[0])
            .args(&args[1..])
            .current_dir(dir)
            .status()
            .unwrap();
        assert!(st.success(), "cmd {:?} failed", args);
    }

    /// A minimal committed git repo so materialize can build a worktree from it.
    fn make_repo(root: &std::path::Path, name: &str) -> std::path::PathBuf {
        let p = root.join(name);
        std::fs::create_dir_all(&p).unwrap();
        sh(&p, &["git", "init", "-q"]);
        sh(&p, &["git", "config", "user.email", "t@t.t"]);
        sh(&p, &["git", "config", "user.name", "t"]);
        std::fs::write(p.join("README.md"), "# x\n").unwrap();
        sh(&p, &["git", "add", "-A"]);
        sh(&p, &["git", "commit", "-q", "-m", "init"]);
        p
    }

    #[tokio::test]
    async fn approve_deny_pending_against_db() {
        // Hold the shared env lock for the whole window WEFT_HOME is set, so the
        // default-home paths test can't observe our override. Panic-tolerant.
        let _env = crate::paths::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tag = format!("weft-planner-{}", std::process::id());
        let root = std::env::temp_dir().join(format!("{tag}-root"));
        let weft_home = std::env::temp_dir().join(format!("{tag}-home"));
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&weft_home);
        std::env::set_var("WEFT_HOME", weft_home.to_str().unwrap());
        let repo_path = make_repo(&root, "api");

        let db = Db::connect("sqlite::memory:").await.unwrap();
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        let ra = repo::add_repo_ref(&db, ws.id, "api", repo_path.to_str().unwrap(), "main", "claude")
            .await
            .unwrap();
        let t = repo::create_thread(&db, ws.id, "t1", "feature", "claude").await.unwrap();

        // Proposal: one known-repo write (pending) + one unknown-repo write (pending).
        let proposal = Proposal {
            rationale: "r".into(),
            directions: vec![
                ProposedDirection {
                    name: "Payments".into(),
                    repo: "api".into(),
                    reason: "add discount endpoint".into(),
                    mandate: "impl-only".into(),
                    decision: "".into(),
                },
                ProposedDirection {
                    name: "Ghost".into(),
                    repo: "nope".into(),
                    reason: "n/a".into(),
                    mandate: "".into(),
                    decision: "".into(),
                },
            ],
        };
        save_proposal(&db, t.id, &proposal).await.unwrap();

        // pending_writes surfaces only the known-repo, undecided one (index 0).
        let pending = pending_writes(&db, t.id).await.unwrap();
        assert_eq!(pending.len(), 1, "only the known-repo write is pending");
        assert_eq!(pending[0].index, 0);
        assert_eq!(pending[0].repo_name, "api");
        assert_eq!(pending[0].reason, "add discount endpoint");

        // Approve index 0 -> a real direction is created bound to the repo + reason.
        let id = approve_direction(&db, t.id, 0, "codex").await.unwrap();
        let dirs = repo::list_directions(&db, t.id).await.unwrap();
        assert_eq!(dirs.len(), 1, "exactly one direction created");
        assert_eq!(dirs[0].id, id);
        assert_eq!(dirs[0].repo_id, ra.id);
        assert_eq!(dirs[0].tool, "codex", "card-picked tool lands on the direction");
        // No longer pending once approved.
        assert!(pending_writes(&db, t.id).await.unwrap().is_empty());

        // Re-proposing wipes decisions back to "" (whole array replaced).
        save_proposal(&db, t.id, &proposal).await.unwrap();
        assert_eq!(pending_writes(&db, t.id).await.unwrap().len(), 1);

        // Approve the SAME index again -> idempotent: same id, no second direction.
        let id2 = approve_direction(&db, t.id, 0, "codex").await.unwrap();
        assert_eq!(id2, id, "idempotent approve returns the existing direction");
        let dirs2 = repo::list_directions(&db, t.id).await.unwrap();
        assert_eq!(dirs2.len(), 1, "no second direction created on re-approve");

        // Deny the unknown-repo write -> returns (name, repo), marks it denied,
        // and pending_writes drops it (it was never known anyway).
        let (name, repo_name) = deny_direction(&db, t.id, 1).await.unwrap();
        assert_eq!(name, "Ghost");
        assert_eq!(repo_name, "nope");
        let p = repo::get_plan(&db, t.id).await.unwrap().unwrap();
        let stored: Proposal = serde_json::from_str(&p.proposal).unwrap();
        assert_eq!(stored.directions[1].decision, "denied");

        // Cleanup.
        let removed = repo::delete_thread_cascade(&db, t.id).await.unwrap();
        let _ = materialize::cleanup_worktrees(&db, &removed).await;
        std::env::remove_var("WEFT_HOME");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&weft_home);
    }
}
