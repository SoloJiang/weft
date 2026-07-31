//! GitHub `PrHost` backend: shells out to the user's own `gh` CLI — already
//! authenticated, so this needs zero credential management (the same
//! tradeoff `check.rs` and this repo's CLAUDE.md review workflow already
//! make). Two-layer split mirrors `check.rs`'s `run_check` / `infer_checks`:
//! [`GitHubHost::fetch_status`] does I/O and error classification;
//! [`parse_pr_json`] is a pure function, unit- and mutation-tested against
//! fixture JSON, that never touches a process.

use super::{
    CiStatus, ConflictStatus, HostError, HostKind, PrHost, PrLifecycle, PrSnapshot, PrTarget,
    ReviewStatus,
};
use std::process::Command;

pub struct GitHubHost;

/// `gh pr view --json` field list. Every field here was confirmed live
/// against this repo's own PRs while building this backend (`gh pr view 148
/// --repo SoloJiang/weft --json ...` / PR #146) rather than assumed from
/// memory — see `parse_pr_json`'s doc for the specific gotchas that
/// confirmed.
const JSON_FIELDS: &str = "url,title,state,headRefOid,baseRefName,mergeable,reviewDecision,statusCheckRollup";

impl PrHost for GitHubHost {
    fn kind(&self) -> HostKind {
        HostKind::GitHub
    }

    fn fetch_status(&self, target: &PrTarget) -> Result<PrSnapshot, HostError> {
        // `super::qualified_repo_slug` folds `target.host_base` in (GitHub Enterprise support —
        // Codex review, PR #159 repo.rs:3873: this call used to build a bare OWNER/REPO
        // argument with no host at all, always querying `gh`'s own configured default instead
        // of the recorded install) and refuses an embedded '/' in any of the three inputs
        // before this can ever shell out — the confirmed SSRF this crate's `[HOST/]OWNER/REPO`
        // grammar otherwise allows (see `parse_pr_url`'s doc).
        let repo_slug = super::qualified_repo_slug(&target.host_base, &target.owner, &target.repo)
            .map_err(|message| HostError::Other { message })?;
        let out = Command::new("gh")
            .args(["pr", "view", &target.number.to_string(), "--repo", &repo_slug, "--json", JSON_FIELDS])
            // Checks run user tooling that a GUI launch's minimal PATH can't
            // resolve (Homebrew/local installs of `gh`) — same reasoning as
            // `check::run_check`.
            .env("PATH", crate::detect::tool_path())
            .output();
        let out = match out {
            Ok(o) => o,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(HostError::CliMissing { program: "gh".to_string() })
            }
            Err(e) => return Err(HostError::Other { message: e.to_string() }),
        };
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            return Err(classify_gh_error(&stderr));
        }
        let stdout = String::from_utf8_lossy(&out.stdout);
        parse_pr_json(&stdout).map_err(|message| HostError::Other { message })
    }
}

/// Classify a non-zero `gh` exit's stderr into a specific, honest
/// [`HostError`] instead of a generic failure — so a missing login degrades
/// gracefully with an actionable message rather than reading as "this PR is
/// blocked" (issue #110's explicit "must gracefully degrade AND honestly
/// report" requirement).
fn classify_gh_error(stderr: &str) -> HostError {
    let lower = stderr.to_lowercase();
    // "could not resolve to a pullrequest" is the exact wording confirmed live
    // (`gh pr view 99999 --repo SoloJiang/weft`); "could not find" / "no pull
    // requests found" cover other gh versions/phrasings for the same case.
    if lower.contains("could not resolve")
        || lower.contains("could not find")
        || lower.contains("no pull requests found")
    {
        HostError::NotFound
    } else if lower.contains("gh auth login") || lower.contains("not logged") || lower.contains("authentication") {
        HostError::NotAuthenticated { program: "gh".to_string() }
    } else {
        HostError::Other { message: stderr.trim().to_string() }
    }
}

// --- pure JSON parsing -------------------------------------------------

#[derive(serde::Deserialize, Default)]
struct RawCheck {
    #[serde(default)]
    conclusion: String,
    #[serde(default)]
    status: String,
}

#[derive(serde::Deserialize)]
struct RawPr {
    #[serde(default)]
    url: String,
    #[serde(default)]
    title: String,
    state: String,
    #[serde(rename = "headRefOid", default)]
    head_ref_oid: String,
    #[serde(rename = "baseRefName", default)]
    base_ref_name: String,
    #[serde(default)]
    mergeable: String,
    #[serde(rename = "reviewDecision", default)]
    review_decision: String,
    #[serde(rename = "statusCheckRollup", default)]
    status_check_rollup: Vec<RawCheck>,
}

/// Parse one `gh pr view --json ...` response into a normalized snapshot.
/// Pure — no process, no filesystem — so every branch below is a plain unit
/// test against a fixture string. Gotchas confirmed against this repo's live
/// PRs while building this (not assumed from memory):
///   * `reviewDecision` is `""` for an open, otherwise-healthy, simply
///     not-yet-reviewed PR — NOT a value to treat as "approved".
///   * `mergeable` can be `"UNKNOWN"` even for a currently-open PR right after
///     a push (GitHub hasn't finished computing it yet), and reliably reports
///     it for a MERGED PR (GitHub stops bothering) — either way, "UNKNOWN"
///     must stay `ConflictStatus::Unknown`, never default to `Clean`.
///   * an in-progress check-run reports `conclusion` as an EMPTY STRING, not
///     null/absent (this exact gotcha already bit PR #140 in this repo) — so
///     `""` must mean pending, never "vacuously passing".
fn parse_pr_json(raw: &str) -> Result<PrSnapshot, String> {
    let parsed: RawPr =
        serde_json::from_str(raw).map_err(|e| format!("unexpected gh output: {e}"))?;
    Ok(PrSnapshot {
        head_sha: parsed.head_ref_oid,
        base_ref: parsed.base_ref_name,
        url: parsed.url,
        title: parsed.title,
        lifecycle: lifecycle_of(&parsed.state),
        ci: ci_of(&parsed.status_check_rollup),
        review: review_of(&parsed.review_decision),
        conflict: conflict_of(&parsed.mergeable),
    })
}

fn lifecycle_of(state: &str) -> PrLifecycle {
    match state {
        "MERGED" => PrLifecycle::Merged,
        "CLOSED" => PrLifecycle::Closed,
        // "OPEN" and any unrecognized future value both default open — never
        // silently treat an unrecognized state as "done, stop watching".
        _ => PrLifecycle::Open,
    }
}

fn ci_of(rollup: &[RawCheck]) -> CiStatus {
    if rollup.is_empty() {
        return CiStatus::NotConfigured;
    }
    let mut any_failing = false;
    let mut any_pending = false;
    for c in rollup {
        if c.status != "COMPLETED" || c.conclusion.is_empty() {
            any_pending = true;
            continue;
        }
        match c.conclusion.as_str() {
            "SUCCESS" | "NEUTRAL" | "SKIPPED" => {}
            // FAILURE, CANCELLED, TIMED_OUT, ACTION_REQUIRED, STALE, and any
            // future conclusion we don't yet recognize all count as failing —
            // "全绿" means every rung is an explicit pass, not "not a known
            // failure code".
            _ => any_failing = true,
        }
    }
    if any_failing {
        CiStatus::Failing
    } else if any_pending {
        CiStatus::Pending
    } else {
        CiStatus::Passing
    }
}

fn review_of(decision: &str) -> ReviewStatus {
    match decision {
        "APPROVED" => ReviewStatus::Approved,
        "CHANGES_REQUESTED" => ReviewStatus::ChangesRequested,
        // "REVIEW_REQUIRED" or "" (no review recorded yet/configured) — either
        // way, not a positive approval signal. `unresolved_discussions: None`
        // — this MVP doesn't walk `reviewThreads` (see `ReviewStatus`'s doc),
        // so that sub-signal stays honestly unknown, never a false `Some(false)`.
        _ => ReviewStatus::AwaitingApproval { unresolved_discussions: None },
    }
}

fn conflict_of(mergeable: &str) -> ConflictStatus {
    match mergeable {
        "MERGEABLE" => ConflictStatus::Clean,
        "CONFLICTING" => ConflictStatus::Conflicting,
        _ => ConflictStatus::Unknown {
            reason: "GitHub hasn't finished computing mergeability yet".to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real response shape (`gh pr view 148 --repo SoloJiang/weft --json ...`)
    /// captured while building this backend: a MERGED PR with all-green CI and
    /// `mergeable: "UNKNOWN"` (GitHub stops computing it once merged) and an
    /// empty `reviewDecision`.
    const MERGED_FIXTURE: &str = r#"{"baseRefName":"main","headRefOid":"f4c22ba9d10196f4e010a900ae396f6b119d2d8f","mergeable":"UNKNOWN","reviewDecision":"","state":"MERGED","statusCheckRollup":[{"__typename":"CheckRun","conclusion":"SUCCESS","status":"COMPLETED","name":"lint-and-frontend"},{"__typename":"CheckRun","conclusion":"SUCCESS","status":"COMPLETED","name":"rust-test (ubuntu-latest)"}],"title":"fix(ui): centralize thread activity status","url":"https://github.com/SoloJiang/weft/pull/148"}"#;

    /// Real response shape (`gh pr view 146 --repo SoloJiang/weft --json ...`):
    /// an OPEN, unreviewed, all-green, conflict-free PR.
    const OPEN_CLEAN_FIXTURE: &str = r#"{"baseRefName":"main","headRefOid":"b14597e647e22150c23478f5a516acfcd8dce100","mergeable":"MERGEABLE","reviewDecision":"","state":"OPEN","statusCheckRollup":[{"__typename":"CheckRun","conclusion":"SUCCESS","status":"COMPLETED","name":"lint-and-frontend"}],"title":"fix(ask): let read-only builtins skip the human, on a closed allowlist","url":"https://github.com/SoloJiang/weft/pull/146"}"#;

    #[test]
    fn parses_the_real_merged_fixture() {
        let s = parse_pr_json(MERGED_FIXTURE).unwrap();
        assert_eq!(s.lifecycle, PrLifecycle::Merged);
        assert_eq!(s.ci, CiStatus::Passing);
        assert_eq!(
            s.review,
            ReviewStatus::AwaitingApproval { unresolved_discussions: None },
            "empty reviewDecision is NOT Approved"
        );
        assert_eq!(
            s.conflict,
            ConflictStatus::Unknown {
                reason: "GitHub hasn't finished computing mergeability yet".to_string()
            },
            "mergeable:UNKNOWN on a merged PR must stay Unknown, never default to Clean"
        );
        assert_eq!(s.head_sha, "f4c22ba9d10196f4e010a900ae396f6b119d2d8f");
    }

    #[test]
    fn parses_the_real_open_clean_fixture() {
        let s = parse_pr_json(OPEN_CLEAN_FIXTURE).unwrap();
        assert_eq!(s.lifecycle, PrLifecycle::Open);
        assert_eq!(s.ci, CiStatus::Passing);
        assert_eq!(s.conflict, ConflictStatus::Clean);
    }

    #[test]
    fn empty_status_check_rollup_is_not_configured_not_failing() {
        assert_eq!(ci_of(&[]), CiStatus::NotConfigured);
    }

    #[test]
    fn an_in_progress_checks_empty_conclusion_string_is_pending_not_passing() {
        // The exact PR #140 gotcha this repo already paid for once.
        let running = RawCheck { status: "IN_PROGRESS".to_string(), conclusion: String::new() };
        let done = RawCheck { status: "COMPLETED".to_string(), conclusion: "SUCCESS".to_string() };
        assert_eq!(ci_of(&[done, running]), CiStatus::Pending);
    }

    #[test]
    fn one_failing_check_fails_the_whole_rollup_even_if_others_passed() {
        let ok = RawCheck { status: "COMPLETED".to_string(), conclusion: "SUCCESS".to_string() };
        let bad = RawCheck { status: "COMPLETED".to_string(), conclusion: "FAILURE".to_string() };
        assert_eq!(ci_of(&[ok, bad]), CiStatus::Failing);
    }

    #[test]
    fn failing_takes_priority_over_pending_in_a_mixed_rollup() {
        let running = RawCheck { status: "IN_PROGRESS".to_string(), conclusion: String::new() };
        let bad = RawCheck { status: "COMPLETED".to_string(), conclusion: "FAILURE".to_string() };
        assert_eq!(ci_of(&[running, bad]), CiStatus::Failing);
    }

    #[test]
    fn neutral_and_skipped_conclusions_count_as_passing() {
        let neutral = RawCheck { status: "COMPLETED".to_string(), conclusion: "NEUTRAL".to_string() };
        let skipped = RawCheck { status: "COMPLETED".to_string(), conclusion: "SKIPPED".to_string() };
        assert_eq!(ci_of(&[neutral, skipped]), CiStatus::Passing);
    }

    #[test]
    fn review_decision_variants_map_correctly() {
        assert_eq!(review_of("APPROVED"), ReviewStatus::Approved);
        assert_eq!(review_of("CHANGES_REQUESTED"), ReviewStatus::ChangesRequested);
        assert_eq!(
            review_of("REVIEW_REQUIRED"),
            ReviewStatus::AwaitingApproval { unresolved_discussions: None }
        );
        assert_eq!(review_of(""), ReviewStatus::AwaitingApproval { unresolved_discussions: None });
    }

    #[test]
    fn mergeable_variants_map_correctly() {
        assert_eq!(conflict_of("MERGEABLE"), ConflictStatus::Clean);
        assert_eq!(conflict_of("CONFLICTING"), ConflictStatus::Conflicting);
        assert!(matches!(conflict_of("UNKNOWN"), ConflictStatus::Unknown { .. }));
    }

    #[test]
    fn lifecycle_variants_map_correctly_and_default_open() {
        assert_eq!(lifecycle_of("OPEN"), PrLifecycle::Open);
        assert_eq!(lifecycle_of("CLOSED"), PrLifecycle::Closed);
        assert_eq!(lifecycle_of("MERGED"), PrLifecycle::Merged);
        assert_eq!(lifecycle_of("SOMETHING_NEW_GH_ADDS_LATER"), PrLifecycle::Open);
    }

    #[test]
    fn malformed_json_is_an_error_not_a_panic() {
        assert!(parse_pr_json("not json").is_err());
        assert!(parse_pr_json("").is_err());
    }

    #[test]
    fn classify_gh_error_recognizes_missing_auth() {
        assert_eq!(
            classify_gh_error("To get started with GitHub CLI, please run:  gh auth login"),
            HostError::NotAuthenticated { program: "gh".to_string() }
        );
    }

    #[test]
    fn classify_gh_error_recognizes_not_found() {
        assert_eq!(
            classify_gh_error("GraphQL: Could not resolve to a PullRequest"),
            HostError::NotFound
        );
        assert_eq!(classify_gh_error("no pull requests found"), HostError::NotFound);
    }

    #[test]
    fn classify_gh_error_falls_back_to_other_with_the_message_preserved() {
        assert_eq!(
            classify_gh_error("some unexpected network error"),
            HostError::Other { message: "some unexpected network error".to_string() }
        );
    }

    #[test]
    fn fetch_status_refuses_an_embedded_slash_before_shelling_out() {
        // Defense in depth alongside `host::parse_pr_url`'s own rejection:
        // even a `PrTarget` built some OTHER way (bypassing that parser) must
        // never reach `Command::new("gh")` with an owner/repo that could be
        // reinterpreted as a host override. No process/network call happens
        // in this test — the guard fires before `Command::new` either way.
        //
        // IMPORTANT: this asserts the guard's OWN message content, not just
        // the `HostError::Other` VARIANT. An earlier version of this test
        // asserted only the variant, which is a false-green safety net:
        // `classify_gh_error`'s catch-all ALSO produces `HostError::Other`
        // for an unrecognized real `gh` failure, so with the guard deleted
        // this test stayed green anyway — the UNGUARDED call proceeds to a
        // REAL `gh pr view --repo "evil.example.org/ownerx/repox"`, which an
        // independent review confirmed genuinely reaches the network (an
        // HTTPS POST to `https://evil.example.org/api/graphql`), and still
        // fails with the SAME `HostError::Other` variant — just a different
        // message. Checking the message is what actually distinguishes "the
        // guard fired" from "gh failed some other way", and it's checkable
        // without ever spawning a process (the guard returns before
        // `Command::new` runs).
        let host = GitHubHost;
        let bad_owner = PrTarget {
            host_base: String::new(),
            owner: "evil.example.org/ownerx".to_string(),
            repo: "repox".to_string(),
            number: 5,
        };
        match host.fetch_status(&bad_owner) {
            Err(HostError::Other { message }) => {
                assert!(message.contains("host override"), "got: {message}");
            }
            other => panic!("expected the embedded-slash guard's own error, got {other:?}"),
        }

        let bad_repo = PrTarget {
            host_base: String::new(),
            owner: "owner".to_string(),
            repo: "a/b".to_string(),
            number: 5,
        };
        match host.fetch_status(&bad_repo) {
            Err(HostError::Other { message }) => {
                assert!(message.contains("host override"), "got: {message}");
            }
            other => panic!("expected the embedded-slash guard's own error, got {other:?}"),
        }

        // Codex review, PR #159 repo.rs:3873: `host_base` now reaches this call too (it used
        // to be entirely absent from `PrTarget`) — the guard must cover it just as strictly.
        let bad_host_base = PrTarget {
            host_base: "evil.example.org/extra".to_string(),
            owner: "owner".to_string(),
            repo: "repo".to_string(),
            number: 5,
        };
        match host.fetch_status(&bad_host_base) {
            Err(HostError::Other { message }) => {
                assert!(message.contains("host override"), "got: {message}");
            }
            other => panic!("expected the embedded-slash guard's own error for a smuggled host_base, got {other:?}"),
        }
    }

    #[test]
    fn kind_reports_github() {
        // `fetch_status` itself is deliberately NOT exercised here: it shells
        // out to the real `gh` CLI, which — unlike a nonexistent binary — IS
        // present and often authenticated on both dev machines and GitHub
        // Actions runners, so a test that actually calls it would either hit
        // the network non-deterministically or silently depend on this
        // machine's login state. The I/O/parsing split above (mirroring
        // `check::run_check` / `infer_checks`) exists exactly so the pure half
        // (`parse_pr_json`, `classify_gh_error`, the `*_of` mappers) carries
        // the test weight instead.
        assert_eq!(GitHubHost.kind(), HostKind::GitHub);
    }
}
