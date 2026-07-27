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
//!
//! READ/WRITE BOUNDARY (issue #110 T3 added the write side): [`PrHost`]'s two
//! methods, `kind` and `fetch_status`, are BOTH read-only — there is no
//! mutating counterpart on the trait — so `monitor` and `github`'s `PrHost`
//! impl are READ-ONLY by construction, not just by convention, and that
//! stayed true through T3. The one mutating
//! action this module tree performs (`gh pr merge`) lives ENTIRELY in
//! [`automerge`], has its own independent spawned loop
//! (`automerge::spawn_pr_automerge_watch`), and is never reachable from
//! `monitor`'s sweep. [`gate`] is [`automerge`]'s pure decision function,
//! split out the same way [`judge`] is split out from `monitor` — see
//! `gate`'s and `automerge`'s own module docs for the full reasoning.

pub mod automerge;
pub mod gate;
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

/// Review signal.
///
/// This is intentionally NOT a single GitHub-shaped aggregate enum, even
/// though GitHub's own `reviewDecision` field legitimately IS one (it already
/// folds in required-reviewer/CODEOWNERS rules for you). GitLab has no
/// equivalent aggregate at all: approval state comes from the separate Merge
/// Request Approvals API, and "are there unresolved discussion threads" comes
/// from a THIRD, independent source (the Discussions API's per-note
/// `resolved`) — collapsing those two into one bucket (as an earlier version
/// of this type did, by literally renaming GitHub's `reviewDecision` values)
/// would make it impossible to tell a human WHICH of the two is actually
/// blocking, once a GitLab backend exists. `ChangesRequested` stays a
/// GitHub-only concept (GitLab has no formal "changes requested" state) —
/// a future GitLab backend simply never produces that variant, which is
/// honest (it's a real GitHub-specific signal), not a conflation.
///
/// MVP scope note: GitHub's mapping (`github::review_of`) does not walk
/// `reviewThreads.isResolved` — that needs paginated GraphQL with a
/// documented prior pagination bug in this very repo (dropping the newest/
/// unresolved threads past 100) — so it reports `unresolved_discussions:
/// None` (honestly unknown), never `Some(false)` ("definitely none
/// unresolved", which it never checked). A repo-specific convention layered
/// on top of either host's raw signal (e.g. a review-bot's 👍 reaction
/// counting as "all clear") stays the calling agent's job per its own
/// CLAUDE.md, not this neutral state machine's.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ReviewStatus {
    Unknown { reason: String },
    /// GitHub-only: a maintainer explicitly requested changes.
    ChangesRequested,
    /// Not (yet) approved. `unresolved_discussions` is the SEPARATE "are
    /// there open discussion threads" signal, kept apart from approval so
    /// neither backend has to force one into the other:
    /// - `None` — this backend doesn't check thread-resolution at all
    ///   (GitHub, this MVP: confirmed live against this repo's own PRs while
    ///   building this — an open, otherwise-healthy PR reports
    ///   `reviewDecision: ""`, i.e. not-yet-reviewed, which says nothing
    ///   about discussion threads).
    /// - `Some(_)` — a backend that actually knows (a future GitLab backend,
    ///   from its Discussions API).
    AwaitingApproval { unresolved_discussions: Option<bool> },
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
///
/// SECURITY: this is a trust boundary, not a convenience parser. The output
/// feeds `gh --repo`/`glab --repo`, whose `[HOST/]OWNER/REPO` grammar treats
/// an EXTRA leading path segment as a HOST OVERRIDE. A prior version used
/// `str::split_once('/')` for GitHub's owner/repo, which only looks at the
/// FIRST `/` — an attacker-supplied URL with one extra path segment before
/// `/pull/` (`https://github.com/evil.example.org/ownerx/repox/pull/5`) then
/// silently parsed as `owner="evil.example.org"`, `repo="ownerx/repox"`,
/// which `github::GitHubHost::fetch_status` formatted into a 3-segment
/// `--repo` value — reinterpreted by `gh` as "talk to host
/// evil.example.org", a real SSRF confirmed against a live `gh` binary
/// (`register_pr` is auto-approved with zero human confirmation, and the
/// sweep would have repeated the request every `WEFT_PR_SWEEP_SECS`). Fixed
/// by requiring EXACTLY the expected segment count (never silently absorbing
/// an extra one into `repo`) AND a character allowlist per segment (blocks
/// `@`/`:`/whitespace/control-character smuggling even within a single
/// segment). `github::GitHubHost::fetch_status` also independently refuses to
/// proceed if `owner`/`repo` contain `/`, so a `PrTarget` built some other
/// way (bypassing this parser) can't reopen the same hole.
pub fn parse_pr_url(url: &str) -> Option<PrUrlParts> {
    let url = url.trim();
    let after_scheme = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    let (host, rest) = after_scheme.split_once('/')?;
    if host.is_empty() || !is_plain_hostname(host) {
        return None;
    }

    if let Some(idx) = rest.find("/pull/") {
        let (path, tail) = rest.split_at(idx);
        let number = parse_leading_number(tail.trim_start_matches("/pull/"))?;
        // GitHub owner/repo is EXACTLY two path segments — never more, never
        // fewer. Rejecting a third segment outright (instead of folding it
        // into `repo`, as a naive `split_once` does) is what closes the SSRF
        // documented above.
        let mut segs = path.split('/');
        let owner = segs.next()?;
        let repo = segs.next()?;
        if segs.next().is_some() {
            return None;
        }
        if !is_valid_path_segment(owner) || !is_valid_path_segment(repo) {
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
        // GitLab's namespace legitimately nests (`group/subgroup/...`), so
        // the OVERALL `owner` string may contain `/` — but every individual
        // segment (each namespace level, and the trailing project name) must
        // still be a plain, single slug: no segment may itself smuggle a
        // `/`-like structural character. (A path-position "is this really a
        // host override" ambiguity is inherent to GitLab's arbitrary-depth
        // namespaces and can't be fully resolved from the URL shape alone —
        // there is no live `glab` invocation yet for this to reach, see
        // `resolve_host`; whichever future backend adds one must not trust a
        // URL-derived multi-segment owner as a `--repo`/`-R` argument without
        // separately confirming which prefix, if any, is a host override.)
        if !owner.split('/').all(is_valid_path_segment) || !is_valid_path_segment(repo) {
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

/// A single path segment (a GitHub owner/repo, or one GitLab namespace
/// level/project) is "plain" if it contains only characters GitHub/GitLab
/// themselves allow in that position — alphanumeric, `-`, `_`, `.`. Notably
/// excludes `/` (would smuggle an extra segment), `@`/`:` (userinfo/port/
/// scheme-like), whitespace, and control characters — anything a downstream
/// `--repo`/`-R` flag parser could plausibly reinterpret structurally.
fn is_valid_path_segment(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// A bare hostname (`github.com`, `github.acme-corp.com`, `git.internal:8443`)
/// — alphanumeric, `-`, `.`, and `:` for an explicit port. Rejects anything
/// that could itself carry a path/userinfo/scheme fragment; `host_base` is
/// currently stored for display only (never fed to a `gh`/`glab` invocation),
/// but validating it here means it stays that way even if a future backend
/// starts using it for GHE/self-hosted targeting.
fn is_plain_hostname(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | ':'))
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

    // --- adversarial: SSRF via a smuggled extra path segment -------------
    //
    // `github::GitHubHost::fetch_status` formats `owner`/`repo` into a single
    // `--repo` argument for `gh`, whose `[HOST/]OWNER/REPO` grammar treats an
    // extra leading segment as a HOST OVERRIDE. Every case below is a shape
    // that, before the fix, slipped a THIRD segment past a naive
    // `split_once('/')` and got silently absorbed into `repo` instead of
    // being rejected.

    #[test]
    fn parse_pr_url_rejects_an_extra_leading_segment_before_owner_repo() {
        // The exact repro from the adversarial review: a 3-segment path where
        // the naive parser read segment 1 as `owner` (attacker-controlled)
        // and segments 2+3 (still containing a `/`) as `repo`.
        assert!(
            parse_pr_url("https://github.com/evil.example.org/ownerx/repox/pull/5").is_none(),
            "a 3-segment path before /pull/ must be rejected outright, not folded into repo"
        );
    }

    #[test]
    fn parse_pr_url_rejects_an_extra_segment_anywhere_before_pull() {
        // Same shape, but the attacker-controlled segment is LAST instead of
        // first — still must not silently become part of `repo`.
        assert!(parse_pr_url("https://github.com/owner/repo/evil.example.org/pull/5").is_none());
        // Four segments — not just the specific 3-segment repro.
        assert!(parse_pr_url("https://github.com/a/b/c/d/pull/5").is_none());
    }

    #[test]
    fn parse_pr_url_rejects_too_few_segments_before_pull() {
        assert!(parse_pr_url("https://github.com/owner/pull/5").is_none());
        assert!(parse_pr_url("https://github.com/pull/5").is_none());
    }

    #[test]
    fn parse_pr_url_rejects_structural_characters_smuggled_within_a_single_segment() {
        // Even a single, correctly-counted segment must not carry `@`/`:`/
        // whitespace/control characters that a downstream CLI arg parser
        // could reinterpret (userinfo, port, scheme, or a raw newline
        // splitting the argument some other way).
        assert!(parse_pr_url("https://github.com/owner:evil/repo/pull/5").is_none());
        assert!(parse_pr_url("https://github.com/owner/re@po/pull/5").is_none());
        assert!(parse_pr_url("https://github.com/own er/repo/pull/5").is_none());
        assert!(parse_pr_url("https://github.com/owner/re\npo/pull/5").is_none());
        assert!(parse_pr_url("https://github.com/owner/repo\\x/pull/5").is_none());
    }

    #[test]
    fn parse_pr_url_rejects_userinfo_smuggled_into_the_host_position() {
        // `user@host` in URL syntax — the "host" segment must not carry a
        // userinfo prefix that could confuse a downstream consumer of
        // `host_base` about which part is actually the hostname.
        assert!(parse_pr_url("https://github.com@evil.example/owner/repo/pull/5").is_none());
    }

    #[test]
    fn parse_pr_url_rejects_a_dot_segment_disguised_as_a_third_path_component() {
        assert!(parse_pr_url("https://github.com/../owner/repo/pull/5").is_none());
    }

    #[test]
    fn parse_pr_url_still_accepts_the_legitimate_two_segment_shape() {
        // Regression guard alongside the rejections above: the fix must not
        // have overcorrected into rejecting normal URLs (dots/hyphens/
        // underscores are common in real owner/repo names).
        let parts = parse_pr_url("https://github.com/my-org_1/my.repo_name/pull/42").unwrap();
        assert_eq!(parts.owner, "my-org_1");
        assert_eq!(parts.repo, "my.repo_name");
    }

    #[test]
    fn parse_pr_url_gitlab_rejects_structural_characters_in_any_namespace_segment_or_project() {
        // The nested-subgroup case must still validate EACH segment even
        // though the overall owner string legitimately contains `/`.
        assert!(
            parse_pr_url("https://gitlab.com/group/evil:host/project/-/merge_requests/5")
                .is_none()
        );
        assert!(
            parse_pr_url("https://gitlab.com/group/sub/pro@ject/-/merge_requests/5").is_none()
        );
    }

    #[test]
    fn parse_pr_url_gitlab_still_accepts_legitimate_nested_subgroups_with_plain_segments() {
        let parts =
            parse_pr_url("https://gitlab.com/a-group/sub_group/my.project/-/merge_requests/7")
                .unwrap();
        assert_eq!(parts.owner, "a-group/sub_group");
        assert_eq!(parts.repo, "my.project");
    }
}
