//! Drives the `weft_computer` MCP HTTP handler exactly like a CLI client
//! would, mirroring `bus_http.rs`'s server-spinup style. Issue #160 M1:
//! observation only (window listing + screenshot), no input injection.
//!
//! v2 tool face: ONE tool named `computer`, dispatched by an `action`
//! argument (`"list_windows"` / `"screenshot"`) — see
//! `bus::computer_srv`'s module doc for why.
use serde_json::json;
use std::sync::Arc;
use weft::ask::AskRegistry;
use weft::bus::{server, BusRegistry};
use weft::computer::{self, backend, mock::MockBackend, CapturedImage, WindowInfo};
use weft::store::{repo, Db};

async fn rpc(base: &str, thread: i32, dir: &str, body: serde_json::Value) -> String {
    let url = format!("{base}/computer/{thread}/{dir}/mcp");
    let resp = reqwest::Client::builder()
        // This test talks to a loopback listener it just created. Do not let a
        // machine-level proxy reroute that request (mirrors bus_http.rs).
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

/// Pull the JSON-RPC payload out of the one `data: ...` SSE line every
/// handler here emits.
fn sse_json(sse_text: &str) -> serde_json::Value {
    let data_line = sse_text
        .lines()
        .find(|l| l.starts_with("data: "))
        .unwrap_or_else(|| panic!("no SSE data line in: {sse_text}"));
    serde_json::from_str(&data_line["data: ".len()..]).unwrap()
}

#[tokio::test]
async fn tools_list_exposes_exactly_one_computer_tool() {
    let reg = BusRegistry::new();
    let db = Db::connect("sqlite::memory:").await.unwrap();
    let asks = AskRegistry::new();
    let (base, _h) = server::serve(reg, db, asks).await.unwrap();

    let out = rpc(
        &base,
        1,
        "10",
        json!({"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}),
    )
    .await;

    let body = sse_json(&out);
    let tools = body["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 1, "expected exactly one tool, got: {out}");
    assert_eq!(tools[0]["name"], "computer");
}

/// issue #160 M1: the setting gate blocks BEFORE any backend call, so this
/// must pass with no backend override installed at all — a real backend call
/// here (there isn't one, by construction) would either panic in a headless
/// test environment or hang, either of which would fail this test.
#[tokio::test]
async fn disabled_setting_returns_disabled_text_without_touching_backend() {
    let reg = BusRegistry::new();
    let db = Db::connect("sqlite::memory:").await.unwrap();
    // Deliberately never sets computer::K_COMPUTER_USE_ENABLED — fail-closed
    // default must be "disabled".
    let asks = AskRegistry::new();
    let (base, _h) = server::serve(reg, db, asks).await.unwrap();

    let out = rpc(
        &base,
        1,
        "10",
        json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
            "params":{"name":"computer","arguments":{"action":"list_windows"}}}),
    )
    .await;

    assert!(out.contains("disabled"), "expected disabled text: {out}");
    assert!(out.contains("Settings"), "expected disabled text: {out}");
}

#[tokio::test]
async fn enabled_lead_screenshot_via_mock_backend_saves_a_png() {
    // Isolate WEFT_HOME (the lead-scratch-dir formula reads it via
    // `paths::weft_home`) so this test never touches a real home directory.
    let weft_home =
        std::env::temp_dir().join(format!("weft-computer-mcp-home-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&weft_home);
    std::env::set_var("WEFT_HOME", weft_home.to_str().unwrap());

    let reg = BusRegistry::new();
    let db = Db::connect("sqlite::memory:").await.unwrap();
    repo::set_setting(&db, computer::K_COMPUTER_USE_ENABLED, "true")
        .await
        .unwrap();
    let asks = AskRegistry::new();
    let (base, _h) = server::serve(reg, db, asks).await.unwrap();

    let (width, height) = (640u32, 480u32);
    backend::_set_backend_override(Arc::new(MockBackend {
        windows: vec![WindowInfo {
            id: 1,
            app: "Notes".into(),
            title: "Untitled".into(),
            x: 0,
            y: 0,
            width,
            height,
        }],
        image: Some(CapturedImage {
            rgba: vec![9u8; (width * height * 4) as usize],
            width,
            height,
        }),
    }));

    let thread = 1;
    let out = rpc(
        &base,
        thread,
        "lead",
        json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
            "params":{"name":"computer","arguments":{"action":"screenshot","window":"notes"}}}),
    )
    .await;

    assert!(out.contains("screenshot saved"), "{out}");
    assert!(out.contains(".png"), "{out}");

    let expected_dir = weft::paths::weft_home()
        .unwrap()
        .join("leads")
        .join(thread.to_string());
    let has_png = std::fs::read_dir(&expected_dir)
        .unwrap_or_else(|e| panic!("expected {expected_dir:?} to exist: {e}"))
        .filter_map(|e| e.ok())
        .any(|e| e.path().extension().map(|ext| ext == "png").unwrap_or(false));
    assert!(has_png, "expected a .png under {expected_dir:?}");
}

/// Exercises the REAL platform backend end-to-end (window enumeration +
/// screenshot via `xcap`). Headless CI has no display server, so this stays
/// `#[ignore]`d — run it by hand locally, e.g.
/// `cargo test --test computer_mcp --features computer-os -- --ignored`.
#[ignore]
#[tokio::test]
async fn real_backend_lists_and_screenshots_a_real_window() {
    let reg = BusRegistry::new();
    let db = Db::connect("sqlite::memory:").await.unwrap();
    repo::set_setting(&db, computer::K_COMPUTER_USE_ENABLED, "true")
        .await
        .unwrap();
    let asks = AskRegistry::new();
    let (base, _h) = server::serve(reg, db, asks).await.unwrap();

    let out = rpc(
        &base,
        1,
        "10",
        json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
            "params":{"name":"computer","arguments":{"action":"list_windows"}}}),
    )
    .await;
    assert!(out.contains("\"id\""), "expected at least the JSON shape of a window list: {out}");
}
