//! Drives the bus HTTP MCP handler exactly like a CLI client would: initialize,
//! tools/list, then two directions exchanging a message.
use weft::ask::AskRegistry;
use weft::bus::{server, BusRegistry};
use weft::store::Db;

async fn rpc(base: &str, thread: i32, dir: &str, body: serde_json::Value) -> String {
    let url = format!("{base}/bus/{thread}/{dir}/mcp");
    let resp = reqwest::Client::new()
        .post(url)
        .header("Accept", "application/json, text/event-stream")
        .json(&body)
        .send()
        .await
        .unwrap();
    resp.text().await.unwrap()
}

#[tokio::test]
async fn two_directions_exchange_a_message() {
    let reg = BusRegistry::new();
    let db = Db::connect("sqlite::memory:").await.unwrap();
    let asks = AskRegistry::new();
    let (base, _h) = server::serve(reg, db, asks).await.unwrap();

    // both directions initialize (registers membership)
    for dir in ["10", "20"] {
        let out = rpc(
            &base,
            1,
            dir,
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
        1,
        "10",
        serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}),
    )
    .await;
    assert!(tl.contains("bus_post") && tl.contains("bus_inbox"));

    // dir 10 posts to dir 20
    rpc(
        &base,
        1,
        "10",
        serde_json::json!({"jsonrpc":"2.0","id":3,"method":"tools/call",
            "params":{"name":"bus_post","arguments":{"to":"20","text":"hello-20"}}}),
    )
    .await;

    // dir 20 reads its inbox -> sees the message
    let inbox = rpc(
        &base,
        1,
        "20",
        serde_json::json!({"jsonrpc":"2.0","id":4,"method":"tools/call",
            "params":{"name":"bus_inbox","arguments":{}}}),
    )
    .await;
    assert!(
        inbox.contains("hello-20"),
        "inbox should contain the posted message: {inbox}"
    );
    assert!(inbox.contains("\\\"from\\\":\\\"10\\\"") || inbox.contains("\"from\":\"10\""));
}
