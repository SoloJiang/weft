//! Drives the Ask Bridge's real HTTP endpoint the way a CLI's PreToolUse hook
//! does, to prove the read-only-builtin allowlist (issue #96 follow-up) decides
//! the way it claims to END TO END — allowlist ∧ risk verdict ∧ containment in
//! the session's own directories, with weft's DB (not the payload) supplying
//! the directories.
//!
//! Two shapes of assertion, because "allowed" and "asked" are observed
//! differently:
//!
//! - AUTO-APPROVED: the POST returns on its own with an `allow` decision. No
//!   human, no card.
//! - GATED: the POST BLOCKS (the bridge holds the tool call for up to an hour
//!   waiting for the human), so the test watches for the card to appear in the
//!   registry and answers it. A bug that auto-approved one of these would show
//!   up as `wait_for_card` timing out with an already-completed request.

use std::path::Path;
use std::time::Duration;
use weft::ask::{Answer, AskRegistry};
use weft::bus::{server, BusRegistry};
use weft::store::{repo, Db};

/// One temp tree per test, removed on drop so a failing assert can't leak it.
struct TempTree(std::path::PathBuf);

impl TempTree {
    fn new(tag: &str) -> Self {
        let p = std::env::temp_dir().join(format!("weft-ask-allow-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        TempTree(std::fs::canonicalize(&p).unwrap())
    }

    fn dir(&self, rel: &str) -> std::path::PathBuf {
        let p = self.0.join(rel);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn file(&self, rel: &str) -> String {
        let p = self.0.join(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&p, b"x").unwrap();
        p.to_string_lossy().to_string()
    }
}

impl Drop for TempTree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn file_in(dir: &Path, rel: &str) -> String {
    let p = dir.join(rel);
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(&p, b"x").unwrap();
    p.to_string_lossy().to_string()
}

/// POST a PreToolUse payload exactly as the injected hook script does.
async fn ask(
    base: &str,
    thread: i32,
    dir: &str,
    engine: &str,
    tool_name: &str,
    tool_input: serde_json::Value,
) -> String {
    let url = format!("{base}/ask/{thread}/{dir}?tool={engine}");
    reqwest::Client::new()
        .post(url)
        .json(&serde_json::json!({ "tool_name": tool_name, "tool_input": tool_input }))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap()
}

/// `ask`, for the cases that must come back on their OWN — no human, no card.
///
/// The bound matters: a regression that sends one of these to the human doesn't
/// return an error, it BLOCKS for `ASK_WAIT` (an hour) waiting for a click that
/// no test will ever make. Without this the failure mode is a hung CI job with
/// nothing to read; with it, it's a named assertion. Found by mutating
/// `safe_scope` to match nothing and watching the suite hang instead of fail.
async fn ask_unattended(
    base: &str,
    thread: i32,
    dir: &str,
    engine: &str,
    tool_name: &str,
    tool_input: serde_json::Value,
) -> String {
    match tokio::time::timeout(
        Duration::from_secs(10),
        ask(base, thread, dir, engine, tool_name, tool_input),
    )
    .await
    {
        Ok(body) => body,
        Err(_) => panic!("{engine}/{tool_name} must decide without a human, but it blocked"),
    }
}

/// Wait for the bridge to surface a card, so a GATED case can be answered
/// instead of hanging for `ASK_WAIT` (an hour). Fails the test if none appears —
/// which is exactly how an over-permissive allowlist would present.
async fn wait_for_card(asks: &AskRegistry, what: &str) -> u64 {
    for _ in 0..200 {
        if let Some(a) = asks.open().first() {
            return a.id;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("{what} must surface a Needs-you card, but none appeared");
}

/// A workspace + repo + thread + direction + worktree, wired the way a real
/// dispatch wires them. Returns (base_url, asks, thread_id, direction_id).
async fn worker_session(
    repo_path: &Path,
    worktree_path: &Path,
) -> (String, AskRegistry, i32, i32, tokio::task::JoinHandle<()>) {
    let db = Db::connect("sqlite::memory:").await.unwrap();
    let ws = repo::create_workspace(&db, "w").await.unwrap();
    let r = repo::add_repo_ref(
        &db,
        ws.id,
        "r",
        &repo_path.to_string_lossy(),
        "main",
        "",
        true,
    )
    .await
    .unwrap();
    let t = repo::create_thread(&db, ws.id, "issue", "issue", "claude")
        .await
        .unwrap();
    let d = repo::create_direction(&db, t.id, "task", "claude", r.id, "why", "impl-only", "main")
        .await
        .unwrap();
    repo::record_worktree(
        &db,
        r.id,
        d.id,
        "wt-branch",
        &worktree_path.to_string_lossy(),
        true,
        true,
        "",
    )
    .await
    .unwrap();
    let asks = AskRegistry::new();
    let (base, h) = server::serve(BusRegistry::new(), db, asks.clone())
        .await
        .unwrap();
    (base, asks, t.id, d.id, h)
}

/// The storm this change exists to end: a worker reading its own worktree.
#[tokio::test]
async fn read_inside_the_worktree_is_auto_approved() {
    let tree = TempTree::new("inside");
    let wt = tree.dir("wt");
    let target = file_in(&wt, "src/main.rs");
    let (base, _asks, thread, dir, _h) = worker_session(&tree.dir("repo"), &wt).await;

    let out = ask_unattended(
        &base,
        thread,
        &dir.to_string(),
        "claude",
        "Read",
        serde_json::json!({ "file_path": target }),
    )
    .await;
    assert!(
        out.contains("\"permissionDecision\":\"allow\""),
        "a read inside the worktree must not cost a human click, got {out}"
    );
    // Grep/Glob too — including the no-`path` form that defaults to the
    // session's own cwd.
    for (tool, input) in [
        ("Grep", serde_json::json!({ "pattern": "TODO" })),
        (
            "Glob",
            serde_json::json!({ "pattern": "**/*.rs", "path": wt.to_string_lossy() }),
        ),
    ] {
        let out = ask_unattended(&base, thread, &dir.to_string(), "claude", tool, input).await;
        assert!(
            out.contains("\"permissionDecision\":\"allow\""),
            "{tool} inside the worktree must be auto-approved, got {out}"
        );
    }
}

/// Containment is the load-bearing half: the SAME tool pointed outside the
/// session's directories must still stop for the human. Auto-approving here
/// would make weft MORE permissive than claude's own default, which prompts for
/// reads outside the working directory.
#[tokio::test]
async fn read_outside_the_worktree_still_surfaces_the_card() {
    let tree = TempTree::new("outside");
    let wt = tree.dir("wt");
    // Deliberately DULL: an ordinary source file, so the ONLY thing that can
    // refuse it is containment. A credential-shaped name (`id_rsa`, `.env`)
    // would be caught by the risk veto instead and this test would pass with
    // containment removed entirely — verified by mutation, not assumed.
    let outside = tree.file("elsewhere/notes.rs");
    let (base, asks, thread, dir, _h) = worker_session(&tree.dir("repo"), &wt).await;

    let base2 = base.clone();
    let dir_s = dir.to_string();
    let call = tokio::spawn(async move {
        ask(
            &base2,
            thread,
            &dir_s,
            "claude",
            "Read",
            serde_json::json!({ "file_path": outside }),
        )
        .await
    });

    let id = wait_for_card(&asks, "a read outside the worktree").await;
    assert!(asks.answer(id, Answer::Deny));
    let out = call.await.unwrap();
    assert!(
        out.contains("\"permissionDecision\":\"deny\""),
        "the human's Deny must reach the engine, got {out}"
    );
}

/// The reason the risk verdict is kept as a SECOND condition: a repo's own
/// `.env` sits INSIDE the worktree, so containment alone would wave it through.
#[tokio::test]
async fn credential_file_inside_the_worktree_still_surfaces_the_card() {
    let tree = TempTree::new("dotenv");
    let wt = tree.dir("wt");
    let dotenv = file_in(&wt, ".env");
    let (base, asks, thread, dir, _h) = worker_session(&tree.dir("repo"), &wt).await;

    let base2 = base.clone();
    let dir_s = dir.to_string();
    let call = tokio::spawn(async move {
        ask(
            &base2,
            thread,
            &dir_s,
            "claude",
            "Read",
            serde_json::json!({ "file_path": dotenv }),
        )
        .await
    });

    let id = wait_for_card(&asks, "reading a .env inside the worktree").await;
    assert!(asks.answer(id, Answer::Allow));
    let out = call.await.unwrap();
    assert!(out.contains("\"permissionDecision\":\"allow\""));
}

/// A mutating builtin is untouched by any of this, even on a path the read
/// tools would have been allowed to touch.
#[tokio::test]
async fn write_inside_the_worktree_still_surfaces_the_card() {
    let tree = TempTree::new("write");
    let wt = tree.dir("wt");
    let target = file_in(&wt, "src/main.rs");
    let (base, asks, thread, dir, _h) = worker_session(&tree.dir("repo"), &wt).await;

    let base2 = base.clone();
    let dir_s = dir.to_string();
    let call = tokio::spawn(async move {
        ask(
            &base2,
            thread,
            &dir_s,
            "claude",
            "Write",
            serde_json::json!({ "file_path": target, "content": "x" }),
        )
        .await
    });

    let id = wait_for_card(&asks, "a Write inside the worktree").await;
    assert!(asks.answer(id, Answer::Deny));
    assert!(call.await.unwrap().contains("\"permissionDecision\":\"deny\""));
}

/// A `NoTarget` builtin has nothing to contain, so it is decided on the name
/// alone — no session lookup at all. Asserted against an id pair that resolves
/// to NOTHING, which is what makes "no lookup" observable.
#[tokio::test]
async fn no_target_builtins_need_no_session_lookup() {
    let db = Db::connect("sqlite::memory:").await.unwrap();
    let asks = AskRegistry::new();
    let (base, _h) = server::serve(BusRegistry::new(), db, asks.clone())
        .await
        .unwrap();

    for (engine, tool, input) in [
        (
            "claude",
            "TodoWrite",
            serde_json::json!({"todos":[{"content":"x","status":"pending","activeForm":"x"}]}),
        ),
        ("claude", "ToolSearch", serde_json::json!({"query":"x"})),
        (
            "codex",
            "update_plan",
            serde_json::json!({"plan":[{"step":"x","status":"pending"}]}),
        ),
    ] {
        let out = ask_unattended(&base, 999, "424242", engine, tool, input).await;
        assert!(
            out.contains("\"permissionDecision\":\"allow\""),
            "{engine}/{tool} must be auto-approved on its name alone, got {out}"
        );
    }
}

/// A lead reads across its workspace's repos (its own cwd is an almost-empty
/// scratch dir), and that traffic is the other half of the 23-minute freeze.
/// Guarded by `WEFT_HOME` so the test never touches the real one.
#[tokio::test]
async fn lead_reads_its_workspace_repo_without_asking() {
    let tree = TempTree::new("lead");
    let repo_dir = tree.dir("repo");
    let target = file_in(&repo_dir, "src/lib.rs");
    std::env::set_var("WEFT_HOME", tree.dir("home"));

    let db = Db::connect("sqlite::memory:").await.unwrap();
    let ws = repo::create_workspace(&db, "w").await.unwrap();
    repo::add_repo_ref(
        &db,
        ws.id,
        "r",
        &repo_dir.to_string_lossy(),
        "main",
        "",
        true,
    )
    .await
    .unwrap();
    let t = repo::create_thread(&db, ws.id, "issue", "issue", "claude")
        .await
        .unwrap();
    let asks = AskRegistry::new();
    let (base, _h) = server::serve(BusRegistry::new(), db, asks.clone())
        .await
        .unwrap();

    let out = ask_unattended(
        &base,
        t.id,
        "lead",
        "claude",
        "Read",
        serde_json::json!({ "file_path": target }),
    )
    .await;
    assert!(
        out.contains("\"permissionDecision\":\"allow\""),
        "a lead reading its own workspace repo must not block, got {out}"
    );
}

/// Fail-closed on identity: a direction id that isn't THIS thread's is a stale
/// or forged route, so it resolves to no directories and nothing is contained —
/// even for a path that would be fine for the direction's real thread.
#[tokio::test]
async fn direction_from_another_thread_fails_closed() {
    let tree = TempTree::new("crossthread");
    let wt = tree.dir("wt");
    let target = file_in(&wt, "src/main.rs");
    let (base, asks, thread, dir, _h) = worker_session(&tree.dir("repo"), &wt).await;

    let base2 = base.clone();
    let dir_s = dir.to_string();
    let other_thread = thread + 4242;
    let call = tokio::spawn(async move {
        ask(
            &base2,
            other_thread,
            &dir_s,
            "claude",
            "Read",
            serde_json::json!({ "file_path": target }),
        )
        .await
    });

    let id = wait_for_card(&asks, "a direction routed under the wrong thread").await;
    assert!(asks.answer(id, Answer::Deny));
    assert!(call.await.unwrap().contains("\"permissionDecision\":\"deny\""));
}

/// The same fail-closed identity check, exercised through the form that names
/// NO path — the hole Codex found on PR #146.
///
/// `direction_from_another_thread_fails_closed` only ever sent a `Read` with an
/// explicit `file_path`, so it was caught by the absolute-path rule and never
/// proved anything about the identity check itself. A cwd-defaulting
/// `Grep {"pattern": …}` has nothing for that rule to reject, so a stale or
/// cross-thread route could read from an unverified cwd with no human in the
/// loop. Same wrong-thread route, targetless call: must still surface the card.
#[tokio::test]
async fn targetless_grep_on_a_foreign_route_fails_closed() {
    let tree = TempTree::new("targetless");
    let wt = tree.dir("wt");
    file_in(&wt, "src/main.rs");
    let (base, asks, thread, dir, _h) = worker_session(&tree.dir("repo"), &wt).await;

    // Sanity: the SAME targetless call on the session's REAL route is
    // auto-approved — so the assertion below is about identity, not about
    // targetless calls being refused across the board.
    let ok = ask_unattended(
        &base,
        thread,
        &dir.to_string(),
        "claude",
        "Grep",
        serde_json::json!({ "pattern": "TODO" }),
    )
    .await;
    assert!(
        ok.contains("\"permissionDecision\":\"allow\""),
        "a targetless Grep on its own session must stay auto-approved, got {ok}"
    );

    let base2 = base.clone();
    let dir_s = dir.to_string();
    let other_thread = thread + 4242;
    let call = tokio::spawn(async move {
        ask(
            &base2,
            other_thread,
            &dir_s,
            "claude",
            "Grep",
            serde_json::json!({ "pattern": "TODO" }),
        )
        .await
    });

    let id = wait_for_card(&asks, "a targetless Grep on a foreign route").await;
    assert!(asks.answer(id, Answer::Deny));
    assert!(call.await.unwrap().contains("\"permissionDecision\":\"deny\""));
}
