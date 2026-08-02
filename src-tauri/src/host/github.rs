//! GitHub `PrHost` backend: shells out to the user's own `gh` CLI — already
//! authenticated, so this needs zero credential management (the same
//! tradeoff `check.rs` and this repo's CLAUDE.md review workflow already
//! make). Two-layer split mirrors `check.rs`'s `run_check` / `infer_checks`:
//! [`GitHubHost::fetch_status`] does I/O and error classification;
//! [`parse_pr_json`] is a pure function, unit- and mutation-tested against
//! fixture JSON, that never touches a process.

use super::{
    CiStatus, ConflictStatus, HostError, HostKind, PrHost, PrLifecycle, PrSnapshot, PrTarget,
    ReviewStatus, ThreadStatus,
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

    /// ORDER IS LOAD-BEARING: threads are read FIRST, the scalar axes
    /// (`reviewDecision`/`statusCheckRollup`/`mergeable`/`state`) LAST.
    ///
    /// Two calls cannot be one instant, so this snapshot always spans a
    /// window, and `automerge::evaluate_row` acts on it immediately
    /// afterwards. `--match-head-commit` closes the part of that window where
    /// the HEAD moves, but a review submitted on an UNCHANGED head is
    /// invisible to it — so whichever axis is read first is the one that can
    /// go stale before the merge fires.
    ///
    /// Reading threads first puts the revocable axes last: a reviewer who
    /// submits CHANGES_REQUESTED (or a check that flips red) while pagination
    /// is still running is now caught by the `gh pr view` that follows, where
    /// the previous order read APPROVED first and then spent the whole
    /// pagination blind to it. The residual runs the other way and is
    /// strictly smaller: a NEW thread opened during the single `gh pr view`
    /// call (~one request) is missed, versus a full paginated walk (many, on
    /// a PR with hundreds of threads). A review submission that opens threads
    /// almost always moves `reviewDecision` too, which this order does see.
    ///
    /// Closing the window entirely would mean one atomic GraphQL query for
    /// every axis — which for >100 threads still needs later pages, and would
    /// mean hand-rolling the `statusCheckRollup` union that `gh pr view
    /// --json` currently normalizes, on the exact path that authorizes an
    /// irreversible merge. The server-side answer the Settings copy already
    /// recommends (branch protection) is what actually makes this atomic;
    /// this order minimizes the client-side exposure meanwhile.
    fn fetch_status(&self, target: &PrTarget) -> Result<PrSnapshot, HostError> {
        fetch_status_ordered(target, fetch_review_threads, fetch_scalar_axes)
    }
}

/// Reader of the thread axis. A plain non-capturing `fn` pointer, the same
/// seam shape `automerge::HostResolver` uses (and for the same stated
/// reason: a property with no way to substitute a fake had zero regression
/// coverage).
type ThreadsReader = fn(&PrTarget) -> ThreadStatus;
/// Reader of everything `gh pr view --json` answers.
type ScalarReader = fn(&PrTarget) -> Result<PrSnapshot, HostError>;

/// The order guarantee from [`GitHubHost::fetch_status`]'s doc, as the only
/// statement of it — extracted so a test can assert it instead of trusting a
/// comment. Restoring the old order (scalars first) makes
/// `threads_are_read_before_the_revocable_axes` fail.
fn fetch_status_ordered(
    target: &PrTarget,
    read_threads: ThreadsReader,
    read_scalars: ScalarReader,
) -> Result<PrSnapshot, HostError> {
    let threads = read_threads(target);
    // A scalar-read failure discards the thread reading with it, on purpose:
    // the probe as a whole failed, and half a snapshot must never be
    // persisted as if it were a reading.
    let mut snapshot = read_scalars(target)?;
    snapshot.threads = threads;
    Ok(snapshot)
}

/// `gh pr view --json` — lifecycle, CI, review decision, conflict.
///
/// Leaves `threads` at whatever [`parse_pr_json`] defaults it to (`Unknown`);
/// [`fetch_status_ordered`] overwrites it. A SECOND call is needed at all
/// because this response has no field exposing thread resolution —
/// `reviewThreads` is GraphQL-only.
fn fetch_scalar_axes(target: &PrTarget) -> Result<PrSnapshot, HostError> {
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

/// The paginated `reviewThreads` read. `first: 100` is GitHub's per-page
/// maximum, and `--paginate` walks `$endCursor` until the server says there
/// is nothing left. Both halves are load-bearing and were confirmed live
/// against this repo's own PR #35 while building this (151 threads: page 1
/// returned exactly 100 with `hasNextPage: true`, page 2 the remaining 51):
/// a bare `first: 100` would have silently dropped 51 threads — the NEWEST
/// ones, i.e. exactly the round most likely to still be unresolved. That is
/// not a hypothetical: it is a bug this repo has already shipped once.
const THREADS_QUERY: &str = "\
query($owner: String!, $repo: String!, $number: Int!, $endCursor: String) {
  repository(owner: $owner, name: $repo) {
    pullRequest(number: $number) {
      reviewThreads(first: 100, after: $endCursor) {
        nodes { isResolved }
        pageInfo { hasNextPage endCursor }
      }
    }
  }
}";

/// Read every review thread and reduce it to one [`ThreadStatus`]. Returns
/// the status rather than a `Result` because EVERY failure mode here has the
/// same, single correct answer — `Unknown` — and making that structurally
/// unavoidable is the point: see [`parse_review_threads_json`] for why a
/// zero-valued fallback is the specific outcome this function is designed to
/// make unreachable.
fn fetch_review_threads(target: &PrTarget) -> ThreadStatus {
    let hostname = match super::api_hostname(&target.host_base, &target.owner, &target.repo) {
        Ok(h) => h,
        Err(message) => return ThreadStatus::Unknown { reason: message },
    };
    let mut cmd = Command::new("gh");
    cmd.args(["api", "graphql", "--paginate"]);
    // `gh api` has no `--repo`, so the recorded host has to arrive by its own
    // flag or the call silently queries `gh`'s configured default instead —
    // the exact drift `PrTarget`'s doc records as a real bug on the other
    // read path.
    if let Some(h) = &hostname {
        cmd.args(["--hostname", h]);
    }
    cmd.args(["-f", &format!("owner={}", target.owner)])
        .args(["-f", &format!("repo={}", target.repo)])
        // `-F` (typed) for the Int, `-f` (always a raw string) for the two
        // string variables: `-F` would coerce an all-digit owner/repo name
        // into a number and the query would reject it as the wrong type.
        .args(["-F", &format!("number={}", target.number)])
        .args(["-f", &format!("query={THREADS_QUERY}")])
        .env("PATH", crate::detect::tool_path());
    let out = match cmd.output() {
        Ok(o) => o,
        Err(e) => return ThreadStatus::Unknown { reason: format!("could not run gh: {e}") },
    };
    if !out.status.success() {
        // A signal-killed `gh` writes no stderr at all, and an empty reason is
        // not merely unhelpful: `last_error == ""` is that column's sentinel
        // for "no error", so a blank one would leave a row whose failure
        // streak is climbing while its error column reads clean (Codex review
        // round 3 P2). `store::repo::apply_pull_request_snapshot` refuses a
        // blank reason as a backstop; this makes the message actually useful.
        let stderr = String::from_utf8_lossy(&out.stderr);
        return ThreadStatus::Unknown {
            reason: threads_failure_reason(&stderr, &out.status.to_string()),
        };
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    match parse_review_threads_json(&stdout) {
        Ok(status) => status,
        Err(reason) => ThreadStatus::Unknown { reason },
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
        // `gh pr view --json` carries no thread-resolution field at all, so
        // this response simply cannot answer that axis. `Unknown` rather than
        // `Unchecked`: the latter reads as vacuously clear downstream, so a
        // future caller that parsed a PR and forgot the separate
        // `fetch_review_threads` call would silently get an all-clear it
        // never earned. `fetch_status` overwrites this immediately.
        threads: ThreadStatus::Unknown {
            reason: "review threads have not been read yet".to_string(),
        },
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
        // way, not a positive approval signal.
        _ => ReviewStatus::AwaitingApproval,
    }
}

// --- review threads ----------------------------------------------------

#[derive(serde::Deserialize)]
struct RawThreadNode {
    #[serde(rename = "isResolved")]
    is_resolved: bool,
}

#[derive(serde::Deserialize)]
struct RawPageInfo {
    #[serde(rename = "hasNextPage")]
    has_next_page: bool,
}

#[derive(serde::Deserialize)]
struct RawReviewThreads {
    nodes: Vec<RawThreadNode>,
    #[serde(rename = "pageInfo")]
    page_info: RawPageInfo,
}

#[derive(serde::Deserialize)]
struct RawThreadsPullRequest {
    #[serde(rename = "reviewThreads")]
    review_threads: RawReviewThreads,
}

#[derive(serde::Deserialize)]
struct RawThreadsRepository {
    #[serde(rename = "pullRequest")]
    pull_request: Option<RawThreadsPullRequest>,
}

#[derive(serde::Deserialize)]
struct RawThreadsData {
    repository: Option<RawThreadsRepository>,
}

#[derive(serde::Deserialize)]
struct RawThreadsPage {
    data: Option<RawThreadsData>,
    #[serde(default)]
    errors: Vec<serde_json::Value>,
}

/// The diagnostic for a non-zero `gh api graphql` exit. Pure, so the
/// empty-stderr branch is a unit test rather than a code path that only
/// appears when someone signals the process.
///
/// `gh` killed by a signal writes no stderr at all, and an empty reason is
/// not merely unhelpful: `last_error == ""` is that column's sentinel for "no
/// error", so a blank one produces a row whose failure streak is climbing
/// while its error column reads clean (Codex review round 3 P2).
/// `store::repo::apply_pull_request_snapshot` refuses a blank reason as a
/// structural backstop; this is what makes the message actually useful, by
/// falling back to the exit status the caller does still have.
fn threads_failure_reason(stderr: &str, status: &str) -> String {
    let stderr = stderr.trim();
    if stderr.is_empty() {
        return format!("gh api graphql failed without writing any diagnostic ({status})");
    }
    stderr.to_string()
}

/// Reduce a `gh api graphql --paginate` response to one [`ThreadStatus`].
/// Pure — no process, no filesystem — so every branch below is a plain unit
/// test against a fixture string.
///
/// **The output is a stream of CONCATENATED JSON objects, not one per line
/// and not an array.** Confirmed live while building this: two pages arrive
/// as `…}}}}}}{"data":…` with no separator and no newline between them.
/// Parsing it by splitting on newlines and swallowing the failures is not a
/// hypothetical mistake — it is the exact bug this repo shipped once before
/// (a monitor reported "0 unresolved" for two consecutive rounds while 5 then
/// 7 findings sat open, and that zero was reported onward as "ready to
/// merge"). [`serde_json::Deserializer::into_iter`] decodes the stream
/// properly, which is what makes that class of mistake unavailable here
/// rather than merely discouraged.
///
/// Every failure returns `Err`, which the caller turns into
/// `ThreadStatus::Unknown`. NOTHING here falls back to a count of zero,
/// because a silent zero is indistinguishable from a genuine all-clear and
/// errs in the single direction that would let an unattended merge fire over
/// open review threads. That includes three cases a naive reader would treat
/// as success:
///   * an EMPTY stream (`gh` exited 0 having printed nothing) — zero pages is
///     not zero threads;
///   * a page after the first that fails to decode — a partial count is not a
///     count;
///   * a stream whose LAST page still reports `hasNextPage: true` — pagination
///     stopped early (a rate limit, a truncated pipe), so the threads most
///     likely to be unresolved are precisely the ones missing. This check is
///     what makes the pagination contract verified rather than assumed.
///
/// An OUTDATED thread (the code under it changed) still counts when it is
/// unresolved. That is not leniency traded away: GitHub does not auto-resolve
/// those, and its own "Require conversation resolution before merging" branch
/// protection blocks on them too — so counting them keeps this gate aligned
/// with the server-side rule it is meant to anticipate, rather than quietly
/// admitting a class of open thread the host itself would refuse.
fn parse_review_threads_json(raw: &str) -> Result<ThreadStatus, String> {
    let mut unresolved: u32 = 0;
    let mut pages = 0usize;
    let mut last_has_next = false;
    for page in serde_json::Deserializer::from_str(raw).into_iter::<RawThreadsPage>() {
        let page = page.map_err(|e| format!("reviewThreads page {} did not parse: {e}", pages + 1))?;
        if let Some(first) = page.errors.first() {
            return Err(format!("GraphQL returned an error: {first}"));
        }
        let threads = page
            .data
            .and_then(|d| d.repository)
            .and_then(|r| r.pull_request)
            .map(|p| p.review_threads)
            .ok_or_else(|| format!("reviewThreads page {} has no repository.pullRequest", pages + 1))?;
        unresolved += threads.nodes.iter().filter(|n| !n.is_resolved).count() as u32;
        last_has_next = threads.page_info.has_next_page;
        pages += 1;
    }
    if pages == 0 {
        return Err("gh returned no reviewThreads pages at all".to_string());
    }
    if last_has_next {
        return Err("reviewThreads pagination ended while the server still reported another page — the read is incomplete".to_string());
    }
    if unresolved == 0 {
        Ok(ThreadStatus::AllResolved)
    } else {
        Ok(ThreadStatus::Unresolved { count: unresolved })
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
            ReviewStatus::AwaitingApproval,
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
            ReviewStatus::AwaitingApproval
        );
        assert_eq!(review_of(""), ReviewStatus::AwaitingApproval);
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

    // --- read ordering ----------------------------------------------------

    thread_local! {
        /// Which reader ran, in order. Thread-local rather than a process
        /// static so no serialization lock is needed between parallel tests.
        static ORDER: std::cell::RefCell<Vec<&'static str>> = const { std::cell::RefCell::new(Vec::new()) };
    }

    fn recording_threads_reader(_: &PrTarget) -> ThreadStatus {
        ORDER.with(|o| o.borrow_mut().push("threads"));
        ThreadStatus::AllResolved
    }

    fn recording_scalar_reader(_: &PrTarget) -> Result<PrSnapshot, HostError> {
        ORDER.with(|o| o.borrow_mut().push("scalars"));
        parse_pr_json(OPEN_CLEAN_FIXTURE)
            .map_err(|message| HostError::Other { message })
    }

    fn failing_scalar_reader(_: &PrTarget) -> Result<PrSnapshot, HostError> {
        ORDER.with(|o| o.borrow_mut().push("scalars"));
        Err(HostError::NotFound)
    }

    fn probe_target() -> PrTarget {
        PrTarget {
            host_base: String::new(),
            owner: "acme".to_string(),
            repo: "widgets".to_string(),
            number: 1,
        }
    }

    /// The safety property from `fetch_status`'s doc, asserted rather than
    /// merely asserted-in-prose: the axes a reviewer can REVOKE
    /// (`reviewDecision`, CI) are read LAST, so a revocation landing during
    /// thread pagination is still seen before the merge decision. Two shell
    /// calls cannot be one instant; this is what decides which side of the
    /// window is exposed. Swap the two lines in `fetch_status_ordered` and
    /// this goes red.
    #[test]
    fn threads_are_read_before_the_revocable_axes() {
        ORDER.with(|o| o.borrow_mut().clear());
        let snapshot =
            fetch_status_ordered(&probe_target(), recording_threads_reader, recording_scalar_reader)
                .unwrap();
        assert_eq!(
            ORDER.with(|o| o.borrow().clone()),
            vec!["threads", "scalars"],
            "reviewDecision/CI must be the freshest thing before the merge decision"
        );
        assert_eq!(
            snapshot.threads,
            ThreadStatus::AllResolved,
            "the thread reading must survive onto the snapshot the scalar read produced"
        );
    }

    /// A failed scalar read is a failed PROBE — the thread reading taken
    /// moments earlier must not be persisted on its own as if it were a
    /// snapshot.
    #[test]
    fn a_failed_scalar_read_discards_the_thread_reading_with_it() {
        ORDER.with(|o| o.borrow_mut().clear());
        let result =
            fetch_status_ordered(&probe_target(), recording_threads_reader, failing_scalar_reader);
        assert!(matches!(result, Err(HostError::NotFound)));
        assert_eq!(
            ORDER.with(|o| o.borrow().clone()),
            vec!["threads", "scalars"],
            "the thread read still happened first — it is its RESULT that is dropped"
        );
    }

    /// The SSRF guard on the THREADS path specifically.
    ///
    /// `fetch_status_refuses_an_embedded_slash_before_shelling_out` cannot
    /// cover this: it asserts on the `Err` that `fetch_scalar_axes` produces,
    /// so it passes whether or not `fetch_review_threads` checked anything —
    /// and since the reorder, the threads read is the one that runs FIRST,
    /// i.e. the first thing that could shell out with a smuggled host. Its
    /// refusal is a silent `Unknown` rather than an `Err`, so it needs its
    /// own assertion or the guard is only covered by `api_hostname`'s unit
    /// test, never by the call site actually using it.
    ///
    /// Reaching `Command::new` here would run the operator's real `gh`.
    #[test]
    fn the_threads_read_refuses_an_embedded_slash_before_shelling_out() {
        for (host_base, owner, repo) in [
            ("evil.example.org/extra", "acme", "widgets"),
            ("", "evil.example.org/acme", "widgets"),
            ("", "acme", "evil.example.org/widgets"),
        ] {
            let target = PrTarget {
                host_base: host_base.to_string(),
                owner: owner.to_string(),
                repo: repo.to_string(),
                number: 1,
            };
            match fetch_review_threads(&target) {
                ThreadStatus::Unknown { reason } => assert!(
                    reason.contains("host override"),
                    "must refuse for the guard's reason, not some incidental \
                     failure that would also appear if it had shelled out: {reason}"
                ),
                other => panic!(
                    "expected a refusal before any process spawned for \
                     host_base={host_base:?} owner={owner:?} repo={repo:?}, got {other:?}"
                ),
            }
        }
    }

    /// A failure that wrote no stderr must still say something — a blank
    /// reason is indistinguishable from "no error" in the column it lands in.
    #[test]
    fn a_thread_query_failure_without_stderr_still_carries_a_diagnostic() {
        for blank in ["", "   ", "\n\t"] {
            let reason = threads_failure_reason(blank, "signal: 9 (SIGKILL)");
            assert!(!reason.trim().is_empty(), "stderr={blank:?} produced a blank reason");
            assert!(
                reason.contains("signal: 9"),
                "must fall back to the exit status the caller still has: {reason}"
            );
        }
        // Real stderr is passed through, trimmed, and does NOT get the
        // fallback bolted on.
        let reason = threads_failure_reason("  HTTP 403: Resource not accessible\n", "exit status: 1");
        assert_eq!(reason, "HTTP 403: Resource not accessible");
    }

    // --- reviewThreads: the pagination + parsing trap ---------------------

    /// Build a page exactly the way the live API returns one.
    fn page(resolved: usize, unresolved: usize, has_next: bool) -> String {
        let mut nodes: Vec<&str> = Vec::new();
        nodes.extend(std::iter::repeat_n(r#"{"isResolved":true}"#, resolved));
        nodes.extend(std::iter::repeat_n(r#"{"isResolved":false}"#, unresolved));
        format!(
            r#"{{"data":{{"repository":{{"pullRequest":{{"reviewThreads":{{"nodes":[{}],"pageInfo":{{"hasNextPage":{},"endCursor":"Y3Vyc29yOnYyOpK0"}}}}}}}}}}}}"#,
            nodes.join(","),
            has_next
        )
    }

    /// The shape captured live from `gh api graphql --paginate` against this
    /// repo's own PR #35 (151 threads over two pages): the page objects are
    /// CONCATENATED with no separator and no newline — `…}}}}}}{"data":…`.
    /// This is the fixture the rest of these tests depend on, so the property
    /// that makes them meaningful is asserted here rather than assumed: a
    /// future edit that "tidies" the fixture onto separate lines would
    /// otherwise silently turn a real regression test into a vacuous one.
    #[test]
    fn the_paginated_stream_really_is_concatenated_with_no_separator() {
        let stream = format!("{}{}", page(100, 0, true), page(51, 0, false));
        assert!(!stream.contains('\n'), "no newline separates gh's pages");
        assert!(stream.contains("}}{\"data\""), "pages abut directly: {stream:.80}");
        assert_eq!(parse_review_threads_json(&stream).unwrap(), ThreadStatus::AllResolved);
    }

    /// A count must span EVERY page. `first: 100` alone would have seen only
    /// the first page here — and GitHub returns threads oldest-first, so the
    /// ones it drops are the newest, i.e. the round most likely still open.
    #[test]
    fn unresolved_threads_on_a_later_page_are_counted() {
        let stream = format!("{}{}", page(100, 0, true), page(48, 3, false));
        assert_eq!(
            parse_review_threads_json(&stream).unwrap(),
            ThreadStatus::Unresolved { count: 3 },
            "a page-2-only finding must not vanish"
        );
    }

    /// Pagination that stopped while the server still had more is a
    /// TRUNCATED read, not a small one. Without this check the caller would
    /// get a confident undercount built from whichever pages did arrive.
    #[test]
    fn a_final_page_still_reporting_has_next_page_is_unknown_not_a_count() {
        let stream = format!("{}{}", page(100, 0, true), page(51, 0, true));
        let err = parse_review_threads_json(&stream).unwrap_err();
        assert!(err.contains("incomplete"), "got: {err}");
    }

    /// The PR #126 recurrence, reproduced: a page in the stream that will not
    /// decode. Swallowing it (`except: continue`) yielded a confident
    /// "0 unresolved" for two consecutive rounds while findings sat open.
    ///
    /// The fixture is deliberately a COMPLETE page (`hasNextPage: false`)
    /// followed by an undecodable fragment, not a truncated final page. That
    /// shape is what isolates this guard: with a truncated final page the
    /// pagination check fires too, so the test would stay green even with
    /// error-swallowing restored, and would have been pinning nothing. Here
    /// the pagination check is satisfied, so refusing is this guard's doing
    /// alone — a mutation run confirmed the weaker fixture, then this one.
    #[test]
    fn a_page_that_fails_to_decode_is_unknown_not_a_partial_count() {
        let complete_then_garbage = format!("{}{{\"data\":{{\"repository\":", page(9, 0, false));
        assert!(
            parse_review_threads_json(&complete_then_garbage).is_err(),
            "an undecodable page must not be skipped over to report a confident count \
             from whichever pages did decode"
        );
        // The truncated-final-page shape is refused too (belt and braces —
        // the pagination check owns that one).
        let truncated = format!("{}{{\"data\":{{\"repository\":", page(100, 0, true));
        assert!(parse_review_threads_json(&truncated).is_err());
    }

    /// Zero pages is not zero threads.
    #[test]
    fn an_empty_or_whitespace_stream_is_unknown_not_all_resolved() {
        for raw in ["", "   ", "\n"] {
            let err = parse_review_threads_json(raw).unwrap_err();
            assert!(err.contains("no reviewThreads pages"), "raw={raw:?} got: {err}");
        }
    }

    /// GraphQL reports partial failures in-band, in a response that is itself
    /// perfectly well-formed JSON — in two shapes. A field-level failure
    /// carries `data: null`; a query-level one (validation, a bad token)
    /// omits `data` ENTIRELY. Both must reach the `errors` branch and keep
    /// the server's own diagnostic, rather than the second one dying as a
    /// generic decode failure that says nothing useful. Serde's implicit
    /// treatment of `Option` fields as optional is what makes the second
    /// shape decode at all, so it is pinned here rather than assumed.
    #[test]
    fn an_in_band_graphql_error_is_unknown_even_though_the_json_parses() {
        for raw in [
            r#"{"data":null,"errors":[{"message":"Resource not accessible"}]}"#,
            r#"{"errors":[{"message":"Resource not accessible"}]}"#,
        ] {
            let err = parse_review_threads_json(raw).unwrap_err();
            assert!(
                err.contains("Resource not accessible"),
                "raw={raw} must surface the server's own message, got: {err}"
            );
        }
    }

    /// An outdated thread is still an open thread. GitHub does not
    /// auto-resolve them and its own conversation-resolution branch
    /// protection blocks on them, so neither does this.
    #[test]
    fn an_outdated_but_unresolved_thread_still_counts() {
        let raw = r#"{"data":{"repository":{"pullRequest":{"reviewThreads":{"nodes":[{"isResolved":false,"isOutdated":true}],"pageInfo":{"hasNextPage":false,"endCursor":null}}}}}}"#;
        assert_eq!(
            parse_review_threads_json(raw).unwrap(),
            ThreadStatus::Unresolved { count: 1 }
        );
    }

    /// A well-formed response for a PR that isn't there must not read as
    /// "no threads, all clear".
    #[test]
    fn a_missing_pull_request_is_unknown_not_all_resolved() {
        let raw = r#"{"data":{"repository":{"pullRequest":null}}}"#;
        let err = parse_review_threads_json(raw).unwrap_err();
        assert!(err.contains("pullRequest"), "got: {err}");
    }

    /// The positive direction, so none of the above can pass by refusing
    /// everything: a genuine all-clear is reported as such, and a genuine
    /// count is exact.
    #[test]
    fn a_single_complete_page_reports_the_exact_state() {
        assert_eq!(parse_review_threads_json(&page(7, 0, false)).unwrap(), ThreadStatus::AllResolved);
        assert_eq!(
            parse_review_threads_json(&page(7, 2, false)).unwrap(),
            ThreadStatus::Unresolved { count: 2 }
        );
        assert_eq!(
            parse_review_threads_json(&page(0, 0, false)).unwrap(),
            ThreadStatus::AllResolved,
            "a PR with no threads at all is genuinely all-clear"
        );
    }

    /// `parse_pr_json` cannot answer this axis, and must say so rather than
    /// leaving a value that reads as clear — `fetch_status` overwrites it,
    /// but a future caller that forgot to must not get a free pass.
    #[test]
    fn a_pr_json_parse_leaves_threads_unknown_never_unchecked_or_resolved() {
        let s = parse_pr_json(OPEN_CLEAN_FIXTURE).unwrap();
        assert!(
            matches!(s.threads, ThreadStatus::Unknown { .. }),
            "got {:?} — Unchecked and AllResolved both read as clear downstream",
            s.threads
        );
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
