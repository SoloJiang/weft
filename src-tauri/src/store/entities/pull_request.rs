use sea_orm::entity::prelude::*;

/// One tracked PR/MR (issue #110 T1): the durable, host-agnostic row that
/// replaces "the agent remembers what it's watching" with a real DB row plus
/// a background monitor (`crate::host::monitor`). Denormalizes `thread_id` +
/// `repo_id` alongside `direction_id` — same shape as `session` — so the
/// sweep and Needs-you routing never need to join through `direction` to find
/// them.
///
/// `host_kind`/`host_base`/`host_owner`/`host_repo` are recorded ONCE at
/// registration time (`register_pr`, parsed from the PR/MR URL the agent just
/// got back from `gh pr create` / a future `glab mr create`) and never
/// re-inferred later — the running state machine trusts what it was told, it
/// doesn't re-guess.
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, serde::Serialize, serde::Deserialize)]
#[sea_orm(table_name = "pull_request")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i32,
    pub thread_id: i32,
    /// The task/branch this PR/MR came from. 0 = unset (shouldn't happen for a
    /// row created via `register_pr`, which always runs in a direction's own
    /// bus context) — same "0 = unset" convention as `direction.repo_id`.
    #[sea_orm(default_value = 0)]
    pub direction_id: i32,
    #[sea_orm(default_value = 0)]
    pub repo_id: i32,
    /// "github" | "gitlab" — which `crate::host::PrHost` backend owns this
    /// row. See `crate::host::HostKind`.
    pub host_kind: String,
    /// Hostname the backend talks to: "github.com" for GitHub.com, a GHE
    /// hostname, or a self-hosted GitLab hostname. Display/diagnostics only —
    /// dispatch always goes by `host_kind`, never by parsing this back.
    #[sea_orm(default_value = "")]
    pub host_base: String,
    /// Owner/org (GitHub) or namespace/group path (GitLab; may contain `/`
    /// for a nested subgroup).
    pub host_owner: String,
    /// Repo/project slug on the host (may differ from weft's local repo name).
    pub host_repo: String,
    pub number: i32,
    #[sea_orm(default_value = "")]
    pub url: String,
    #[sea_orm(default_value = "")]
    pub title: String,
    #[sea_orm(default_value = "")]
    pub head_sha: String,
    #[sea_orm(default_value = "")]
    pub base_ref: String,
    /// "open" | "closed" | "merged" — see `crate::host::PrLifecycle`. The
    /// background sweep only lists `open` rows (`repo::list_open_pull_requests`);
    /// once this leaves "open" there is nothing left to poll.
    #[sea_orm(default_value = "open")]
    pub lifecycle: String,
    /// JSON-serialized `crate::host::CiStatus`. "" = never successfully
    /// checked yet.
    #[sea_orm(default_value = "")]
    pub ci_status: String,
    /// JSON-serialized `crate::host::ReviewStatus`.
    #[sea_orm(default_value = "")]
    pub review_status: String,
    /// JSON-serialized `crate::host::ConflictStatus`.
    #[sea_orm(default_value = "")]
    pub conflict_status: String,
    /// JSON-serialized `crate::host::MergeReadiness` — always RECOMPUTED from
    /// the three axes above on a successful sweep, never independently set.
    #[sea_orm(default_value = "")]
    pub merge_readiness: String,
    /// Unix seconds (as string, same convention as `created_at` elsewhere in
    /// this store) of the last completed sweep attempt, success or failure.
    /// "" = never checked (freshly registered).
    #[sea_orm(default_value = "")]
    pub last_checked_at: String,
    /// Diagnostic text from the last FAILED fetch attempt; "" when the last
    /// attempt succeeded. The honest "can we even tell" signal (issue #107
    /// lesson) — a failed probe never silently reuses the last snapshot's
    /// axes as if they were fresh.
    #[sea_orm(default_value = "")]
    pub last_error: String,
    pub created_at: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
