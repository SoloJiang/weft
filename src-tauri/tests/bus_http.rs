//! Drives the bus HTTP MCP handler exactly like a CLI client would: initialize,
//! tools/list, then two directions exchanging a message.
use weft::ask::AskRegistry;
use weft::bus::{server, BusRegistry};
use weft::store::Db;

async fn rpc_for_session(
    base: &str,
    thread: i32,
    dir: &str,
    session_id: Option<i32>,
    body: serde_json::Value,
) -> String {
    let mut url = format!("{base}/bus/{thread}/{dir}/mcp");
    if let Some(session_id) = session_id {
        url.push_str(&format!("?session_id={session_id}"));
    }
    let resp = reqwest::Client::builder()
        // This test talks to a loopback listener it just created. Do not let a
        // machine-level proxy reroute that request (and make the test depend on
        // a developer's local proxy availability).
        .no_proxy()
        .build()
        .unwrap()
        .post(url)
        .header("Accept", "application/json, text/event-stream")
        .json(&body)
        .send()
        .await
        .unwrap();
    resp.text().await.unwrap()
}

async fn rpc(base: &str, thread: i32, dir: &str, body: serde_json::Value) -> String {
    rpc_for_session(base, thread, dir, None, body).await
}

#[tokio::test]
async fn two_directions_exchange_a_message() {
    let reg = BusRegistry::new();
    let db = Db::connect("sqlite::memory:").await.unwrap();
    let workspace = weft::store::repo::create_workspace(&db, "bus exchange")
        .await
        .unwrap();
    let repo = weft::store::repo::add_repo_ref(
        &db,
        workspace.id,
        "repo",
        "/tmp/bus-exchange",
        "main",
        "",
        true,
    )
    .await
    .unwrap();
    let thread = weft::store::repo::create_thread(
        &db,
        workspace.id,
        "bus exchange",
        "feature",
        "codex",
    )
    .await
    .unwrap();
    let first_direction = weft::store::repo::create_direction(
        &db,
        thread.id,
        "first",
        "codex",
        repo.id,
        "why",
        "impl-only",
        "",
    )
    .await
    .unwrap();
    let second_direction = weft::store::repo::create_direction(
        &db,
        thread.id,
        "second",
        "codex",
        repo.id,
        "why",
        "impl-only",
        "",
    )
    .await
    .unwrap();
    let first_session = weft::store::repo::create_session(
        &db,
        first_direction.id,
        repo.id,
        "codex",
        "/tmp/bus-exchange-first",
    )
    .await
    .unwrap();
    let second_session = weft::store::repo::create_session(
        &db,
        second_direction.id,
        repo.id,
        "codex",
        "/tmp/bus-exchange-second",
    )
    .await
    .unwrap();
    let asks = AskRegistry::new();
    let (base, _h) = server::serve(reg, db, asks).await.unwrap();
    let first = first_direction.id.to_string();
    let second = second_direction.id.to_string();

    // both directions initialize (registers membership)
    for (dir, session_id) in [(&first, first_session.id), (&second, second_session.id)] {
        let out = rpc_for_session(
            &base,
            thread.id,
            dir,
            Some(session_id),
            serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}),
        )
        .await;
        assert!(
            out.contains("weft_bus"),
            "initialize must return serverInfo"
        );
    }

    // tools/list exposes bus_post
    let tl = rpc(
        &base,
        thread.id,
        &first,
        serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}),
    )
    .await;
    assert!(tl.contains("bus_post") && tl.contains("bus_inbox"));

    // dir 10 posts to dir 20
    rpc_for_session(
        &base,
        thread.id,
        &first,
        Some(first_session.id),
        serde_json::json!({"jsonrpc":"2.0","id":3,"method":"tools/call",
            "params":{"name":"bus_post","arguments":{"to":second,"text":"hello-20"}}}),
    )
    .await;

    // dir 20 reads its inbox -> sees the message
    let inbox = rpc_for_session(
        &base,
        thread.id,
        &second,
        Some(second_session.id),
        serde_json::json!({"jsonrpc":"2.0","id":4,"method":"tools/call",
            "params":{"name":"bus_inbox","arguments":{}}}),
    )
    .await;
    assert!(
        inbox.contains("hello-20"),
        "inbox should contain the posted message: {inbox}"
    );
    let escaped_from = format!("\\\"from\\\":\\\"{}\\\"", first_direction.id);
    let raw_from = format!("\"from\":\"{}\"", first_direction.id);
    assert!(inbox.contains(&escaped_from) || inbox.contains(&raw_from));
}

/// issue #96: an engine/model switch cancels any ask still pending for the
/// (thread, dir) being torn down (`AskRegistry::cancel_for`) so it doesn't sit
/// in Needs-you for up to an hour waiting on an engine that no longer exists.
/// The Ask Bridge's own response to that cancellation must be an EXPLICIT,
/// well-formed deny — not the bare `{}` this endpoint used to return on
/// timeout/cancel, which every weft-spawned engine (always headless) has no
/// sane way to fall back from, and which was confirmed fail-open for TWO of
/// the three hook consumers (see `handle_ask`'s doc for the code-level
/// citations): opencode's hook plugin reads it as an implicit ALLOW (only the
/// literal string "deny" throws), and codex's global hook script exits 0 with
/// no output, which Codex's own hook contract treats as success/continue.
#[tokio::test]
async fn ask_bridge_cancel_returns_explicit_deny() {
    let reg = BusRegistry::new();
    let db = Db::connect("sqlite::memory:").await.unwrap();
    let workspace = weft::store::repo::create_workspace(&db, "ask bridge")
        .await
        .unwrap();
    let repo = weft::store::repo::add_repo_ref(
        &db,
        workspace.id,
        "repo",
        "/tmp/ask-bridge",
        "main",
        "",
        true,
    )
    .await
    .unwrap();
    let thread = weft::store::repo::create_thread(
        &db,
        workspace.id,
        "ask bridge",
        "feature",
        "claude",
    )
    .await
    .unwrap();
    let direction = weft::store::repo::create_direction(
        &db,
        thread.id,
        "worker",
        "claude",
        repo.id,
        "why",
        "impl-only",
        "",
    )
    .await
    .unwrap();
    let session = weft::store::repo::create_session(
        &db,
        direction.id,
        repo.id,
        "claude",
        "/tmp/ask-bridge-worker",
    )
    .await
    .unwrap();
    let asks = AskRegistry::new();
    let (base, _h) = server::serve(reg, db, asks.clone()).await.unwrap();

    let url = format!(
        "{base}/ask/{}/{}?tool=claude&session_id={}",
        thread.id, direction.id, session.id
    );
    let req = reqwest::Client::new()
        .post(&url)
        .json(&serde_json::json!({"tool_name": "Bash", "tool_input": {"command": "ls"}}))
        .send();
    let req = tokio::spawn(req);

    // Poll rather than a fixed sleep: wait for the ask to actually register as
    // open before cancelling it, bounded so a regression fails fast instead of
    // hanging the suite.
    let mut waited = std::time::Duration::ZERO;
    let step = std::time::Duration::from_millis(20);
    while asks.open().is_empty() {
        tokio::time::sleep(step).await;
        waited += step;
        assert!(waited < std::time::Duration::from_secs(5), "ask never registered as open");
    }
    assert_eq!(asks.open().len(), 1);

    // Mirrors what a tool/model switch does to a stale ask on the engine it is
    // tearing down.
    asks.cancel_for(thread.id, &direction.id.to_string());

    let resp = req.await.unwrap().unwrap();
    assert!(resp.status().is_success());
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(
        body["hookSpecificOutput"]["permissionDecision"], "deny",
        "cancel must resolve to an explicit, well-formed deny, not an ambiguous empty body: {body}"
    );
    assert!(asks.open().is_empty());
}
