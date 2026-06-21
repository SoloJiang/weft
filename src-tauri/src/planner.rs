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

/// Pure predicate: do `existing_base` (a direction's RECORDED branch-off base) and
/// `proposed_base` (a re-proposal's requested base) refer to the SAME base — i.e. is the
/// existing direction a base-compatible reuse target for the proposal?
///
/// This is the SAME normalize_target/effective comparison `reconcile_reuse` performs, factored
/// out so the lane-reuse `.find` predicates can PREFER a base-compatible direction when duplicate
/// same-name/repo lanes have different bases (R50-4) — instead of matching the first by name+repo
/// and then erroring in `reconcile_reuse` on a base mismatch. Returns true when:
///   - both are the default (empty existing/proposed, or existing "HEAD" + empty proposed), OR
///   - their effective (normalized) bases are equal.
/// An empty existing base paired with a NON-empty proposed base is NOT compatible (a legacy row's
/// true base is unknown — reconcile_reuse rejects it). The detached-HEAD moved-target case is left
/// to reconcile_reuse: it is about a moved TARGET, not base identity, so it must not affect which
/// direction is SELECTED.
fn base_compatible(
    existing_base: &str,
    proposed_base: &str,
    repo_path: &std::path::Path,
    base_ref: &str,
    base_ref_is_default: bool,
) -> bool {
    let effective = |b: &str| -> String {
        let b = b.trim();
        if b.is_empty() {
            crate::git::live_default_branch(repo_path).unwrap_or_else(|| {
                crate::git::recorded_base_or_default(repo_path, base_ref, base_ref_is_default)
            })
        } else {
            crate::git::normalize_target(b)
        }
    };
    let existing_base = existing_base.trim();
    let proposed_base = proposed_base.trim();
    if existing_base.is_empty() {
        // Legacy/blank-recorded base: reusable only by a blank (default) re-proposal.
        return proposed_base.is_empty();
    }
    if existing_base == "HEAD" && proposed_base.is_empty() {
        return true;
    }
    effective(existing_base) == effective(proposed_base)
}

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
    base_ref_is_default: bool,
) -> Result<()> {
    // Empty recorded base spans two cases that share the legacy blank-reuse shortcut but a
    // DETACHED-HEAD lane must not: it records base "" + target = the branch-off COMMIT (a
    // 40-hex sha). A blank re-proposal means "fork from current HEAD" — reusing the lane is
    // only correct while HEAD still points at that stored commit. If HEAD advanced, the
    // re-proposal wants the NEW HEAD, so reuse of the stale lane must be rejected.
    if existing.base_branch.trim().is_empty() {
        if !proposed_base.trim().is_empty() {
            anyhow::bail!(
                "direction {:?} predates base tracking (unknown branch-off base); delete the sub-task to recreate it from {:?}",
                existing.name, proposed_base
            );
        }
        // Detached-HEAD lane = empty base paired with a full commit-oid target. Hash-agnostic
        // (SHA-1 40-hex OR SHA-256 64-hex) via git object identity, not a hard-coded length.
        let target = existing.target_branch.trim();
        let is_detached_head = crate::git::is_full_commit_oid(repo_path, target);
        if is_detached_head && crate::git::head_commit_full(repo_path).as_deref() != Some(target) {
            anyhow::bail!(
                "direction {:?} is based on a detached HEAD that has since moved; \
                 delete the sub-task to recreate it from the new HEAD",
                existing.name
            );
        }
        return Ok(());
    }
    // Non-empty recorded base: the final guard is the SAME comparison `base_compatible` uses
    // (incl. the "HEAD" recorded base == blank re-proposal shortcut), kept here as the single
    // source of truth so the reuse `.find` predicates and this guard never diverge.
    if !base_compatible(
        &existing.base_branch,
        proposed_base,
        repo_path,
        base_ref,
        base_ref_is_default,
    ) {
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

/// A STRICTLY-MONOTONIC proposal version string (R50-2). `save_proposal` writes this into the
/// plan's `created_at` ("last proposed at") so the frontend can reset a dirty base edit on ANY
/// re-proposal — including two that land within the SAME second (where a coarse `now()` would
/// repeat and the reset would NOT fire). A wall-clock nanosecond stamp is combined with a
/// process-wide atomic counter so the value is unique and increasing even under back-to-back
/// saves on a low-resolution clock.
fn proposal_version() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    format!("{nanos}-{seq}")
}

/// Per-thread serialization gate for plan-mutating ops. Two confirm/approve/deny/save_proposal
/// calls for the SAME thread never overlap, so the read→create/reuse→materialize→commit dance is
/// atomic per thread — eliminating the TOCTOU races (duplicate directions, a loser's rollback
/// deleting a winner's direction, a re-propose landing mid-confirm). Different threads don't block
/// each other. The REGISTRY is a lock-free DashMap; the per-thread value is a tokio Mutex held
/// across the whole op (incl. slow git materialize — tokio::sync::Mutex is async/await-safe).
fn thread_gate(thread_id: i32) -> std::sync::Arc<tokio::sync::Mutex<()>> {
    static GATES: std::sync::OnceLock<dashmap::DashMap<i32, std::sync::Arc<tokio::sync::Mutex<()>>>> =
        std::sync::OnceLock::new();
    let gates = GATES.get_or_init(dashmap::DashMap::new);
    // Clone the Arc OUT of the entry so the DashMap shard guard drops before any .await
    // (never hold a DashMap guard across await — deadlock risk).
    let arc = gates
        .entry(thread_id)
        .or_insert_with(|| std::sync::Arc::new(tokio::sync::Mutex::new(())))
        .clone();
    arc
}

/// Store (replace) the proposal for a thread, status = "proposed".
///
/// TRUST BOUNDARY (R47-3): both callers (commands.rs and bus/server.rs) feed a
/// LEAD-supplied payload. `ProposedDirection.decision` is `#[serde(default)]`, so a
/// malformed/hostile `propose_directions` call can inject `decision="approved"`/`"denied"`
/// — which would drop the lane from `pending_writes` (bypassing a required human approval)
/// and make `confirm` skip it (dropping a write with NO human action). The lead must NEVER
/// set decisions; only `approve_direction`/`deny_direction` do (via `persist_decision` /
/// `update_plan_proposal_cas`, NOT this function). So scrub every decision to "" (pending)
/// before storing. This is safe: approve/deny never route through save_proposal, so no
/// server-set decision is lost.
pub async fn save_proposal(db: &Db, thread_id: i32, proposal: &Proposal) -> Result<()> {
    let gate = thread_gate(thread_id);
    let _gate = gate.lock().await;
    let mut p = proposal.clone();
    for d in &mut p.directions {
        d.decision = String::new();
    }
    let json = serde_json::to_string(&p)?;
    // Bump the proposal VERSION on EVERY re-propose (R50-2). `upsert_plan` uses `version` as the
    // INSERT created_at but PRESERVES created_at on UPDATE; for a re-propose (existing row) the
    // explicit set_plan_created_at below applies the fresh version so the frontend reliably resets
    // a dirty base edit even when name/repo/base are unchanged.
    let version = proposal_version();
    repo::upsert_plan(db, thread_id, &json, "proposed", &version).await?;
    repo::set_plan_created_at(db, thread_id, &version).await?;
    Ok(())
}

/// Resolve a plan ROW (already read) against its workspace repos. Derives the
/// resolved proposal from exactly the row passed in — so a caller that needs the
/// resolved form AND a compare-and-swap baseline from the SAME snapshot (confirm)
/// can read the plan once and feed it here, instead of reading the row twice with
/// a re-propose racing in between.
async fn resolved_from_plan(
    db: &Db,
    thread_id: i32,
    p: &crate::store::entities::plan::Model,
) -> Result<ResolvedProposal> {
    let proposal: Proposal = serde_json::from_str(&p.proposal).unwrap_or_default();
    let repos = workspace_repos(db, thread_id).await?;
    let directions = proposal
        .directions
        .iter()
        .map(|d| resolve(d, &repos))
        .collect();
    Ok(ResolvedProposal {
        thread_id,
        rationale: proposal.rationale,
        status: p.status.clone(),
        // The plan's `created_at` doubles as the proposal VERSION ("last proposed at"): bumped on
        // every save_proposal (R50-2) so the frontend can reset a dirty base edit on ANY re-propose.
        created_at: p.created_at.clone(),
        directions,
    })
}

/// The stored proposal for a thread, resolved against its workspace repos.
pub async fn get_resolved(db: &Db, thread_id: i32) -> Result<Option<ResolvedProposal>> {
    let Some(p) = repo::get_plan(db, thread_id).await? else {
        return Ok(None);
    };
    Ok(Some(resolved_from_plan(db, thread_id, &p).await?))
}

#[derive(Clone, Debug, Serialize)]
pub struct ResolvedProposal {
    pub thread_id: i32,
    pub rationale: String,
    pub status: String,
    /// Proposal VERSION ("last proposed at"): the plan's `created_at`, bumped on every
    /// save_proposal (R50-2). The frontend includes it in the base-field reset condition so a
    /// re-propose with the SAME name/repo/base still discards an unblurred (dirty) edit.
    pub created_at: String,
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
    // Serialize all plan mutations for this thread: held across the whole confirm (read → reuse/
    // create → materialize → CAS commit) so no concurrent confirm/approve/deny/save_proposal can
    // interleave. This is what makes the read→commit dance atomic and the TOCTOU races impossible.
    let gate = thread_gate(thread_id);
    let _gate = gate.lock().await;
    // R44-2 / R42-4: read the plan ROW exactly ONCE and derive BOTH the resolved proposal we act
    // on AND the compare-and-swap baseline (start_plan.proposal / .status) from that single
    // snapshot. Reading get_resolved then a SEPARATE get_plan let a re-propose land between them —
    // leaving `resolved` on the OLD proposal while the CAS guarded the NEW one, so confirm would
    // materialize superseded lanes yet mark the fresh proposal confirmed. One row = one truth.
    let start_plan = repo::get_plan(db, thread_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("no proposal to confirm for thread {thread_id}"))?;
    let resolved = resolved_from_plan(db, thread_id, &start_plan).await?;
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
        let mut consumed: std::collections::HashSet<i32> = std::collections::HashSet::new();
        // Pre-claim directions owned by approved/denied lanes so a same-name pending sibling
        // doesn't re-dispatch them.
        for p in &resolved.directions {
            if p.repo.known && (p.decision == "approved" || p.decision == "denied") {
                // Exclude terminal (done) directions from reuse: a completed lane is history,
                // not a resumable target — a re-proposal must create fresh work.
                if let Some(d) = all.iter().find(|d| p.name == d.name && p.repo.repo_id == d.repo_id && !consumed.contains(&d.id) && d.status != "done") {
                    consumed.insert(d.id);
                }
            }
        }
        let mut matching: Vec<i32> = Vec::new();
        for p in &resolved.directions {
            if !p.repo.known || p.decision == "approved" || p.decision == "denied" { continue; }
            // Exclude terminal (done) directions from reuse: a completed lane is history,
            // not a resumable target — a re-proposal must create fresh work.
            if let Some(d) = all.iter().find(|d| p.name == d.name && p.repo.repo_id == d.repo_id && !consumed.contains(&d.id) && d.status != "done") {
                consumed.insert(d.id);
                matching.push(d.id);
            }
        }
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
    // Claim each existing direction at most once. An already-approved/denied lane (handled
    // via per-card approve/deny, skipped below) still OWNS its direction in existing_dirs;
    // without claiming it, a same-name+repo PENDING sibling's reuse `.find` would match and
    // reuse that approved direction instead of creating its own — silently dropping the
    // pending lane. Pre-claim the approved/denied lanes' directions so duplicates fall
    // through to create their own.
    let mut consumed: std::collections::HashSet<i32> = std::collections::HashSet::new();
    for d in &resolved.directions {
        if d.decision == "approved" || d.decision == "denied" {
            // Prefer claiming the BASE-COMPATIBLE direction among same-name/repo duplicates so the
            // PENDING sibling's reuse find (which is base-aware) is left the matching one — mirrors
            // the main find's predicate (R50-4). Best-effort repo lookup: if it fails, fall back to
            // a name+repo-only claim (the main loop still reconciles the real base).
            let repo_info = repo::get_repo(db, d.repo.repo_id).await.ok().flatten();
            let claimed = existing_dirs
                .iter()
                // Exclude terminal (done) directions from reuse: a completed lane is history,
                // not a resumable target — a re-proposal must create fresh work.
                .find(|x| {
                    x.name == d.name
                        && x.repo_id == d.repo.repo_id
                        && !consumed.contains(&x.id)
                        && x.status != "done"
                        && repo_info.as_ref().is_none_or(|r| {
                            base_compatible(
                                &x.base_branch,
                                &d.base_branch,
                                std::path::Path::new(&r.local_git_path),
                                &r.base_ref,
                                r.base_ref_is_default,
                            )
                        })
                })
                // Fall back to a name+repo-only claim when no base-compatible match exists, so an
                // approved/denied lane's direction is still claimed (a pending sibling must not reuse it).
                .or_else(|| {
                    existing_dirs.iter().find(|x| {
                        x.name == d.name
                            && x.repo_id == d.repo.repo_id
                            && !consumed.contains(&x.id)
                            && x.status != "done"
                    })
                });
            if let Some(ex) = claimed {
                consumed.insert(ex.id);
            }
        }
    }
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
        // create a duplicate direction/worktree. Fetch the repo BEFORE the find so the
        // reuse `.find` can prefer a BASE-COMPATIBLE direction (R50-4): when duplicate
        // same-name/repo lanes have different bases, matching by name+repo alone could pick
        // a base-incompatible one and make reconcile_reuse error even though a compatible
        // direction with the requested base exists. (All candidates share d.repo.repo_id.)
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
        let repo_path = std::path::Path::new(&repo_ref.local_git_path);
        if let Some(ex) = existing_dirs
            .iter()
            // Exclude terminal (done) directions from reuse: a completed lane is history,
            // not a resumable target — a re-proposal must create fresh work. Prefer a
            // base-COMPATIBLE direction so a same-name lane with a different base is skipped.
            .find(|x| {
                x.name == d.name
                    && x.repo_id == d.repo.repo_id
                    && !consumed.contains(&x.id)
                    && x.status != "done"
                    && base_compatible(
                        &x.base_branch,
                        &d.base_branch,
                        repo_path,
                        &repo_ref.base_ref,
                        repo_ref.base_ref_is_default,
                    )
            })
        {
            let ex_id = ex.id;
            // Claim this direction so a later same-name+repo duplicate lane can't reuse it.
            consumed.insert(ex_id);
            if let Err(err) = reconcile_reuse(
                ex,
                &d.base_branch,
                repo_path,
                &repo_ref.base_ref,
                repo_ref.base_ref_is_default,
            ) {
                rollback_created(db, &created_now).await;
                return Err(err);
            }
            // Materialize the reused lane HERE (it may have a reclaimed worktree dir to recreate).
            // The per-thread gate serializes plan mutations, so no re-propose can land during this
            // confirm — the final CAS can no longer reject, so doing every side effect BEFORE the
            // commit is safe. On failure roll back the lanes created in THIS attempt (created_now;
            // the reused lane existed before and is never torn down) and bail, so nothing is marked
            // confirmed — pending_writes keeps surfacing the Needs cards (no post-commit stranding).
            if let Err(err) = materialize::materialize_direction(db, ex_id).await {
                rollback_created(db, &created_now).await;
                return Err(err);
            }
            dispatch_ids.push(ex_id);
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
    // Test-only seam: let a test land a re-propose in the window before the CAS to exercise the
    // defensive rollback below (mirrors approve_persist_gate). In production the per-thread gate
    // serializes plan mutations, so no save_proposal/confirm can land here — see the CAS note.
    #[cfg(test)]
    tests::confirm_cas_gate(db, thread_id).await;
    // FINAL commit, after ALL materialization. Now that the per-thread gate serializes every
    // plan-mutating op, no re-propose (save_proposal) can land between our snapshot read and here,
    // so this CAS can no longer reject in production — that's why materializing the reused lanes
    // BEFORE this point is safe (a failure above already bailed without marking confirmed). The
    // !applied branch is kept purely defensively: should it ever fire, roll back the lanes created
    // in this attempt and bail so the plan is NOT left "confirmed" with stale lanes.
    if !repo::mark_plan_confirmed_cas(db, thread_id, &start_plan.proposal, &start_plan.status).await? {
        rollback_created(db, &created_now).await;
        anyhow::bail!("plan changed during confirm (re-proposed); please retry");
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
    // Serialize all plan mutations for this thread (see `thread_gate`): a concurrent
    // approve/confirm/save_proposal for the same thread can no longer interleave with this read →
    // CAS → materialize, so the existing CAS-then-materialize-then-revert ordering is race-free.
    let gate = thread_gate(thread_id);
    let _gate = gate.lock().await;
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
    // R44-3: claim directions owned by OTHER already-approved/denied lanes so approving a
    // duplicate-name+repo sibling doesn't reuse one of THEIRS. Without this, when two pending
    // lanes share name+repo and one sibling is already approved (owning its direction), approving
    // the other matches that sibling's direction by name+repo ALONE — marking this lane approved
    // but returning the SIBLING's id, so this lane's sub-task is never created. Mirrors confirm's
    // consumed-set pre-pass.
    let mut consumed: std::collections::HashSet<i32> = std::collections::HashSet::new();
    for (j, pj) in proposal.directions.iter().enumerate() {
        if j == index {
            continue;
        }
        if pj.decision == "approved" || pj.decision == "denied" {
            let rj = resolve(pj, &repos);
            // Prefer claiming the BASE-COMPATIBLE direction among same-name/repo duplicates so the
            // lane being approved (whose find is base-aware) is left the matching one — mirrors
            // confirm's pre-pass (R50-4). Best-effort repo lookup with a name+repo-only fallback.
            let repo_info = repo::get_repo(db, rj.repo.repo_id).await.ok().flatten();
            let claimed = dirs
                .iter()
                // Exclude terminal (done) directions from reuse: a completed lane is history,
                // not a resumable target — a re-proposal must create fresh work.
                .find(|d| {
                    d.name == rj.name
                        && d.repo_id == rj.repo.repo_id
                        && !consumed.contains(&d.id)
                        && d.status != "done"
                        && repo_info.as_ref().is_none_or(|r| {
                            base_compatible(
                                &d.base_branch,
                                &rj.base_branch,
                                std::path::Path::new(&r.local_git_path),
                                &r.base_ref,
                                r.base_ref_is_default,
                            )
                        })
                })
                .or_else(|| {
                    dirs.iter().find(|d| {
                        d.name == rj.name
                            && d.repo_id == rj.repo.repo_id
                            && !consumed.contains(&d.id)
                            && d.status != "done"
                    })
                });
            if let Some(d) = claimed {
                consumed.insert(d.id);
            }
        }
    }
    // Fetch the repo BEFORE the existing-lookup so the find can prefer a BASE-COMPATIBLE
    // direction (R50-4): among same-name/repo duplicates with different bases, matching by
    // name+repo alone could pick a base-incompatible one and make reconcile_reuse error even
    // though a compatible direction with the requested base exists. (All candidates share
    // resolved.repo.repo_id.)
    let repo_ref = repo::get_repo(db, resolved.repo.repo_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("repo {} not found", resolved.repo.repo_id))?;
    let repo_path = std::path::Path::new(&repo_ref.local_git_path);
    if let Some(existing) = dirs
        .iter()
        // Exclude terminal (done) directions from reuse: a completed lane is history,
        // not a resumable target — a re-proposal must create fresh work. Prefer a
        // base-COMPATIBLE direction so a same-name lane with a different base is skipped.
        .find(|d| {
            d.name == resolved.name
                && d.repo_id == resolved.repo.repo_id
                && !consumed.contains(&d.id)
                && d.status != "done"
                && base_compatible(
                    &d.base_branch,
                    &resolved.base_branch,
                    repo_path,
                    &repo_ref.base_ref,
                    repo_ref.base_ref_is_default,
                )
        })
    {
        // The existing direction's worktree is already branched off its stored base;
        // a re-proposal that changes the base can't silently re-base a live (possibly
        // worker-occupied) worktree. Surface the conflict rather than approving with a
        // mismatched base. (Same base → idempotent reuse.)
        // Also handles legacy rows (base_branch == "") where the true base is unknown:
        // those may only be reused when the re-proposal is also default (empty base).
        // Uses the LIVE remote default for comparison (not the cached origin/HEAD).
        reconcile_reuse(
            existing,
            &resolved.base_branch,
            repo_path,
            &repo_ref.base_ref,
            repo_ref.base_ref_is_default,
        )?;
        // Already created (e.g. the lead re-proposed and the decision was reset).
        // Idempotent: don't create a second direction/worktree, but DO re-materialize
        // in case the worktree dir was reclaimed (exists=false) — so the lane has a
        // live worktree to dispatch. Mirror what the normal (non-reuse) path does below.
        let id = existing.id;
        // Test-only seam: let a test land a re-propose in the window between our read and the CAS.
        #[cfg(test)]
        tests::approve_persist_gate(db, thread_id).await;
        proposal.directions[index].decision = "approved".to_string();
        // CAS the approval BEFORE any disk side effect: if a re-propose landed in the window the
        // CAS rejects and we bail WITHOUT recreating the reclaimed worktree (which would undo the
        // user's disk-reclaim for an approval that never applied).
        persist_decision(db, thread_id, &proposal, &plan).await?;
        // Approval committed — now idempotently recreate the worktree if its dir was reclaimed.
        if let Err(err) = materialize::materialize_direction(db, id).await {
            // R45-2: the approval is persisted but rematerialization failed (path is now a plain
            // dir, the branch no longer descends from base, …) — revert the lane to pending so the
            // Needs card stays retryable (else refreshNeeds drops it with no worker dispatched).
            // Best-effort CAS against the approved proposal we just wrote; if a re-propose has
            // since landed the CAS no-ops (the card is superseded anyway).
            let mut reverted = proposal.clone();
            if let Some(d) = reverted.directions.get_mut(index) {
                d.decision = String::new();
            }
            if let (Ok(approved_json), Ok(reverted_json)) =
                (serde_json::to_string(&proposal), serde_json::to_string(&reverted))
            {
                let _ = repo::update_plan_proposal_cas(
                    db,
                    thread_id,
                    &reverted_json,
                    &approved_json,
                    &plan.status,
                )
                .await;
            }
            return Err(err);
        }
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
    // Test-only seam: let a test land a re-propose / confirm in the window between
    // our plan read and the CAS, deterministically driving the CAS rejection below.
    #[cfg(test)]
    tests::approve_persist_gate(db, thread_id).await;
    proposal.directions[index].decision = "approved".to_string();
    if let Err(err) = persist_decision(db, thread_id, &proposal, &plan).await {
        // The CAS rejected: a lead re-proposal (or a confirm) landed after we read the
        // plan, so the frontend will dispatch nothing for this approve. The direction +
        // its worktree/branch we just created would otherwise LEAK in the DB/on disk —
        // roll them back (honoring created_checkout/created_branch) before propagating.
        let _ = materialize::rollback_direction(db, dir.id).await;
        return Err(err);
    }
    Ok(dir.id)
}

/// Deny one proposed direction (by index): mark it denied in the stored
/// proposal. Returns the denied direction's (name, repo_name) for the caller to
/// relay to the lead over the bus.
pub async fn deny_direction(db: &Db, thread_id: i32, index: usize) -> Result<(String, String)> {
    // Serialize all plan mutations for this thread (see `thread_gate`).
    let gate = thread_gate(thread_id);
    let _gate = gate.lock().await;
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
///
/// `expected_base` is the base the client was editing FROM (the persisted value the
/// field rendered). The name/repo guard can't catch a re-propose of the SAME lane
/// (same name+repo) that changed only the base — and that re-propose predates this
/// read, so the CAS can't catch it either. Optimistic-concurrency on the base field
/// closes that gap: if the lane's current base no longer equals `expected_base`, a
/// fresher base already landed and we reject rather than overwriting it with a stale one.
pub async fn set_direction_base(
    db: &Db,
    thread_id: i32,
    index: usize,
    expected_name: &str,
    expected_repo: &str,
    expected_base: &str,
    base: &str,
) -> Result<()> {
    // Serialize all plan mutations for this thread (see `thread_gate`): the base edit's
    // read → guard → CAS can no longer interleave with a concurrent confirm/approve/save_proposal.
    let gate = thread_gate(thread_id);
    let _gate = gate.lock().await;
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
    // Same-identity re-propose guard: a re-propose of the SAME lane that changed only the
    // base would pass the name/repo check above and predate this read (so the CAS misses
    // it). Reject when the lane's current base drifted from what the client edited from.
    if pd.base_branch != expected_base {
        anyhow::bail!(
            "proposal lane at index {index} base changed to {:?} (re-proposed); stale base {base:?} not applied",
            pd.base_branch
        );
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

    /// Per-thread one-shot "race action" the approve flow fires (via
    /// `approve_persist_gate`) between its plan read and its CAS write. A test arms it
    /// with `arm_approve_race(thread, new_proposal_json, new_status)`; when approve
    /// reaches the gate it REPLACES the stored plan outright (simulating a lead
    /// re-propose or a confirm landing in the window), then disarms — so the
    /// subsequent CAS, which expects the originally-read proposal/status, deterministically
    /// rejects. Behind `#[cfg(test)]`, so production never references this.
    fn approve_race_map() -> &'static std::sync::Mutex<std::collections::HashMap<i32, (String, String)>> {
        static M: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<i32, (String, String)>>> =
            std::sync::OnceLock::new();
        M.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
    }

    fn arm_approve_race(thread_id: i32, new_proposal_json: &str, new_status: &str) {
        approve_race_map()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(thread_id, (new_proposal_json.to_string(), new_status.to_string()));
    }

    /// Fired by `approve_direction` (test build only) just before its CAS. If a race is
    /// armed for `thread_id`, apply it ONCE (replacing the stored plan) and disarm.
    pub(super) async fn approve_persist_gate(db: &Db, thread_id: i32) {
        let armed = approve_race_map()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&thread_id);
        if let Some((json, status)) = armed {
            // Replace the stored plan unconditionally (NOT a CAS) — this stands in for the
            // racing lead re-propose / confirm that the real CAS must then detect.
            let created = repo::get_plan(db, thread_id)
                .await
                .ok()
                .flatten()
                .map(|p| p.created_at)
                .unwrap_or_else(now);
            let _ = repo::upsert_plan(db, thread_id, &json, &status, &created).await;
        }
    }

    /// Per-thread one-shot "race action" the CONFIRM flow fires (via `confirm_cas_gate`)
    /// between its single plan-snapshot read and its final CAS write. A test arms it with
    /// `arm_confirm_race(thread, new_proposal_json, new_status)`; when confirm reaches the
    /// gate it REPLACES the stored plan outright (simulating a lead re-propose landing in
    /// the window), then disarms — so the subsequent CAS, which expects the snapshot's
    /// proposal/status, deterministically rejects. Mirrors the approve race seam.
    fn confirm_race_map() -> &'static std::sync::Mutex<std::collections::HashMap<i32, (String, String)>> {
        static M: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<i32, (String, String)>>> =
            std::sync::OnceLock::new();
        M.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
    }

    fn arm_confirm_race(thread_id: i32, new_proposal_json: &str, new_status: &str) {
        confirm_race_map()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(thread_id, (new_proposal_json.to_string(), new_status.to_string()));
    }

    /// Fired by `confirm` (test build only) just before its CAS. If a race is armed for
    /// `thread_id`, apply it ONCE (replacing the stored plan) and disarm.
    pub(super) async fn confirm_cas_gate(db: &Db, thread_id: i32) {
        let armed = confirm_race_map()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&thread_id);
        if let Some((json, status)) = armed {
            let created = repo::get_plan(db, thread_id)
                .await
                .ok()
                .flatten()
                .map(|p| p.created_at)
                .unwrap_or_else(now);
            let _ = repo::upsert_plan(db, thread_id, &json, &status, &created).await;
        }
    }

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
        let ra = repo::add_repo_ref(&db, ws.id, "api", repo_path.to_str().unwrap(), "main", "", true)
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
        let _ra = repo::add_repo_ref(&db, ws.id, "api", root.join("api").to_str().unwrap(), "main", "", true).await.unwrap();
        let t = repo::create_thread(&db, ws.id, "t1", "feature", "claude").await.unwrap();
        let proposal = Proposal { rationale: "r".into(), directions: vec![
            ProposedDirection { name:"A".into(), repo:"api".into(), reason:"r".into(), mandate:"".into(), base_branch:"".into(), decision:"".into() },
            ProposedDirection { name:"B".into(), repo:"api".into(), reason:"r".into(), mandate:"".into(), base_branch:"".into(), decision:"".into() },
        ]};
        save_proposal(&db, t.id, &proposal).await.unwrap();
        // Simulate a confirmed plan, then a targeted base edit must NOT downgrade status.
        let plan = repo::get_plan(&db, t.id).await.unwrap().unwrap();
        repo::upsert_plan(&db, t.id, &plan.proposal, "confirmed", &plan.created_at).await.unwrap();
        set_direction_base(&db, t.id, 1, "B", "api", "", "develop").await.unwrap();
        let after = repo::get_plan(&db, t.id).await.unwrap().unwrap();
        assert_eq!(after.status, "confirmed", "targeted base edit must not downgrade status");
        let parsed: Proposal = serde_json::from_str(&after.proposal).unwrap();
        assert_eq!(parsed.directions[1].base_branch, "develop");
        assert_eq!(parsed.directions[0].base_branch, "", "other directions untouched");
        // Out-of-range index errors.
        assert!(set_direction_base(&db, t.id, 9, "Z", "api", "", "x").await.is_err());
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
        let _ra = repo::add_repo_ref(&db, ws.id, "api", root.join("api").to_str().unwrap(), "main", "", true).await.unwrap();
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

    /// R32-4: approve_direction CREATES + MATERIALIZES the direction (worktree/branch)
    /// and only THEN runs persist_decision (the CAS). If a lead re-proposal landed after
    /// approve read the old proposal, the CAS rejects — and the just-created direction +
    /// its worktree must NOT leak. Drive the CAS failure with the test gate: a re-propose
    /// is applied in the window between approve's read and its CAS.
    #[tokio::test]
    async fn approve_rolls_back_when_persist_cas_rejects() {
        let _env = crate::paths::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tag = format!("weft-approve-casleak-{}", std::process::id());
        let root = std::env::temp_dir().join(format!("{tag}-root"));
        let weft_home = std::env::temp_dir().join(format!("{tag}-home"));
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&weft_home);
        std::env::set_var("WEFT_HOME", weft_home.to_str().unwrap());
        let _repo_path = make_repo(&root, "api");
        let db = Db::connect("sqlite::memory:").await.unwrap();
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        let _ra = repo::add_repo_ref(&db, ws.id, "api", root.join("api").to_str().unwrap(), "main", "", true).await.unwrap();
        let t = repo::create_thread(&db, ws.id, "t1", "feature", "claude").await.unwrap();
        // The proposal approve will READ (a valid default base → create+materialize succeed).
        let original = Proposal { rationale:"r".into(), directions: vec![
            ProposedDirection { name:"A".into(), repo:"api".into(), reason:"r".into(), mandate:"".into(), base_branch:"".into(), decision:"".into() },
        ]};
        save_proposal(&db, t.id, &original).await.unwrap();
        // Arm a re-propose to land in the window between approve's plan read and its CAS:
        // a DIFFERENT stored proposal (extra lane) so the CAS's expected no longer matches.
        let reproposed = Proposal { rationale:"reproposed".into(), directions: vec![
            ProposedDirection { name:"A".into(), repo:"api".into(), reason:"r".into(), mandate:"".into(), base_branch:"".into(), decision:"".into() },
            ProposedDirection { name:"B".into(), repo:"api".into(), reason:"r2".into(), mandate:"".into(), base_branch:"".into(), decision:"".into() },
        ]};
        arm_approve_race(t.id, &serde_json::to_string(&reproposed).unwrap(), "proposed");

        // The branch + checkout-dir approve will create for lane "A" (derived from the
        // thread kind/title) — capture them so we can assert they are torn down.
        let repo_dir = root.join("api");
        let branch = crate::git::choose_branch_name(&repo_dir, "feature", "t1", &[]);
        let wt_path = crate::materialize::worktree_path(&repo_dir, &branch);

        let res = approve_direction(&db, t.id, 0, "codex").await;
        assert!(res.is_err(), "approve must error when the CAS rejects a stale re-propose");

        // No leak: the just-created direction row is gone …
        let dirs = repo::list_directions(&db, t.id).await.unwrap();
        assert!(dirs.is_empty(), "the direction created before the rejected CAS must be rolled back, not leaked");
        // … its worktree row is gone …
        assert!(repo::list_worktrees(&db, None).await.unwrap().is_empty(), "no leaked worktree row");
        // … the on-disk checkout directory is physically removed …
        assert!(!wt_path.exists(), "the worktree checkout dir must be physically removed on rollback");
        // … and the weft-created branch is deleted.
        let branch_check = std::process::Command::new("git")
            .args(["rev-parse", "--verify", &branch])
            .current_dir(&repo_dir)
            .output()
            .unwrap();
        assert!(!branch_check.status.success(), "the weft-created branch must be deleted on rollback");
        // The fresh re-propose survives intact (the stale approve clobbered nothing).
        let after = repo::get_plan(&db, t.id).await.unwrap().unwrap();
        let parsed: Proposal = serde_json::from_str(&after.proposal).unwrap();
        assert_eq!(parsed.directions.len(), 2, "the racing re-propose is left intact");
        assert_eq!(parsed.directions[0].decision, "", "no stale 'approved' written onto the re-propose");

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
        let _ra = repo::add_repo_ref(&db, ws.id, "api", root.join("api").to_str().unwrap(), "main", "", true).await.unwrap();
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
        let _ra = repo::add_repo_ref(&db, ws.id, "api", root.join("api").to_str().unwrap(), "main", "", true).await.unwrap();
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
        let _ra = repo::add_repo_ref(&db, ws.id, "api", root.join("api").to_str().unwrap(), "main", "", true).await.unwrap();
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
        // Editing FROM the bad "no-such-xyz" base the proposal still holds.
        set_direction_base(&db, t.id, 1, "B", "api", "no-such-xyz", "").await.unwrap();
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
        let _ra = repo::add_repo_ref(&db, ws.id, "api", root.join("api").to_str().unwrap(), "main", "", true).await.unwrap();
        let t = repo::create_thread(&db, ws.id, "t1", "feature", "claude").await.unwrap();
        let proposal = Proposal { rationale:"r".into(), directions: vec![
            ProposedDirection { name:"A".into(), repo:"api".into(), reason:"r".into(), mandate:"".into(), base_branch:"".into(), decision:"".into() },
        ]};
        save_proposal(&db, t.id, &proposal).await.unwrap();
        // Correct identity (editing FROM the empty default base) → applies.
        set_direction_base(&db, t.id, 0, "A", "api", "", "develop").await.unwrap();
        let p1: Proposal = serde_json::from_str(&repo::get_plan(&db, t.id).await.unwrap().unwrap().proposal).unwrap();
        assert_eq!(p1.directions[0].base_branch, "develop");
        // Wrong identity (lane changed under the index) → error, no write.
        assert!(set_direction_base(&db, t.id, 0, "B", "api", "develop", "main").await.is_err());
        let p2: Proposal = serde_json::from_str(&repo::get_plan(&db, t.id).await.unwrap().unwrap().proposal).unwrap();
        assert_eq!(p2.directions[0].base_branch, "develop", "stale-identity save must not overwrite");
        std::env::remove_var("WEFT_HOME");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&weft_home);
    }

    /// R32-2: a blur-save from an OLDER proposal must not overwrite a SAME-IDENTITY
    /// (same name+repo) re-propose that changed the base. The name/repo guard passes for
    /// such a re-propose and it predates set_direction_base's read (so the CAS misses it);
    /// optimistic-concurrency on the base field (expected_base) closes the gap.
    #[tokio::test]
    async fn set_direction_base_rejects_stale_same_identity_base() {
        let _env = crate::paths::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tag = format!("weft-basecc-{}", std::process::id());
        let root = std::env::temp_dir().join(format!("{tag}-root"));
        let weft_home = std::env::temp_dir().join(format!("{tag}-home"));
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&weft_home);
        std::env::set_var("WEFT_HOME", weft_home.to_str().unwrap());
        let _repo_path = make_repo(&root, "api");
        let db = Db::connect("sqlite::memory:").await.unwrap();
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        let _ra = repo::add_repo_ref(&db, ws.id, "api", root.join("api").to_str().unwrap(), "main", "", true).await.unwrap();
        let t = repo::create_thread(&db, ws.id, "t1", "feature", "claude").await.unwrap();
        // The lead re-proposed the SAME lane name/repo with a base of "develop" (e.g. the
        // fresh value). A stale blur-save from the OLD proposal was editing FROM "" → its
        // expected_base ("") no longer matches the lane's current base ("develop").
        let proposal = Proposal { rationale:"r".into(), directions: vec![
            ProposedDirection { name:"A".into(), repo:"api".into(), reason:"r".into(), mandate:"".into(), base_branch:"develop".into(), decision:"".into() },
        ]};
        save_proposal(&db, t.id, &proposal).await.unwrap();
        // Stale save: same name/repo, but expected_base "" ≠ live "develop" → reject.
        assert!(
            set_direction_base(&db, t.id, 0, "A", "api", "", "feature/old").await.is_err(),
            "a stale base save from an older same-identity proposal must be rejected"
        );
        let p1: Proposal = serde_json::from_str(&repo::get_plan(&db, t.id).await.unwrap().unwrap().proposal).unwrap();
        assert_eq!(p1.directions[0].base_branch, "develop", "the fresh base must survive the stale save");
        // A save editing FROM the matching current base ("develop") applies.
        set_direction_base(&db, t.id, 0, "A", "api", "develop", "release").await.unwrap();
        let p2: Proposal = serde_json::from_str(&repo::get_plan(&db, t.id).await.unwrap().unwrap().proposal).unwrap();
        assert_eq!(p2.directions[0].base_branch, "release", "matching expected_base applies");
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
        let _ra = repo::add_repo_ref(&db, ws.id, "api", root.join("api").to_str().unwrap(), "main", "", true).await.unwrap();
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
        let _ra = repo::add_repo_ref(&db, ws.id, "api", repo_path.to_str().unwrap(), &def, "", true).await.unwrap();
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
        let _ra = repo::add_repo_ref(&db, ws.id, "api", root.join("api").to_str().unwrap(), "main", "", true).await.unwrap();
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
    async fn confirm_duplicate_name_lane_does_not_reuse_approved_sibling() {
        // R41-3: a proposal with TWO lanes of the SAME name+repo, ONE already approved. confirm
        // skips the approved lane (decision="approved") but its direction stays in existing_dirs;
        // the PENDING sibling's reuse `.find(name+repo)` then matches that APPROVED direction and
        // reuses it instead of creating its own — the pending lane is silently dropped. Claiming
        // each existing direction at most once makes the approved direction pre-consumed, so the
        // pending sibling finds no unconsumed match and creates a SECOND, distinct direction.
        let _env = crate::paths::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tag = format!("weft-confirm-dupname-{}", std::process::id());
        let root = std::env::temp_dir().join(format!("{tag}-root"));
        let weft_home = std::env::temp_dir().join(format!("{tag}-home"));
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&weft_home);
        std::env::set_var("WEFT_HOME", weft_home.to_str().unwrap());
        let _repo_path = make_repo(&root, "api");
        let db = Db::connect("sqlite::memory:").await.unwrap();
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        let ra = repo::add_repo_ref(&db, ws.id, "api", root.join("api").to_str().unwrap(), "main", "", true).await.unwrap();
        let t = repo::create_thread(&db, ws.id, "t1", "feature", "claude").await.unwrap();
        // TWO lanes, same name "A" + same repo "api".
        let lane = || ProposedDirection { name:"A".into(), repo:"api".into(), reason:"r".into(), mandate:"".into(), base_branch:"".into(), decision:"".into() };
        let prop = Proposal { rationale:"r".into(), directions: vec![lane(), lane()] };
        save_proposal(&db, t.id, &prop).await.unwrap();
        // Approve the FIRST lane (index 0): creates + materializes direction #1, sets its decision.
        let approved_id = approve_direction(&db, t.id, 0, "codex").await.unwrap();
        // The stored proposal now has directions[0].decision="approved", directions[1] pending.
        let ids = confirm(&db, t.id).await.unwrap();
        // The pending sibling must get its OWN direction, not reuse the approved one.
        let dirs = repo::list_directions(&db, t.id).await.unwrap();
        let same: Vec<_> = dirs.iter().filter(|d| d.name == "A" && d.repo_id == ra.id).collect();
        assert_eq!(same.len(), 2, "the pending duplicate-name lane must create a SECOND distinct direction");
        assert_eq!(ids.len(), 1, "confirm dispatches only the pending lane (the approved one is handled separately)");
        assert!(!ids.contains(&approved_id), "the pending lane's dispatched id must NOT be the approved direction's id");
        assert!(same.iter().any(|d| d.id == approved_id), "the approved direction still exists");
        assert!(same.iter().any(|d| d.id == ids[0]), "the new pending direction is among the same-name lanes");
        let removed = repo::delete_thread_cascade(&db, t.id).await.unwrap();
        let _ = materialize::cleanup_worktrees(&db, &removed).await;
        std::env::remove_var("WEFT_HOME");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&weft_home);
    }

    /// R44-3: approve_direction must not reuse an APPROVED sibling's direction. Two pending lanes
    /// share name+repo; approving index 0 creates D0. Approving index 1 must create its OWN second
    /// distinct direction (not return D0) — the consumed-set pre-pass claims the approved sibling's
    /// D0 so the index-1 lookup finds no unconsumed match and creates D1. Mirrors confirm's
    /// `confirm_duplicate_name_lane_does_not_reuse_approved_sibling`.
    #[tokio::test]
    async fn approve_duplicate_pending_lane_creates_own_direction_when_sibling_approved() {
        let _env = crate::paths::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tag = format!("weft-approve-dupname-{}", std::process::id());
        let root = std::env::temp_dir().join(format!("{tag}-root"));
        let weft_home = std::env::temp_dir().join(format!("{tag}-home"));
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&weft_home);
        std::env::set_var("WEFT_HOME", weft_home.to_str().unwrap());
        let _repo_path = make_repo(&root, "api");
        let db = Db::connect("sqlite::memory:").await.unwrap();
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        let ra = repo::add_repo_ref(&db, ws.id, "api", root.join("api").to_str().unwrap(), "main", "", true).await.unwrap();
        let t = repo::create_thread(&db, ws.id, "t1", "feature", "claude").await.unwrap();
        // TWO pending lanes, same name "A" + same repo "api".
        let lane = || ProposedDirection { name:"A".into(), repo:"api".into(), reason:"r".into(), mandate:"".into(), base_branch:"".into(), decision:"".into() };
        let prop = Proposal { rationale:"r".into(), directions: vec![lane(), lane()] };
        save_proposal(&db, t.id, &prop).await.unwrap();

        // Approve index 0 → creates D0 and marks directions[0].decision="approved".
        let d0 = approve_direction(&db, t.id, 0, "codex").await.unwrap();

        // Approve index 1 → must create a SECOND, distinct direction (not reuse D0).
        let d1 = approve_direction(&db, t.id, 1, "codex").await.unwrap();
        assert_ne!(d1, d0, "R44-3: the duplicate-name pending sibling must get its OWN direction, not D0");

        // Both directions exist; index 1 is now approved in the stored proposal.
        let dirs = repo::list_directions(&db, t.id).await.unwrap();
        let same: Vec<_> = dirs.iter().filter(|d| d.name == "A" && d.repo_id == ra.id).collect();
        assert_eq!(same.len(), 2, "R44-3: two distinct same-name directions exist after approving both lanes");
        assert!(same.iter().any(|d| d.id == d0));
        assert!(same.iter().any(|d| d.id == d1));
        let p = repo::get_plan(&db, t.id).await.unwrap().unwrap();
        let stored: Proposal = serde_json::from_str(&p.proposal).unwrap();
        assert_eq!(stored.directions[1].decision, "approved", "R44-3: lane index 1 is marked approved");

        let removed = repo::delete_thread_cascade(&db, t.id).await.unwrap();
        let _ = materialize::cleanup_worktrees(&db, &removed).await;
        std::env::remove_var("WEFT_HOME");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&weft_home);
    }

    /// A completed (status="done", terminal) direction must NOT be reused by confirm. When the
    /// lead re-proposes a lane whose previous same-name+repo sub-task already reached "done",
    /// reuse would return the done id and resume the old native session WITHOUT a fresh brief
    /// and WITHOUT moving it out of "done" — newly-approved work looks accepted while no new
    /// worker starts. confirm must instead create a FRESH direction, leaving the done one as
    /// history. Red-first: before excluding "done" from reuse, confirm returns the done id.
    #[tokio::test]
    async fn confirm_does_not_reuse_a_done_direction_creates_fresh() {
        let _env = crate::paths::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tag = format!("weft-confirm-done-{}", std::process::id());
        let root = std::env::temp_dir().join(format!("{tag}-root"));
        let weft_home = std::env::temp_dir().join(format!("{tag}-home"));
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&weft_home);
        std::env::set_var("WEFT_HOME", weft_home.to_str().unwrap());
        let _repo_path = make_repo(&root, "api");
        let db = Db::connect("sqlite::memory:").await.unwrap();
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        let ra = repo::add_repo_ref(&db, ws.id, "api", root.join("api").to_str().unwrap(), "main", "", true).await.unwrap();
        let t = repo::create_thread(&db, ws.id, "t1", "feature", "claude").await.unwrap();
        let lane = || ProposedDirection { name:"A".into(), repo:"api".into(), reason:"r".into(), mandate:"".into(), base_branch:"".into(), decision:"".into() };
        let prop = Proposal { rationale:"r".into(), directions: vec![lane()] };
        // Approve A (creates + materializes the first direction), then mark it terminal ("done").
        save_proposal(&db, t.id, &prop).await.unwrap();
        let done_id = approve_direction(&db, t.id, 0, "codex").await.unwrap();
        repo::set_direction_status(&db, done_id, "done").await.unwrap();
        // Lead re-proposes the SAME name+repo lane (pending), then the user clicks Create.
        save_proposal(&db, t.id, &prop).await.unwrap();
        let ids = confirm(&db, t.id).await.unwrap();
        // confirm must create a NEW, distinct direction and dispatch it — not reuse the done one.
        assert_eq!(ids.len(), 1, "confirm dispatches exactly the fresh lane");
        assert_ne!(ids[0], done_id, "confirm must NOT reuse the completed (done) direction; it creates fresh");
        let dirs = repo::list_directions(&db, t.id).await.unwrap();
        let same: Vec<_> = dirs.iter().filter(|d| d.name == "A" && d.repo_id == ra.id).collect();
        assert_eq!(same.len(), 2, "a SECOND distinct direction exists alongside the completed one");
        // The done direction is untouched (still terminal, kept as history).
        let done_row = dirs.iter().find(|d| d.id == done_id).expect("the done direction still exists");
        assert_eq!(done_row.status, "done", "the completed direction stays done (history), untouched");
        assert!(same.iter().any(|d| d.id == ids[0]), "the dispatched id is the fresh same-name direction");
        let removed = repo::delete_thread_cascade(&db, t.id).await.unwrap();
        let _ = materialize::cleanup_worktrees(&db, &removed).await;
        std::env::remove_var("WEFT_HOME");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&weft_home);
    }

    /// R50-4: when duplicate same-name/repo directions have DIFFERENT bases, confirm's reuse
    /// `.find` must pick the BASE-COMPATIBLE one, not whichever the DB returns first. Before the
    /// fix the find matched by name+repo+!consumed ONLY, so a base-INCOMPATIBLE direction found
    /// first reached `reconcile_reuse`, which errored on the base mismatch — blocking an
    /// idempotent confirm whose requested base matches a DIFFERENT existing direction. Red-first:
    /// without base-aware selection, confirm errors instead of reusing the `release` direction.
    #[tokio::test]
    async fn confirm_reuses_base_compatible_direction_among_duplicates() {
        let _env = crate::paths::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tag = format!("weft-confirm-basecompat-{}", std::process::id());
        let root = std::env::temp_dir().join(format!("{tag}-root"));
        let weft_home = std::env::temp_dir().join(format!("{tag}-home"));
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&weft_home);
        std::env::set_var("WEFT_HOME", weft_home.to_str().unwrap());
        let repo_path = make_repo(&root, "api");
        // Two real explicit bases so each direction can materialize off its own.
        sh(&repo_path, &["git", "branch", "develop"]);
        sh(&repo_path, &["git", "branch", "release"]);
        let db = Db::connect("sqlite::memory:").await.unwrap();
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        let ra = repo::add_repo_ref(&db, ws.id, "api", repo_path.to_str().unwrap(), "main", "", true).await.unwrap();
        let t = repo::create_thread(&db, ws.id, "t1", "feature", "claude").await.unwrap();

        // Seed TWO existing directions, same name+repo, DIFFERENT bases. Insert `develop` FIRST
        // so the name+repo-only `.find` would match it before `release`. Materialize both so they
        // are live lanes with worktrees (the reuse path re-materializes whichever is chosen).
        let dir_dev = repo::create_direction(&db, t.id, "A", "claude", ra.id, "r", "plan+impl", "develop").await.unwrap();
        materialize::materialize_direction(&db, dir_dev.id).await.unwrap();
        let dir_rel = repo::create_direction(&db, t.id, "A", "claude", ra.id, "r", "plan+impl", "release").await.unwrap();
        materialize::materialize_direction(&db, dir_rel.id).await.unwrap();

        // Re-propose the SAME name+repo lane with base `release`, then Create. confirm must reuse
        // the `release` direction (dir_rel), NOT error on the develop-first base mismatch.
        let lane = ProposedDirection { name:"A".into(), repo:"api".into(), reason:"r".into(), mandate:"".into(), base_branch:"release".into(), decision:"".into() };
        let prop = Proposal { rationale:"r".into(), directions: vec![lane] };
        save_proposal(&db, t.id, &prop).await.unwrap();
        let ids = confirm(&db, t.id).await.unwrap();
        assert_eq!(ids, vec![dir_rel.id], "confirm reuses the base-COMPATIBLE (release) direction, not the develop one, and not a new lane");

        let removed = repo::delete_thread_cascade(&db, t.id).await.unwrap();
        let _ = materialize::cleanup_worktrees(&db, &removed).await;
        std::env::remove_var("WEFT_HOME");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&weft_home);
    }

    /// approve_direction must NOT reuse a completed (status="done", terminal) direction. After a
    /// lane reaches "done" and the lead re-proposes it (decision reset to ""), approving it again
    /// must create a FRESH direction (fresh worker), leaving the done one as history. Mirrors
    /// `confirm_does_not_reuse_a_done_direction_creates_fresh`. Red-first: before excluding
    /// "done" from reuse, approve returns the done id.
    #[tokio::test]
    async fn approve_does_not_reuse_a_done_direction_creates_fresh() {
        let _env = crate::paths::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tag = format!("weft-approve-done-{}", std::process::id());
        let root = std::env::temp_dir().join(format!("{tag}-root"));
        let weft_home = std::env::temp_dir().join(format!("{tag}-home"));
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&weft_home);
        std::env::set_var("WEFT_HOME", weft_home.to_str().unwrap());
        let _repo_path = make_repo(&root, "api");
        let db = Db::connect("sqlite::memory:").await.unwrap();
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        let ra = repo::add_repo_ref(&db, ws.id, "api", root.join("api").to_str().unwrap(), "main", "", true).await.unwrap();
        let t = repo::create_thread(&db, ws.id, "t1", "feature", "claude").await.unwrap();
        let lane = || ProposedDirection { name:"A".into(), repo:"api".into(), reason:"r".into(), mandate:"".into(), base_branch:"".into(), decision:"".into() };
        let prop = Proposal { rationale:"r".into(), directions: vec![lane()] };
        // Approve A (creates + materializes direction #1), then mark it terminal ("done").
        save_proposal(&db, t.id, &prop).await.unwrap();
        let done_id = approve_direction(&db, t.id, 0, "codex").await.unwrap();
        repo::set_direction_status(&db, done_id, "done").await.unwrap();
        // Lead re-proposes the SAME lane (decision resets to ""); user approves it again.
        save_proposal(&db, t.id, &prop).await.unwrap();
        let new_id = approve_direction(&db, t.id, 0, "codex").await.unwrap();
        assert_ne!(new_id, done_id, "approve must NOT reuse the completed (done) direction; it creates fresh");
        let dirs = repo::list_directions(&db, t.id).await.unwrap();
        let same: Vec<_> = dirs.iter().filter(|d| d.name == "A" && d.repo_id == ra.id).collect();
        assert_eq!(same.len(), 2, "a SECOND distinct direction exists alongside the completed one");
        let done_row = dirs.iter().find(|d| d.id == done_id).expect("the done direction still exists");
        assert_eq!(done_row.status, "done", "the completed direction stays done (history), untouched");
        assert!(same.iter().any(|d| d.id == new_id), "the new direction is the fresh same-name lane");
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
        let _ra = repo::add_repo_ref(&db, ws.id, "api", root.join("api").to_str().unwrap(), "main", "", true).await.unwrap();
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
        let _ra = repo::add_repo_ref(&db, ws.id, "api", repo_path.to_str().unwrap(), "main", "", true)
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
        let _ra = repo::add_repo_ref(&db, ws.id, "api", root.join("api").to_str().unwrap(), "main", "", true).await.unwrap();
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

    /// R46-1 (replaces the old R44-1 deferred-materialize test): now that the per-thread gate
    /// serializes plan mutations, no re-propose can land mid-confirm, so the final CAS can never
    /// reject in production and materialization happens BEFORE the commit. The CAS `!applied` branch
    /// survives only defensively. Drive it via the test seam and assert the property that still
    /// holds: a rejected CAS leaves the plan NOT "confirmed" (the racing re-propose survives intact)
    /// and rolls back the lanes CREATED in this attempt (created_now) — a reused pre-existing lane
    /// is never torn down. Then the no-race path: a clean confirm recreates the reused dir and
    /// dispatches its id.
    #[tokio::test]
    async fn confirm_defensive_cas_rejection_rolls_back_created_and_leaves_plan_unconfirmed() {
        let _env = crate::paths::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tag = format!("weft-confirm-reuse-casleak-{}", std::process::id());
        let root = std::env::temp_dir().join(format!("{tag}-root"));
        let weft_home = std::env::temp_dir().join(format!("{tag}-home"));
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&weft_home);
        std::env::set_var("WEFT_HOME", weft_home.to_str().unwrap());
        let _repo_path = make_repo(&root, "api");
        let db = Db::connect("sqlite::memory:").await.unwrap();
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        let _ra = repo::add_repo_ref(&db, ws.id, "api", root.join("api").to_str().unwrap(), "main", "", true).await.unwrap();
        let t = repo::create_thread(&db, ws.id, "t1", "feature", "claude").await.unwrap();

        // Lane A: approved via Needs-you → materialized (a reusable, pre-existing lane).
        let prop_a = Proposal { rationale: "r".into(), directions: vec![
            ProposedDirection { name: "A".into(), repo: "api".into(), reason: "r".into(), mandate: "".into(), base_branch: "".into(), decision: "".into() },
        ]};
        save_proposal(&db, t.id, &prop_a).await.unwrap();
        let a_id = approve_direction(&db, t.id, 0, "codex").await.unwrap();

        // Re-propose A (reset to pending) PLUS a brand-new pending lane B → confirm's reuse path
        // handles A and CREATES B (B lands in created_now, so a rejected CAS must roll B back).
        let prop_ab = Proposal { rationale: "r".into(), directions: vec![
            ProposedDirection { name: "A".into(), repo: "api".into(), reason: "r".into(), mandate: "".into(), base_branch: "".into(), decision: "".into() },
            ProposedDirection { name: "B".into(), repo: "api".into(), reason: "rb".into(), mandate: "".into(), base_branch: "".into(), decision: "".into() },
        ]};
        save_proposal(&db, t.id, &prop_ab).await.unwrap();
        // Arm a re-propose to land in confirm's CAS window — a DIFFERENT stored proposal so the
        // snapshot's expected proposal no longer matches and the defensive CAS rejects.
        let reproposed = Proposal { rationale: "reproposed".into(), directions: vec![
            ProposedDirection { name: "A".into(), repo: "api".into(), reason: "r".into(), mandate: "".into(), base_branch: "".into(), decision: "".into() },
            ProposedDirection { name: "C".into(), repo: "api".into(), reason: "rc".into(), mandate: "".into(), base_branch: "".into(), decision: "".into() },
        ]};
        arm_confirm_race(t.id, &serde_json::to_string(&reproposed).unwrap(), "proposed");

        let res = confirm(&db, t.id).await;
        assert!(res.is_err(), "confirm must error when the defensive CAS rejects a stale re-propose");
        // The newly-created lane B was rolled back (created_now); the pre-existing reused lane A stays.
        let dirs = repo::list_directions(&db, t.id).await.unwrap();
        assert!(dirs.iter().any(|d| d.id == a_id), "reused lane A must NOT be torn down on CAS rejection");
        assert!(!dirs.iter().any(|d| d.name == "B"), "lane created in this attempt is rolled back on CAS rejection");
        // The fresh re-propose survives intact (no stale 'confirmed' clobbered onto it).
        let after = repo::get_plan(&db, t.id).await.unwrap().unwrap();
        assert_eq!(after.status, "proposed", "rejected confirm leaves the racing re-propose unconfirmed");

        // Reclaim A's worktree dir, then the no-race path: re-propose A alone (matches the snapshot)
        // and confirm cleanly → the CAS passes, A's reclaimed dir IS recreated, A's id dispatched.
        let wts = repo::list_worktrees(&db, Some(a_id)).await.unwrap();
        assert_eq!(wts.len(), 1, "precondition: one worktree row for A");
        let wt_path = std::path::PathBuf::from(&wts[0].path);
        let repo_p = root.join("api");
        let _ = crate::git::remove_worktree(&repo_p, &wt_path);
        let _ = std::fs::remove_dir_all(&wt_path);
        assert!(!wt_path.exists(), "precondition: reclaimed worktree dir is gone before the clean confirm");
        save_proposal(&db, t.id, &prop_a).await.unwrap();
        let ids = confirm(&db, t.id).await.unwrap();
        assert_eq!(ids, vec![a_id], "no-race: confirm reuses A and dispatches its id");
        assert!(wt_path.exists(), "no-race: a committed confirm recreates the reused dir (materialize-before-commit)");

        let removed = repo::delete_thread_cascade(&db, t.id).await.unwrap();
        let _ = materialize::cleanup_worktrees(&db, &removed).await;
        std::env::remove_var("WEFT_HOME");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&weft_home);
    }

    /// R46-3: confirm now materializes a reused lane BEFORE the final commit (the gate makes the CAS
    /// unrejectable, so all side effects can precede it). If that rematerialization FAILS — the
    /// reused lane's worktree dir was reclaimed and its recorded path is now a PLAIN dir — confirm
    /// must bail BEFORE marking the plan "confirmed" and roll back lanes created in this attempt, so
    /// pending_writes keeps surfacing the Needs cards (no post-commit stranding with no worker).
    /// Red-first: with the OLD deferred (post-CAS) materialize, the plan would already be "confirmed"
    /// when this failure hit.
    #[tokio::test]
    async fn confirm_bails_before_marking_confirmed_when_reused_materialize_fails() {
        let _env = crate::paths::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tag = format!("weft-confirm-reuse-matfail-{}", std::process::id());
        let root = std::env::temp_dir().join(format!("{tag}-root"));
        let weft_home = std::env::temp_dir().join(format!("{tag}-home"));
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&weft_home);
        std::env::set_var("WEFT_HOME", weft_home.to_str().unwrap());
        let _repo_path = make_repo(&root, "api");
        let db = Db::connect("sqlite::memory:").await.unwrap();
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        let _ra = repo::add_repo_ref(&db, ws.id, "api", root.join("api").to_str().unwrap(), "main", "", true).await.unwrap();
        let t = repo::create_thread(&db, ws.id, "t1", "feature", "claude").await.unwrap();
        let prop = Proposal { rationale: "r".into(), directions: vec![
            ProposedDirection { name: "A".into(), repo: "api".into(), reason: "r".into(), mandate: "".into(), base_branch: "".into(), decision: "".into() },
        ]};

        // Approve A via Needs-you → A is materialized (a reusable, pre-existing lane).
        save_proposal(&db, t.id, &prop).await.unwrap();
        let a_id = approve_direction(&db, t.id, 0, "codex").await.unwrap();

        // Reclaim A's worktree dir, then plant a PLAIN directory at the SAME recorded path so the
        // reuse re-materialize fails deterministically (mirrors R45-2): add_worktree_synced's
        // path-exists check bails because a plain dir is "not a worktree of this repo on <branch>".
        let wts = repo::list_worktrees(&db, Some(a_id)).await.unwrap();
        assert_eq!(wts.len(), 1, "precondition: one worktree row after approve");
        let wt_path = std::path::PathBuf::from(&wts[0].path);
        let repo_p = root.join("api");
        let _ = crate::git::remove_worktree(&repo_p, &wt_path);
        let _ = std::fs::remove_dir_all(&wt_path);
        std::fs::create_dir_all(&wt_path).unwrap();
        std::fs::write(wt_path.join("stray.txt"), b"not a worktree").unwrap();
        assert!(
            !crate::git::is_registered_worktree(&repo_p, &wt_path, &wts[0].branch),
            "precondition: the planted plain dir is NOT a registered worktree"
        );

        // Re-propose A (resets its decision to "") so confirm takes the REUSE path for A.
        save_proposal(&db, t.id, &prop).await.unwrap();
        let res = confirm(&db, t.id).await;
        assert!(res.is_err(), "R46-3: confirm must error when the reused lane's rematerialize fails");

        // The plan must NOT be left "confirmed" — it still surfaces via get_resolved / pending_writes
        // so the Needs card stays retryable (no stranding with no worker dispatched).
        let after = get_resolved(&db, t.id).await.unwrap().unwrap();
        assert_ne!(after.status, "confirmed", "R46-3: a pre-commit materialize failure must NOT mark the plan confirmed");
        let pending = pending_writes(&db, t.id).await.unwrap();
        assert_eq!(pending.len(), 1, "R46-3: the Needs card still surfaces after the failed confirm");
        assert_eq!(pending[0].name, "A");

        let removed = repo::delete_thread_cascade(&db, t.id).await.unwrap();
        let _ = materialize::cleanup_worktrees(&db, &removed).await;
        std::env::remove_var("WEFT_HOME");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&weft_home);
    }

    /// The per-thread gate serializes plan mutations: two concurrent `confirm` calls for the SAME
    /// thread can't interleave, so exactly ONE direction is created for a single pending lane (no
    /// duplicate) and BOTH calls return the same id (the loser blocks, then hits the idempotent
    /// confirmed fast-path). Without the gate the read→create→commit dance races and can double-
    /// create. Uses a shared `Db` (SeaORM `DatabaseConnection` is internally a clonable pool handle)
    /// across two `tokio::spawn`ed tasks.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn thread_gate_serializes_concurrent_confirms() {
        let _env = crate::paths::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tag = format!("weft-gate-concurrent-{}", std::process::id());
        let root = std::env::temp_dir().join(format!("{tag}-root"));
        let weft_home = std::env::temp_dir().join(format!("{tag}-home"));
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&weft_home);
        std::env::set_var("WEFT_HOME", weft_home.to_str().unwrap());
        let _repo_path = make_repo(&root, "api");
        // A file-backed (not :memory:) DB so the cloned pool handles in both tasks share one store;
        // an in-memory SQLite connection is private to its single connection.
        let db_file = weft_home.join("gate-concurrent.sqlite");
        std::fs::create_dir_all(&weft_home).unwrap();
        let db = Db::connect(&format!("sqlite://{}?mode=rwc", db_file.to_str().unwrap()))
            .await
            .unwrap();
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        let _ra = repo::add_repo_ref(&db, ws.id, "api", root.join("api").to_str().unwrap(), "main", "", true).await.unwrap();
        let t = repo::create_thread(&db, ws.id, "t1", "feature", "claude").await.unwrap();
        let prop = Proposal { rationale: "r".into(), directions: vec![
            ProposedDirection { name: "A".into(), repo: "api".into(), reason: "r".into(), mandate: "".into(), base_branch: "".into(), decision: "".into() },
        ]};
        save_proposal(&db, t.id, &prop).await.unwrap();

        // Fire two confirms concurrently for the same thread.
        let db1 = db.clone();
        let db2 = db.clone();
        let tid = t.id;
        let h1 = tokio::spawn(async move { confirm(&db1, tid).await });
        let h2 = tokio::spawn(async move { confirm(&db2, tid).await });
        let r1 = h1.await.unwrap();
        let r2 = h2.await.unwrap();

        // Both succeed (the gate serializes them; the loser hits the idempotent confirmed fast-path).
        let ids1 = r1.expect("first confirm ok");
        let ids2 = r2.expect("second confirm ok");
        assert_eq!(ids1.len(), 1, "first confirm dispatches the single lane");
        assert_eq!(ids2.len(), 1, "second confirm returns the same single lane (idempotent)");
        assert_eq!(ids1, ids2, "both confirms return the SAME direction id (no duplicate)");

        // Exactly ONE direction exists for the lane — the gate prevented a duplicate.
        let dirs = repo::list_directions(&db, t.id).await.unwrap();
        let a_dirs: Vec<_> = dirs.iter().filter(|d| d.name == "A").collect();
        assert_eq!(a_dirs.len(), 1, "exactly one direction for lane A despite two concurrent confirms");

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
        let _ra = repo::add_repo_ref(&db, ws.id, "api", root.join("api").to_str().unwrap(), "main", "", true).await.unwrap();
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
        let _ra = repo::add_repo_ref(&db, ws.id, "api", root.join("api").to_str().unwrap(), "main", "", true).await.unwrap();
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

    /// R43-3: the idempotent REUSE approve path must CAS the approval BEFORE recreating a
    /// reclaimed worktree. If a lead re-propose lands in the window (the CAS then rejects),
    /// approve must bail WITHOUT recreating the checkout/branch — otherwise the user's
    /// disk-reclaim is undone for an approval that never applied. Contrast the no-race path,
    /// which recreates + returns Ok (covered by R17-2 above).
    #[tokio::test]
    async fn approve_reuse_cas_rejection_does_not_recreate_reclaimed_worktree() {
        let _env = crate::paths::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tag = format!("weft-approve-reuse-casleak-{}", std::process::id());
        let root = std::env::temp_dir().join(format!("{tag}-root"));
        let weft_home = std::env::temp_dir().join(format!("{tag}-home"));
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&weft_home);
        std::env::set_var("WEFT_HOME", weft_home.to_str().unwrap());
        let _repo_path = make_repo(&root, "api");
        let db = Db::connect("sqlite::memory:").await.unwrap();
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        let _ra = repo::add_repo_ref(&db, ws.id, "api", root.join("api").to_str().unwrap(), "main", "", true).await.unwrap();
        let t = repo::create_thread(&db, ws.id, "t1", "feature", "claude").await.unwrap();
        let prop = Proposal { rationale: "r".into(), directions: vec![
            ProposedDirection { name: "A".into(), repo: "api".into(), reason: "r".into(), mandate: "".into(), base_branch: "".into(), decision: "".into() },
        ]};

        // First approve: creates and materializes the direction (decision="approved").
        save_proposal(&db, t.id, &prop).await.unwrap();
        let dir_id = approve_direction(&db, t.id, 0, "codex").await.unwrap();

        // Simulate the user reclaiming the worktree dir on disk.
        let wts = crate::store::repo::list_worktrees(&db, Some(dir_id)).await.unwrap();
        assert_eq!(wts.len(), 1, "precondition: one worktree row after first approve");
        let wt_path = std::path::PathBuf::from(&wts[0].path);
        let repo_p = root.join("api");
        let _ = crate::git::remove_worktree(&repo_p, &wt_path);
        let _ = std::fs::remove_dir_all(&wt_path);
        assert!(!wt_path.exists(), "precondition: reclaimed worktree dir is gone before re-approve");

        // Re-propose (resets the decision to "") so re-approve takes the idempotent REUSE path.
        save_proposal(&db, t.id, &prop).await.unwrap();
        // Arm a re-propose to land in the window between approve's plan read and its CAS — a
        // DIFFERENT stored proposal (extra lane) so the CAS's expected no longer matches.
        let reproposed = Proposal { rationale: "reproposed".into(), directions: vec![
            ProposedDirection { name: "A".into(), repo: "api".into(), reason: "r".into(), mandate: "".into(), base_branch: "".into(), decision: "".into() },
            ProposedDirection { name: "B".into(), repo: "api".into(), reason: "r2".into(), mandate: "".into(), base_branch: "".into(), decision: "".into() },
        ]};
        arm_approve_race(t.id, &serde_json::to_string(&reproposed).unwrap(), "proposed");

        // Re-approve the REUSE path: the gate fires the re-propose, the CAS rejects → Err.
        let res = approve_direction(&db, t.id, 0, "codex").await;
        assert!(res.is_err(), "R43-3: approve must error when the CAS rejects a stale re-propose");
        // The reclaimed worktree dir must STILL be absent — the side effect was gated behind the CAS.
        assert!(
            !wt_path.exists(),
            "R43-3: a rejected reuse approve must NOT recreate the reclaimed worktree dir"
        );
        // The fresh re-propose survives intact (no stale 'approved' clobbered onto it).
        let after = repo::get_plan(&db, t.id).await.unwrap().unwrap();
        let parsed: Proposal = serde_json::from_str(&after.proposal).unwrap();
        assert_eq!(parsed.directions.len(), 2, "the racing re-propose is left intact");
        assert_eq!(parsed.directions[0].decision, "", "no stale 'approved' written onto the re-propose");

        let removed = repo::delete_thread_cascade(&db, t.id).await.unwrap();
        let _ = materialize::cleanup_worktrees(&db, &removed).await;
        std::env::remove_var("WEFT_HOME");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&weft_home);
    }

    /// R45-2: the idempotent REUSE approve path CASes the approval BEFORE recreating a
    /// reclaimed worktree. If that rematerialization then FAILS (the recorded path is now a
    /// plain dir, the branch no longer descends from base, …), the approval is already
    /// persisted — so the lane must be REVERTED to pending, else refreshNeeds drops the Needs
    /// card with no worker dispatched and the user has no retry path. Contrast the no-failure
    /// path (R17-2 above), which persists "approved" + returns Ok.
    #[tokio::test]
    async fn approve_reuse_reverts_decision_when_rematerialize_fails() {
        let _env = crate::paths::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tag = format!("weft-approve-reuse-revert-{}", std::process::id());
        let root = std::env::temp_dir().join(format!("{tag}-root"));
        let weft_home = std::env::temp_dir().join(format!("{tag}-home"));
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&weft_home);
        std::env::set_var("WEFT_HOME", weft_home.to_str().unwrap());
        let _repo_path = make_repo(&root, "api");
        let db = Db::connect("sqlite::memory:").await.unwrap();
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        let _ra = repo::add_repo_ref(&db, ws.id, "api", root.join("api").to_str().unwrap(), "main", "", true).await.unwrap();
        let t = repo::create_thread(&db, ws.id, "t1", "feature", "claude").await.unwrap();
        let prop = Proposal { rationale: "r".into(), directions: vec![
            ProposedDirection { name: "A".into(), repo: "api".into(), reason: "r".into(), mandate: "".into(), base_branch: "".into(), decision: "".into() },
        ]};

        // First approve: creates and materializes the direction (decision="approved").
        save_proposal(&db, t.id, &prop).await.unwrap();
        let dir_id = approve_direction(&db, t.id, 0, "codex").await.unwrap();

        // Reclaim the worktree dir, then plant a PLAIN directory at the SAME recorded path so
        // rematerialize fails deterministically: add_worktree_synced's path-exists check bails
        // because a plain dir is "not a worktree of this repo on <branch>".
        let wts = crate::store::repo::list_worktrees(&db, Some(dir_id)).await.unwrap();
        assert_eq!(wts.len(), 1, "precondition: one worktree row after first approve");
        let wt_path = std::path::PathBuf::from(&wts[0].path);
        let repo_p = root.join("api");
        let _ = crate::git::remove_worktree(&repo_p, &wt_path);
        let _ = std::fs::remove_dir_all(&wt_path);
        std::fs::create_dir_all(&wt_path).unwrap();
        std::fs::write(wt_path.join("stray.txt"), b"not a worktree").unwrap();
        assert!(
            !crate::git::is_registered_worktree(&repo_p, &wt_path, &wts[0].branch),
            "precondition: the planted plain dir is NOT a registered worktree"
        );

        // Re-propose (resets the decision to "") so re-approve takes the idempotent REUSE path.
        save_proposal(&db, t.id, &prop).await.unwrap();
        let res = approve_direction(&db, t.id, 0, "codex").await;
        assert!(res.is_err(), "R45-2: approve must error when rematerialize fails");

        // The lane must be REVERTED to pending — not left "approved" — so the Needs card stays.
        let after = repo::get_plan(&db, t.id).await.unwrap().unwrap();
        let parsed: Proposal = serde_json::from_str(&after.proposal).unwrap();
        assert_eq!(
            parsed.directions[0].decision, "",
            "R45-2: a failed rematerialize must revert the approval to pending, not leave it 'approved'"
        );

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
        let _ra = repo::add_repo_ref(&db, ws.id, "api", root.join("api").to_str().unwrap(), "main", "", true).await.unwrap();
        let t = repo::create_thread(&db, ws.id, "t1", "feature", "claude").await.unwrap();

        // Re-propose with BOTH lanes pending (the lead can ONLY emit pending lanes —
        // save_proposal scrubs any injected decision, R47-3). The human then approves A
        // individually via approve_direction; B stays pending for confirm to create.
        let prop_ab = Proposal { rationale: "r".into(), directions: vec![
            ProposedDirection { name: "A".into(), repo: "api".into(), reason: "r".into(), mandate: "".into(), base_branch: "".into(), decision: "".into() },
            ProposedDirection { name: "B".into(), repo: "api".into(), reason: "r".into(), mandate: "".into(), base_branch: "".into(), decision: "".into() },
        ]};
        save_proposal(&db, t.id, &prop_ab).await.unwrap();
        // Lane A is approved via approve_direction (the legitimate decision path) → decision="approved".
        let a_id = approve_direction(&db, t.id, 0, "codex").await.unwrap();
        // confirm → creates B only (A is "approved" → skipped by main path).
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

    /// R42-2: a confirmed plan with TWO same-name+repo lanes — one approved (owning its
    /// own direction) and one pending (owning its own) — must, on the idempotent fast-path
    /// retry, return ONLY the pending lane's direction id. Without consuming directions, the
    /// `.any(...)` match returned EVERY same-name direction, re-dispatching the approved one.
    #[tokio::test]
    async fn confirmed_fast_path_does_not_redispatch_approved_duplicate_sibling() {
        let _env = crate::paths::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tag = format!("weft-fastpath-dup-{}", std::process::id());
        let root = std::env::temp_dir().join(format!("{tag}-root"));
        let weft_home = std::env::temp_dir().join(format!("{tag}-home"));
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&weft_home);
        std::env::set_var("WEFT_HOME", weft_home.to_str().unwrap());
        let _repo_path = make_repo(&root, "api");
        let db = Db::connect("sqlite::memory:").await.unwrap();
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        let _ra = repo::add_repo_ref(&db, ws.id, "api", root.join("api").to_str().unwrap(), "main", "", true).await.unwrap();
        let t = repo::create_thread(&db, ws.id, "t1", "feature", "claude").await.unwrap();

        // Propose TWO same-name+repo `A` lanes, BOTH pending (the lead can only emit
        // pending lanes — save_proposal scrubs any injected decision, R47-3).
        let prop_dup = Proposal { rationale: "r".into(), directions: vec![
            ProposedDirection { name: "A".into(), repo: "api".into(), reason: "r".into(), mandate: "".into(), base_branch: "".into(), decision: "".into() },
            ProposedDirection { name: "A".into(), repo: "api".into(), reason: "r".into(), mandate: "".into(), base_branch: "".into(), decision: "".into() },
        ]};
        save_proposal(&db, t.id, &prop_dup).await.unwrap();
        // Approve the FIRST `A` lane individually via approve_direction (the legitimate
        // decision path) → creates the APPROVED lane's direction (a_id). The second `A`
        // stays pending. confirm pre-claims the approved lane's a_id (R41-3), so the
        // pending lane creates its OWN distinct direction (b_id).
        let a_id = approve_direction(&db, t.id, 0, "codex").await.unwrap();
        let confirm_ids = confirm(&db, t.id).await.unwrap();
        assert_eq!(confirm_ids.len(), 1, "confirm creates only the pending duplicate lane");
        let b_id = confirm_ids[0];
        assert_ne!(b_id, a_id, "the pending lane owns a direction distinct from the approved one");

        // Plan is now "confirmed". The fast-path retry must return ONLY the pending lane's
        // b_id — NOT the approved a_id (which must not be re-dispatched here).
        let retry_ids = confirm(&db, t.id).await.unwrap();
        assert_eq!(retry_ids.len(), 1, "R42-2: fast-path returns exactly one id (count == 1)");
        assert_eq!(retry_ids, vec![b_id], "R42-2: fast-path returns only the pending duplicate lane");
        assert!(!retry_ids.contains(&a_id), "R42-2: the approved duplicate sibling must NOT be re-dispatched");

        let removed = repo::delete_thread_cascade(&db, t.id).await.unwrap();
        let _ = materialize::cleanup_worktrees(&db, &removed).await;
        std::env::remove_var("WEFT_HOME");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&weft_home);
    }

    /// The confirmed (idempotent) fast-path must also exclude completed (status="done", terminal)
    /// directions from reuse. After a plan is confirmed and its lane's direction reaches "done", a
    /// retry of confirm (plan still "confirmed") must NOT re-dispatch the done id — the done lane is
    /// history, not a resumable target. Red-first: before excluding "done", the fast-path's match
    /// over all directions returns the done id.
    #[tokio::test]
    async fn confirmed_fast_path_excludes_done_direction() {
        let _env = crate::paths::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tag = format!("weft-fastpath-done-{}", std::process::id());
        let root = std::env::temp_dir().join(format!("{tag}-root"));
        let weft_home = std::env::temp_dir().join(format!("{tag}-home"));
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&weft_home);
        std::env::set_var("WEFT_HOME", weft_home.to_str().unwrap());
        let _repo_path = make_repo(&root, "api");
        let db = Db::connect("sqlite::memory:").await.unwrap();
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        let _ra = repo::add_repo_ref(&db, ws.id, "api", root.join("api").to_str().unwrap(), "main", "", true).await.unwrap();
        let t = repo::create_thread(&db, ws.id, "t1", "feature", "claude").await.unwrap();
        let prop = Proposal { rationale:"r".into(), directions: vec![
            ProposedDirection { name:"A".into(), repo:"api".into(), reason:"r".into(), mandate:"".into(), base_branch:"".into(), decision:"".into() },
        ]};
        save_proposal(&db, t.id, &prop).await.unwrap();
        // First confirm: creates the lane and marks the plan "confirmed".
        let first_ids = confirm(&db, t.id).await.unwrap();
        assert_eq!(first_ids.len(), 1, "first confirm creates the single lane");
        let done_id = first_ids[0];
        // The lane completes (terminal). The plan stays "confirmed" (no re-propose).
        repo::set_direction_status(&db, done_id, "done").await.unwrap();
        // Retry hits the idempotent fast-path (plan == "confirmed"). It must NOT return the done id.
        let retry_ids = confirm(&db, t.id).await.unwrap();
        assert!(!retry_ids.contains(&done_id), "fast-path must NOT re-dispatch the completed (done) lane");
        assert!(retry_ids.is_empty(), "no reusable (non-done) lane remains, so the fast-path returns nothing");
        // The done direction is untouched.
        let dirs = repo::list_directions(&db, t.id).await.unwrap();
        let done_row = dirs.iter().find(|d| d.id == done_id).expect("the done direction still exists");
        assert_eq!(done_row.status, "done", "the completed direction stays done (history), untouched");
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
        let _ra = repo::add_repo_ref(&db, ws.id, "api", root.join("api").to_str().unwrap(), "main", "", true).await.unwrap();
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

    /// R47-3 (SECURITY): `save_proposal` is the trust boundary for the lead's payload.
    /// `ProposedDirection.decision` has `#[serde(default)]`, so a malformed/hostile
    /// `propose_directions` call can inject `decision="approved"`/`"denied"`. Those lanes
    /// would then vanish from `pending_writes` (a required human approval silently bypassed)
    /// and `confirm` would skip them (a write dropped with NO human action). The lead must
    /// NEVER set decisions — only approve/deny do. So `save_proposal` must scrub every
    /// direction's decision to "" before storing.
    #[tokio::test]
    async fn save_proposal_scrubs_injected_decisions() {
        let _env = crate::paths::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tag = format!("weft-scrub-{}", std::process::id());
        let root = std::env::temp_dir().join(format!("{tag}-root"));
        let weft_home = std::env::temp_dir().join(format!("{tag}-home"));
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&weft_home);
        std::env::set_var("WEFT_HOME", weft_home.to_str().unwrap());
        let repo_a = make_repo(&root, "api");
        let repo_b = make_repo(&root, "web");

        let db = Db::connect("sqlite::memory:").await.unwrap();
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        repo::add_repo_ref(&db, ws.id, "api", repo_a.to_str().unwrap(), "main", "", true)
            .await
            .unwrap();
        repo::add_repo_ref(&db, ws.id, "web", repo_b.to_str().unwrap(), "main", "", true)
            .await
            .unwrap();
        let t = repo::create_thread(&db, ws.id, "t1", "feature", "claude")
            .await
            .unwrap();

        // Hostile proposal: BOTH directions carry an injected decision the lead must
        // never be able to set.
        let proposal = Proposal {
            rationale: "r".into(),
            directions: vec![
                ProposedDirection {
                    name: "Payments".into(),
                    repo: "api".into(),
                    reason: "add discount endpoint".into(),
                    mandate: "impl-only".into(),
                    base_branch: "".into(),
                    decision: "approved".into(),
                },
                ProposedDirection {
                    name: "Web".into(),
                    repo: "web".into(),
                    reason: "wire it up".into(),
                    mandate: "".into(),
                    base_branch: "".into(),
                    decision: "denied".into(),
                },
            ],
        };
        save_proposal(&db, t.id, &proposal).await.unwrap();

        // The stored/resolved proposal must show NO decisions — all scrubbed to pending.
        let resolved = get_resolved(&db, t.id).await.unwrap().unwrap();
        for d in &resolved.directions {
            assert_eq!(
                d.decision, "",
                "R47-3: lead-injected decision must be scrubbed to pending, got {:?} on {:?}",
                d.decision, d.name
            );
        }
        // And both lanes must surface as pending writes (the human approval was NOT bypassed).
        let pending = pending_writes(&db, t.id).await.unwrap();
        assert_eq!(
            pending.len(),
            2,
            "R47-3: both lanes must be pending (injected approved/denied must not drop them)"
        );

        // Cleanup.
        let removed = repo::delete_thread_cascade(&db, t.id).await.unwrap();
        let _ = materialize::cleanup_worktrees(&db, &removed).await;
        std::env::remove_var("WEFT_HOME");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&weft_home);
    }

    /// R50-2: every save_proposal (re-propose) must bump the proposal VERSION exposed on
    /// ResolvedProposal, so the frontend can reset a dirty (unblurred) base edit on ANY
    /// re-proposal — even one that keeps the same name/repo AND base. The version is the plan's
    /// `created_at` (repurposed as "last proposed at"); it must STRICTLY change between two
    /// back-to-back saves (a coarse second-granular timestamp would not — hence a monotonic
    /// version). Red-first: before bumping, the version is identical across re-proposals.
    #[tokio::test]
    async fn save_proposal_bumps_proposal_version() {
        let _env = crate::paths::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tag = format!("weft-version-bump-{}", std::process::id());
        let root = std::env::temp_dir().join(format!("{tag}-root"));
        let weft_home = std::env::temp_dir().join(format!("{tag}-home"));
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&weft_home);
        std::env::set_var("WEFT_HOME", weft_home.to_str().unwrap());
        let _repo_path = make_repo(&root, "api");
        let db = Db::connect("sqlite::memory:").await.unwrap();
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        repo::add_repo_ref(&db, ws.id, "api", root.join("api").to_str().unwrap(), "main", "", true).await.unwrap();
        let t = repo::create_thread(&db, ws.id, "t1", "feature", "claude").await.unwrap();
        let prop = Proposal { rationale: "r".into(), directions: vec![
            ProposedDirection { name: "A".into(), repo: "api".into(), reason: "r".into(), mandate: "".into(), base_branch: "".into(), decision: "".into() },
        ]};

        // First save → capture the exposed version.
        save_proposal(&db, t.id, &prop).await.unwrap();
        let v1 = get_resolved(&db, t.id).await.unwrap().unwrap().created_at;
        assert!(!v1.is_empty(), "version must be exposed on ResolvedProposal");

        // Re-propose the SAME proposal (same name/repo/base) → version must STRICTLY change.
        save_proposal(&db, t.id, &prop).await.unwrap();
        let v2 = get_resolved(&db, t.id).await.unwrap().unwrap().created_at;
        assert_ne!(v1, v2, "every re-proposal must bump the proposal version, even with identical content");

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
        let result = reconcile_reuse(&existing, "", repo_path, "main", true);
        assert!(result.is_ok(), "R18-4: blank re-proposal must reuse a HEAD-based direction, got: {:?}", result.err());

        // An explicit non-empty base against a HEAD-based direction: must still conflict
        // (we only relax the blank-proposal case).
        let conflict = reconcile_reuse(&existing, "develop", repo_path, "main", true);
        assert!(conflict.is_err(), "R18-4: explicit base against HEAD-based direction must conflict");
    }

    /// R47-4: a stored explicit base can now be QUALIFIED (`refs/heads/develop` or
    /// `refs/remotes/origin/develop`). `reconcile_reuse`'s `effective` closure must
    /// normalize with the SAME canonical normalizer the materialization path uses
    /// (`git::normalize_target`, which strips refs/remotes/origin/, refs/heads/, and
    /// origin/) so a later bare-equivalent re-proposal (`develop`) is NOT wrongly
    /// rejected as a base change — that would block idempotent approve/confirm. A
    /// genuinely different base (`release`) must still bail.
    #[test]
    fn reconcile_reuse_matches_qualified_base_against_bare_reproposal() {
        use crate::store::entities::direction;
        let make = |base: &str| direction::Model {
            id: 1,
            thread_id: 1,
            name: "A".to_string(),
            slug: "a".to_string(),
            tool: "codex".to_string(),
            repo_id: 1,
            reason: "r".to_string(),
            status: "queued".to_string(),
            mandate: "plan+impl".to_string(),
            base_branch: base.to_string(),
            target_branch: "".to_string(),
            branch: "feat/a".to_string(),
            created_at: "0".to_string(),
        };
        // Non-empty bases are normalized purely by string prefix-stripping — no repo
        // touched — so an inert path is fine.
        let repo_path = std::path::Path::new("/tmp/weft-nonexistent-reconcile-qualified");

        // refs/heads/develop (locally-qualified) vs bare `develop` re-proposal → reuse Ok.
        let local_qualified = make("refs/heads/develop");
        assert!(
            reconcile_reuse(&local_qualified, "develop", repo_path, "main", true).is_ok(),
            "R47-4: refs/heads/develop must match a bare `develop` re-proposal (no spurious base-change)"
        );
        // refs/remotes/origin/develop (remote-qualified) vs bare `develop` → reuse Ok.
        let remote_qualified = make("refs/remotes/origin/develop");
        assert!(
            reconcile_reuse(&remote_qualified, "develop", repo_path, "main", true).is_ok(),
            "R47-4: refs/remotes/origin/develop must match a bare `develop` re-proposal"
        );
        // A genuinely different base still bails.
        assert!(
            reconcile_reuse(&local_qualified, "release", repo_path, "main", true).is_err(),
            "R47-4: a genuinely different base (release vs develop) must still conflict"
        );
    }

    /// R37-3: a DETACHED-HEAD lane records `base_branch==""` + `target_branch=<40-hex sha>`
    /// (the branch-off commit). On a blank re-proposal ("fork from current HEAD") it must be
    /// reused ONLY when HEAD is unchanged (head_commit_full == the stored sha). If the repo's
    /// HEAD advanced since, reuse would fork the re-proposal from a stale commit — so bail.
    /// A TRUE-legacy lane (target is empty or a branch name, not a sha) keeps the legacy
    /// blank-reuse shortcut.
    #[test]
    fn reconcile_reuse_rejects_moved_detached_head() {
        use crate::store::entities::direction;
        use std::process::Command as Cmd;
        let tag = format!("weft-reconcile-detached-{}", std::process::id());
        let repo_path = std::env::temp_dir().join(tag);
        let _ = std::fs::remove_dir_all(&repo_path);
        crate::git::init_repo(&repo_path).unwrap();
        let sha = |rev: &str| -> String {
            String::from_utf8(
                Cmd::new("git").args(["rev-parse", rev]).current_dir(&repo_path).output().unwrap().stdout,
            ).unwrap().trim().to_string()
        };
        // The branch-off COMMIT x — what a detached-HEAD lane stores as its target.
        let x = sha("HEAD");
        assert_eq!(x.len(), 40, "precondition: full 40-char sha");

        let make = |base: &str, target: &str| direction::Model {
            id: 1,
            thread_id: 1,
            name: "A".to_string(),
            slug: "a".to_string(),
            tool: "codex".to_string(),
            repo_id: 1,
            reason: "r".to_string(),
            status: "queued".to_string(),
            mandate: "plan+impl".to_string(),
            base_branch: base.to_string(),
            target_branch: target.to_string(),
            branch: "feat/a".to_string(),
            created_at: "0".to_string(),
        };

        // Detached-HEAD lane: base "", target = x. HEAD == x → blank re-proposal reuses.
        let detached = make("", &x);
        assert!(
            reconcile_reuse(&detached, "", &repo_path, "main", true).is_ok(),
            "blank re-proposal must reuse a detached-HEAD lane whose stored commit == current HEAD"
        );
        // A non-blank proposed base against a detached-HEAD lane still bails (legacy branch).
        assert!(
            reconcile_reuse(&detached, "develop", &repo_path, "main", true).is_err(),
            "an explicit base against a detached-HEAD lane must still conflict"
        );

        // Advance HEAD to a NEW commit y → head_commit_full now != x.
        Cmd::new("git").args(["commit", "-q", "--allow-empty", "-m", "advance"])
            .current_dir(&repo_path).status().unwrap();
        assert_ne!(sha("HEAD"), x, "precondition: HEAD advanced to y");
        // Blank re-proposal against the SAME stored x must now bail (HEAD moved).
        assert!(
            reconcile_reuse(&detached, "", &repo_path, "main", true).is_err(),
            "a blank re-proposal must NOT reuse a detached-HEAD lane after HEAD moved off the stored commit"
        );

        // TRUE-legacy lanes are UNCHANGED: base "" + target "" (or a branch name) → blank reuse Ok.
        let legacy_empty = make("", "");
        assert!(
            reconcile_reuse(&legacy_empty, "", &repo_path, "main", true).is_ok(),
            "a legacy lane (empty target) must keep the blank-reuse shortcut"
        );
        let legacy_branch = make("", "develop");
        assert!(
            reconcile_reuse(&legacy_branch, "", &repo_path, "main", true).is_ok(),
            "a legacy lane (branch-name target, not a sha) must keep the blank-reuse shortcut"
        );

        let _ = std::fs::remove_dir_all(&repo_path);
    }
}
