//! Host abstraction (issue #110 T1): a normalized, host-agnostic view of a
//! PR/MR's review state, computed ONCE here and never re-derived from raw
//! host vocabulary anywhere past this module. A `PrHost` implementation
//! translates its host's native shapes — GitHub's check-run `conclusion` /
//! `reviewDecision` / `mergeable`; a future GitLab backend's pipeline
//! `status` / discussion `resolved` / `merge_status` — into the types below.
//! Nothing outside `host::github` (and, later, a `host::gitlab`) may branch on
//! a host-native string; the DB row, the monitor, and the judgement in
//! [`judge`] only ever see [`CiStatus`] / [`ReviewStatus`] / [`ConflictStatus`].
//!
//! MVP ships only [`github::GitHubHost`] (via the `gh` CLI, inheriting the
//! user's own login — zero credential management, same tradeoff the repo's
//! CLAUDE.md review workflow and `check.rs` already make). [`HostKind::GitLab`]
//! is a real, exhaustively-matched variant from day one — the shape a
//! self-hosted GitLab backend (`glab` CLI, for the same zero-credential
//! reason) plugs into later — but [`resolve_host`] returns
//! `HostError::Unsupported` for it until that backend exists. Adding a third
//! `HostKind` is a compile error here until every exhaustive match in this
//! module (dispatch, verdicts in `judge`) is updated for it.
//!
//! Config model note (issue #110's explicit constraint): GitHub is
//! effectively one hostname (github.com / a GHE alias), but every self-hosted
//! GitLab instance has ITS OWN hostname — so host binding can never be a
//! single global setting. This crate resolves it PER ROW instead of even
//! per-repo: `host_kind`/`host_base` are recorded once at registration time
//! (`register_pr`, from the URL the agent already has — see [`parse_pr_url`]),
//! not re-inferred from a repo's `origin` remote later. That sidesteps
//! guessing a hostname's kind from string content (`gitlab.mycompany.com` vs
//! an internal domain with no recognizable name) entirely: the caller who
//! just ran `gh pr create` / `glab mr create` already KNOWS which one it used.

pub mod github;
pub mod judge;
pub mod monitor;

use serde::{Deserialize, Serialize};

/// Which `PrHost` backend a tracked row is bound to. See the module doc for
/// why this is recorded per-row rather than inferred from a global setting.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostKind {
    GitHub,
    GitLab,
}

impl HostKind {
    pub fn as_str(self) -> &'static str {
        match self {
            HostKind::GitHub => "github",
            HostKind::GitLab => "gitlab",
        }
    }

    /// Parse the `pull_request.host_kind` column value. Unknown text has no
    /// safe default to fall back to (guessing GitHub for a typo'd value would
    /// silently mis-dispatch), so this returns `None` rather than guessing.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "github" => Some(HostKind::GitHub),
            "gitlab" => Some(HostKind::GitLab),
            _ => None,
        }
    }

    /// The UI-visible native noun for this host's change-unit — issue #110's
    /// explicit requirement that the interface use each host's OWN vocabulary
    /// ("Pull request" for GitHub, "Merge request" for GitLab) even though the
    /// state machine above stays neutral.
    pub fn native_noun(self) -> &'static str {
        match self {
            HostKind::GitHub => "Pull request",
            HostKind::GitLab => "Merge request",
        }
    }

    /// Short form for compact references (`PR #12` / `MR #12`).
    pub fn native_abbrev(self) -> &'static str {
        match self {
            HostKind::GitHub => "PR",
            HostKind::GitLab => "MR",
        }
    }
}

/// One PR/MR's identity + where to find it, host-agnostic. A `PrHost` maps
/// this into its own CLI invocation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrTarget {
    pub owner: String,
    pub repo: String,
    pub number: i32,
}

/// Lifecycle of the change unit itself — distinct from merge READINESS, which
/// only makes sense while `Open` (see `judge::notice_text`'s caller in
/// `monitor`, which treats a non-`Open` row as "nothing left to judge").
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrLifecycle {
    Open,
    Closed,
    Merged,
}

impl PrLifecycle {
    pub fn as_str(self) -> &'static str {
        match self {
            PrLifecycle::Open => "open",
            PrLifecycle::Closed => "closed",
            PrLifecycle::Merged => "merged",
        }
    }
}

/// CI signal, normalized from either host's rollup. `Unknown` means the PROBE
/// couldn't tell (missing CLI, auth failure, transient error) — this must
/// NEVER collapse into `NotConfigured` or `Failing`: "we couldn't check" and
/// "we checked and it's clean/broken" are different, both legitimate, facts
/// (issue #107's probe-failed-vs-probe-succeeded-but-empty lesson).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum CiStatus {
    Unknown { reason: String },
    /// No CI configured at all (an empty rollup) — vacuously non-blocking,
    /// same spirit as `check::infer_checks` never inventing a runner.
    NotConfigured,
    Pending,
    Passing,
    Failing,
}

/// Review signal. MVP scope note: normalized from GitHub's own aggregate
/// `reviewDecision` (which already folds in required-reviewer/CODEOWNERS
/// rules), NOT a per-thread `reviewThreads.isResolved` walk — that needs
/// paginated GraphQL with a documented prior pagination bug in this very repo
/// (dropping the newest/unresolved threads past 100), which is more machinery
/// than an MVP host-agnostic signal needs. A repo-specific convention layered
/// on top of this (e.g. a review-bot's 👍 reaction counting as "all clear") is
/// exactly that — repo-specific — and stays the calling agent's job per its
/// own CLAUDE.md, not this neutral state machine's.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ReviewStatus {
    Unknown { reason: String },
    ChangesRequested,
    /// Not yet reviewed / no decision recorded (GitHub's `REVIEW_REQUIRED` or
    /// an empty `reviewDecision` — confirmed live against this repo's own PRs
    /// while building this: an open, otherwise-healthy PR reports `""`).
    AwaitingReview,
    Approved,
}

/// Conflict signal, normalized from either host's mergeability computation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ConflictStatus {
    /// The host hasn't finished computing mergeability yet — a real, common,
    /// transient GitHub state (confirmed live: even a MERGED PR can report
    /// `mergeable: "UNKNOWN"` once GitHub stops bothering to compute it).
    Unknown { reason: String },
    Clean,
    Conflicting,
}

/// The `truly mergeable` bar (this repo's CLAUDE.md "GitHub Remote Review
/// Workflow" section), turned into code instead of prose: CI green × review
/// clear/approved × no conflict. See `judge::merge_readiness` for the
/// (mutation-tested) derivation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum MergeReadiness {
    Ready,
    Blocked { reasons: Vec<String> },
    /// At least one axis couldn't be determined — a DIFFERENT fact from
    /// `Blocked`, and never silently folded into `Ready` or `Blocked` (the
    /// same probe-failed-vs-empty honesty `CiStatus::Unknown` requires).
    Indeterminate { reasons: Vec<String> },
}

/// One normalized snapshot of a PR/MR's state, as reported by a `PrHost`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrSnapshot {
    pub head_sha: String,
    pub base_ref: String,
    pub url: String,
    pub title: String,
    pub lifecycle: PrLifecycle,
    pub ci: CiStatus,
    pub review: ReviewStatus,
    pub conflict: ConflictStatus,
}

/// Why a fetch failed — always surfaced honestly (`monitor` never treats this
/// as "nothing changed" or silently substitutes an empty/clean state).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostError {
    /// This `HostKind` has no backend implementation yet (GitLab, for now).
    Unsupported,
    CliMissing { program: String },
    NotAuthenticated { program: String },
    /// The PR/MR itself couldn't be found (deleted, wrong number, wrong repo).
    NotFound,
    /// Any other failure (network, malformed response, unexpected exit). The
    /// message is diagnostic only — never parsed back by a caller.
    Other { message: String },
}

impl HostError {
    /// One-line diagnostic for `pull_request.last_error` / logs.
    pub fn message(&self) -> String {
        match self {
            HostError::Unsupported => "this host backend isn't implemented yet".to_string(),
            HostError::CliMissing { program } => format!("{program} is not installed"),
            HostError::NotAuthenticated { program } => format!("{program} is not logged in"),
            HostError::NotFound => {
                "not found on the host (deleted, wrong number, or wrong repo)".to_string()
            }
            HostError::Other { message } => message.clone(),
        }
    }
}

/// A host backend: fetch one PR/MR's normalized state. Implementations MUST
/// NOT leak host-native strings/shapes past the return type here.
pub trait PrHost: Send + Sync {
    fn kind(&self) -> HostKind;
    fn fetch_status(&self, target: &PrTarget) -> Result<PrSnapshot, HostError>;
}

/// Exhaustive dispatch from a recorded [`HostKind`] to its backend. Adding a
/// third `HostKind` variant is a compile error here until this match handles
/// it — the same discriminated-state discipline CLAUDE.md requires of UI
/// code, applied to host dispatch.
pub fn resolve_host(kind: HostKind) -> Result<Box<dyn PrHost>, HostError> {
    match kind {
        HostKind::GitHub => Ok(Box::new(github::GitHubHost)),
        HostKind::GitLab => Err(HostError::Unsupported),
    }
}

/// The pieces of a PR/MR web URL `register_pr` needs, extracted so the
/// calling agent only has to hand over the URL it already has (from `gh pr
/// create` / a future `glab mr create`'s own output) instead of separately
/// asserting host/owner/repo/number itself (which could drift from the URL).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrUrlParts {
    pub host_kind: HostKind,
    pub host_base: String,
    pub owner: String,
    pub repo: String,
    pub number: i32,
}

/// Parse a PR/MR web URL into its host-agnostic parts. Recognizes GitHub's
/// `.../<owner>/<repo>/pull/<n>` and GitLab's `.../<namespace>/<project>/-/
/// merge_requests/<n>` (namespace may itself contain `/` for a nested
/// subgroup — the LAST segment before `/-/merge_requests/` is the project,
/// everything before it is the namespace). Returns `None` for anything else
/// rather than guessing.
pub fn parse_pr_url(url: &str) -> Option<PrUrlParts> {
    let url = url.trim();
    let after_scheme = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    let (host, rest) = after_scheme.split_once('/')?;
    if host.is_empty() {
        return None;
    }

    if let Some(idx) = rest.find("/pull/") {
        let (path, tail) = rest.split_at(idx);
        let number = parse_leading_number(tail.trim_start_matches("/pull/"))?;
        let (owner, repo) = path.split_once('/')?;
        if owner.is_empty() || repo.is_empty() {
            return None;
        }
        return Some(PrUrlParts {
            host_kind: HostKind::GitHub,
            host_base: host.to_string(),
            owner: owner.to_string(),
            repo: repo.to_string(),
            number,
        });
    }

    if let Some(idx) = rest.find("/-/merge_requests/") {
        let (path, tail) = rest.split_at(idx);
        let number = parse_leading_number(tail.trim_start_matches("/-/merge_requests/"))?;
        let (owner, repo) = path.rsplit_once('/')?;
        if owner.is_empty() || repo.is_empty() {
            return None;
        }
        return Some(PrUrlParts {
            host_kind: HostKind::GitLab,
            host_base: host.to_string(),
            owner: owner.to_string(),
            repo: repo.to_string(),
            number,
        });
    }

    None
}

/// The number at the start of a path tail, stopping at the next `/` (or
/// end-of-string) — tolerates a trailing `/files`, `/commits`, `#issuecomment-…`
/// etc. that GitHub/GitLab sometimes append.
fn parse_leading_number(tail: &str) -> Option<i32> {
    let digits: String = tail.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    digits.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_kind_round_trips_through_its_string_form() {
        assert_eq!(HostKind::parse("github"), Some(HostKind::GitHub));
        assert_eq!(HostKind::parse("gitlab"), Some(HostKind::GitLab));
        assert_eq!(HostKind::parse("bitbucket"), None, "unknown text must not guess a default");
        assert_eq!(HostKind::GitHub.as_str(), "github");
        assert_eq!(HostKind::GitLab.as_str(), "gitlab");
    }

    #[test]
    fn native_terminology_is_host_specific() {
        assert_eq!(HostKind::GitHub.native_noun(), "Pull request");
        assert_eq!(HostKind::GitHub.native_abbrev(), "PR");
        assert_eq!(HostKind::GitLab.native_noun(), "Merge request");
        assert_eq!(HostKind::GitLab.native_abbrev(), "MR");
    }

    #[test]
    fn resolve_host_dispatches_github_and_marks_gitlab_unsupported() {
        assert_eq!(resolve_host(HostKind::GitHub).unwrap().kind(), HostKind::GitHub);
        // `Box<dyn PrHost>` isn't `Debug`, so `.unwrap_err()` can't be used here
        // (it needs the Ok side to be Debug too) — match explicitly instead.
        match resolve_host(HostKind::GitLab) {
            Err(HostError::Unsupported) => {}
            Ok(_) => panic!("expected GitLab to be Unsupported, got Ok"),
            Err(other) => panic!("expected Unsupported, got {other:?}"),
        }
    }

    #[test]
    fn parse_pr_url_recognizes_github_pull_urls() {
        let parts = parse_pr_url("https://github.com/SoloJiang/weft/pull/148").unwrap();
        assert_eq!(parts.host_kind, HostKind::GitHub);
        assert_eq!(parts.host_base, "github.com");
        assert_eq!(parts.owner, "SoloJiang");
        assert_eq!(parts.repo, "weft");
        assert_eq!(parts.number, 148);
    }

    #[test]
    fn parse_pr_url_recognizes_ghe_hostnames() {
        // A GitHub Enterprise install uses the SAME `/pull/` shape on its own
        // hostname — must resolve to GitHub, not fall through to GitLab.
        let parts = parse_pr_url("https://github.acme-corp.com/team/svc/pull/9").unwrap();
        assert_eq!(parts.host_kind, HostKind::GitHub);
        assert_eq!(parts.host_base, "github.acme-corp.com");
        assert_eq!(parts.number, 9);
    }

    #[test]
    fn parse_pr_url_recognizes_gitlab_merge_request_urls() {
        let parts = parse_pr_url("https://gitlab.com/my-group/my-project/-/merge_requests/12").unwrap();
        assert_eq!(parts.host_kind, HostKind::GitLab);
        assert_eq!(parts.owner, "my-group");
        assert_eq!(parts.repo, "my-project");
        assert_eq!(parts.number, 12);
    }

    #[test]
    fn parse_pr_url_handles_self_hosted_gitlab_with_nested_subgroups() {
        // The whole point of #110's GitLab requirement: an arbitrary internal
        // hostname, with a namespace path that can itself contain `/`.
        let parts =
            parse_pr_url("https://git.internal.example/platform/backend/svc/-/merge_requests/3")
                .unwrap();
        assert_eq!(parts.host_kind, HostKind::GitLab);
        assert_eq!(parts.host_base, "git.internal.example");
        assert_eq!(parts.owner, "platform/backend");
        assert_eq!(parts.repo, "svc");
        assert_eq!(parts.number, 3);
    }

    #[test]
    fn parse_pr_url_tolerates_a_trailing_path_or_fragment() {
        let parts = parse_pr_url("https://github.com/acme/widgets/pull/5/files").unwrap();
        assert_eq!(parts.number, 5);
    }

    #[test]
    fn parse_pr_url_rejects_unrecognized_shapes() {
        assert!(parse_pr_url("https://github.com/acme/widgets").is_none());
        assert!(parse_pr_url("not a url").is_none());
        assert!(parse_pr_url("ftp://github.com/acme/widgets/pull/1").is_none());
        // A number-less tail must not parse as PR 0 or panic.
        assert!(parse_pr_url("https://github.com/acme/widgets/pull/").is_none());
    }
}
