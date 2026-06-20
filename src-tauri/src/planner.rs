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

/// Deserialize a JSON string OR null into a String (null/absent → ""). The lead
/// tool may emit `base_branch: null` for "use the repo default"; without this,
/// serde rejects the whole Proposal and `call_planner` drops every direction.
fn de_string_or_null<'de, D: serde::Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    Ok(Option::<String>::deserialize(d)?.unwrap_or_default())
}

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
    /// Branch in the target repo to branch the work off; empty = repo default.
    /// `#[serde(default)]` covers a missing key; `deserialize_with` covers `null`.
    #[serde(default, deserialize_with = "de_string_or_null")]
    pub base_branch: String,
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
    pub base_branch: String,
    pub decision: String,
}

/// Resolve one proposed direction's write-repo name to a workspace repo id.
/// `repos` is (id, name); an unknown name is kept with `known = false`.
pub fn resolve(dir: &ProposedDirection, repos: &[(i32, String)]) -> ResolvedDirection {
    let id = repos
        .iter()
        .find(|(_, n)| *n == dir.repo)
        .map(|(id, _)| *id);
    ResolvedDirection {
        name: dir.name.clone(),
        repo: ScopeEntry {
            repo_id: id.unwrap_or(-1),
            repo_name: dir.repo.clone(),
            known: id.is_some(),
        },
        reason: dir.reason.clone(),
        mandate: repo::normalize_mandate(&dir.mandate).to_string(),
        base_branch: dir.base_branch.clone(),
        decision: dir.decision.clone(),
    }
}

// ---- Direction reuse reconciliation ----

/// Decide whether a re-proposal can reuse an already-materialized direction of the
/// same name+repo, or must be rejected (the worktree is branched off a fixed base; a
/// live worktree can't be re-based). `existing.base_branch` is the immutable
/// branch-off base recorded at materialize: "" means a legacy row (materialized
/// before base tracking) whose true base is unknown. Compares EFFECTIVE bases —
/// empty resolves via the LIVE remote default (cached fallback), non-empty strips
/// `origin/`. Ok(()) = safe to reuse; Err = reject (delete + recreate).
///
/// Treats an existing recorded base of `"HEAD"` as equivalent to an empty/default
/// base: it means the blank-base path fell all the way back to the detached HEAD
/// (no main/master in the repo) — a blank re-proposal is the same intent and must
/// reuse safely without a conflict. Only genuinely different explicit bases
/// (e.g. existing "develop" vs proposed "main") are treated as conflicts.
fn reconcile_reuse(
    existing: &crate::store::entities::direction::Model,
    proposed_base: &str,
    repo_path: &std::path::Path,
    base_ref: &str,
) -> Result<()> {
    let effective = |b: &str| -> String {
        let b = b.trim();
        if b.is_empty() {
            // Match materialize's resolution: live default, else the recorded base_ref
            // (the live default captured at register) when it still resolves, else the
            // cached chain — so the reuse comparison agrees with what was materialized.
            crate::git::live_default_branch(repo_path)
                .unwrap_or_else(|| crate::git::recorded_base_or_default(repo_path, base_ref))
        } else {
            b.strip_prefix("origin/").unwrap_or(b).to_string()
        }
    };
    // Legacy row: true base unknown → reuse only if the re-proposal is also default.
    if existing.base_branch.trim().is_empty() {
        if !proposed_base.trim().is_empty() {
            anyhow::bail!(
                "direction {:?} predates base tracking (unknown branch-off base); delete the sub-task to recreate it from {:?}",
                existing.name, proposed_base
            );
        }
        return Ok(());
    }
    // "HEAD" as the recorded base means the blank-base fallback landed on a detached
    // HEAD (no main/master branch in the repo). A blank re-proposal is the same
    // default intent — allow reuse without treating it as a genuine base mismatch.
    // Only a truly non-empty, non-HEAD explicit base is considered a conflict.
    let existing_base = existing.base_branch.trim();
    if existing_base == "HEAD" && proposed_base.trim().is_empty() {
        return Ok(());
    }
    if effective(existing_base) != effective(proposed_base) {
        anyhow::bail!(
            "direction {:?} already exists with base {:?}; delete the sub-task to recreate it from {:?}",
            existing.name, existing.base_branch, proposed_base
        );
    }
    Ok(())
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
    let directions = proposal
        .directions
        .iter()
        .map(|d| resolve(d, &repos))
        .collect();
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
///
/// Atomic: if ANY lane fails to create or materialize, ALL lanes created in this
/// attempt are rolled back (worktree on disk + branch + DB rows) and the error is
/// returned. A corrected retry therefore always starts clean — no partial state
/// survives for it to accidentally reuse.
///
/// Idempotent on a fully-confirmed plan: if the plan is already "confirmed" the
/// existing direction ids are returned without re-creating anything (covers the
/// dispatch-retry case where the frontend calls confirm again to redispatch workers).
pub async fn confirm(db: &Db, thread_id: i32) -> Result<Vec<i32>> {
    let resolved = get_resolved(db, thread_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("no proposal to confirm for thread {thread_id}"))?;
    // Idempotent fast-path: the plan was already fully confirmed in a prior call.
    // Return ONLY the ids belonging to lanes that confirm itself creates/dispatches:
    // - known repo, AND
    // - decision was NOT "approved" or "denied" at confirm time (those are handled
    //   by approve_direction / deny_direction and must not be re-dispatched here).
    // This mirrors exactly the lane-selection filter in the main confirm path below.
    // Re-materialize each lane before returning so a reclaimed worktree dir is
    // recreated and the dispatched worker has a live checkout. materialize_direction
    // is idempotent when the dir already exists.
    if resolved.status == "confirmed" {
        let all = repo::list_directions(db, thread_id).await?;
        let matching: Vec<i32> = all
            .into_iter()
            .filter(|d| {
                resolved.directions.iter().any(|p| {
                    p.repo.known
                        && p.name == d.name
                        && p.repo.repo_id == d.repo_id
                        && p.decision != "approved"
                        && p.decision != "denied"
                })
            })
            .map(|d| d.id)
            .collect();
        for &id in &matching {
            materialize::materialize_direction(db, id).await?;
        }
        return Ok(matching);
    }
    let existing_dirs = repo::list_directions(db, thread_id).await?;
    let tool = crate::tools::default_tool(db).await;
    // dispatch_ids = every lane to dispatch (reused + newly created); returned to caller.
    // created_now  = ONLY lanes created+materialized in THIS attempt; the rollback set.
    // Reused lanes are NEVER in created_now — they must not be torn down on a later
    // failure because they existed (and may be running) before this confirm call.
    let mut dispatch_ids: Vec<i32> = Vec::new();
    let mut created_now: Vec<i32> = Vec::new();
    for d in &resolved.directions {
        if !d.repo.known {
            continue; // unknown repo name never resolved to a worktree-able repo
        }
        if d.decision == "approved" || d.decision == "denied" {
            continue; // already handled via per-card approve/deny
        }
        // A lane may already be materialized (e.g. approved via Needs-you, then the
        // lead re-proposed it resetting the decision to ""). Reuse the same-base lane
        // (and still return its id for dispatch) or reject a base change — never
        // create a duplicate direction/worktree.
        if let Some(ex) = existing_dirs
            .iter()
            .find(|x| x.name == d.name && x.repo_id == d.repo.repo_id)
        {
            let repo_ref = match repo::get_repo(db, d.repo.repo_id).await {
                Ok(Some(r)) => r,
                Ok(None) => {
                    rollback_created(db, &created_now).await;
                    anyhow::bail!("repo {} not found", d.repo.repo_id);
                }
                Err(e) => {
                    rollback_created(db, &created_now).await;
                    return Err(e.into());
                }
            };
            if let Err(err) = reconcile_reuse(
                ex,
                &d.base_branch,
                std::path::Path::new(&repo_ref.local_git_path),
                &repo_ref.base_ref,
            ) {
                rollback_created(db, &created_now).await;
                return Err(err);
            }
            // Re-materialize in case the worktree dir was reclaimed (exists=false):
            // materialize_direction is idempotent when the dir already exists, so
            // calling it here is always safe. Mirror what the normal (non-reuse) path
            // already does below.
            if let Err(err) = materialize::materialize_direction(db, ex.id).await {
                rollback_created(db, &created_now).await;
                return Err(err);
            }
            dispatch_ids.push(ex.id);
            continue;
        }
        let dir = match repo::create_direction(
            db,
            thread_id,
            &d.name,
            &tool,
            d.repo.repo_id,
            &d.reason,
            &d.mandate,
            &d.base_branch,
        )
        .await
        {
            Ok(dir) => dir,
            Err(err) => {
                rollback_created(db, &created_now).await;
                return Err(err);
            }
        };
        if let Err(err) = materialize::materialize_direction(db, dir.id).await {
            // The failing lane has no worktree yet; drop its row, then roll back
            // the earlier (materialized) lanes so a corrected retry starts clean.
            let _ = repo::delete_direction(db, dir.id).await;
            rollback_created(db, &created_now).await;
            return Err(err);
        }
        created_now.push(dir.id);
        dispatch_ids.push(dir.id);
    }
    if let Some(p) = repo::get_plan(db, thread_id).await? {
        repo::upsert_plan(db, thread_id, &p.proposal, "confirmed", &p.created_at).await?;
    }
    Ok(dispatch_ids)
}

/// Tear down lanes created in the current confirm attempt (used on any failure to keep
/// confirm atomic). Best-effort per lane — errors are ignored so the original failure
/// is the one returned.
async fn rollback_created(db: &Db, ids: &[i32]) {
    for &id in ids {
        let _ = materialize::rollback_direction(db, id).await;
    }
}

/// Approve one proposed direction (by index): mark it approved in the stored
/// proposal, create the real direction bound to its repo + reason using the
/// human-selected `tool`, and materialize its worktree. Returns the new
/// direction id.
///
/// Idempotent on re-approve: if the direction already exists, its id is
/// returned and a differing `tool` pick is ignored — the first pick wins.
pub async fn approve_direction(db: &Db, thread_id: i32, index: usize, tool: &str) -> Result<i32> {
    if !crate::detect::TOOL_PRIORITY.contains(&tool) {
        anyhow::bail!(
            "unknown tool {tool:?}; expected one of {:?}",
            crate::detect::TOOL_PRIORITY
        );
    }
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
        anyhow::bail!(
            "repo {:?} is not a known workspace repo",
            resolved.repo.repo_name
        );
    }
    let dirs = repo::list_directions(db, thread_id).await?;
    if let Some(existing) = dirs
        .iter()
        .find(|d| d.name == resolved.name && d.repo_id == resolved.repo.repo_id)
    {
        // The existing direction's worktree is already branched off its stored base;
        // a re-proposal that changes the base can't silently re-base a live (possibly
        // worker-occupied) worktree. Surface the conflict rather than approving with a
        // mismatched base. (Same base → idempotent reuse.)
        // Also handles legacy rows (base_branch == "") where the true base is unknown:
        // those may only be reused when the re-proposal is also default (empty base).
        // Uses the LIVE remote default for comparison (not the cached origin/HEAD).
        let repo_ref = repo::get_repo(db, resolved.repo.repo_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("repo {} not found", resolved.repo.repo_id))?;
        reconcile_reuse(
            existing,
            &resolved.base_branch,
            std::path::Path::new(&repo_ref.local_git_path),
            &repo_ref.base_ref,
        )?;
        // Already created (e.g. the lead re-proposed and the decision was reset).
        // Idempotent: don't create a second direction/worktree, but DO re-materialize
        // in case the worktree dir was reclaimed (exists=false) — so the lane has a
        // live worktree to dispatch. Mirror what the normal (non-reuse) path does below.
        let id = existing.id;
        if let Err(err) = materialize::materialize_direction(db, id).await {
            return Err(err);
        }
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
        &resolved.base_branch,
    )
    .await?;
    if let Err(err) = materialize::materialize_direction(db, dir.id).await {
        // Roll back the just-created row so a corrected retry starts clean and
        // doesn't hit the idempotent fast-path with a worktree-less task.
        let _ = repo::delete_direction(db, dir.id).await;
        return Err(err);
    }
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

/// Set one proposed direction's base branch in the stored proposal, by index.
/// Targeted read-modify-write that preserves the plan's status + created_at, so
/// concurrent edits to different directions don't clobber and a confirmed plan is
/// never downgraded back to "proposed".
///
/// `expected_name` and `expected_repo` are the lane identity the caller edited:
/// if the lead re-proposed while the blur-save was in flight, the direction at
/// `index` may now belong to a DIFFERENT lane — we verify and reject rather than
/// silently overwriting the wrong lane.
pub async fn set_direction_base(
    db: &Db,
    thread_id: i32,
    index: usize,
    expected_name: &str,
    expected_repo: &str,
    base: &str,
) -> Result<()> {
    let plan = repo::get_plan(db, thread_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("no proposal for thread {thread_id}"))?;
    let mut proposal: Proposal = serde_json::from_str(&plan.proposal).unwrap_or_default();
    let pd = proposal
        .directions
        .get_mut(index)
        .ok_or_else(|| anyhow::anyhow!("direction index {index} out of range"))?;
    // Verify the lane still has the identity the client edited — a re-propose may
    // have replaced the proposal and shifted what sits at this index.
    if pd.name != expected_name || pd.repo != expected_repo {
        anyhow::bail!("proposal lane at index {index} changed (re-proposed); base not applied");
    }
    pd.base_branch = base.trim().to_string();
    persist_decision(db, thread_id, &proposal, &plan).await?;
    Ok(())
}

async fn persist_decision(
    db: &Db,
    thread_id: i32,
    proposal: &Proposal,
    plan: &crate::store::entities::plan::Model,
) -> Result<()> {
    let json = serde_json::to_string(proposal)?;
    // CAS on the proposal we read (`plan.proposal`): if a lead re-proposal landed between
    // our read and this write, the stored proposal no longer matches, and we reject rather
    // than clobbering the fresh re-propose with our stale full proposal (the frontend's
    // base-save latch then aborts confirm/approve/deny and refreshes).
    let applied =
        repo::update_plan_proposal_cas(db, thread_id, &json, &plan.proposal, &plan.status).await?;
    if !applied {
        anyhow::bail!("proposal changed (re-proposed) before the edit was written; not applied");
    }
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
    pub base_branch: String,
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
                base_branch: d.base_branch.clone(),
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
            base_branch: "".into(),
            decision: "".into(),
        };
        let r = resolve(&d, &repos());
        assert_eq!(r.name, "Payments");
        assert_eq!(r.mandate, "plan+impl"); // empty mandate normalizes to the default
        assert_eq!(r.reason, "add the discount endpoint");
        assert_eq!(
            r.repo,
            ScopeEntry {
                repo_id: 2,
                repo_name: "api".into(),
                known: true
            }
        );
    }

    #[test]
    fn unknown_repo_name_is_flagged_not_dropped() {
        let d = ProposedDirection {
            name: "X".into(),
            repo: "ghost-repo".into(),
            reason: "whatever".into(),
            mandate: "impl-only".into(),
            base_branch: "".into(),
            decision: "".into(),
        };
        let r = resolve(&d, &repos());
        assert!(!r.repo.known);
        assert_eq!(r.mandate, "impl-only");
        assert_eq!(r.repo.repo_id, -1);
    }

    #[test]
    fn proposal_parses_base_branch_and_defaults_empty() {
        let p: Proposal = serde_json::from_str(
            r#"{ "directions": [ { "name": "a", "repo": "api", "base_branch": "develop" }, { "name": "b", "repo": "api" } ] }"#,
        )
        .unwrap();
        assert_eq!(p.directions[0].base_branch, "develop");
        assert_eq!(p.directions[1].base_branch, "", "absent base_branch defaults to empty");
    }

    #[test]
    fn proposal_parses_with_missing_and_legacy_fields() {
        // Legacy proposals carried a "tool" per direction; serde must ignore it.
        let p: Proposal =
            serde_json::from_str(r#"{ "directions": [ { "name": "wip", "tool": "claude" } ] }"#)
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
            base_branch: "".into(),
            decision: "approved".into(),
        };
        let r = resolve(&d, &repos());
        assert_eq!(r.decision, "approved");
    }

    #[test]
    fn pending_filter_skips_decided_and_unknown() {
        let rs = vec![
            resolve(
                &ProposedDirection {
                    name: "a".into(),
                    repo: "api".into(),
                    reason: "r".into(),
                    mandate: "".into(),
                    base_branch: "".into(),
                    decision: "".into(),
                },
                &repos(),
            ),
            resolve(
                &ProposedDirection {
                    name: "b".into(),
                    repo: "api".into(),
                    reason: "r".into(),
                    mandate: "".into(),
                    base_branch: "".into(),
                    decision: "approved".into(),
                },
                &repos(),
            ),
            resolve(
                &ProposedDirection {
                    name: "c".into(),
                    repo: "ghost".into(),
                    reason: "r".into(),
                    mandate: "".into(),
                    base_branch: "".into(),
                    decision: "".into(),
                },
                &repos(),
            ),
        ];
        let pending: Vec<_> = rs
            .iter()
            .enumerate()
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
        let _env = crate::paths::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tag = format!("weft-planner-{}", std::process::id());
        let root = std::env::temp_dir().join(format!("{tag}-root"));
        let weft_home = std::env::temp_dir().join(format!("{tag}-home"));
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&weft_home);
        std::env::set_var("WEFT_HOME", weft_home.to_str().unwrap());
        let repo_path = make_repo(&root, "api");

        let db = Db::connect("sqlite::memory:").await.unwrap();
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        let ra = repo::add_repo_ref(&db, ws.id, "api", repo_path.to_str().unwrap(), "main", "")
            .await
            .unwrap();
        let t = repo::create_thread(&db, ws.id, "t1", "feature", "claude")
            .await
            .unwrap();

        // Proposal: one known-repo write (pending) + one unknown-repo write (pending).
        let proposal = Proposal {
            rationale: "r".into(),
            directions: vec![
                ProposedDirection {
                    name: "Payments".into(),
                    repo: "api".into(),
                    reason: "add discount endpoint".into(),
                    mandate: "impl-only".into(),
                    base_branch: "".into(),
                    decision: "".into(),
                },
                ProposedDirection {
                    name: "Ghost".into(),
                    repo: "nope".into(),
                    reason: "n/a".into(),
                    mandate: "".into(),
                    base_branch: "".into(),
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

        // An unknown tool name is rejected before anything is created.
        assert!(
            approve_direction(&db, t.id, 0, "foo").await.is_err(),
            "unknown tool must be rejected"
        );
        assert!(
            repo::list_directions(&db, t.id).await.unwrap().is_empty(),
            "rejected approve creates nothing"
        );

        // Approve index 0 -> a real direction is created bound to the repo + reason.
        let id = approve_direction(&db, t.id, 0, "codex").await.unwrap();
        let dirs = repo::list_directions(&db, t.id).await.unwrap();
        assert_eq!(dirs.len(), 1, "exactly one direction created");
        assert_eq!(dirs[0].id, id);
        assert_eq!(dirs[0].repo_id, ra.id);
        assert_eq!(
            dirs[0].tool, "codex",
            "card-picked tool lands on the direction"
        );
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

        // Re-approve with a DIFFERENT tool -> still idempotent: the first pick
        // wins, the new pick is ignored, and no second direction appears.
        let id3 = approve_direction(&db, t.id, 0, "claude").await.unwrap();
        assert_eq!(
            id3, id,
            "idempotent re-approve ignores a different tool pick"
        );
        let dirs3 = repo::list_directions(&db, t.id).await.unwrap();
        assert_eq!(
            dirs3.len(),
            1,
            "no second direction created on differing re-approve"
        );
        assert_eq!(dirs3[0].tool, "codex", "first tool pick wins on re-approve");

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

    #[tokio::test]
    async fn set_direction_base_targeted_keeps_status() {
        let _env = crate::paths::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tag = format!("weft-setbase-{}", std::process::id());
        let root = std::env::temp_dir().join(format!("{tag}-root"));
        let weft_home = std::env::temp_dir().join(format!("{tag}-home"));
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&weft_home);
        std::env::set_var("WEFT_HOME", weft_home.to_str().unwrap());
        let _repo_path = make_repo(&root, "api");
        let db = Db::connect("sqlite::memory:").await.unwrap();
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        let _ra = repo::add_repo_ref(&db, ws.id, "api", root.join("api").to_str().unwrap(), "main", "").await.unwrap();
        let t = repo::create_thread(&db, ws.id, "t1", "feature", "claude").await.unwrap();
        let proposal = Proposal { rationale: "r".into(), directions: vec![
            ProposedDirection { name:"A".into(), repo:"api".into(), reason:"r".into(), mandate:"".into(), base_branch:"".into(), decision:"".into() },
            ProposedDirection { name:"B".into(), repo:"api".into(), reason:"r".into(), mandate:"".into(), base_branch:"".into(), decision:"".into() },
        ]};
        save_proposal(&db, t.id, &proposal).await.unwrap();
        // Simulate a confirmed plan, then a targeted base edit must NOT downgrade status.
        let plan = repo::get_plan(&db, t.id).await.unwrap().unwrap();
        repo::upsert_plan(&db, t.id, &plan.proposal, "confirmed", &plan.created_at).await.unwrap();
        set_direction_base(&db, t.id, 1, "B", "api", "develop").await.unwrap();
        let after = repo::get_plan(&db, t.id).await.unwrap().unwrap();
        assert_eq!(after.status, "confirmed", "targeted base edit must not downgrade status");
        let parsed: Proposal = serde_json::from_str(&after.proposal).unwrap();
        assert_eq!(parsed.directions[1].base_branch, "develop");
        assert_eq!(parsed.directions[0].base_branch, "", "other directions untouched");
        // Out-of-range index errors.
        assert!(set_direction_base(&db, t.id, 9, "Z", "api", "x").await.is_err());
        std::env::remove_var("WEFT_HOME");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&weft_home);
    }

    #[tokio::test]
    async fn approve_with_bad_explicit_base_rolls_back() {
        let _env = crate::paths::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tag = format!("weft-rollback-{}", std::process::id());
        let root = std::env::temp_dir().join(format!("{tag}-root"));
        let weft_home = std::env::temp_dir().join(format!("{tag}-home"));
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&weft_home);
        std::env::set_var("WEFT_HOME", weft_home.to_str().unwrap());
        let _repo_path = make_repo(&root, "api"); // local repo, only the default branch, no remote
        let db = Db::connect("sqlite::memory:").await.unwrap();
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        let _ra = repo::add_repo_ref(&db, ws.id, "api", root.join("api").to_str().unwrap(), "main", "").await.unwrap();
        let t = repo::create_thread(&db, ws.id, "t1", "feature", "claude").await.unwrap();
        // An explicit base that does not exist locally or on a remote.
        let proposal = Proposal { rationale: "r".into(), directions: vec![
            ProposedDirection { name:"A".into(), repo:"api".into(), reason:"r".into(), mandate:"impl-only".into(), base_branch:"no-such-xyz".into(), decision:"".into() },
        ]};
        save_proposal(&db, t.id, &proposal).await.unwrap();
        let res = approve_direction(&db, t.id, 0, "codex").await;
        assert!(res.is_err(), "approve with an unresolvable explicit base must error");
        assert!(repo::list_directions(&db, t.id).await.unwrap().is_empty(),
            "failed approve must leave NO orphan direction row");
        // Proposal still pending (decision not set).
        let plan = repo::get_plan(&db, t.id).await.unwrap().unwrap();
        let parsed: Proposal = serde_json::from_str(&plan.proposal).unwrap();
        assert_eq!(parsed.directions[0].decision, "", "decision stays unset after a failed approve");
        std::env::remove_var("WEFT_HOME");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&weft_home);
    }

    #[tokio::test]
    async fn confirm_is_idempotent_no_duplicate_directions() {
        let _env = crate::paths::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tag = format!("weft-confirm-idem-{}", std::process::id());
        let root = std::env::temp_dir().join(format!("{tag}-root"));
        let weft_home = std::env::temp_dir().join(format!("{tag}-home"));
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&weft_home);
        std::env::set_var("WEFT_HOME", weft_home.to_str().unwrap());
        let _repo_path = make_repo(&root, "api");
        let db = Db::connect("sqlite::memory:").await.unwrap();
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        let _ra = repo::add_repo_ref(&db, ws.id, "api", root.join("api").to_str().unwrap(), "main", "").await.unwrap();
        let t = repo::create_thread(&db, ws.id, "t1", "feature", "claude").await.unwrap();
        let proposal = Proposal { rationale: "r".into(), directions: vec![
            ProposedDirection { name:"A".into(), repo:"api".into(), reason:"r".into(), mandate:"".into(), base_branch:"".into(), decision:"".into() },
        ]};
        save_proposal(&db, t.id, &proposal).await.unwrap();
        let first = confirm(&db, t.id).await.unwrap();
        assert_eq!(first.len(), 1);
        assert_eq!(repo::list_directions(&db, t.id).await.unwrap().len(), 1);
        // A second confirm (e.g. after a partial-failure retry) must NOT duplicate the
        // already-created lane.
        let _second = confirm(&db, t.id).await.unwrap();
        assert_eq!(repo::list_directions(&db, t.id).await.unwrap().len(), 1, "no duplicate direction on re-confirm");
        let removed = repo::delete_thread_cascade(&db, t.id).await.unwrap();
        let _ = materialize::cleanup_worktrees(&db, &removed).await;
        std::env::remove_var("WEFT_HOME");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&weft_home);
    }

    #[tokio::test]
    async fn confirm_retry_returns_existing_id_for_dispatch() {
        let _env = crate::paths::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tag = format!("weft-confirm-redispatch-{}", std::process::id());
        let root = std::env::temp_dir().join(format!("{tag}-root"));
        let weft_home = std::env::temp_dir().join(format!("{tag}-home"));
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&weft_home);
        std::env::set_var("WEFT_HOME", weft_home.to_str().unwrap());
        let _repo_path = make_repo(&root, "api");
        let db = Db::connect("sqlite::memory:").await.unwrap();
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        let _ra = repo::add_repo_ref(&db, ws.id, "api", root.join("api").to_str().unwrap(), "main", "").await.unwrap();
        let t = repo::create_thread(&db, ws.id, "t1", "feature", "claude").await.unwrap();
        let proposal = Proposal { rationale:"r".into(), directions: vec![
            ProposedDirection { name:"A".into(), repo:"api".into(), reason:"r".into(), mandate:"".into(), base_branch:"".into(), decision:"".into() },
        ]};
        save_proposal(&db, t.id, &proposal).await.unwrap();
        let first = confirm(&db, t.id).await.unwrap();
        assert_eq!(first.len(), 1);
        let id = first[0];
        // A retry must STILL return that id (so the frontend dispatches a worker for it),
        // without creating a duplicate.
        let second = confirm(&db, t.id).await.unwrap();
        assert_eq!(second, vec![id], "retry returns the existing id for dispatch");
        assert_eq!(repo::list_directions(&db, t.id).await.unwrap().len(), 1, "no duplicate");
        let removed = repo::delete_thread_cascade(&db, t.id).await.unwrap();
        let _ = materialize::cleanup_worktrees(&db, &removed).await;
        std::env::remove_var("WEFT_HOME");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&weft_home);
    }

    #[test]
    fn proposal_parses_null_base_branch_as_empty() {
        let p: Proposal = serde_json::from_str(
            r#"{ "directions": [ { "name":"a", "repo":"api", "base_branch": null } ] }"#,
        ).unwrap();
        assert_eq!(p.directions.len(), 1, "null base_branch must NOT drop the proposal");
        assert_eq!(p.directions[0].base_branch, "");
    }

    #[tokio::test]
    async fn confirm_is_atomic_rolls_back_all_on_failure() {
        let _env = crate::paths::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tag = format!("weft-confirm-atomic-{}", std::process::id());
        let root = std::env::temp_dir().join(format!("{tag}-root"));
        let weft_home = std::env::temp_dir().join(format!("{tag}-home"));
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&weft_home);
        std::env::set_var("WEFT_HOME", weft_home.to_str().unwrap());
        let _repo_path = make_repo(&root, "api");
        let db = Db::connect("sqlite::memory:").await.unwrap();
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        let _ra = repo::add_repo_ref(&db, ws.id, "api", root.join("api").to_str().unwrap(), "main", "").await.unwrap();
        let t = repo::create_thread(&db, ws.id, "t1", "feature", "claude").await.unwrap();
        // Lane A: default base (ok). Lane B: bad explicit base → materialize fails.
        let proposal = Proposal { rationale:"r".into(), directions: vec![
            ProposedDirection { name:"A".into(), repo:"api".into(), reason:"r".into(), mandate:"".into(), base_branch:"".into(), decision:"".into() },
            ProposedDirection { name:"B".into(), repo:"api".into(), reason:"r".into(), mandate:"".into(), base_branch:"no-such-xyz".into(), decision:"".into() },
        ]};
        save_proposal(&db, t.id, &proposal).await.unwrap();
        let res = confirm(&db, t.id).await;
        assert!(res.is_err(), "confirm must fail on the bad lane");
        assert!(repo::list_directions(&db, t.id).await.unwrap().is_empty(),
            "atomic: NO directions remain after a failed confirm (lane A rolled back too)");
        // Fix B's base and retry → both lanes created cleanly, no duplicates.
        set_direction_base(&db, t.id, 1, "B", "api", "").await.unwrap();
        let ids = confirm(&db, t.id).await.unwrap();
        assert_eq!(ids.len(), 2, "retry creates both lanes");
        assert_eq!(repo::list_directions(&db, t.id).await.unwrap().len(), 2, "exactly two, no duplicates");
        let removed = repo::delete_thread_cascade(&db, t.id).await.unwrap();
        let _ = materialize::cleanup_worktrees(&db, &removed).await;
        std::env::remove_var("WEFT_HOME");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&weft_home);
    }

    #[tokio::test]
    async fn set_direction_base_rejects_stale_lane_identity() {
        let _env = crate::paths::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tag = format!("weft-laneid-{}", std::process::id());
        let root = std::env::temp_dir().join(format!("{tag}-root"));
        let weft_home = std::env::temp_dir().join(format!("{tag}-home"));
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&weft_home);
        std::env::set_var("WEFT_HOME", weft_home.to_str().unwrap());
        let _repo_path = make_repo(&root, "api");
        let db = Db::connect("sqlite::memory:").await.unwrap();
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        let _ra = repo::add_repo_ref(&db, ws.id, "api", root.join("api").to_str().unwrap(), "main", "").await.unwrap();
        let t = repo::create_thread(&db, ws.id, "t1", "feature", "claude").await.unwrap();
        let proposal = Proposal { rationale:"r".into(), directions: vec![
            ProposedDirection { name:"A".into(), repo:"api".into(), reason:"r".into(), mandate:"".into(), base_branch:"".into(), decision:"".into() },
        ]};
        save_proposal(&db, t.id, &proposal).await.unwrap();
        // Correct identity → applies.
        set_direction_base(&db, t.id, 0, "A", "api", "develop").await.unwrap();
        let p1: Proposal = serde_json::from_str(&repo::get_plan(&db, t.id).await.unwrap().unwrap().proposal).unwrap();
        assert_eq!(p1.directions[0].base_branch, "develop");
        // Wrong identity (lane changed under the index) → error, no write.
        assert!(set_direction_base(&db, t.id, 0, "B", "api", "main").await.is_err());
        let p2: Proposal = serde_json::from_str(&repo::get_plan(&db, t.id).await.unwrap().unwrap().proposal).unwrap();
        assert_eq!(p2.directions[0].base_branch, "develop", "stale-identity save must not overwrite");
        std::env::remove_var("WEFT_HOME");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&weft_home);
    }

    #[tokio::test]
    async fn approve_rejects_base_change_on_existing_direction() {
        let _env = crate::paths::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tag = format!("weft-approve-basechg-{}", std::process::id());
        let root = std::env::temp_dir().join(format!("{tag}-root"));
        let weft_home = std::env::temp_dir().join(format!("{tag}-home"));
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&weft_home);
        std::env::set_var("WEFT_HOME", weft_home.to_str().unwrap());
        let _repo_path = make_repo(&root, "api");
        let db = Db::connect("sqlite::memory:").await.unwrap();
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        let _ra = repo::add_repo_ref(&db, ws.id, "api", root.join("api").to_str().unwrap(), "main", "").await.unwrap();
        let t = repo::create_thread(&db, ws.id, "t1", "feature", "claude").await.unwrap();
        let mk = |base: &str| Proposal { rationale:"r".into(), directions: vec![
            ProposedDirection { name:"A".into(), repo:"api".into(), reason:"r".into(), mandate:"".into(), base_branch:base.into(), decision:"".into() },
        ]};
        // First approve with empty base → creates + materializes (base ""→default).
        save_proposal(&db, t.id, &mk("")).await.unwrap();
        let id = approve_direction(&db, t.id, 0, "codex").await.unwrap();
        // Re-propose the same name/repo with a DIFFERENT base, re-approve → must ERROR
        // (can't re-base the already-materialized worktree).
        save_proposal(&db, t.id, &mk("develop")).await.unwrap();
        let res = approve_direction(&db, t.id, 0, "codex").await;
        assert!(res.is_err(), "approving a re-proposed direction with a changed base must error");
        assert_eq!(repo::list_directions(&db, t.id).await.unwrap().len(), 1, "no second direction created");
        // Re-approve with the SAME (empty) base → idempotent reuse, no error.
        save_proposal(&db, t.id, &mk("")).await.unwrap();
        let id3 = approve_direction(&db, t.id, 0, "codex").await.unwrap();
        assert_eq!(id3, id, "same base → idempotent reuse returns the existing id");
        let removed = repo::delete_thread_cascade(&db, t.id).await.unwrap();
        let _ = materialize::cleanup_worktrees(&db, &removed).await;
        std::env::remove_var("WEFT_HOME");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&weft_home);
    }

    #[tokio::test]
    async fn approve_allows_reproposed_default_spelled_out() {
        let _env = crate::paths::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tag = format!("weft-approve-eff-{}", std::process::id());
        let root = std::env::temp_dir().join(format!("{tag}-root"));
        let weft_home = std::env::temp_dir().join(format!("{tag}-home"));
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&weft_home);
        std::env::set_var("WEFT_HOME", weft_home.to_str().unwrap());
        let repo_path = make_repo(&root, "api");
        let def = crate::git::current_branch(&repo_path).unwrap(); // repo default (main/master)
        let db = Db::connect("sqlite::memory:").await.unwrap();
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        let _ra = repo::add_repo_ref(&db, ws.id, "api", repo_path.to_str().unwrap(), &def, "").await.unwrap();
        let t = repo::create_thread(&db, ws.id, "t1", "feature", "claude").await.unwrap();
        let mk = |base: &str| Proposal { rationale:"r".into(), directions: vec![
            ProposedDirection { name:"A".into(), repo:"api".into(), reason:"r".into(), mandate:"".into(), base_branch:base.into(), decision:"".into() },
        ]};
        // First approve with empty base → materializes off the default.
        save_proposal(&db, t.id, &mk("")).await.unwrap();
        let id = approve_direction(&db, t.id, 0, "codex").await.unwrap();
        // Re-propose spelling out the SAME default branch → effective bases match →
        // idempotent reuse, NOT a conflict error.
        save_proposal(&db, t.id, &mk(&def)).await.unwrap();
        let id2 = approve_direction(&db, t.id, 0, "codex").await.unwrap();
        assert_eq!(id2, id, "same effective base (default spelled out) must reuse, not error");
        let removed = repo::delete_thread_cascade(&db, t.id).await.unwrap();
        let _ = materialize::cleanup_worktrees(&db, &removed).await;
        std::env::remove_var("WEFT_HOME");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&weft_home);
    }

    #[tokio::test]
    async fn confirm_reuses_existing_approved_lane_no_duplicate() {
        let _env = crate::paths::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tag = format!("weft-confirm-reuse-{}", std::process::id());
        let root = std::env::temp_dir().join(format!("{tag}-root"));
        let weft_home = std::env::temp_dir().join(format!("{tag}-home"));
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&weft_home);
        std::env::set_var("WEFT_HOME", weft_home.to_str().unwrap());
        let _repo_path = make_repo(&root, "api");
        let db = Db::connect("sqlite::memory:").await.unwrap();
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        let _ra = repo::add_repo_ref(&db, ws.id, "api", root.join("api").to_str().unwrap(), "main", "").await.unwrap();
        let t = repo::create_thread(&db, ws.id, "t1", "feature", "claude").await.unwrap();
        let prop = Proposal { rationale:"r".into(), directions: vec![
            ProposedDirection { name:"A".into(), repo:"api".into(), reason:"r".into(), mandate:"".into(), base_branch:"".into(), decision:"".into() },
        ]};
        // Approve A via the Needs-you path (creates + materializes).
        save_proposal(&db, t.id, &prop).await.unwrap();
        let approved_id = approve_direction(&db, t.id, 0, "codex").await.unwrap();
        // Lead re-proposes the SAME lane (decision resets to ""), then user clicks Create.
        save_proposal(&db, t.id, &prop).await.unwrap();
        let ids = confirm(&db, t.id).await.unwrap();
        assert_eq!(repo::list_directions(&db, t.id).await.unwrap().len(), 1, "confirm must NOT duplicate the approved lane");
        assert!(ids.contains(&approved_id), "confirm returns the existing lane id for dispatch");
        let removed = repo::delete_thread_cascade(&db, t.id).await.unwrap();
        let _ = materialize::cleanup_worktrees(&db, &removed).await;
        std::env::remove_var("WEFT_HOME");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&weft_home);
    }

    #[tokio::test]
    async fn approve_rejects_base_specific_reproposal_of_legacy_empty_base() {
        let _env = crate::paths::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tag = format!("weft-legacy-base-{}", std::process::id());
        let root = std::env::temp_dir().join(format!("{tag}-root"));
        let weft_home = std::env::temp_dir().join(format!("{tag}-home"));
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&weft_home);
        std::env::set_var("WEFT_HOME", weft_home.to_str().unwrap());
        let _repo_path = make_repo(&root, "api");
        let db = Db::connect("sqlite::memory:").await.unwrap();
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        let _ra = repo::add_repo_ref(&db, ws.id, "api", root.join("api").to_str().unwrap(), "main", "").await.unwrap();
        let t = repo::create_thread(&db, ws.id, "t1", "feature", "claude").await.unwrap();
        let mk = |base: &str| Proposal { rationale:"r".into(), directions: vec![
            ProposedDirection { name:"A".into(), repo:"api".into(), reason:"r".into(), mandate:"".into(), base_branch:base.into(), decision:"".into() },
        ]};
        save_proposal(&db, t.id, &mk("")).await.unwrap();
        let id = approve_direction(&db, t.id, 0, "codex").await.unwrap();
        // Simulate a LEGACY row: blank out the recorded base (as the migration would for
        // a pre-PR materialized direction).
        repo::set_direction_base_branch(&db, id, "").await.unwrap();
        // A base-specific re-proposal can't be verified against an unknown legacy base → reject.
        save_proposal(&db, t.id, &mk("develop")).await.unwrap();
        assert!(approve_direction(&db, t.id, 0, "codex").await.is_err(),
            "base-specific re-proposal of a legacy empty-base direction must be rejected");
        // A default (empty) re-proposal is allowed to reuse.
        save_proposal(&db, t.id, &mk("")).await.unwrap();
        assert_eq!(approve_direction(&db, t.id, 0, "codex").await.unwrap(), id, "empty re-proposal reuses the legacy lane");
        let removed = repo::delete_thread_cascade(&db, t.id).await.unwrap();
        let _ = materialize::cleanup_worktrees(&db, &removed).await;
        std::env::remove_var("WEFT_HOME");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&weft_home);
    }

    #[tokio::test]
    async fn pending_writes_carry_base_branch() {
        let _env = crate::paths::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tag = format!("weft-pwbase-{}", std::process::id());
        let root = std::env::temp_dir().join(format!("{tag}-root"));
        let weft_home = std::env::temp_dir().join(format!("{tag}-home"));
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&weft_home);
        std::env::set_var("WEFT_HOME", weft_home.to_str().unwrap());
        let repo_path = make_repo(&root, "api");

        let db = Db::connect("sqlite::memory:").await.unwrap();
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        let _ra = repo::add_repo_ref(&db, ws.id, "api", repo_path.to_str().unwrap(), "main", "")
            .await
            .unwrap();
        let t = repo::create_thread(&db, ws.id, "t1", "feature", "claude").await.unwrap();
        let proposal = Proposal {
            rationale: "r".into(),
            directions: vec![ProposedDirection {
                name: "Payments".into(),
                repo: "api".into(),
                reason: "add endpoint".into(),
                mandate: "".into(),
                base_branch: "develop".into(),
                decision: "".into(),
            }],
        };
        save_proposal(&db, t.id, &proposal).await.unwrap();
        let pending = pending_writes(&db, t.id).await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].base_branch, "develop", "base_branch flows into PendingWrite");

        std::env::remove_var("WEFT_HOME");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&weft_home);
    }

    #[tokio::test]
    async fn confirm_does_not_roll_back_a_reused_lane_when_a_later_lane_fails() {
        // Lane A: approved via Needs-you (pre-existing, materialized). Lane B: a NEW
        // lane with a bad explicit base that fails to materialize. The failed confirm
        // must NOT tear down A (it existed before this attempt).
        let _env = crate::paths::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tag = format!("weft-confirm-reuse-norollback-{}", std::process::id());
        let root = std::env::temp_dir().join(format!("{tag}-root"));
        let weft_home = std::env::temp_dir().join(format!("{tag}-home"));
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&weft_home);
        std::env::set_var("WEFT_HOME", weft_home.to_str().unwrap());
        let _repo_path = make_repo(&root, "api");
        let db = Db::connect("sqlite::memory:").await.unwrap();
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        let _ra = repo::add_repo_ref(&db, ws.id, "api", root.join("api").to_str().unwrap(), "main", "").await.unwrap();
        let t = repo::create_thread(&db, ws.id, "t1", "feature", "claude").await.unwrap();
        // Approve A (empty base) via Needs-you → A materialized.
        let prop_a = Proposal { rationale: "r".into(), directions: vec![
            ProposedDirection { name: "A".into(), repo: "api".into(), reason: "r".into(), mandate: "".into(), base_branch: "".into(), decision: "".into() },
        ]};
        save_proposal(&db, t.id, &prop_a).await.unwrap();
        let a_id = approve_direction(&db, t.id, 0, "codex").await.unwrap();
        // Re-propose A (reset) + add B with a bad explicit base.
        let prop2 = Proposal { rationale: "r".into(), directions: vec![
            ProposedDirection { name: "A".into(), repo: "api".into(), reason: "r".into(), mandate: "".into(), base_branch: "".into(), decision: "".into() },
            ProposedDirection { name: "B".into(), repo: "api".into(), reason: "r".into(), mandate: "".into(), base_branch: "no-such-xyz".into(), decision: "".into() },
        ]};
        save_proposal(&db, t.id, &prop2).await.unwrap();
        let res = confirm(&db, t.id).await;
        assert!(res.is_err(), "confirm fails on B's bad base");
        // A must still exist (reused, not in the rollback set); B must not.
        let dirs = repo::list_directions(&db, t.id).await.unwrap();
        assert!(dirs.iter().any(|d| d.id == a_id), "reused lane A must NOT be rolled back");
        assert!(!dirs.iter().any(|d| d.name == "B"), "failed new lane B is rolled back");
        let removed = repo::delete_thread_cascade(&db, t.id).await.unwrap();
        let _ = materialize::cleanup_worktrees(&db, &removed).await;
        std::env::remove_var("WEFT_HOME");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&weft_home);
    }

    /// R17-1: confirm REUSES a direction whose worktree dir was reclaimed (exists=false)
    /// → after confirm, the worktree dir must exist again.
    #[tokio::test]
    async fn confirm_reuse_rematerializes_reclaimed_worktree() {
        let _env = crate::paths::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tag = format!("weft-confirm-remat-{}", std::process::id());
        let root = std::env::temp_dir().join(format!("{tag}-root"));
        let weft_home = std::env::temp_dir().join(format!("{tag}-home"));
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&weft_home);
        std::env::set_var("WEFT_HOME", weft_home.to_str().unwrap());
        let _repo_path = make_repo(&root, "api");
        let db = Db::connect("sqlite::memory:").await.unwrap();
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        let _ra = repo::add_repo_ref(&db, ws.id, "api", root.join("api").to_str().unwrap(), "main", "").await.unwrap();
        let t = repo::create_thread(&db, ws.id, "t1", "feature", "claude").await.unwrap();
        let prop = Proposal { rationale: "r".into(), directions: vec![
            ProposedDirection { name: "A".into(), repo: "api".into(), reason: "r".into(), mandate: "".into(), base_branch: "".into(), decision: "".into() },
        ]};

        // First confirm: creates and materializes the direction.
        save_proposal(&db, t.id, &prop).await.unwrap();
        let first_ids = confirm(&db, t.id).await.unwrap();
        assert_eq!(first_ids.len(), 1);
        let dir_id = first_ids[0];

        // Find the worktree path and simulate a reclaim: remove the on-disk directory.
        let wts = crate::store::repo::list_worktrees(&db, Some(dir_id)).await.unwrap();
        assert_eq!(wts.len(), 1, "precondition: one worktree row after first confirm");
        let wt_path = std::path::PathBuf::from(&wts[0].path);
        // Remove the dir (simulate reclaim via git worktree remove + force remove).
        let repo_p = root.join("api");
        let _ = crate::git::remove_worktree(&repo_p, &wt_path);
        let _ = std::fs::remove_dir_all(&wt_path);
        assert!(!wt_path.exists(), "precondition: dir must be gone before re-confirm");

        // Re-propose (resets status to "proposed") and confirm again → reuse path.
        save_proposal(&db, t.id, &prop).await.unwrap();
        let second_ids = confirm(&db, t.id).await.unwrap();
        assert_eq!(second_ids, vec![dir_id], "re-confirm returns the existing id");
        assert!(wt_path.exists(), "R17-1: worktree dir must be recreated after confirm reuse");

        let removed = repo::delete_thread_cascade(&db, t.id).await.unwrap();
        let _ = materialize::cleanup_worktrees(&db, &removed).await;
        std::env::remove_var("WEFT_HOME");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&weft_home);
    }

    /// R17-2: approve_direction REUSES a direction whose worktree dir was reclaimed
    /// → after approve, the worktree dir must exist again.
    #[tokio::test]
    async fn approve_reuse_rematerializes_reclaimed_worktree() {
        let _env = crate::paths::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tag = format!("weft-approve-remat-{}", std::process::id());
        let root = std::env::temp_dir().join(format!("{tag}-root"));
        let weft_home = std::env::temp_dir().join(format!("{tag}-home"));
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&weft_home);
        std::env::set_var("WEFT_HOME", weft_home.to_str().unwrap());
        let _repo_path = make_repo(&root, "api");
        let db = Db::connect("sqlite::memory:").await.unwrap();
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        let _ra = repo::add_repo_ref(&db, ws.id, "api", root.join("api").to_str().unwrap(), "main", "").await.unwrap();
        let t = repo::create_thread(&db, ws.id, "t1", "feature", "claude").await.unwrap();
        let prop = Proposal { rationale: "r".into(), directions: vec![
            ProposedDirection { name: "A".into(), repo: "api".into(), reason: "r".into(), mandate: "".into(), base_branch: "".into(), decision: "".into() },
        ]};

        // First approve: creates and materializes the direction.
        save_proposal(&db, t.id, &prop).await.unwrap();
        let dir_id = approve_direction(&db, t.id, 0, "codex").await.unwrap();

        // Find worktree and simulate reclaim.
        let wts = crate::store::repo::list_worktrees(&db, Some(dir_id)).await.unwrap();
        assert_eq!(wts.len(), 1, "precondition: one worktree row after first approve");
        let wt_path = std::path::PathBuf::from(&wts[0].path);
        let repo_p = root.join("api");
        let _ = crate::git::remove_worktree(&repo_p, &wt_path);
        let _ = std::fs::remove_dir_all(&wt_path);
        assert!(!wt_path.exists(), "precondition: dir must be gone before re-approve");

        // Re-propose (resets proposal; decision clears) then re-approve → reuse path.
        save_proposal(&db, t.id, &prop).await.unwrap();
        let dir_id2 = approve_direction(&db, t.id, 0, "codex").await.unwrap();
        assert_eq!(dir_id2, dir_id, "re-approve returns the existing id");
        assert!(wt_path.exists(), "R17-2: worktree dir must be recreated after approve reuse");

        let removed = repo::delete_thread_cascade(&db, t.id).await.unwrap();
        let _ = materialize::cleanup_worktrees(&db, &removed).await;
        std::env::remove_var("WEFT_HOME");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&weft_home);
    }

    /// R17-3: a proposal with one lane approved individually (via approve_direction)
    /// and one lane created by confirm → a retry of confirm (after it's "confirmed")
    /// must return ONLY the confirm-created lane's id, NOT the individually-approved one.
    #[tokio::test]
    async fn confirmed_fast_path_excludes_individually_approved_lanes() {
        let _env = crate::paths::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tag = format!("weft-fastpath-approved-{}", std::process::id());
        let root = std::env::temp_dir().join(format!("{tag}-root"));
        let weft_home = std::env::temp_dir().join(format!("{tag}-home"));
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&weft_home);
        std::env::set_var("WEFT_HOME", weft_home.to_str().unwrap());
        let _repo_path = make_repo(&root, "api");
        let db = Db::connect("sqlite::memory:").await.unwrap();
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        let _ra = repo::add_repo_ref(&db, ws.id, "api", root.join("api").to_str().unwrap(), "main", "").await.unwrap();
        let t = repo::create_thread(&db, ws.id, "t1", "feature", "claude").await.unwrap();

        // Lane A is approved via approve_direction (decision="approved").
        // Lane B is pending (decision="") → confirm will create+dispatch it.
        let prop_a_only = Proposal { rationale: "r".into(), directions: vec![
            ProposedDirection { name: "A".into(), repo: "api".into(), reason: "r".into(), mandate: "".into(), base_branch: "".into(), decision: "".into() },
        ]};
        save_proposal(&db, t.id, &prop_a_only).await.unwrap();
        let a_id = approve_direction(&db, t.id, 0, "codex").await.unwrap();

        // Now re-propose with BOTH A (already approved) and B (pending).
        // After approve_direction set A's decision to "approved", we need a new proposal
        // that has A with decision="" (re-propose resets) + B with decision="".
        let prop_ab = Proposal { rationale: "r".into(), directions: vec![
            ProposedDirection { name: "A".into(), repo: "api".into(), reason: "r".into(), mandate: "".into(), base_branch: "".into(), decision: "approved".into() },
            ProposedDirection { name: "B".into(), repo: "api".into(), reason: "r".into(), mandate: "".into(), base_branch: "".into(), decision: "".into() },
        ]};
        // Save this combined proposal (A=approved, B=pending) and confirm → creates B only.
        save_proposal(&db, t.id, &prop_ab).await.unwrap();
        let confirm_ids = confirm(&db, t.id).await.unwrap();
        // confirm should dispatch only B (A is "approved" → skipped by main path).
        assert_eq!(confirm_ids.len(), 1, "confirm creates only lane B (A is already approved)");
        let b_id = confirm_ids[0];
        assert_ne!(b_id, a_id, "B must be a different direction from A");

        // Now the plan is "confirmed". A retry must return ONLY B's id.
        let retry_ids = confirm(&db, t.id).await.unwrap();
        assert_eq!(retry_ids, vec![b_id],
            "R17-3: confirmed fast-path must return only confirm-created lane B, NOT approved lane A");
        assert!(!retry_ids.contains(&a_id),
            "R17-3: individually-approved lane A must NOT appear in fast-path retry");

        let removed = repo::delete_thread_cascade(&db, t.id).await.unwrap();
        let _ = materialize::cleanup_worktrees(&db, &removed).await;
        std::env::remove_var("WEFT_HOME");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&weft_home);
    }

    /// R18-3: confirmed fast-path re-materializes a reclaimed worktree dir. After
    /// confirm, a lane's dir is reclaimed; a retry of confirm (still "confirmed")
    /// must recreate the dir so the dispatched worker has a live checkout.
    #[tokio::test]
    async fn confirmed_fast_path_rematerializes_reclaimed_lane() {
        let _env = crate::paths::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tag = format!("weft-fastpath-remat-{}", std::process::id());
        let root = std::env::temp_dir().join(format!("{tag}-root"));
        let weft_home = std::env::temp_dir().join(format!("{tag}-home"));
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&weft_home);
        std::env::set_var("WEFT_HOME", weft_home.to_str().unwrap());
        let _repo_path = make_repo(&root, "api");
        let db = Db::connect("sqlite::memory:").await.unwrap();
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        let _ra = repo::add_repo_ref(&db, ws.id, "api", root.join("api").to_str().unwrap(), "main", "").await.unwrap();
        let t = repo::create_thread(&db, ws.id, "t1", "feature", "claude").await.unwrap();
        let prop = Proposal { rationale: "r".into(), directions: vec![
            ProposedDirection { name: "A".into(), repo: "api".into(), reason: "r".into(), mandate: "".into(), base_branch: "".into(), decision: "".into() },
        ]};

        // First confirm: creates and materializes lane A.
        save_proposal(&db, t.id, &prop).await.unwrap();
        let first_ids = confirm(&db, t.id).await.unwrap();
        assert_eq!(first_ids.len(), 1);
        let dir_id = first_ids[0];

        // Find the worktree and reclaim it (remove the on-disk dir).
        let wts = repo::list_worktrees(&db, Some(dir_id)).await.unwrap();
        assert_eq!(wts.len(), 1, "precondition: one worktree row after first confirm");
        let wt_path = std::path::PathBuf::from(&wts[0].path);
        let repo_p = root.join("api");
        let _ = crate::git::remove_worktree(&repo_p, &wt_path);
        let _ = std::fs::remove_dir_all(&wt_path);
        assert!(!wt_path.exists(), "precondition: dir must be gone before retry");

        // The plan is now "confirmed". A retry (fast-path) must recreate the dir.
        let second_ids = confirm(&db, t.id).await.unwrap();
        assert_eq!(second_ids, vec![dir_id], "fast-path retry returns the existing id");
        assert!(wt_path.exists(), "R18-3: fast-path retry must re-materialize the reclaimed dir");

        let removed = repo::delete_thread_cascade(&db, t.id).await.unwrap();
        let _ = materialize::cleanup_worktrees(&db, &removed).await;
        std::env::remove_var("WEFT_HOME");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&weft_home);
    }

    /// R18-4: reconcile_reuse treats a recorded base of "HEAD" as equivalent to an
    /// empty/default proposed base — a blank re-proposal must reuse a HEAD-based
    /// direction without a conflict error.
    #[test]
    fn reconcile_reuse_treats_head_as_default() {
        use crate::store::entities::direction;
        // Build a minimal direction model with base_branch="HEAD".
        let existing = direction::Model {
            id: 1,
            thread_id: 1,
            name: "A".to_string(),
            slug: "a".to_string(),
            tool: "codex".to_string(),
            repo_id: 1,
            reason: "r".to_string(),
            status: "queued".to_string(),
            mandate: "plan+impl".to_string(),
            base_branch: "HEAD".to_string(),
            target_branch: "".to_string(),
            branch: "feat/a".to_string(),
            created_at: "0".to_string(),
        };

        // A blank re-proposal with the repo on a non-existent path so
        // live_default_branch returns None. The reconcile_reuse special-case for
        // "HEAD" fires before effective() is called.
        let repo_path = std::path::Path::new("/tmp/weft-nonexistent-reconcile-head");

        // Blank re-proposal: must NOT error.
        let result = reconcile_reuse(&existing, "", repo_path, "main");
        assert!(result.is_ok(), "R18-4: blank re-proposal must reuse a HEAD-based direction, got: {:?}", result.err());

        // An explicit non-empty base against a HEAD-based direction: must still conflict
        // (we only relax the blank-proposal case).
        let conflict = reconcile_reuse(&existing, "develop", repo_path, "main");
        assert!(conflict.is_err(), "R18-4: explicit base against HEAD-based direction must conflict");
    }
}
