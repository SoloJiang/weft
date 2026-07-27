//! Auto-merge executor (issue #110 T3): the ONLY mutating write path for a
//! tracked PR/MR anywhere in this crate. Deliberately separate from
//! `host::monitor` (read-only sweep) and `host::github`'s `PrHost` impl
//! (read-only `fetch_status`) — see `host`'s own module doc for why that
//! separation is load-bearing, not stylistic: `host::monitor` never calls
//! anything in this file, and nothing in this file is reachable except from
//! this file's own spawned loop. Opt-in, default OFF (`K_AUTO_MERGE_ENABLED`)
//! — merging a PR is an irreversible action with no human confirming the
//! specific merge, so the default posture is "tell me it's ready", not
//! "merge it for me" (see `auto_merge_enabled`'s doc).
//!
//! Flow, once per sweep tick:
//!   1. [`spawn_pr_automerge_watch`] — its own timer loop, independent of
//!      `host::monitor::spawn_pr_watch`'s (separate `WEFT_PR_AUTOMERGE_SWEEP_
//!      SECS` cadence). Re-reads the SAME `pull_request` rows `host::monitor`
//!      already keeps fresh, via the EXISTING `repo::list_open_pull_requests`
//!      — no new store functions, no store/ edits at all.
//!   2. Each row's stored state is parsed and run through the pure gate
//!      `gate::decide_auto_merge`. Only a `Merge` verdict proceeds.
//!   3. [`run_gh_merge`] — `gh pr merge --squash --match-head-commit <sha>`.
//!      The ONE new `Command::new` in this entire feature.
//!      `--match-head-commit` makes GitHub itself refuse the merge if the
//!      head has moved since the judgement this attempt is based on,
//!      closing the "merged code nobody reviewed" gap server-side, with no
//!      extra network round trip on our side to race against.
//!   4. Regardless of outcome, ONE fresh `PrHost::fetch_status` read (this
//!      file's only read — the same call `host::monitor` itself makes) is
//!      taken and persisted via the EXISTING `repo::apply_pull_request_
//!      snapshot` / `repo::mark_pull_request_probe_error` — so the row (and
//!      the NEXT sweep, by either loop) always reflects ground truth — and a
//!      durable, honest timeline marker is left either way (see
//!      [`insert_automerge_marker`]'s doc for why it is kind `"text"`, not a
//!      new `ChatTimeline` marker kind).
//!
//! Race safety without an in-memory lock: within one sweep tick, rows are
//! processed strictly sequentially (`for pr in open { maybe_merge_one(...).
//! await }`), and [`spawn_pr_automerge_watch`]'s loop only sleeps — starting
//! the NEXT tick — after the WHOLE pass returns. So a second attempt at the
//! same row can never overlap the first attempt's confirmation write. The one
//! residual gap is a process crash between step 3 succeeding and step 4's
//! write landing: the stored row would still read `Ready` next boot, so a
//! retry is attempted — but by then the fresh read in step 4 (now step 3 of
//! the retry) observes the PR as `Merged` on GitHub and `run_gh_merge` itself
//! fails cleanly (GitHub refuses to re-merge a merged PR) rather than
//! double-merging anything. Bounded, self-healing, never destructive.

use std::process::Command;

use tauri::{AppHandle, Emitter, Manager};

use super::gate::{self, AutoMergeDecision};
use super::judge;
use super::{HostError, HostKind, PrLifecycle, PrTarget};
use crate::store::entities::pull_request;
use crate::store::{repo, Db};

/// The `app_setting` key for the opt-in switch. Unset (fresh install, or an
/// upgraded DB that predates this feature) reads as OFF — see
/// `auto_merge_enabled`'s doc for the fail-closed treatment of a DB read
/// error too.
pub const K_AUTO_MERGE_ENABLED: &str = "pr_auto_merge_enabled";

/// Default sweep cadence for THIS feature's own loop — deliberately NOT
/// shared with `host::monitor::PR_SWEEP_DEFAULT_SECS`, even though both
/// currently default to the same number, because the two loops must stay
/// independently schedulable (see this module's doc on why they are
/// separate loops at all). Override with `WEFT_PR_AUTOMERGE_SWEEP_SECS`;
/// `0` disables the loop entirely.
const PR_AUTOMERGE_SWEEP_DEFAULT_SECS: u64 = 60;

/// How stale a row's last SUCCESSFUL probe may be before this sweep refuses
/// to act on its stored `Ready` verdict, even with zero recorded probe
/// failures (`probe_fail_count == 0`) — the OTHER way a row's `Ready` column
/// can outlive its truth: not a failing probe (see `gate::AutoMergeSkipReason
/// ::ProbeFailing`), but a STALLED one (the sweep loop itself wedged, or the
/// whole process was suspended for hours and just resumed). Ten sweep
/// intervals at `host::monitor`'s own default cadence — generous enough to
/// ride out ordinary scheduling jitter, far short of "hours-old".
const MAX_READY_AGE_SECS: i64 = 600;

/// Start the runtime PR/MR auto-merge sweep. Call once at app setup,
/// alongside `host::monitor::spawn_pr_watch` (see `lib.rs`) — NOT instead of
/// it, and not chained off it; see this module's doc for why the two stay
/// separate loops.
pub fn spawn_pr_automerge_watch(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let sweep_secs = crate::commands::env_secs(
            "WEFT_PR_AUTOMERGE_SWEEP_SECS",
            PR_AUTOMERGE_SWEEP_DEFAULT_SECS,
        );
        if sweep_secs == 0 {
            return; // disabled
        }
        run_automerge_sweep(&app).await;
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(sweep_secs)).await;
            run_automerge_sweep(&app).await;
        }
    });
}

/// One sweep pass. Short-circuits before listing any rows when the feature
/// is off (the default for almost every install) rather than doing real
/// per-row work just to gate on `enabled` — same shape as `host::monitor::
/// run_pr_sweep`'s own `try_state` guards.
async fn run_automerge_sweep(app: &AppHandle) {
    let Some(db) = app.try_state::<Db>() else {
        return;
    };
    let db = Db(db.0.clone(), db.1);

    if !auto_merge_enabled(&db).await {
        return;
    }

    // No probe-failure ceiling here (`i32::MAX`, effectively "don't exclude
    // anything at the query level") — unlike `host::monitor`'s own sweep,
    // which stops RETRYING a persistently-failing row past its give-up
    // threshold. This feature must still SEE every open row so its own gate
    // (`gate::AutoMergeSkipReason::ProbeFailing`) is what skips a failing
    // one, visibly and testably, rather than an invisible SQL-level
    // exclusion silently doing the same job.
    let open = match repo::list_open_pull_requests(&db, i32::MAX).await {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[weft][automerge] sweep: could not list tracked PR/MRs: {e}");
            return;
        }
    };
    for pr in open {
        maybe_merge_one(app, &db, pr).await;
    }
}

/// Gate one row, and if it clears the bar, execute + confirm + record the
/// merge attempt. No-op (silently) for every `Skip` verdict — a blocked or
/// indeterminate row already has `host::monitor`'s own Needs-you notice
/// telling the human why, when that is warranted; this feature only speaks
/// up when it actually ACTS (see `insert_automerge_marker`'s doc).
async fn maybe_merge_one(app: &AppHandle, db: &Db, pr: pull_request::Model) {
    let Some(host_kind) = HostKind::parse(&pr.host_kind) else {
        return; // unrecognized host_kind on the row — nothing sane to do
    };
    let now = repo::now_unix();
    let lifecycle = gate::parse_lifecycle(&pr.lifecycle);
    let readiness = gate::parse_readiness(&pr.merge_readiness);
    let age = gate::age_secs(&pr.last_checked_at, &now);

    let decision = gate::decide_auto_merge(
        true, // this sweep already confirmed `enabled` via `auto_merge_enabled`'s
        // fail-closed read before listing rows; passed through explicitly
        // (rather than hard-coding the gate to skip the check) so
        // `gate::decide_auto_merge` stays a complete, independently
        // testable decision — see that function's own doc.
        host_kind,
        lifecycle,
        &readiness,
        pr.probe_fail_count,
        age,
        MAX_READY_AGE_SECS,
    );
    if decision != AutoMergeDecision::Merge {
        return;
    }

    let merge_result =
        run_gh_merge(&pr.host_owner, &pr.host_repo, pr.number, &pr.head_sha);

    // Regardless of outcome: one fresh, honest read, persisted the same way
    // `host::monitor` persists every probe — never leave the row (or the
    // human) trusting a pre-attempt snapshot once we know more.
    let target =
        PrTarget { owner: pr.host_owner.clone(), repo: pr.host_repo.clone(), number: pr.number };
    let confirmed = tokio::task::spawn_blocking(move || {
        super::resolve_host(host_kind).and_then(|h| h.fetch_status(&target))
    })
    .await
    .unwrap_or_else(|join_err| {
        Err(HostError::Other {
            message: format!("internal: automerge confirmation join error: {join_err}"),
        })
    });

    let current_state_text = match &confirmed {
        Ok(snapshot) => {
            let confirmed_readiness =
                judge::merge_readiness(&snapshot.ci, &snapshot.review, &snapshot.conflict);
            if let Err(e) =
                repo::apply_pull_request_snapshot(db, pr.id, snapshot, &confirmed_readiness).await
            {
                eprintln!(
                    "[weft][automerge] pr #{}: could not save confirmation snapshot: {e}",
                    pr.id
                );
            }
            lifecycle_state_text(snapshot.lifecycle).to_string()
        }
        Err(err) => {
            if let Err(store_err) = repo::mark_pull_request_probe_error(db, pr.id, &err.message()).await
            {
                eprintln!(
                    "[weft][automerge] pr #{}: could not save confirmation probe error: {store_err}",
                    pr.id
                );
            }
            format!("暂时无法确认当前状态({})", err.message())
        }
    };

    let marker_text = match &merge_result {
        Ok(()) => format!(
            "✅ 自动合并完成:{} #{} 已 squash 合并到 {}。当前状态:{}。",
            host_kind.native_abbrev(),
            pr.number,
            pr.base_ref,
            current_state_text
        ),
        Err(reason) => format!(
            "❌ 自动合并失败:{} #{} — {}。当前状态:{}。",
            host_kind.native_abbrev(),
            pr.number,
            reason,
            current_state_text
        ),
    };
    insert_automerge_marker(app, db, pr.thread_id, &marker_text).await;
}

/// Exhaustive, human-facing (Chinese; dev-English tokens kept — same
/// backend-composed-notice convention `judge::ci_reason`/`review_reason`/
/// `conflict_reason` already document and use) description of a FRESH
/// lifecycle read, for the "当前状态" clause of both the success and failure
/// markers. `Merged` is listed even in the success branch's own text because
/// a merge attempt this file made is not the only way a PR becomes merged —
/// see this module's doc on the crash-then-retry case, where a SECOND
/// attempt's failure marker must be able to say "already merged" instead of
/// reading as a fresh, alarming failure.
fn lifecycle_state_text(lifecycle: PrLifecycle) -> &'static str {
    match lifecycle {
        PrLifecycle::Open => "仍是 open,未合并",
        PrLifecycle::Merged => "已是 merged 状态",
        PrLifecycle::Closed => "已被关闭,未合并",
    }
}

/// The ONE mutating call in this entire feature — `gh pr merge`. Nothing in
/// `host::monitor` / `host::github` / `host::judge` / `host::gate` ever calls
/// this, and this file only calls it from `maybe_merge_one`, gated by
/// `gate::decide_auto_merge` returning `Merge`.
///
/// `--match-head-commit` makes GitHub itself refuse the merge if `head_sha`
/// has moved since the judgement this attempt is based on — server-side
/// enforcement that cannot be raced between "we decided" and "the API call
/// executes", stronger than any client-side re-check this process could do
/// on its own. `--squash` matches this repo's own established merge
/// convention (CLAUDE.md: "user habit squash merge"; also the only strategy
/// this repo's own PR workflow uses).
fn run_gh_merge(owner: &str, repo: &str, number: i32, head_sha: &str) -> Result<(), String> {
    // Same defense-in-depth guard `github::GitHubHost::fetch_status` applies
    // independently of `parse_pr_url`'s own validation, for the exact same
    // reason: `--repo` here is built the same way (a single joined
    // `owner/repo` argument), so it inherits the exact same confirmed SSRF
    // risk (see `parse_pr_url`'s doc) and gets the exact same guard.
    if owner.contains('/') || repo.contains('/') {
        return Err(format!(
            "refusing to merge a repo slug with an embedded '/' (owner={owner:?}, repo={repo:?}) — this would be reinterpreted as a host override"
        ));
    }
    // A `Ready` verdict can only ever be produced from a SUCCESSFUL snapshot
    // (which always sets a real `head_sha`), so this should be unreachable
    // through the gate above — kept anyway as cheap, independent insurance
    // against ever asking `gh` to match an empty/absent commit.
    if head_sha.is_empty() {
        return Err("refusing to merge: no confirmed head_sha on record".to_string());
    }
    let repo_slug = format!("{owner}/{repo}");
    let out = Command::new("gh")
        .args(build_merge_args(number, &repo_slug, head_sha))
        // Checks run user tooling that a GUI launch's minimal PATH can't
        // resolve (Homebrew/local installs of `gh`) — same reasoning as
        // `github::GitHubHost::fetch_status` / `check::run_check`.
        .env("PATH", crate::detect::tool_path())
        .output();
    let out = match out {
        Ok(o) => o,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err("gh is not installed".to_string())
        }
        Err(e) => return Err(e.to_string()),
    };
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(stderr.trim().to_string());
    }
    Ok(())
}

/// The exact `gh pr merge` argument vector, pulled out as its own pure
/// function so the presence of `--match-head-commit` — this feature's
/// server-side head-consistency enforcement, the mechanism `gate`'s own doc
/// points to instead of a second client-side sha comparison — is
/// independently, directly unit-tested below without ever spawning a
/// process. `run_gh_merge` is the only caller; nothing here talks to `gh`.
fn build_merge_args(number: i32, repo_slug: &str, head_sha: &str) -> Vec<String> {
    vec![
        "pr".to_string(),
        "merge".to_string(),
        number.to_string(),
        "--repo".to_string(),
        repo_slug.to_string(),
        "--squash".to_string(),
        "--match-head-commit".to_string(),
        head_sha.to_string(),
    ]
}

/// A durable, visible timeline record of an auto-merge attempt — the same
/// "system-owned, always part of the record" treatment `lead_chat::commands`
/// already gives an engine switch / a failed quota fail-over. Deliberately
/// `kind: "text"` (a plain system chat bubble), NOT a new dedicated
/// `ChatTimeline` marker kind: this feature's territory explicitly excludes
/// `ChatTimeline.tsx` (a fail-over-marker-copy change is in flight there
/// concurrently — see this PR's own notes), and `kind: "text"` already
/// renders any `{"text": ...}` content as a normal message bubble with zero
/// frontend changes, for either `role`. The message text itself is
/// backend-composed Chinese (dev-English tokens kept) rather than routed
/// through `src/i18n` — the same established exception `judge::notice_text`/
/// `give_up_text` already document and use for backend-composed notices: the
/// backend has no channel to learn the user's UI language at all today, and
/// this is exactly that same class of dynamically-composed system text, not
/// static interactive UI copy (the opt-in TOGGLE's own label/hint/confirm
/// text, which IS static UI copy, goes through `src/i18n/en.ts` + `zh.ts` as
/// usual).
async fn insert_automerge_marker(app: &AppHandle, db: &Db, thread_id: i32, text: &str) {
    let turn_id = repo::next_turn_id(db, thread_id).await.unwrap_or(1);
    let content = serde_json::json!({ "text": text }).to_string();
    match repo::insert_lead_message(db, thread_id, None, turn_id, "system", "text", &content, "complete")
        .await
    {
        Ok(m) => {
            let _ = app.emit(
                crate::lead_chat::engine::EVENT,
                crate::lead_chat::engine::Push::Message { thread_id, message: m },
            );
        }
        Err(e) => eprintln!("[weft][automerge] marker insert failed: {e}"),
    }
}

/// Whether the opt-in auto-merge switch is on. Mirrors `engine_routing::
/// quota_failover_for_db`'s exact fail-closed shape (issue #97/#143's
/// established pattern for an opt-in automation switch): a DB read error
/// (corrupt settings row, a locked/unreachable DB) is NOT treated as "off,
/// proceed with default behavior" implicitly — it is logged and returns
/// `false` explicitly, because "off" and "we could not tell" must never
/// collapse into the same code path for an IRREVERSIBLE action. See
/// `try_auto_merge_enabled` for the underlying `Result`-returning read.
pub async fn auto_merge_enabled(db: &Db) -> bool {
    match try_auto_merge_enabled(db).await {
        Ok(v) => v,
        Err(err) => {
            eprintln!("[weft][automerge] policy read failed; auto-merge disabled: {err}");
            false
        }
    }
}

/// The underlying settings read, for a caller that needs to distinguish "off"
/// from "could not tell" (`get_pr_auto_merge_enabled`'s own test coverage;
/// see `commands::get_quota_failover_enabled`'s identical shape).
pub async fn try_auto_merge_enabled(db: &Db) -> anyhow::Result<bool> {
    Ok(is_enabled(repo::get_setting(db, K_AUTO_MERGE_ENABLED).await?.as_deref()))
}

fn is_enabled(raw: Option<&str>) -> bool {
    matches!(raw.map(str::trim), Some("1") | Some("true"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sea_orm::ConnectionTrait;

    // --- run_gh_merge: guards that must fire before any process spawns ----

    #[test]
    fn run_gh_merge_refuses_an_embedded_slash_before_shelling_out() {
        // Mirrors `github::tests::fetch_status_refuses_an_embedded_slash_
        // before_shelling_out` exactly, including WHY the assertion checks
        // the guard's own message text rather than just success/failure: a
        // real `gh pr merge` failure (auth, not found, ...) could otherwise
        // make a deleted guard look identical to a passing test.
        match run_gh_merge("evil.example.org/ownerx", "repox", 5, "abc123") {
            Err(message) => assert!(message.contains("host override"), "got: {message}"),
            Ok(()) => panic!("expected the embedded-slash guard to fire"),
        }
        match run_gh_merge("owner", "a/b", 5, "abc123") {
            Err(message) => assert!(message.contains("host override"), "got: {message}"),
            Ok(()) => panic!("expected the embedded-slash guard to fire"),
        }
    }

    #[test]
    fn run_gh_merge_refuses_an_empty_head_sha_before_shelling_out() {
        match run_gh_merge("owner", "repo", 5, "") {
            Err(message) => assert!(message.contains("head_sha"), "got: {message}"),
            Ok(()) => panic!("expected the empty-head_sha guard to fire"),
        }
    }

    // --- build_merge_args: the head-consistency enforcement must actually
    // reach the `gh` invocation, not just exist in a doc comment -----------

    #[test]
    fn build_merge_args_always_squashes_and_pins_match_head_commit_to_the_exact_sha() {
        let args = build_merge_args(42, "acme/widgets", "deadbeef");
        assert_eq!(
            args,
            vec!["pr", "merge", "42", "--repo", "acme/widgets", "--squash", "--match-head-commit", "deadbeef"]
        );
        assert!(args.contains(&"--squash".to_string()), "must always squash-merge, matching this repo's convention");
        let idx = args
            .iter()
            .position(|a| a == "--match-head-commit")
            .expect("--match-head-commit must be present — this is GitHub's own server-side enforcement that the merged commit is the one that was judged Ready, and this crate does not re-check it any other way");
        assert_eq!(
            args[idx + 1],
            "deadbeef",
            "the value right after the flag must be the exact judged head_sha, not some other field"
        );
    }

    // --- lifecycle_state_text: exhaustive, always distinguishable ---------

    #[test]
    fn lifecycle_state_text_is_distinct_for_every_variant() {
        let open = lifecycle_state_text(PrLifecycle::Open);
        let merged = lifecycle_state_text(PrLifecycle::Merged);
        let closed = lifecycle_state_text(PrLifecycle::Closed);
        assert_ne!(open, merged);
        assert_ne!(open, closed);
        assert_ne!(merged, closed);
        assert!(merged.contains("merged"));
        assert!(open.contains("open"));
    }

    // --- is_enabled --------------------------------------------------------

    #[test]
    fn is_enabled_recognizes_only_the_explicit_on_values() {
        assert!(is_enabled(Some("1")));
        assert!(is_enabled(Some("true")));
        assert!(!is_enabled(Some("0")));
        assert!(!is_enabled(Some("false")));
        assert!(
            !is_enabled(Some("yes")),
            "unlike the automatic-routing flag (which also accepts on/yes), this stays as narrow as quota_failover's own 1/true-only precedent — appropriate for an irreversible action"
        );
        assert!(!is_enabled(None));
        assert!(!is_enabled(Some("")));
    }

    // --- DB-backed fail-closed behavior (mirrors engine_routing::tests::
    // routing_policy_read_errors_fail_closed) ------------------------------

    #[tokio::test]
    async fn unset_setting_reads_as_disabled() {
        let db = Db::connect("sqlite::memory:").await.unwrap();
        assert!(!auto_merge_enabled(&db).await);
        assert!(!try_auto_merge_enabled(&db).await.unwrap());
    }

    #[tokio::test]
    async fn explicit_setting_values_round_trip() {
        let db = Db::connect("sqlite::memory:").await.unwrap();
        repo::set_setting(&db, K_AUTO_MERGE_ENABLED, "1").await.unwrap();
        assert!(auto_merge_enabled(&db).await);
        repo::set_setting(&db, K_AUTO_MERGE_ENABLED, "0").await.unwrap();
        assert!(!auto_merge_enabled(&db).await);
    }

    #[tokio::test]
    async fn a_db_read_failure_fails_closed_never_silently_enabled() {
        let db = Db::connect("sqlite::memory:").await.unwrap();
        db.0.execute_unprepared("DROP TABLE app_setting").await.unwrap();

        let checked = try_auto_merge_enabled(&db).await;
        assert!(checked.is_err(), "the underlying error must still be observable to a caller that wants it");
        assert!(!auto_merge_enabled(&db).await, "the fail-closed wrapper must return false, never true, on a read error");
    }
}
