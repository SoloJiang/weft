//! Integration coverage for the storage/process collector behind issue #171.

use sea_orm::{ActiveModelTrait, EntityTrait, Set};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tempfile::TempDir;
use weft::ask::AskRegistry;
use weft::bus::BusRegistry;
use weft::host::{CiStatus, ConflictStatus, ReviewStatus, ThreadStatus, UpstreamStatus};
use weft::materialize::materialize_direction;
use weft::readiness::{CheckExecution, IssueReadiness, LaneReadiness, ReasonCode};
use weft::store::{
    entities::{direction, pull_request, worktree},
    repo, Db,
};

struct Fixture {
    _temp: TempDir,
    db: Db,
    bus: BusRegistry,
    asks: AskRegistry,
    thread_id: i32,
    direction_id: i32,
    repo_id: i32,
    // Readiness uses process-global probe limits and single-flight state. Keep
    // one integration fixture alive at a time so unrelated tests cannot spend
    // another fixture's bounded probe budget. This drops after `_temp`, so the
    // repository and worktrees are also cleaned up before the next test starts.
    _serial: tokio::sync::OwnedMutexGuard<()>,
}

const FIXTURE_DIRECTION_ID_BLOCK: i32 = 10_000;

fn next_fixture_direction_id() -> i32 {
    // CheckFlight and its admission gate are process-global and keyed only by
    // direction ID. Independent in-memory SQLite fixtures otherwise all begin
    // at ID 1. Reserve a range because a fixture can add follow-up directions.
    static NEXT_DIRECTION_ID: AtomicI32 = AtomicI32::new(1_000_000_000);
    NEXT_DIRECTION_ID.fetch_add(FIXTURE_DIRECTION_ID_BLOCK, Ordering::SeqCst)
}

fn readiness_fixture_lock() -> Arc<tokio::sync::Mutex<()>> {
    static LOCK: OnceLock<Arc<tokio::sync::Mutex<()>>> = OnceLock::new();
    Arc::clone(LOCK.get_or_init(|| Arc::new(tokio::sync::Mutex::new(()))))
}

struct HostIdentity {
    base: String,
    owner: String,
    repo: String,
}

/// Mirrors the durable JSON shape of the private `planner::Proposal` type.
/// Integration tests are outside the crate, so they validate its store
/// contract without widening the planner module's production visibility.
#[derive(serde::Serialize)]
struct StoredProposalLane {
    name: String,
    repo: String,
    reason: String,
    mandate: String,
    base_branch: String,
    decision: String,
    direction_id: i32,
}

fn unix_secs() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("test clock is after Unix epoch")
        .as_secs()
        .to_string()
}

fn proposed_lane(name: &str, decision: &str, direction_id: i32) -> StoredProposalLane {
    StoredProposalLane {
        name: name.to_string(),
        repo: "readiness-repo".to_string(),
        reason: "readiness fixture lane".to_string(),
        mandate: "impl-only".to_string(),
        base_branch: "main".to_string(),
        decision: decision.to_string(),
        direction_id,
    }
}

fn proposal_json(directions: Vec<StoredProposalLane>) -> String {
    serde_json::json!({
        "rationale": "readiness fixture proposal",
        "directions": directions,
    })
    .to_string()
}

async fn persist_plan(db: &Db, thread_id: i32, status: &str, directions: Vec<StoredProposalLane>) {
    repo::upsert_plan(db, thread_id, &proposal_json(directions), status, "0")
        .await
        .expect("persist proposal");
}

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(dir)
        .status()
        .expect("git must start in test fixture");
    assert!(status.success(), "git {:?} failed", args);
}

fn make_repo(root: &Path) -> PathBuf {
    let path = root.join("repo");
    std::fs::create_dir_all(&path).expect("test repo directory");
    git(&path, &["init", "-q", "-b", "main"]);
    git(&path, &["config", "user.email", "readiness@test.invalid"]);
    git(&path, &["config", "user.name", "Readiness Test"]);
    std::fs::write(path.join("README.md"), "# readiness fixture\n").expect("fixture readme");
    git(&path, &["add", "README.md"]);
    git(&path, &["commit", "-q", "-m", "fixture"]);
    path
}

fn make_repo_with_counting_passing_check(root: &Path) -> PathBuf {
    let path = make_repo(root);
    std::fs::write(
        path.join("readiness-check.sh"),
        "#!/bin/sh\nprintf 'run\\n' >> .readiness-check-count\n",
    )
    .expect("counting check script");
    std::fs::write(
        path.join(".gitignore"),
        ".readiness-check-count\nnode_modules/\n",
    )
    .expect("counter ignore rule");
    std::fs::write(
        path.join("package.json"),
        r#"{"scripts":{"build":"sh ./readiness-check.sh"}}"#,
    )
    .expect("counting check package manifest");
    git(
        &path,
        &["add", "readiness-check.sh", ".gitignore", "package.json"],
    );
    git(&path, &["commit", "-q", "-m", "add readiness check"]);
    path
}

fn make_repo_with_origin(root: &Path) -> (PathBuf, HostIdentity) {
    let source = make_repo(root);
    let source = source.canonicalize().expect("canonical fixture source");
    let clone = root.join("clone");
    let remote = format!("file://localhost{}", source.display());
    let clone_path = clone.to_str().expect("utf8 clone path");
    git(root, &["clone", "-q", &remote, clone_path]);
    git(&clone, &["config", "user.email", "readiness@test.invalid"]);
    git(&clone, &["config", "user.name", "Readiness Test"]);
    let source_path = source
        .to_str()
        .expect("utf8 source path")
        .trim_start_matches('/');
    let (owner, repo) = source_path
        .rsplit_once('/')
        .expect("fixture source has an owner path");
    (
        clone,
        HostIdentity {
            base: "localhost".to_string(),
            owner: owner.to_string(),
            repo: repo.to_string(),
        },
    )
}

async fn fixture_for_repo(temp: TempDir, repo_path: PathBuf, plan_status: Option<&str>) -> Fixture {
    let serial = readiness_fixture_lock().lock_owned().await;
    let db = Db::connect("sqlite::memory:").await.expect("memory db");
    let workspace = repo::create_workspace(&db, "readiness workspace")
        .await
        .expect("workspace");
    let repo_ref = repo::add_repo_ref(
        &db,
        workspace.id,
        "readiness-repo",
        repo_path.to_str().expect("utf8 path"),
        "main",
        "",
        true,
    )
    .await
    .expect("repo ref");
    let thread = repo::create_thread(&db, workspace.id, "readiness issue", "feature", "claude")
        .await
        .expect("thread");
    let direction_id = next_fixture_direction_id();
    let direction = direction::ActiveModel {
        id: Set(direction_id),
        thread_id: Set(thread.id),
        name: Set("implementation".to_string()),
        slug: Set("implementation".to_string()),
        tool: Set("claude".to_string()),
        branch: Set(format!("feature/readiness-{direction_id}")),
        status: Set("queued".to_string()),
        repo_id: Set(repo_ref.id),
        reason: Set("implement readiness test fixture".to_string()),
        engine_pinned: Set(false),
        mandate: Set("impl-only".to_string()),
        base_branch: Set("main".to_string()),
        created_at: Set(unix_secs()),
        ..Default::default()
    }
    .insert(&db.0)
    .await
    .expect("direction");
    let worktrees = materialize_direction(&db, direction.id)
        .await
        .expect("materialized worktree");
    assert_eq!(worktrees.len(), 1, "fixture has one worktree");
    repo::set_direction_status(&db, direction.id, "review")
        .await
        .expect("review status");

    if let Some(status) = plan_status {
        let directions = match status {
            "confirmed" => vec![proposed_lane("implementation", "approved", direction.id)],
            "proposed" => vec![proposed_lane("pending implementation", "", 0)],
            "withdrawn" => vec![proposed_lane("withdrawn implementation", "", 0)],
            _ => vec![proposed_lane("unknown plan status", "", 0)],
        };
        persist_plan(&db, thread.id, status, directions).await;
    }

    Fixture {
        _temp: temp,
        db,
        bus: BusRegistry::new(),
        asks: AskRegistry::new(),
        thread_id: thread.id,
        direction_id: direction.id,
        repo_id: repo_ref.id,
        _serial: serial,
    }
}

async fn fixture(plan_status: Option<&str>) -> Fixture {
    let temp = tempfile::tempdir().expect("temporary fixture root");
    let repo_path = make_repo(temp.path());
    fixture_for_repo(temp, repo_path, plan_status).await
}

async fn fixture_with_origin(plan_status: Option<&str>) -> (Fixture, HostIdentity) {
    let temp = tempfile::tempdir().expect("temporary fixture root");
    let (repo_path, host) = make_repo_with_origin(temp.path());
    (fixture_for_repo(temp, repo_path, plan_status).await, host)
}

async fn remove_registered_worktrees(fixture: &Fixture) {
    let registered = repo::list_worktrees(&fixture.db, Some(fixture.direction_id))
        .await
        .expect("registered worktrees");
    assert_eq!(registered.len(), 1, "fixture has one registered worktree");
    for row in registered {
        worktree::Entity::delete_by_id(row.id)
            .exec(&fixture.db.0)
            .await
            .expect("remove worktree row");
    }
}

async fn add_failing_build_script(fixture: &Fixture) {
    let registered = repo::list_worktrees(&fixture.db, Some(fixture.direction_id))
        .await
        .expect("registered worktrees");
    assert_eq!(registered.len(), 1, "fixture has one registered worktree");
    std::fs::write(
        Path::new(&registered[0].path).join("package.json"),
        r#"{"scripts":{"build":"exit 1"}}"#,
    )
    .expect("failing build fixture");
    let worktree = Path::new(&registered[0].path);
    git(worktree, &["add", "package.json"]);
    git(worktree, &["commit", "-q", "-m", "add failing readiness check"]);
}

async fn add_passing_build_script_for_direction(fixture: &Fixture, direction_id: i32) {
    let registered = repo::list_worktrees(&fixture.db, Some(direction_id))
        .await
        .expect("registered worktrees");
    assert_eq!(registered.len(), 1, "fixture has one registered worktree");
    std::fs::write(
        Path::new(&registered[0].path).join("package.json"),
        r#"{"scripts":{"build":"exit 0"}}"#,
    )
    .expect("passing build fixture");
    let worktree = Path::new(&registered[0].path);
    git(worktree, &["add", "package.json"]);
    git(worktree, &["commit", "-q", "-m", "add passing readiness check"]);
}

async fn add_passing_build_script(fixture: &Fixture) {
    add_passing_build_script_for_direction(fixture, fixture.direction_id).await;
}

async fn add_counting_build_script(fixture: &Fixture) -> PathBuf {
    let registered = repo::list_worktrees(&fixture.db, Some(fixture.direction_id))
        .await
        .expect("registered worktrees");
    assert_eq!(registered.len(), 1, "fixture has one registered worktree");
    let worktree = Path::new(&registered[0].path);
    let counter = worktree.join(".readiness-check-count");
    std::fs::write(
        worktree.join("readiness-check.sh"),
        "#!/bin/sh\nprintf 'run\\n' >> .readiness-check-count\nsleep 1\n",
    )
    .expect("counting build script");
    std::fs::write(
        worktree.join("package.json"),
        r#"{"scripts":{"build":"sh ./readiness-check.sh"}}"#,
    )
    .expect("counting package fixture");
    std::fs::write(worktree.join(".gitignore"), ".readiness-check-count\n")
        .expect("ignore readiness counter");
    git(
        worktree,
        &["add", "readiness-check.sh", "package.json", ".gitignore"],
    );
    git(worktree, &["commit", "-q", "-m", "add counting readiness check"]);
    counter
}

async fn add_hanging_counting_build_script(fixture: &Fixture) -> PathBuf {
    let registered = repo::list_worktrees(&fixture.db, Some(fixture.direction_id))
        .await
        .expect("registered worktrees");
    assert_eq!(registered.len(), 1, "fixture has one registered worktree");
    let worktree = Path::new(&registered[0].path);
    let counter = worktree.join(".readiness-hanging-check-count");
    std::fs::write(
        worktree.join("readiness-hanging-check.sh"),
        "#!/bin/sh\nprintf 'run\\n' >> .readiness-hanging-check-count\nsleep 30\n",
    )
    .expect("hanging build script");
    std::fs::write(
        worktree.join("package.json"),
        r#"{"scripts":{"build":"sh ./readiness-hanging-check.sh"}}"#,
    )
    .expect("hanging package fixture");
    std::fs::write(
        worktree.join(".gitignore"),
        ".readiness-hanging-check-count\n",
    )
    .expect("ignore hanging readiness counter");
    git(
        worktree,
        &[
            "add",
            "readiness-hanging-check.sh",
            "package.json",
            ".gitignore",
        ],
    );
    git(worktree, &["commit", "-q", "-m", "add hanging readiness check"]);
    counter
}

async fn remove_registered_worktree_directories(fixture: &Fixture) {
    let registered = repo::list_worktrees(&fixture.db, Some(fixture.direction_id))
        .await
        .expect("registered worktrees");
    assert_eq!(registered.len(), 1, "fixture has one registered worktree");
    for row in registered {
        std::fs::remove_dir_all(&row.path).expect("remove materialized worktree directory");
    }
}

async fn add_review_direction(fixture: &Fixture, name: &str) -> direction::Model {
    let direction = repo::create_direction(
        &fixture.db,
        fixture.thread_id,
        name,
        "claude",
        fixture.repo_id,
        "upstream readiness fixture",
        "impl-only",
        "main",
    )
    .await
    .expect("upstream direction");
    materialize_direction(&fixture.db, direction.id)
        .await
        .expect("upstream worktree");
    repo::set_direction_status(&fixture.db, direction.id, "review")
        .await
        .expect("upstream review status");
    direction
}

async fn insert_upstream_pr(
    fixture: &Fixture,
    direction_id: i32,
    host: &HostIdentity,
    number: i32,
    lifecycle: &str,
) {
    pull_request::ActiveModel {
        thread_id: Set(fixture.thread_id),
        direction_id: Set(direction_id),
        repo_id: Set(fixture.repo_id),
        host_kind: Set("github".to_string()),
        host_base: Set(host.base.clone()),
        host_owner: Set(host.owner.clone()),
        host_repo: Set(host.repo.clone()),
        number: Set(number),
        url: Set(String::new()),
        title: Set("upstream readiness".to_string()),
        head_sha: Set(String::new()),
        base_ref: Set("main".to_string()),
        lifecycle: Set(lifecycle.to_string()),
        ci_status: Set(serde_json::to_string(&CiStatus::Passing).expect("ci json")),
        review_status: Set(serde_json::to_string(&ReviewStatus::Approved).expect("review json")),
        thread_status: Set(serde_json::to_string(&ThreadStatus::AllResolved).expect("thread json")),
        conflict_status: Set(serde_json::to_string(&ConflictStatus::Clean).expect("conflict json")),
        merge_readiness: Set(String::new()),
        last_checked_at: Set(unix_secs()),
        last_error: Set(String::new()),
        probe_fail_count: Set(0),
        created_at: Set("0".to_string()),
        ..Default::default()
    }
    .insert(&fixture.db.0)
    .await
    .expect("upstream pr");
}

async fn insert_unbound_pr(
    fixture: &Fixture,
    number: i32,
    lifecycle: &str,
    ci: CiStatus,
    last_checked_at: &str,
) {
    pull_request::ActiveModel {
        thread_id: Set(fixture.thread_id),
        direction_id: Set(0),
        repo_id: Set(0),
        host_kind: Set("github".to_string()),
        host_base: Set("github.com".to_string()),
        host_owner: Set("example".to_string()),
        host_repo: Set("readiness".to_string()),
        number: Set(number),
        url: Set(format!(
            "https://github.com/example/readiness/pull/{number}"
        )),
        title: Set("unbound readiness".to_string()),
        head_sha: Set(String::new()),
        base_ref: Set("main".to_string()),
        lifecycle: Set(lifecycle.to_string()),
        ci_status: Set(serde_json::to_string(&ci).expect("ci json")),
        review_status: Set(serde_json::to_string(&ReviewStatus::Approved).expect("review json")),
        thread_status: Set(serde_json::to_string(&ThreadStatus::AllResolved).expect("thread json")),
        conflict_status: Set(serde_json::to_string(&ConflictStatus::Clean).expect("conflict json")),
        merge_readiness: Set(String::new()),
        last_checked_at: Set(last_checked_at.to_string()),
        last_error: Set(String::new()),
        probe_fail_count: Set(0),
        created_at: Set("0".to_string()),
        ..Default::default()
    }
    .insert(&fixture.db.0)
    .await
    .expect("unbound pr");
}

async fn insert_direction_pr(fixture: &Fixture, number: i32, lifecycle: &str) {
    let last_checked_at = if lifecycle == "open" {
        unix_secs()
    } else {
        String::new()
    };
    pull_request::ActiveModel {
        thread_id: Set(fixture.thread_id),
        direction_id: Set(fixture.direction_id),
        repo_id: Set(fixture.repo_id),
        host_kind: Set("github".to_string()),
        host_base: Set("github.com".to_string()),
        host_owner: Set("example".to_string()),
        host_repo: Set("readiness".to_string()),
        number: Set(number),
        url: Set(format!(
            "https://github.com/example/readiness/pull/{number}"
        )),
        title: Set("tracked readiness".to_string()),
        head_sha: Set(String::new()),
        base_ref: Set("main".to_string()),
        lifecycle: Set(lifecycle.to_string()),
        ci_status: Set(serde_json::to_string(&CiStatus::Passing).expect("ci json")),
        review_status: Set(serde_json::to_string(&ReviewStatus::Approved).expect("review json")),
        thread_status: Set(serde_json::to_string(&ThreadStatus::AllResolved).expect("thread json")),
        conflict_status: Set(serde_json::to_string(&ConflictStatus::Clean).expect("conflict json")),
        merge_readiness: Set(String::new()),
        last_checked_at: Set(last_checked_at),
        last_error: Set(String::new()),
        probe_fail_count: Set(0),
        created_at: Set("0".to_string()),
        ..Default::default()
    }
    .insert(&fixture.db.0)
    .await
    .expect("tracked direction pr");
}

#[tokio::test]
async fn zero_lanes_is_unknown_not_review_ready() {
    let db = Db::connect("sqlite::memory:").await.expect("memory db");
    let workspace = repo::create_workspace(&db, "empty workspace")
        .await
        .expect("workspace");
    let thread = repo::create_thread(&db, workspace.id, "empty issue", "feature", "claude")
        .await
        .expect("thread");
    let bus = BusRegistry::new();
    let asks = AskRegistry::new();
    let result = weft::readiness::collect(&db, &bus, &asks, thread.id)
        .await
        .expect("readiness");

    assert_eq!(result.readiness, IssueReadiness::Unknown);
    assert_eq!(result.active_lane_count, 0);
    assert_eq!(result.reasons[0].code, ReasonCode::NoActiveLanes);
}

#[tokio::test]
async fn review_lane_without_inferred_checks_is_unknown() {
    let fixture = fixture(None).await;
    let result = weft::readiness::collect(&fixture.db, &fixture.bus, &fixture.asks, fixture.thread_id)
        .await
        .expect("readiness");

    assert_eq!(result.readiness, IssueReadiness::Unknown);
    assert_eq!(result.active_lane_count, 1);
    assert_eq!(result.lanes[0].readiness, LaneReadiness::Unknown);
    assert_eq!(result.lanes[0].reasons[0].code, ReasonCode::ChecksUnknown);
}

#[tokio::test]
async fn tool_error_does_not_fail_a_worker_turn_with_completed_assistant_text() {
    let fixture = fixture(None).await;
    let session = repo::create_session(
        &fixture.db,
        fixture.direction_id,
        fixture.repo_id,
        "claude",
        "/tmp/readiness-worker",
    )
    .await
    .expect("worker session");
    repo::insert_lead_message(
        &fixture.db,
        fixture.thread_id,
        Some(session.id),
        1,
        "assistant",
        "tool",
        r#"{"tool":"build"}"#,
        "error",
    )
    .await
    .expect("tool error row");
    repo::insert_lead_message(
        &fixture.db,
        fixture.thread_id,
        Some(session.id),
        1,
        "assistant",
        "text",
        r#"{"text":"recovered"}"#,
        "complete",
    )
    .await
    .expect("completed assistant row");
    // A completed worker turn drains through engine::persist_activity into
    // `idle`; keep this fixture on the completed side of the session boundary.
    repo::set_session_status(&fixture.db, session.id, "idle")
        .await
        .expect("completed worker idle status");
    add_passing_build_script(&fixture).await;

    let result = weft::readiness::collect(&fixture.db, &fixture.bus, &fixture.asks, fixture.thread_id)
        .await
        .expect("readiness");

    assert_eq!(result.readiness, IssueReadiness::ReviewReady);
    assert!(result
        .reasons
        .iter()
        .all(|reason| reason.code != ReasonCode::WorkerFailed));
}

#[tokio::test]
async fn assistant_text_error_fails_a_worker_turn_despite_completed_tool_row() {
    let fixture = fixture(None).await;
    let session = repo::create_session(
        &fixture.db,
        fixture.direction_id,
        fixture.repo_id,
        "claude",
        "/tmp/readiness-worker",
    )
    .await
    .expect("worker session");
    repo::insert_lead_message(
        &fixture.db,
        fixture.thread_id,
        Some(session.id),
        1,
        "assistant",
        "tool",
        r#"{"tool":"build"}"#,
        "complete",
    )
    .await
    .expect("completed tool row");
    repo::insert_lead_message(
        &fixture.db,
        fixture.thread_id,
        Some(session.id),
        1,
        "assistant",
        "text",
        r#"{"terminal":"error_before_output"}"#,
        "error",
    )
    .await
    .expect("failed assistant row");
    // The persisted error is a completed turn outcome. `create_session`
    // starts in `starting`, which correctly owns the checkout and preempts
    // terminal worker facts until the engine records the drained idle state.
    repo::set_session_status(&fixture.db, session.id, "idle")
        .await
        .expect("failed worker idle status");

    let result = weft::readiness::collect(&fixture.db, &fixture.bus, &fixture.asks, fixture.thread_id)
        .await
        .expect("readiness");

    assert_eq!(result.readiness, IssueReadiness::Failed);
    assert_eq!(result.reasons[0].code, ReasonCode::WorkerFailed);
    assert_eq!(result.reasons[0].direction_id, Some(fixture.direction_id));
}

#[tokio::test]
async fn working_lane_skips_checks_until_it_claims_completion() {
    let fixture = fixture(None).await;
    repo::set_direction_status(&fixture.db, fixture.direction_id, "working")
        .await
        .expect("working status");
    // If the collector runs this package's build, it fails and the verdict
    // would be ChecksFailing. In-progress lanes must instead skip execution.
    add_failing_build_script(&fixture).await;

    let result = weft::readiness::collect(&fixture.db, &fixture.bus, &fixture.asks, fixture.thread_id)
        .await
        .expect("readiness");

    assert_eq!(result.readiness, IssueReadiness::Unknown);
    assert_eq!(result.reasons[0].code, ReasonCode::InProgress);
}

#[tokio::test]
async fn dirty_review_lane_fails_closed_without_running_checks() {
    let fixture = fixture(None).await;
    let registered = repo::list_worktrees(&fixture.db, Some(fixture.direction_id))
        .await
        .expect("registered worktrees");
    let worktree = Path::new(&registered[0].path);
    let counter = worktree.join("dirty-readiness-check-ran");
    std::fs::write(
        worktree.join("package.json"),
        r#"{"scripts":{"build":"printf run > dirty-readiness-check-ran"}}"#,
    )
    .expect("dirty check fixture");

    let result = weft::readiness::collect(&fixture.db, &fixture.bus, &fixture.asks, fixture.thread_id)
        .await
        .expect("dirty readiness");

    assert_eq!(result.readiness, IssueReadiness::Unknown);
    assert_eq!(result.reasons[0].code, ReasonCode::ChecksUnknown);
    assert!(
        !counter.exists(),
        "delivery readiness must not run checks for an already-dirty target"
    );
}

#[tokio::test]
async fn open_ask_short_circuits_a_hanging_check_runner() {
    let fixture = fixture(None).await;
    let counter = add_hanging_counting_build_script(&fixture).await;
    fixture.bus.ask_human(
        fixture.thread_id,
        &fixture.direction_id.to_string(),
        "choose the release owner",
    );

    let result = tokio::time::timeout(
        Duration::from_millis(500),
        weft::readiness::collect(&fixture.db, &fixture.bus, &fixture.asks, fixture.thread_id),
    )
    .await
    .expect("open ask readiness must not wait for the hanging command")
    .expect("readiness");

    assert_eq!(result.readiness, IssueReadiness::NeedsYou);
    assert_eq!(result.reasons[0].code, ReasonCode::OpenNeed);
    assert!(
        !counter.exists(),
        "a decisive open ask must not start the readiness check runner"
    );
}

#[tokio::test]
async fn occupied_review_worker_statuses_skip_hanging_checks_until_idle() {
    for status in ["running", "starting", "stopped"] {
        let fixture = fixture(None).await;
        let counter = add_hanging_counting_build_script(&fixture).await;
        let session = repo::create_session(
            &fixture.db,
            fixture.direction_id,
            fixture.repo_id,
            "claude",
            "/tmp/readiness-active-worker",
        )
        .await
        .expect("occupied worker session");
        // This is the same session occupancy boundary used by worktree reclaim:
        // starting precedes engine activity, and stopped preserves human-terminal
        // takeover ownership. Neither may race readiness verification.
        repo::set_session_status(&fixture.db, session.id, status)
            .await
            .expect("mark worker occupied");

        let occupied = tokio::time::timeout(
            Duration::from_millis(500),
            weft::readiness::collect(&fixture.db, &fixture.bus, &fixture.asks, fixture.thread_id),
        )
        .await
        .expect("occupied-worker readiness must not wait for the hanging command")
        .expect("occupied-worker readiness");

        assert_eq!(occupied.readiness, IssueReadiness::Unknown, "{status}");
        assert_eq!(occupied.reasons[0].code, ReasonCode::InProgress, "{status}");
        assert!(
            !counter.exists(),
            "a {status} worker must not start the readiness check runner"
        );

        repo::set_session_status(&fixture.db, session.id, "idle")
            .await
            .expect("mark worker idle");
        add_passing_build_script(&fixture).await;
        let idle = tokio::time::timeout(
            Duration::from_secs(3),
            weft::readiness::collect(&fixture.db, &fixture.bus, &fixture.asks, fixture.thread_id),
        )
        .await
        .expect("idle-worker readiness must complete")
        .expect("idle-worker readiness");

        assert_eq!(idle.readiness, IssueReadiness::ReviewReady, "{status}");
    }
}

#[tokio::test]
async fn queued_lane_without_worktree_is_vacuously_in_progress() {
    let fixture = fixture(None).await;
    remove_registered_worktrees(&fixture).await;
    repo::set_direction_status(&fixture.db, fixture.direction_id, "queued")
        .await
        .expect("queued status");

    let result = weft::readiness::collect(&fixture.db, &fixture.bus, &fixture.asks, fixture.thread_id)
        .await
        .expect("readiness");

    assert_eq!(result.readiness, IssueReadiness::Unknown);
    assert_eq!(result.reasons[0].code, ReasonCode::InProgress);
}

#[tokio::test]
async fn working_lane_without_worktree_is_remote_unknown() {
    let fixture = fixture(None).await;
    remove_registered_worktrees(&fixture).await;
    repo::set_direction_status(&fixture.db, fixture.direction_id, "working")
        .await
        .expect("working status");

    let result = weft::readiness::collect(&fixture.db, &fixture.bus, &fixture.asks, fixture.thread_id)
        .await
        .expect("readiness");

    assert_eq!(result.readiness, IssueReadiness::Unknown);
    assert_eq!(result.reasons[0].code, ReasonCode::RemoteUnknown);
}

#[tokio::test]
async fn claimed_completion_failing_checks_remain_blocked() {
    for status in ["review", "done"] {
        let fixture = fixture(None).await;
        repo::set_direction_status(&fixture.db, fixture.direction_id, status)
            .await
            .expect("claimed completion status");
        add_failing_build_script(&fixture).await;

        let result = weft::readiness::collect(&fixture.db, &fixture.bus, &fixture.asks, fixture.thread_id)
            .await
            .expect("readiness");

        assert_eq!(result.readiness, IssueReadiness::Blocked, "{status}");
        assert_eq!(
            result.reasons[0].code,
            ReasonCode::ChecksFailing,
            "{status}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_board_collects_share_one_real_check_run() {
    let fixture = fixture(None).await;
    let counter = add_counting_build_script(&fixture).await;
    let (one, two, three, four) = tokio::join!(
        weft::readiness::collect(&fixture.db, &fixture.bus, &fixture.asks, fixture.thread_id),
        weft::readiness::collect(&fixture.db, &fixture.bus, &fixture.asks, fixture.thread_id),
        weft::readiness::collect(&fixture.db, &fixture.bus, &fixture.asks, fixture.thread_id),
        weft::readiness::collect(&fixture.db, &fixture.bus, &fixture.asks, fixture.thread_id),
    );
    for result in [one, two, three, four] {
        assert_eq!(
            result.expect("concurrent readiness").readiness,
            IssueReadiness::ReviewReady
        );
    }

    let executions = std::fs::read_to_string(counter)
        .expect("check execution counter")
        .lines()
        .count();
    assert_eq!(executions, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn explicit_verification_and_readiness_share_one_real_check_run() {
    let fixture = fixture(None).await;
    let counter = add_counting_build_script(&fixture).await;

    let (readiness, explicit) = tokio::join!(
        weft::readiness::collect(&fixture.db, &fixture.bus, &fixture.asks, fixture.thread_id),
        weft::readiness::verify_direction(&fixture.db, fixture.direction_id),
    );

    assert_eq!(
        readiness.expect("readiness collection").readiness,
        IssueReadiness::ReviewReady
    );
    let reports = explicit.expect("explicit verification report");
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].checks.len(), 1);
    assert_eq!(reports[0].checks[0].status, "pass");

    let executions = std::fs::read_to_string(counter)
        .expect("check execution counter")
        .lines()
        .count();
    assert_eq!(executions, 1);
}

#[tokio::test]
async fn explicit_verification_errors_when_no_checks_are_produced() {
    let fixture = fixture(None).await;

    let error = weft::readiness::verify_direction(&fixture.db, fixture.direction_id)
        .await
        .expect_err("zero-rung explicit verification must not return empty success");

    assert!(error.to_string().contains("verification was not produced"));
}

#[cfg(unix)]
#[tokio::test]
async fn cached_only_collection_never_starts_checks_or_git_fsmonitor_hooks() {
    let temp = tempfile::tempdir().expect("temporary cached-only fixture root");
    let repo_path = make_repo_with_counting_passing_check(temp.path());
    let fixture = fixture_for_repo(temp, repo_path, None).await;
    let registered = repo::list_worktrees(&fixture.db, Some(fixture.direction_id))
        .await
        .expect("registered worktree");
    assert_eq!(registered.len(), 1, "fixture has one registered worktree");
    let worktree = Path::new(&registered[0].path);
    let counter = worktree.join(".readiness-check-count");
    let fsmonitor = worktree.join("cached-only-fsmonitor.sh");
    let fsmonitor_invoked = PathBuf::from(format!("{}.invoked", fsmonitor.display()));
    std::fs::write(
        &fsmonitor,
        "#!/bin/sh\n: > \"$0.invoked\"\nprintf '\\0'\n",
    )
    .expect("write cached-only fsmonitor hook");
    let mut permissions = std::fs::metadata(&fsmonitor)
        .expect("cached-only fsmonitor metadata")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&fsmonitor, permissions)
        .expect("make cached-only fsmonitor executable");
    git(
        worktree,
        &[
            "config",
            "core.fsmonitor",
            fsmonitor.to_str().expect("utf8 fsmonitor path"),
        ],
    );

    // This is the same policy used by the read-only global `issue_status`
    // tool. An empty cache must be fail-closed without starting the package
    // script.
    let without_memo = tokio::time::timeout(
        Duration::from_millis(500),
        weft::readiness::collect_with_check_execution(
            &fixture.db,
            &fixture.bus,
            &fixture.asks,
            fixture.thread_id,
            CheckExecution::CachedOnly,
        ),
    )
    .await
    .expect("cached-only collection must not run the check")
    .expect("cached-only readiness");
    assert_eq!(without_memo.readiness, IssueReadiness::Unknown);
    assert_eq!(without_memo.reasons[0].code, ReasonCode::RemoteUnknown);
    assert!(
        !counter.exists(),
        "cached-only collection must not execute a rung"
    );
    assert!(
        !fsmonitor_invoked.exists(),
        "cached-only collection must not execute repository-configured Git hooks"
    );
}

#[tokio::test]
async fn mixed_lanes_aggregate_to_the_open_ask_verdict() {
    let fixture = fixture(None).await;
    let second = repo::create_direction(
        &fixture.db,
        fixture.thread_id,
        "second implementation",
        "claude",
        fixture.repo_id,
        "second lane",
        "impl-only",
        "main",
    )
    .await
    .expect("second direction");
    materialize_direction(&fixture.db, second.id)
        .await
        .expect("second worktree");
    repo::set_direction_status(&fixture.db, second.id, "review")
        .await
        .expect("second review status");
    fixture.bus.ask_human(
        fixture.thread_id,
        &second.id.to_string(),
        "choose a release note",
    );

    let result = weft::readiness::collect(&fixture.db, &fixture.bus, &fixture.asks, fixture.thread_id)
        .await
        .expect("readiness");

    assert_eq!(result.readiness, IssueReadiness::NeedsYou);
    assert_eq!(result.active_lane_count, 2);
    assert_eq!(result.reasons[0].code, ReasonCode::OpenNeed);
    assert_eq!(result.reasons[0].direction_id, Some(second.id));
}

#[tokio::test]
async fn lead_scope_open_ask_blocks_an_otherwise_ready_issue() {
    let fixture = fixture(None).await;
    fixture
        .bus
        .ask_human(fixture.thread_id, weft::bus::LEAD, "confirm issue scope");

    let result = weft::readiness::collect(&fixture.db, &fixture.bus, &fixture.asks, fixture.thread_id)
        .await
        .expect("readiness");

    assert_eq!(result.readiness, IssueReadiness::NeedsYou);
    let issue_ask = result
        .lanes
        .iter()
        .find(|lane| lane.name == "issue ask")
        .expect("issue-level ask lane");
    assert_eq!(issue_ask.direction_id, 0);
    assert_eq!(issue_ask.readiness, LaneReadiness::NeedsYou);
    assert_eq!(issue_ask.reasons[0].code, ReasonCode::OpenNeed);
    assert_eq!(issue_ask.reasons[0].direction_id, None);
}

#[tokio::test]
async fn unmapped_open_ask_scope_is_issue_wide_fail_closed() {
    let fixture = fixture(None).await;
    fixture.bus.ask_human(
        fixture.thread_id,
        "retired-non-numeric-scope",
        "recover issue scope",
    );

    let result = weft::readiness::collect(&fixture.db, &fixture.bus, &fixture.asks, fixture.thread_id)
        .await
        .expect("readiness");

    assert_eq!(result.readiness, IssueReadiness::NeedsYou);
    assert!(result.lanes.iter().any(|lane| {
        lane.name == "issue ask"
            && lane.readiness == LaneReadiness::NeedsYou
            && lane.reasons.first().is_some_and(|reason| {
                reason.code == ReasonCode::OpenNeed && reason.direction_id.is_none()
            })
    }));
}

#[tokio::test]
async fn permission_ask_for_a_current_direction_blocks_that_lane() {
    let fixture = fixture(None).await;
    let (_id, _answer) = fixture.asks.request(
        fixture.thread_id,
        &fixture.direction_id.to_string(),
        "shell",
        "Run: protected command",
        "protected command",
        weft::ask::RiskLevel::Unknown,
        "protected command",
    );

    let result = weft::readiness::collect(
        &fixture.db,
        &fixture.bus,
        &fixture.asks,
        fixture.thread_id,
    )
    .await
    .expect("readiness");

    assert_eq!(result.readiness, IssueReadiness::NeedsYou);
    let lane = result
        .lanes
        .iter()
        .find(|lane| lane.direction_id == fixture.direction_id)
        .expect("permission ask lane");
    assert_eq!(lane.readiness, LaneReadiness::NeedsYou);
    assert_eq!(lane.reasons[0].code, ReasonCode::OpenNeed);
    assert_eq!(lane.reasons[0].direction_id, Some(fixture.direction_id));
}

#[tokio::test]
async fn stale_permission_scope_is_an_issue_wide_open_need() {
    let fixture = fixture(None).await;
    let (_id, _answer) = fixture.asks.request(
        fixture.thread_id,
        "retired-permission-direction",
        "shell",
        "Run: protected command",
        "protected command",
        weft::ask::RiskLevel::Unknown,
        "protected command",
    );

    let result = weft::readiness::collect(
        &fixture.db,
        &fixture.bus,
        &fixture.asks,
        fixture.thread_id,
    )
    .await
    .expect("readiness");

    assert_eq!(result.readiness, IssueReadiness::NeedsYou);
    assert!(result.lanes.iter().any(|lane| {
        lane.name == "issue ask"
            && lane.readiness == LaneReadiness::NeedsYou
            && lane.reasons.first().is_some_and(|reason| {
                reason.code == ReasonCode::OpenNeed && reason.direction_id.is_none()
            })
    }));
}

#[tokio::test]
async fn confirmed_policy_allows_readiness() {
    let fixture = fixture(Some("confirmed")).await;
    add_passing_build_script(&fixture).await;
    let result = weft::readiness::collect(&fixture.db, &fixture.bus, &fixture.asks, fixture.thread_id)
        .await
        .expect("readiness");

    assert_eq!(result.readiness, IssueReadiness::ReviewReady);
}

#[tokio::test]
async fn proposed_policy_needs_gate() {
    let fixture = fixture(Some("proposed")).await;
    let result = weft::readiness::collect(&fixture.db, &fixture.bus, &fixture.asks, fixture.thread_id)
        .await
        .expect("readiness");

    assert_eq!(result.readiness, IssueReadiness::NeedsYou);
    assert_eq!(result.reasons[0].code, ReasonCode::PolicyGatePending);
}

#[tokio::test]
async fn proposed_materialized_reuse_stays_gated_until_the_plan_is_confirmed() {
    let fixture = fixture(None).await;
    add_passing_build_script(&fixture).await;
    let reused_lane = proposed_lane("reused implementation", "", fixture.direction_id);
    persist_plan(
        &fixture.db,
        fixture.thread_id,
        "proposed",
        vec![reused_lane],
    )
    .await;

    let proposed = weft::readiness::collect(
        &fixture.db,
        &fixture.bus,
        &fixture.asks,
        fixture.thread_id,
    )
    .await
    .expect("proposed readiness");
    assert_eq!(proposed.readiness, IssueReadiness::NeedsYou);
    assert_eq!(proposed.reasons[0].code, ReasonCode::PolicyGatePending);

    persist_plan(
        &fixture.db,
        fixture.thread_id,
        "confirmed",
        vec![proposed_lane(
            "reused implementation",
            "",
            fixture.direction_id,
        )],
    )
    .await;
    let confirmed = weft::readiness::collect(
        &fixture.db,
        &fixture.bus,
        &fixture.asks,
        fixture.thread_id,
    )
    .await
    .expect("confirmed readiness");
    assert_eq!(confirmed.readiness, IssueReadiness::ReviewReady);
}

#[tokio::test]
async fn withdrawn_plan_uses_the_legacy_direction_path() {
    let fixture = fixture(Some("withdrawn")).await;
    add_passing_build_script(&fixture).await;
    let result = weft::readiness::collect(&fixture.db, &fixture.bus, &fixture.asks, fixture.thread_id)
        .await
        .expect("readiness");

    assert_eq!(result.readiness, IssueReadiness::ReviewReady);
    assert_eq!(result.active_lane_count, 1);
    assert_eq!(result.lanes[0].direction_id, fixture.direction_id);
    assert_eq!(result.lanes[0].readiness, LaneReadiness::ReviewReady);
}

#[tokio::test]
async fn proposal_decisions_enumerate_materialized_virtual_and_denied_lanes() {
    let fixture = fixture(None).await;
    repo::set_direction_status(&fixture.db, fixture.direction_id, "working")
        .await
        .expect("working status");
    persist_plan(
        &fixture.db,
        fixture.thread_id,
        "proposed",
        vec![
            proposed_lane(
                "materialized implementation",
                "approved",
                fixture.direction_id,
            ),
            proposed_lane("pending release note", "", 0),
            proposed_lane("denied experiment", "denied", 0),
        ],
    )
    .await;

    let result = weft::readiness::collect(&fixture.db, &fixture.bus, &fixture.asks, fixture.thread_id)
        .await
        .expect("readiness");

    assert_eq!(result.readiness, IssueReadiness::NeedsYou);
    assert_eq!(result.active_lane_count, 2);
    let materialized = result
        .lanes
        .iter()
        .find(|lane| lane.direction_id == fixture.direction_id)
        .expect("materialized lane");
    assert_eq!(materialized.readiness, LaneReadiness::Unknown);
    assert_eq!(materialized.reasons[0].code, ReasonCode::InProgress);
    let pending = result
        .lanes
        .iter()
        .find(|lane| lane.name == "pending release note")
        .expect("pending virtual lane");
    assert_eq!(pending.direction_id, 0);
    assert_eq!(pending.readiness, LaneReadiness::NeedsYou);
    assert_eq!(pending.reasons[0].code, ReasonCode::PolicyGatePending);
    assert_eq!(pending.reasons[0].direction_id, None);
    assert!(result
        .lanes
        .iter()
        .all(|lane| lane.name != "denied experiment"));
}

#[tokio::test]
async fn unsupported_materialized_proposal_decision_needs_a_policy_gate() {
    let fixture = fixture(None).await;
    persist_plan(
        &fixture.db,
        fixture.thread_id,
        "confirmed",
        vec![proposed_lane(
            "implementation",
            "appr0ved",
            fixture.direction_id,
        )],
    )
    .await;

    let result = weft::readiness::collect(&fixture.db, &fixture.bus, &fixture.asks, fixture.thread_id)
        .await
        .expect("readiness");

    assert_eq!(result.readiness, IssueReadiness::NeedsYou);
    let lane = result
        .lanes
        .iter()
        .find(|lane| lane.direction_id == fixture.direction_id)
        .expect("materialized lane");
    assert_eq!(lane.readiness, LaneReadiness::NeedsYou);
    assert_eq!(lane.reasons[0].code, ReasonCode::PolicyGatePending);
    assert_eq!(lane.reasons[0].direction_id, Some(fixture.direction_id));
}

#[tokio::test]
async fn all_pending_proposal_lanes_are_active_needs_you_lanes() {
    let db = Db::connect("sqlite::memory:").await.expect("memory db");
    let workspace = repo::create_workspace(&db, "pending workspace")
        .await
        .expect("workspace");
    let thread = repo::create_thread(&db, workspace.id, "pending issue", "feature", "claude")
        .await
        .expect("thread");
    persist_plan(
        &db,
        thread.id,
        "proposed",
        vec![
            proposed_lane("pending implementation", "", 0),
            proposed_lane("pending release notes", "", 0),
        ],
    )
    .await;

    let bus = BusRegistry::new();
    let asks = AskRegistry::new();
    let result = weft::readiness::collect(&db, &bus, &asks, thread.id)
        .await
        .expect("readiness");

    assert_eq!(result.readiness, IssueReadiness::NeedsYou);
    assert_eq!(result.active_lane_count, 2);
    assert!(result
        .lanes
        .iter()
        .all(|lane| lane.readiness == LaneReadiness::NeedsYou));
    assert!(result
        .reasons
        .iter()
        .all(|reason| reason.code == ReasonCode::PolicyGatePending));
}

#[tokio::test]
async fn empty_stored_proposal_fail_closes_existing_directions_to_a_gate() {
    let fixture = fixture(None).await;
    persist_plan(&fixture.db, fixture.thread_id, "proposed", Vec::new()).await;

    let result = weft::readiness::collect(&fixture.db, &fixture.bus, &fixture.asks, fixture.thread_id)
        .await
        .expect("readiness");

    assert_eq!(result.readiness, IssueReadiness::NeedsYou);
    assert_eq!(result.active_lane_count, 1);
    assert_eq!(result.lanes[0].direction_id, fixture.direction_id);
    assert_eq!(result.reasons[0].code, ReasonCode::PolicyGatePending);
    assert_eq!(result.reasons[0].direction_id, Some(fixture.direction_id));
}

#[tokio::test]
async fn malformed_stored_proposal_json_fail_closes_existing_directions_to_a_gate() {
    let fixture = fixture(None).await;
    repo::upsert_plan(
        &fixture.db,
        fixture.thread_id,
        "not-a-planner-proposal",
        "proposed",
        "0",
    )
    .await
    .expect("persist malformed proposal");

    let result = weft::readiness::collect(&fixture.db, &fixture.bus, &fixture.asks, fixture.thread_id)
        .await
        .expect("readiness");

    assert_eq!(result.readiness, IssueReadiness::NeedsYou);
    assert_eq!(result.active_lane_count, 1);
    assert_eq!(result.lanes[0].direction_id, fixture.direction_id);
    assert_eq!(result.reasons[0].code, ReasonCode::PolicyGatePending);
    assert_eq!(result.reasons[0].direction_id, Some(fixture.direction_id));
}

#[tokio::test]
async fn unreferenced_direction_in_a_parseable_proposal_remains_legacy_allowed() {
    let fixture = fixture(None).await;
    persist_plan(
        &fixture.db,
        fixture.thread_id,
        "confirmed",
        vec![proposed_lane("unmaterialized approved lane", "approved", 0)],
    )
    .await;
    add_passing_build_script(&fixture).await;

    let result = weft::readiness::collect(&fixture.db, &fixture.bus, &fixture.asks, fixture.thread_id)
        .await
        .expect("readiness");

    let legacy = result
        .lanes
        .iter()
        .find(|lane| lane.direction_id == fixture.direction_id)
        .expect("unreferenced legacy direction");
    assert_eq!(legacy.readiness, LaneReadiness::ReviewReady);
    assert!(legacy.reasons.is_empty());
}

#[tokio::test]
async fn missing_materialized_direction_becomes_a_remote_unknown_virtual_lane() {
    let fixture = fixture(None).await;
    let deleted_direction_id = 42_424;
    persist_plan(
        &fixture.db,
        fixture.thread_id,
        "confirmed",
        vec![proposed_lane(
            "deleted implementation",
            "approved",
            deleted_direction_id,
        )],
    )
    .await;

    let result = weft::readiness::collect(&fixture.db, &fixture.bus, &fixture.asks, fixture.thread_id)
        .await
        .expect("readiness");

    assert_eq!(result.readiness, IssueReadiness::Unknown);
    let missing = result
        .lanes
        .iter()
        .find(|lane| lane.direction_id == deleted_direction_id)
        .expect("missing materialized lane");
    assert_eq!(missing.readiness, LaneReadiness::Unknown);
    assert_eq!(missing.reasons[0].code, ReasonCode::RemoteUnknown);
    assert_eq!(missing.reasons[0].direction_id, Some(deleted_direction_id));
}

#[tokio::test]
async fn duplicate_proposal_reference_with_denial_excludes_the_direction() {
    let fixture = fixture(None).await;
    persist_plan(
        &fixture.db,
        fixture.thread_id,
        "proposed",
        vec![
            proposed_lane("approved duplicate", "approved", fixture.direction_id),
            proposed_lane("denied duplicate", "denied", fixture.direction_id),
        ],
    )
    .await;

    let result = weft::readiness::collect(&fixture.db, &fixture.bus, &fixture.asks, fixture.thread_id)
        .await
        .expect("readiness");

    assert_eq!(result.readiness, IssueReadiness::Unknown);
    assert_eq!(result.active_lane_count, 0);
    assert!(result.lanes.is_empty());
    assert_eq!(result.reasons[0].code, ReasonCode::NoActiveLanes);
}

#[tokio::test]
async fn persisted_merged_upstream_releases_the_consumer() {
    let (fixture, host) = fixture_with_origin(None).await;
    let upstream = add_review_direction(&fixture, "upstream implementation").await;
    add_passing_build_script(&fixture).await;
    repo::set_direction_upstream(&fixture.db, fixture.direction_id, upstream.id)
        .await
        .expect("persisted upstream edge");
    insert_upstream_pr(&fixture, upstream.id, &host, 701, "merged").await;

    assert_eq!(
        repo::upstream_merge_state(&fixture.db, fixture.direction_id).await,
        UpstreamStatus::Merged
    );
    let result = weft::readiness::collect(&fixture.db, &fixture.bus, &fixture.asks, fixture.thread_id)
        .await
        .expect("readiness");

    assert_eq!(result.readiness, IssueReadiness::ReviewReady);
    let consumer = result
        .lanes
        .iter()
        .find(|lane| lane.direction_id == fixture.direction_id)
        .expect("consumer lane");
    assert_eq!(consumer.readiness, LaneReadiness::ReviewReady);
}

#[tokio::test]
async fn persisted_pending_or_unregistered_upstream_blocks_the_consumer() {
    for lifecycle in [Some("open"), None] {
        let (fixture, host) = fixture_with_origin(None).await;
        let upstream = add_review_direction(&fixture, "upstream implementation").await;
        repo::set_direction_upstream(&fixture.db, fixture.direction_id, upstream.id)
            .await
            .expect("persisted upstream edge");
        if let Some(lifecycle) = lifecycle {
            insert_upstream_pr(&fixture, upstream.id, &host, 702, lifecycle).await;
        }

        assert!(matches!(
            repo::upstream_merge_state(&fixture.db, fixture.direction_id).await,
            UpstreamStatus::Pending { .. }
        ));
        let result = weft::readiness::collect(&fixture.db, &fixture.bus, &fixture.asks, fixture.thread_id)
            .await
            .expect("readiness");

        assert_eq!(result.readiness, IssueReadiness::Blocked);
        assert_eq!(result.reasons[0].code, ReasonCode::UpstreamUnmet);
        let consumer = result
            .lanes
            .iter()
            .find(|lane| lane.direction_id == fixture.direction_id)
            .expect("consumer lane");
        assert_eq!(consumer.readiness, LaneReadiness::Blocked);
        assert_eq!(consumer.reasons[0].code, ReasonCode::UpstreamUnmet);
    }
}

#[tokio::test]
async fn merged_tracked_pr_outranks_an_unmet_upstream() {
    let (fixture, _) = fixture_with_origin(None).await;
    let upstream = add_review_direction(&fixture, "upstream implementation").await;
    add_passing_build_script_for_direction(&fixture, upstream.id).await;
    repo::set_direction_upstream(&fixture.db, fixture.direction_id, upstream.id)
        .await
        .expect("persisted upstream edge");
    insert_direction_pr(&fixture, 905, "merged").await;

    assert!(matches!(
        repo::upstream_merge_state(&fixture.db, fixture.direction_id).await,
        UpstreamStatus::Pending { .. }
    ));
    let result = weft::readiness::collect(&fixture.db, &fixture.bus, &fixture.asks, fixture.thread_id)
        .await
        .expect("readiness");

    assert_eq!(result.readiness, IssueReadiness::ReviewReady);
    let consumer = result
        .lanes
        .iter()
        .find(|lane| lane.direction_id == fixture.direction_id)
        .expect("consumer lane");
    assert_eq!(consumer.readiness, LaneReadiness::ReviewReady);
}

#[tokio::test]
async fn persisted_dangling_upstream_edge_is_remote_unknown() {
    let (fixture, _) = fixture_with_origin(None).await;
    let _unrelated_direction = add_review_direction(&fixture, "unrelated implementation").await;
    repo::set_direction_upstream(&fixture.db, fixture.direction_id, 4242)
        .await
        .expect("persisted dangling upstream edge");

    assert!(matches!(
        repo::upstream_merge_state(&fixture.db, fixture.direction_id).await,
        UpstreamStatus::Unknown { .. }
    ));
    let result = weft::readiness::collect(&fixture.db, &fixture.bus, &fixture.asks, fixture.thread_id)
        .await
        .expect("readiness");

    assert_eq!(result.readiness, IssueReadiness::Unknown);
    assert_eq!(result.reasons[0].code, ReasonCode::RemoteUnknown);
    let consumer = result
        .lanes
        .iter()
        .find(|lane| lane.direction_id == fixture.direction_id)
        .expect("consumer lane");
    assert_eq!(consumer.readiness, LaneReadiness::Unknown);
    assert_eq!(consumer.reasons[0].code, ReasonCode::RemoteUnknown);
}

#[tokio::test]
async fn never_probed_pr_with_last_error_is_remote_unknown() {
    let fixture = fixture(None).await;
    pull_request::ActiveModel {
        thread_id: Set(fixture.thread_id),
        direction_id: Set(fixture.direction_id),
        repo_id: Set(fixture.repo_id),
        host_kind: Set("github".to_string()),
        host_base: Set("github.com".to_string()),
        host_owner: Set("example".to_string()),
        host_repo: Set("readiness".to_string()),
        number: Set(171),
        url: Set("https://github.com/example/readiness/pull/171".to_string()),
        title: Set("readiness".to_string()),
        head_sha: Set(String::new()),
        base_ref: Set("main".to_string()),
        lifecycle: Set("open".to_string()),
        ci_status: Set(String::new()),
        review_status: Set(String::new()),
        thread_status: Set(String::new()),
        conflict_status: Set(String::new()),
        merge_readiness: Set(String::new()),
        last_checked_at: Set(String::new()),
        last_error: Set("gh is unavailable".to_string()),
        probe_fail_count: Set(1),
        created_at: Set("0".to_string()),
        ..Default::default()
    }
    .insert(&fixture.db.0)
    .await
    .expect("tracked pr");

    let result = weft::readiness::collect(&fixture.db, &fixture.bus, &fixture.asks, fixture.thread_id)
        .await
        .expect("readiness");

    assert_eq!(result.readiness, IssueReadiness::Unknown);
    assert_eq!(result.reasons[0].code, ReasonCode::RemoteUnknown);
}

#[tokio::test]
async fn unbound_failing_pr_blocks_an_otherwise_ready_issue() {
    let fixture = fixture(None).await;
    add_passing_build_script(&fixture).await;
    let checked_at = unix_secs();
    insert_unbound_pr(&fixture, 801, "open", CiStatus::Failing, &checked_at).await;

    let result = weft::readiness::collect(&fixture.db, &fixture.bus, &fixture.asks, fixture.thread_id)
        .await
        .expect("readiness");

    assert_eq!(result.readiness, IssueReadiness::Blocked);
    let local_lane = result
        .lanes
        .iter()
        .find(|lane| lane.direction_id == fixture.direction_id)
        .expect("local lane");
    assert_eq!(local_lane.readiness, LaneReadiness::ReviewReady);
    let unbound = result
        .lanes
        .iter()
        .find(|lane| lane.name == "unbound PR")
        .expect("unbound PR lane");
    assert_eq!(unbound.direction_id, 0);
    assert_eq!(unbound.readiness, LaneReadiness::Blocked);
    assert_eq!(unbound.reasons[0].code, ReasonCode::PrCiFailing);
    assert_eq!(unbound.reasons[0].direction_id, None);
}

#[tokio::test]
async fn stale_unbound_pr_is_remote_unknown() {
    let fixture = fixture(None).await;
    insert_unbound_pr(&fixture, 802, "open", CiStatus::Passing, "1").await;

    let result = weft::readiness::collect(&fixture.db, &fixture.bus, &fixture.asks, fixture.thread_id)
        .await
        .expect("readiness");

    assert_eq!(result.readiness, IssueReadiness::Unknown);
    let unbound = result
        .lanes
        .iter()
        .find(|lane| lane.name == "unbound PR")
        .expect("unbound PR lane");
    assert_eq!(unbound.readiness, LaneReadiness::Unknown);
    assert_eq!(unbound.reasons[0].code, ReasonCode::RemoteUnknown);
    assert_eq!(unbound.reasons[0].direction_id, None);
}

#[tokio::test]
async fn merged_clear_unbound_pr_does_not_change_ready_issue() {
    let fixture = fixture(None).await;
    add_passing_build_script(&fixture).await;
    insert_unbound_pr(&fixture, 803, "merged", CiStatus::Passing, "").await;

    let result = weft::readiness::collect(&fixture.db, &fixture.bus, &fixture.asks, fixture.thread_id)
        .await
        .expect("readiness");

    assert_eq!(result.readiness, IssueReadiness::ReviewReady);
    assert_eq!(result.active_lane_count, 1);
    assert!(result.lanes.iter().all(|lane| lane.direction_id != 0));
}

#[tokio::test]
async fn merged_tracked_pr_keeps_a_reclaimed_done_lane_review_ready() {
    let fixture = fixture(None).await;
    repo::set_direction_status(&fixture.db, fixture.direction_id, "done")
        .await
        .expect("done status");
    insert_direction_pr(&fixture, 904, "merged").await;
    // Done-card cleanup keeps the worktree row but deletes the checkout. That
    // is unknown reconciliation evidence, which terminal merge must outrank.
    remove_registered_worktree_directories(&fixture).await;

    let ready = weft::readiness::collect(&fixture.db, &fixture.bus, &fixture.asks, fixture.thread_id)
        .await
        .expect("readiness after worktree reclaim");
    assert_eq!(ready.readiness, IssueReadiness::ReviewReady);

    fixture.bus.ask_human(
        fixture.thread_id,
        &fixture.direction_id.to_string(),
        "acknowledge post-merge follow-up",
    );
    let needs_you = weft::readiness::collect(&fixture.db, &fixture.bus, &fixture.asks, fixture.thread_id)
        .await
        .expect("readiness with post-merge ask");
    assert_eq!(needs_you.readiness, IssueReadiness::NeedsYou);
    assert_eq!(needs_you.reasons[0].code, ReasonCode::OpenNeed);
}

#[tokio::test]
async fn branch_mismatch_is_execution_drift() {
    let fixture = fixture(None).await;
    let counter = add_counting_build_script(&fixture).await;
    let direction = repo::get_direction(&fixture.db, fixture.direction_id)
        .await
        .expect("direction query")
        .expect("direction exists");
    let mut active: direction::ActiveModel = direction.into();
    active.branch = Set("different-branch".to_string());
    active
        .update(&fixture.db.0)
        .await
        .expect("direction branch update");

    let result = weft::readiness::collect(&fixture.db, &fixture.bus, &fixture.asks, fixture.thread_id)
        .await
        .expect("readiness");

    assert_eq!(result.readiness, IssueReadiness::Blocked);
    assert_eq!(result.reasons[0].code, ReasonCode::ExecutionDrifted);
    assert!(
        !counter.exists(),
        "a drifted checkout must not start the readiness check process"
    );
}

// ---------------------------------------------------------------------
// Evidence ledger collection (issue #174 R1-04)
// ---------------------------------------------------------------------

#[tokio::test]
async fn branch_switch_produces_drifted_execution_evidence() {
    let fixture = fixture(None).await;
    let direction = repo::get_direction(&fixture.db, fixture.direction_id)
        .await
        .expect("direction query")
        .expect("direction exists");
    let mut active: direction::ActiveModel = direction.into();
    active.branch = Set("different-branch".to_string());
    active
        .update(&fixture.db.0)
        .await
        .expect("direction branch update");

    weft::readiness::collect(&fixture.db, &fixture.bus, &fixture.asks, fixture.thread_id)
        .await
        .expect("readiness");

    let rows = repo::list_evidence(&fixture.db, fixture.thread_id, Some(fixture.direction_id), 10)
        .await
        .expect("evidence after branch switch");
    let execution_rows: Vec<_> = rows
        .iter()
        .filter(|row| row.kind == weft::store::repo::EVIDENCE_KIND_EXECUTION)
        .collect();
    assert_eq!(execution_rows.len(), 1, "one execution evidence row: {rows:?}");
    let row = execution_rows[0];
    assert_eq!(row.collection_state, weft::store::repo::EVIDENCE_COLLECTION_OK);
    assert!(row.payload.contains("\"result\":\"drifted\""), "{}", row.payload);
    assert!(
        row.payload.contains("different-branch"),
        "declared branch must be traceable: {}",
        row.payload
    );
    // Drift evidence is the acceptance criterion: it must not be silently
    // overwritten by a LATER CLI-reported success — appending, not editing
    // in place, is what makes that possible.
    assert_eq!(row.superseded_by, 0, "the drift row is still the current fact");
}

#[tokio::test]
async fn a_later_reported_success_supersedes_but_never_erases_a_drift_row() {
    let fixture = fixture(None).await;
    let direction = repo::get_direction(&fixture.db, fixture.direction_id)
        .await
        .expect("direction query")
        .expect("direction exists");
    let mut active: direction::ActiveModel = direction.into();
    active.branch = Set("different-branch".to_string());
    active
        .update(&fixture.db.0)
        .await
        .expect("direction branch update");
    weft::readiness::collect(&fixture.db, &fixture.bus, &fixture.asks, fixture.thread_id)
        .await
        .expect("readiness while drifted");
    let drifted_rows =
        repo::list_evidence(&fixture.db, fixture.thread_id, Some(fixture.direction_id), 10)
            .await
            .expect("evidence while drifted");
    let drift_row = drifted_rows
        .iter()
        .find(|row| row.kind == weft::store::repo::EVIDENCE_KIND_EXECUTION)
        .expect("drift evidence row")
        .clone();

    // The CLI "reports success" by the branch matching again (the only
    // channel readiness has for a worker's self-report of completion).
    let mut active: direction::ActiveModel = repo::get_direction(&fixture.db, fixture.direction_id)
        .await
        .expect("direction query")
        .expect("direction exists")
        .into();
    active.branch = Set(format!("feature/readiness-{}", fixture.direction_id));
    active
        .update(&fixture.db.0)
        .await
        .expect("direction branch restored");
    weft::readiness::collect(&fixture.db, &fixture.bus, &fixture.asks, fixture.thread_id)
        .await
        .expect("readiness after branch restored");

    let rows_after =
        repo::list_evidence(&fixture.db, fixture.thread_id, Some(fixture.direction_id), 10)
            .await
            .expect("evidence after restore");
    let execution_rows: Vec<_> = rows_after
        .iter()
        .filter(|row| row.kind == weft::store::repo::EVIDENCE_KIND_EXECUTION)
        .collect();
    assert_eq!(execution_rows.len(), 2, "the drift row must still exist: {rows_after:?}");
    let still_present_drift_row = execution_rows
        .iter()
        .find(|row| row.id == drift_row.id)
        .expect("the original drift row was not deleted");
    assert!(
        still_present_drift_row.payload.contains("\"result\":\"drifted\""),
        "the drift row's content is never rewritten: {}",
        still_present_drift_row.payload
    );
    assert_ne!(
        still_present_drift_row.superseded_by, 0,
        "the drift row is now superseded, but still present"
    );
    let matched_row = execution_rows
        .iter()
        .find(|row| row.id != drift_row.id)
        .expect("a new matched row was appended");
    assert!(matched_row.payload.contains("\"result\":\"matched\""), "{}", matched_row.payload);
    assert_eq!(matched_row.superseded_by, 0);
}

#[tokio::test]
async fn an_unreadable_extra_worktree_produces_unknown_collection_state_not_success() {
    let fixture = fixture(None).await;
    remove_registered_worktree_directories(&fixture).await;

    let result = weft::readiness::collect(&fixture.db, &fixture.bus, &fixture.asks, fixture.thread_id)
        .await
        .expect("readiness with an unreadable worktree");
    assert_eq!(result.readiness, IssueReadiness::Unknown);
    assert_eq!(result.reasons[0].code, ReasonCode::RemoteUnknown);

    let rows = repo::list_evidence(&fixture.db, fixture.thread_id, Some(fixture.direction_id), 10)
        .await
        .expect("evidence with an unreadable worktree");
    let execution_rows: Vec<_> = rows
        .iter()
        .filter(|row| row.kind == weft::store::repo::EVIDENCE_KIND_EXECUTION)
        .collect();
    assert_eq!(execution_rows.len(), 1, "{rows:?}");
    let row = execution_rows[0];
    assert_eq!(
        row.collection_state,
        weft::store::repo::EVIDENCE_COLLECTION_UNKNOWN,
        "a failed/unreadable collection attempt must never look like success"
    );
    assert!(row.payload.contains("\"result\":\"unknown\""), "{}", row.payload);
    assert!(row.payload.contains("\"readable\":false"), "{}", row.payload);
}

#[tokio::test]
async fn a_dirty_worktree_is_recorded_in_the_observed_set_without_changing_the_matched_verdict() {
    let fixture = fixture(None).await;
    let registered = repo::list_worktrees(&fixture.db, Some(fixture.direction_id))
        .await
        .expect("registered worktrees");
    assert_eq!(registered.len(), 1);
    // An UNCOMMITTED, out-of-scope change: the branch still matches the
    // Lane's declared branch (Matched, per readiness's own reconciliation
    // semantics — untouched by this issue), but the dirty file must still be
    // traceable in the execution evidence's observed set.
    std::fs::write(
        Path::new(&registered[0].path).join("scratch.md"),
        "uncommitted out-of-scope note\n",
    )
    .expect("uncommitted scratch file");

    let result = weft::readiness::collect(&fixture.db, &fixture.bus, &fixture.asks, fixture.thread_id)
        .await
        .expect("readiness with a dirty worktree");
    assert_eq!(
        result.readiness,
        IssueReadiness::Unknown,
        "a dirty worktree cannot produce fresh check evidence, but is still Matched, not Drifted"
    );

    let rows = repo::list_evidence(&fixture.db, fixture.thread_id, Some(fixture.direction_id), 10)
        .await
        .expect("evidence with a dirty worktree");
    let execution_row = rows
        .iter()
        .find(|row| row.kind == weft::store::repo::EVIDENCE_KIND_EXECUTION)
        .expect("execution evidence row");
    assert!(execution_row.payload.contains("\"result\":\"matched\""), "{}", execution_row.payload);
    assert!(
        execution_row.payload.contains("\"dirty\":true"),
        "the out-of-scope uncommitted change must be in the observed set: {}",
        execution_row.payload
    );
}

#[tokio::test]
async fn verification_evidence_is_recorded_and_the_old_row_goes_stale_after_a_new_commit() {
    let fixture = fixture(None).await;
    add_passing_build_script(&fixture).await;

    let first = weft::readiness::collect(&fixture.db, &fixture.bus, &fixture.asks, fixture.thread_id)
        .await
        .expect("readiness after first check run");
    assert_eq!(first.readiness, IssueReadiness::ReviewReady);

    let rows_after_first =
        repo::list_evidence(&fixture.db, fixture.thread_id, Some(fixture.direction_id), 10)
            .await
            .expect("evidence after first run");
    let verification_rows_first: Vec<_> = rows_after_first
        .iter()
        .filter(|row| row.kind == weft::store::repo::EVIDENCE_KIND_VERIFICATION)
        .collect();
    assert_eq!(verification_rows_first.len(), 1, "one fresh verification row: {rows_after_first:?}");
    let first_row = verification_rows_first[0].clone();
    assert!(!first_row.revision.is_empty());

    // A new commit changes HEAD — the check flight's signature cache must not
    // reuse the old evidence.
    let registered = repo::list_worktrees(&fixture.db, Some(fixture.direction_id))
        .await
        .expect("registered worktrees");
    let worktree_path = Path::new(&registered[0].path);
    std::fs::write(worktree_path.join("NOTES.md"), "follow-up notes\n").expect("notes file");
    git(worktree_path, &["add", "NOTES.md"]);
    git(worktree_path, &["commit", "-q", "-m", "second commit"]);

    let second = weft::readiness::collect(&fixture.db, &fixture.bus, &fixture.asks, fixture.thread_id)
        .await
        .expect("readiness after second commit");
    assert_eq!(second.readiness, IssueReadiness::ReviewReady);

    let rows_after_second =
        repo::list_evidence(&fixture.db, fixture.thread_id, Some(fixture.direction_id), 10)
            .await
            .expect("evidence after second run");
    let verification_rows_after: Vec<_> = rows_after_second
        .iter()
        .filter(|row| row.kind == weft::store::repo::EVIDENCE_KIND_VERIFICATION)
        .collect();
    assert_eq!(
        verification_rows_after.len(),
        2,
        "the second commit appends a NEW row rather than mutating the old one: {rows_after_second:?}"
    );
    let newest = verification_rows_after
        .iter()
        .find(|row| row.superseded_by == 0)
        .expect("current row");
    let superseded = verification_rows_after
        .iter()
        .find(|row| row.id != newest.id)
        .expect("superseded row");
    assert_eq!(superseded.id, first_row.id);
    assert_ne!(newest.revision, first_row.revision);
    assert_eq!(superseded.superseded_by, newest.id);

    // The stale/supersede truth table, exercised end-to-end (issue #174's
    // "commit 变化后旧检查证据会变 stale" acceptance criterion): the OLD row
    // reads stale against the NEW head even judged independent of clock time.
    assert_eq!(
        weft::store::repo::evidence_freshness(superseded, Some(newest.revision.as_str()), 0, None),
        weft::store::repo::EvidenceFreshness::Stale,
    );
    assert_eq!(
        weft::store::repo::evidence_freshness(newest, Some(newest.revision.as_str()), 0, None),
        weft::store::repo::EvidenceFreshness::Fresh,
    );
}

#[tokio::test]
async fn evidence_survives_direction_deletion_and_is_still_listable() {
    let fixture = fixture(None).await;
    let direction = repo::get_direction(&fixture.db, fixture.direction_id)
        .await
        .expect("direction query")
        .expect("direction exists");
    let mut active: direction::ActiveModel = direction.into();
    active.branch = Set("different-branch".to_string());
    active
        .update(&fixture.db.0)
        .await
        .expect("direction branch update");
    weft::readiness::collect(&fixture.db, &fixture.bus, &fixture.asks, fixture.thread_id)
        .await
        .expect("readiness while drifted");
    let before = repo::list_evidence(&fixture.db, fixture.thread_id, Some(fixture.direction_id), 10)
        .await
        .expect("evidence before deletion");
    assert!(!before.is_empty());

    direction::Entity::delete_by_id(fixture.direction_id)
        .exec(&fixture.db.0)
        .await
        .expect("delete direction (evidence retention is independent of the FK-less direction_id)");

    let after = repo::list_evidence(&fixture.db, fixture.thread_id, Some(fixture.direction_id), 10)
        .await
        .expect("evidence after deletion must not error on a dangling direction_id");
    assert_eq!(after.len(), before.len(), "evidence is retained after Lane removal");
}
