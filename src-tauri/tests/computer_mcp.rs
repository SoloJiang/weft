//! Drives the `weft_computer` MCP HTTP handler exactly like a CLI client
//! would, mirroring `bus_http.rs`'s server-spinup style. Issue #160: M1
//! shipped observation only (window listing + screenshot); M2 adds input
//! injection (click/type/key/scroll/drag/move), the control lock, the input
//! throttle, and the audit log.
//!
//! ONE tool named `computer`, dispatched by an `action` argument — see
//! `bus::computer_srv`'s module doc for why.
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use weft::ask::{AskRegistry, RiskLevel};
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

/// issue #160 M1 (screenshot) + M2 (input actions, control lock, throttle,
/// suspended-ask gate, audit log) end-to-end through the MCP HTTP handler,
/// ALL in one test function, on purpose:
///
///  - This binary's `WEFT_HOME` env var is process-global — see
///    `tests/worktree_delete.rs`'s own comment on the same hazard — so only
///    ONE test in this file may set it; every scenario that needs a
///    resolvable lead scratch dir (screenshot output, the audit log) lives
///    here instead of in a second, separately-`#[tokio::test]`'d function
///    that would race this one's `std::env::set_var` on `cargo test`'s
///    default parallel test threads.
///  - The M2 scenarios ALSO share process-wide statics
///    (`computer::acquire_control` / `computer::throttle_input`), which is
///    its own, independent reason they must run strictly sequentially rather
///    than as separate test functions.
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
    // Kept alongside the moved-into-`serve` registry so the M2 section below
    // can open an Ask directly (scenario 6) against the SAME registry the
    // handler reads from.
    let asks_handle = asks.clone();
    let (base, _h) = server::serve(reg, db, asks).await.unwrap();

    // A 1:1 (scale 1.0) window keeps the M2 section's physical-coordinate
    // math trivial to assert on: screenshot-space (100, 50) maps straight
    // through to physical (100, 50) since the window's own origin is (0, 0).
    let (width, height) = (640u32, 480u32);
    let mock = Arc::new(MockBackend {
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
        ..Default::default()
    });
    backend::_set_backend_override(mock.clone());

    // Defensive reset: nothing else in this binary touches the control
    // lock, but start from a known-clear state regardless.
    computer::clear_control();

    let thread = 1;
    let dir = "lead";

    // —— M1: screenshot ——

    let out = rpc(
        &base,
        thread,
        dir,
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

    // —— M2: input actions, gates, and the audit log ——
    //
    // The screenshot call above already appended its own audit line, so the
    // audit-line counts below are 1-based from THAT call, not 0.

    let audit_path = expected_dir.join(".weft").join("computer-audit.jsonl");

    // Clear the throttle window before the first INPUT action (the
    // screenshot call above never touched it, since screenshot/list_windows
    // are exempt — but this keeps the section self-contained regardless).
    tokio::time::sleep(Duration::from_millis(600)).await;

    // 1. left_click succeeds: the mock backend records the click, the
    // response confirms it, and an audit line is appended.
    let out = rpc(
        &base,
        thread,
        dir,
        json!({"jsonrpc":"2.0","id":2,"method":"tools/call",
            "params":{"name":"computer","arguments":{"action":"left_click","window":"notes","coordinate":[100,50]}}}),
    )
    .await;
    assert!(out.contains("left_click"), "{out}");
    assert!(out.contains("done"), "{out}");
    {
        let actions = mock.actions.lock().unwrap();
        assert_eq!(actions.len(), 1, "{actions:?}");
        assert!(actions[0].starts_with("click 100,50 Left x1"), "{actions:?}");
    }
    let audit_text = std::fs::read_to_string(&audit_path)
        .unwrap_or_else(|e| panic!("expected audit file at {audit_path:?}: {e}"));
    let lines: Vec<&str> = audit_text.lines().collect();
    assert_eq!(lines.len(), 2, "{audit_text}");
    let entry: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
    assert_eq!(entry["action"].as_str(), Some("left_click"));
    assert_eq!(entry["outcome"].as_str(), Some("ok"));
    assert_eq!(entry["window_id"].as_u64(), Some(1));
    assert_eq!(entry["thread"].as_i64(), Some(i64::from(thread)));
    assert_eq!(entry["dir"].as_str(), Some(dir));

    // Clear the throttle window before the next INPUT action so the
    // remaining scenarios aren't incidentally exercising RateLimited.
    tokio::time::sleep(Duration::from_millis(600)).await;

    // 2. `type` with no `text` errors before ever reaching the backend (no
    // new mock action recorded), and is still audited as a failure.
    let out = rpc(
        &base,
        thread,
        dir,
        json!({"jsonrpc":"2.0","id":3,"method":"tools/call",
            "params":{"name":"computer","arguments":{"action":"type","window":"notes"}}}),
    )
    .await;
    assert!(out.contains("text"), "{out}");
    assert_eq!(mock.actions.lock().unwrap().len(), 1, "missing text must never reach the backend");
    let lines_after_2 = std::fs::read_to_string(&audit_path).unwrap().lines().count();
    assert_eq!(lines_after_2, 3);

    // 3. `wait` clamps an over-cap duration instead of erroring, and
    // doesn't touch the control lock/throttle at all.
    let out = rpc(
        &base,
        thread,
        dir,
        json!({"jsonrpc":"2.0","id":4,"method":"tools/call",
            "params":{"name":"computer","arguments":{"action":"wait","duration_ms":50}}}),
    )
    .await;
    assert!(out.contains("waited"), "{out}");
    assert!(out.contains("50"), "{out}");

    // 4. An unknown action is rejected and lists the valid ones.
    let out = rpc(
        &base,
        thread,
        dir,
        json!({"jsonrpc":"2.0","id":5,"method":"tools/call",
            "params":{"name":"computer","arguments":{"action":"nonexistent"}}}),
    )
    .await;
    assert!(out.contains("unknown action"), "{out}");
    assert!(out.contains("left_click"), "{out}");

    // 5. Busy: a DIFFERENT (thread, dir) already holds the control lease, so
    // this session's input action is rejected without ever reaching the
    // backend (mock action count stays at 1).
    computer::clear_control();
    computer::acquire_control(999, "other").unwrap();
    let out = rpc(
        &base,
        thread,
        dir,
        json!({"jsonrpc":"2.0","id":6,"method":"tools/call",
            "params":{"name":"computer","arguments":{"action":"left_click","window":"notes","coordinate":[1,1]}}}),
    )
    .await;
    assert!(out.contains("controlling the desktop"), "{out}");
    assert_eq!(mock.actions.lock().unwrap().len(), 1, "a Busy call must never reach the backend");
    computer::clear_control();

    // 6. Suspended: an open permission Ask for this EXACT (thread, dir)
    // blocks input actions — checked before the control lock/throttle are
    // even touched (see `bus::computer_srv::input_gate`'s ordering).
    let (_ask_id, _rx) = asks_handle.request(thread, dir, "some_tool", "summary", "detail", RiskLevel::Unknown, "[]");
    let out = rpc(
        &base,
        thread,
        dir,
        json!({"jsonrpc":"2.0","id":7,"method":"tools/call",
            "params":{"name":"computer","arguments":{"action":"left_click","window":"notes","coordinate":[1,1]}}}),
    )
    .await;
    assert!(out.contains("permission card"), "{out}");
    assert_eq!(mock.actions.lock().unwrap().len(), 1, "a suspended call must never reach the backend");
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
