//! Drives the `weft_computer` MCP HTTP handler exactly like a CLI client
//! would, mirroring `bus_http.rs`'s server-spinup style. Issue #160: M1
//! shipped observation only (window listing + screenshot); M2 adds input
//! injection (click/type/key/scroll/drag/move), the control lock, the input
//! throttle, and the audit log; round-2 P1 adds a server-side approval gate
//! in front of every `tools/call` — see `bus::computer_srv::approve`'s doc.
//!
//! ONE tool named `computer`, dispatched by an `action` argument — see
//! `bus::computer_srv`'s module doc for why.
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use weft::ask::{Answer, AskRegistry, FullGrant, GrantSnapshot, RiskLevel};
use weft::bus::{computer_srv, server, BusRegistry};
use weft::computer::{self, backend, mock::MockBackend, CapturedImage, WindowInfo};
use weft::store::{repo, Db};

/// issue #160 round-11 P1 #A: every real call into `/computer/:thread/:dir/
/// mcp` now needs `?key=<per-session token>` — built the SAME way
/// `bus::inject::computer_url` itself would (`computer_srv::
/// computer_session_token`, `#[doc(hidden)] pub` for exactly this reason).
/// Centralized here so every scenario in this file that drives the endpoint
/// through [`rpc`] gets it for free; the handful of call sites that build
/// their own URL (the `?wt=` scenarios below) attach it explicitly.
fn key_query(thread: i32, dir: &str) -> String {
    // No `?wt=` in the URL → the bearer is bound to the absent-worktree case
    // (issue #160 round-13/14 P1). The handful of `?wt=` scenarios below mint
    // their own key for the exact `Some(wt)` their URL carries.
    format!("key={}", computer_srv::computer_session_token(thread, dir, None))
}

async fn rpc(base: &str, thread: i32, dir: &str, body: serde_json::Value) -> String {
    let url = format!("{base}/computer/{thread}/{dir}/mcp?{}", key_query(thread, dir));
    rpc_url(&url, body).await
}

/// Same POST as [`rpc`], against a caller-built URL — lets the round-2 P2 §5
/// `?wt=` query-param tests below attach a worktree id without `rpc`'s own
/// `dir`-only URL shape getting in the way.
async fn rpc_url(url: &str, body: serde_json::Value) -> String {
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

/// Wait for the Ask Bridge to surface a card and return it — the handler's
/// POST blocks until answered, so the caller spawns it, waits here, inspects/
/// answers the card, then awaits the spawned call. Mirrors `tests/
/// ask_builtin_allow.rs`'s own `wait_for_card`, but returns the full `Ask`
/// (not just its id) since M3-B's assertions need `Ask::preview`.
async fn wait_for_card(asks: &AskRegistry, what: &str) -> weft::ask::Ask {
    for _ in 0..200 {
        if let Some(a) = asks.open().first() {
            return a.clone();
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("{what} must surface a Needs-you card, but none appeared");
}

/// Spawn a `tools/call` POST to the `weft_computer` MCP endpoint in the
/// background and return its `JoinHandle` — issue #160 round-2 P1's
/// server-side approval gate blocks the handler until a card is answered
/// (or a standing grant auto-decides it), so the caller spawns this,
/// `wait_for_card`s, inspects/answers the card, then `.await`s the handle
/// for the final response text. `coordinate` is always included (harmless
/// for `screenshot`/`list_windows`, which ignore it) so one helper covers
/// every action this file's gate scenarios need.
fn spawn_computer_call(
    base: &str,
    thread: i32,
    dir: String,
    action: &'static str,
    window: &'static str,
) -> tokio::task::JoinHandle<String> {
    let base = base.to_string();
    tokio::spawn(async move {
        // issue #160 round-33 P1: `coordinate` only rides actions that consume
        // it — `pure_validate`'s per-action allowlist now rejects unrecognized
        // arguments outright, before any card.
        let arguments = if action == "screenshot" {
            json!({"action": action, "window": window})
        } else {
            json!({"action": action, "window": window, "coordinate": [1, 1]})
        };
        rpc(
            &base,
            thread,
            &dir,
            json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
                "params":{"name":"computer","arguments": arguments}}),
        )
        .await
    })
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

// —— issue #160 round-11 P1 #A: per-session bearer on `/computer/:thread/:dir/mcp` ——

/// The correct-key happy path, in isolation from every OTHER test in this
/// file (which all get it for free via [`rpc`]/[`key_query`]): a request
/// carrying the EXACT token `computer_session_token` would mint for this
/// path's own `(thread, dir)` reaches the real handler and gets a normal
/// `tools/list` response — proving the auth gate is not, itself, blocking a
/// legitimately-keyed caller.
#[tokio::test]
async fn endpoint_auth_allows_the_correct_per_session_key() {
    let reg = BusRegistry::new();
    let db = Db::connect("sqlite::memory:").await.unwrap();
    let asks = AskRegistry::new();
    let (base, _h) = server::serve(reg, db, asks).await.unwrap();

    let url = format!("{base}/computer/1/10/mcp?{}", key_query(1, "10"));
    let out = rpc_url(
        &url,
        json!({"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}),
    )
    .await;
    let body = sse_json(&out);
    assert_eq!(body["result"]["tools"][0]["name"], "computer", "{out}");
}

/// A request with NO `?key=` at all — standing in for the URL leaking to (or
/// being guessed by) a caller with no access to the real per-session token —
/// must be rejected outright, as a bare HTTP 401, never an SSE/JSON-RPC body
/// of any shape (which would itself leak information about method/id
/// handling to an unauthenticated caller).
#[tokio::test]
async fn endpoint_auth_rejects_a_request_with_no_key_at_all() {
    let reg = BusRegistry::new();
    let db = Db::connect("sqlite::memory:").await.unwrap();
    let asks = AskRegistry::new();
    let (base, _h) = server::serve(reg, db, asks).await.unwrap();

    let url = format!("{base}/computer/1/10/mcp");
    let resp = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .post(&url)
        .header("Accept", "application/json, text/event-stream")
        .json(&json!({"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::UNAUTHORIZED, "a keyless request must get a bare 401");
}

/// A `?key=` present but WRONG (neither empty nor the real token for this
/// path) must be rejected the identical way a missing one is.
#[tokio::test]
async fn endpoint_auth_rejects_a_wrong_key() {
    let reg = BusRegistry::new();
    let db = Db::connect("sqlite::memory:").await.unwrap();
    let asks = AskRegistry::new();
    let (base, _h) = server::serve(reg, db, asks).await.unwrap();

    let url = format!("{base}/computer/1/10/mcp?key=0000000000000000000000000000000000000000000000000000000000000000");
    let resp = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .post(&url)
        .header("Accept", "application/json, text/event-stream")
        .json(&json!({"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::UNAUTHORIZED, "a wrong key must get a bare 401");
}

/// The exact hazard #A closes: a token minted for a DIFFERENT (thread, dir)
/// — standing in for a same-uid process reading (or guessing) SOME other
/// session's own injected URL — must not authorize THIS (thread, dir): the
/// token is bound to the path's own identity, not just "a valid-looking
/// token for SOME session".
#[tokio::test]
async fn endpoint_auth_rejects_a_key_minted_for_a_different_thread_dir() {
    let reg = BusRegistry::new();
    let db = Db::connect("sqlite::memory:").await.unwrap();
    let asks = AskRegistry::new();
    let (base, _h) = server::serve(reg, db, asks).await.unwrap();

    // A key that is VALID for (thread=1, dir="10")...
    let foreign_key = computer_srv::computer_session_token(1, "10", None);
    // ...used to call a DIFFERENT (thread=2, dir="20").
    let url = format!("{base}/computer/2/20/mcp?key={foreign_key}");
    let resp = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .post(&url)
        .header("Accept", "application/json, text/event-stream")
        .json(&json!({"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        reqwest::StatusCode::UNAUTHORIZED,
        "a key minted for a DIFFERENT (thread, dir) must never authorize this one"
    );
}

/// issue #160 round-15 P1 (Codex inject.rs:364): the SAME per-session token is
/// accepted as `Authorization: Bearer <token>` with NO `?key=` at all — the
/// channel codex uses via `bearer_token_env_var`, so its argv never carries the
/// secret. A wrong bearer is a bare 401 exactly like a wrong `?key=`.
#[tokio::test]
async fn endpoint_auth_accepts_the_bearer_header_and_rejects_a_wrong_one() {
    let reg = BusRegistry::new();
    let db = Db::connect("sqlite::memory:").await.unwrap();
    let asks = AskRegistry::new();
    let (base, _h) = server::serve(reg, db, asks).await.unwrap();

    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let url = format!("{base}/computer/1/10/mcp");

    // The correct token for THIS (thread, dir, absent-wt), via the header only.
    let token = computer_srv::computer_session_token(1, "10", None);
    let resp = client
        .post(&url)
        .header("Accept", "application/json, text/event-stream")
        .header("Authorization", format!("Bearer {token}"))
        .json(&json!({"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        reqwest::StatusCode::OK,
        "the bearer header must authorize exactly like ?key= does"
    );
    let body = resp.text().await.unwrap();
    assert!(body.contains("\"computer\""), "tools/list must answer through the bearer path: {body}");

    // A wrong bearer fails closed with the same bare 401.
    let resp = client
        .post(&url)
        .header("Accept", "application/json, text/event-stream")
        .header("Authorization", "Bearer 0000000000000000000000000000000000000000000000000000000000000000")
        .json(&json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::UNAUTHORIZED);
}

/// `computer_session_token` is exactly what [`inject::computer_url`] appends
/// to the injected URL (proven directly in `bus::inject`'s own unit tests via
/// the identical helper) — this proves the OTHER end of that contract: a
/// `tools/call` carrying that EXACT token for its OWN path is authorized all
/// the way through to a real (denied, for lack of a standing grant) gate
/// card — never short-circuited by the auth check itself. A missing/garbage
/// `wt` combined with a CORRECT key is exercised by the giant test's own
/// section 14 above; this isolates the auth layer from the `wt`-parsing layer
/// entirely (no `wt` at all here).
#[tokio::test]
async fn endpoint_auth_with_the_correct_key_still_reaches_the_real_approval_gate() {
    let reg = BusRegistry::new();
    let db = Db::connect("sqlite::memory:").await.unwrap();
    repo::set_setting(&db, computer::K_COMPUTER_USE_ENABLED, "true").await.unwrap();
    let asks = AskRegistry::new();
    let asks_handle = asks.clone();
    let (base, _h) = server::serve(reg, db, asks).await.unwrap();

    let call = spawn_computer_call(&base, 909_901, "lead".to_string(), "left_click", "notes");
    let card = wait_for_card(&asks_handle, "a correctly-keyed call must still reach the real gate").await;
    assert_eq!(card.tool, "computer");
    assert!(asks_handle.answer(card.id, Answer::Deny));
    let out = call.await.unwrap();
    assert!(out.contains("denied"), "{out}");
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
    // Kept alongside the moved-into-`serve` connection so the M3-B section
    // below can insert its own workspace/thread/direction/worktree rows
    // against the SAME db `server::serve` is reading from.
    let db_handle = db.clone();

    // issue #160 round-2 P1: EVERY `tools/call` now passes through a
    // server-side approval gate (`bus::computer_srv::approve`) before it can
    // reach the backend — including every M1/M2 scenario below, which
    // predate that gate and were never written to answer a card. A Full
    // grant for the primary (thread=1, dir="lead") session used throughout
    // M1/M2 keeps all of that pre-existing coverage focused on what it was
    // actually testing (the screenshot pipeline, the control lock, the
    // throttle, the suspended-ask check) rather than on the gate itself —
    // the gate has its OWN dedicated scenarios further down, on sessions
    // that deliberately start UNGRANTED.
    asks_handle.seed_grants(GrantSnapshot {
        full: vec![FullGrant { thread: 1, dir: "lead".to_string() }],
        always: Vec::new(),
    });

    let (base, _h) = server::serve(reg, db, asks).await.unwrap();

    // A 1:1 (scale 1.0) window keeps the M2 section's physical-coordinate
    // math trivial to assert on: screenshot-space (100, 50) maps straight
    // through to physical (100, 50) since the window's own origin is (0, 0).
    // A second window ("Other", id 2) exists purely for the round-2 P1 §2
    // window-match preview scenario further down — nothing in M1/M2 ever
    // targets it, so its presence doesn't change any existing assertion.
    let (width, height) = (640u32, 480u32);
    let mock = Arc::new(MockBackend {
        windows: vec![
            WindowInfo {
                id: 1,
                app: "Notes".into(),
                title: "Untitled".into(),
                x: 0,
                y: 0,
                width,
                height,
            },
            WindowInfo {
                id: 2,
                app: "Other".into(),
                title: "Untitled".into(),
                x: 0,
                y: 0,
                width,
                height,
            },
        ],
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

    // issue #160 round-15 P2 (Codex computer_srv.rs:2759) gave lead
    // screenshots a dedicated subdirectory; round-16 P1 (Codex
    // computer_srv.rs:2812) then moved the lead's WHOLE output root under
    // Weft-managed storage (`<weft_home>/computer/<thread>/lead`, same shape
    // as the worker lane's `wt-<id>` roots) — the agent-writable lead scratch
    // cwd receives NOTHING computer-related at all anymore.
    let lead_out_root = weft::paths::weft_home()
        .unwrap()
        .join("computer")
        .join(thread.to_string())
        .join("lead");
    let shots_dir = lead_out_root.join("screenshots");
    let has_png = std::fs::read_dir(&shots_dir)
        .unwrap_or_else(|e| panic!("expected {shots_dir:?} to exist: {e}"))
        .filter_map(|e| e.ok())
        .any(|e| e.path().extension().map(|ext| ext == "png").unwrap_or(false));
    assert!(has_png, "expected a .png under {shots_dir:?}");
    let scratch_cwd = weft::paths::weft_home().unwrap().join("leads").join(thread.to_string());
    if let Ok(rd) = std::fs::read_dir(&scratch_cwd) {
        let scratch_has_computer_output = rd.filter_map(|e| e.ok()).any(|e| {
            let p = e.path();
            let is_png = p.extension().map(|ext| ext == "png").unwrap_or(false);
            let is_weft_dir = p.file_name().map(|n| n == ".weft").unwrap_or(false);
            is_png || is_weft_dir
        });
        assert!(
            !scratch_has_computer_output,
            "the agent-writable lead scratch cwd must receive no computer-use output at all"
        );
    }

    // —— M2: input actions, gates, and the audit log ——
    //
    // The screenshot call above already appended its own audit line, so the
    // audit-line counts below are 1-based from THAT call, not 0.

    let audit_path = lead_out_root.join("computer-audit.jsonl");

    // Clear the throttle window before the first INPUT action (the
    // screenshot call above never touched it, since screenshot/list_windows
    // are exempt — but this keeps the section self-contained regardless).
    tokio::time::sleep(Duration::from_millis(600)).await;

    // 1. left_click succeeds: the mock backend records the click, the
    // response confirms it, and an audit line is appended. issue #160
    // round-5 review P1 §6: this session is Full-granted (an Auto approval,
    // no card), and `activate_target` now runs UNCONDITIONALLY — so this
    // records TWO backend actions (`activate`, then `click`), not one.
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
        assert_eq!(actions.len(), 2, "{actions:?}");
        assert_eq!(actions[0], "activate 1", "{actions:?}");
        assert!(actions[1].starts_with("click 100,50 Left x1"), "{actions:?}");
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
    assert_eq!(mock.actions.lock().unwrap().len(), 2, "missing text must never reach the backend");
    let lines_after_2 = std::fs::read_to_string(&audit_path).unwrap().lines().count();
    assert_eq!(lines_after_2, 3);

    // 2b. issue #160 round-5 review P2 §3: a `type` payload over
    // `MAX_TYPE_CHARS` is rejected before ever reaching the backend — no new
    // mock action, and the rejection is itself still audited.
    let over_limit_text = "a".repeat(weft::bus::computer_srv::MAX_TYPE_CHARS + 1);
    let out = rpc(
        &base,
        thread,
        dir,
        json!({"jsonrpc":"2.0","id":301,"method":"tools/call",
            "params":{"name":"computer","arguments":{"action":"type","window":"notes","text":over_limit_text}}}),
    )
    .await;
    assert!(out.contains("too long"), "{out}");
    assert_eq!(
        mock.actions.lock().unwrap().len(),
        2,
        "an over-limit type must never reach the backend"
    );
    let lines_after_2b = std::fs::read_to_string(&audit_path).unwrap().lines().count();
    assert_eq!(lines_after_2b, 4);

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
    // backend (mock action count stays at 2, from scenario 1's activate+click).
    computer::clear_control();
    computer::acquire_control(999, "other", None).unwrap();
    let out = rpc(
        &base,
        thread,
        dir,
        json!({"jsonrpc":"2.0","id":6,"method":"tools/call",
            "params":{"name":"computer","arguments":{"action":"left_click","window":"notes","coordinate":[1,1]}}}),
    )
    .await;
    assert!(out.contains("controlling the desktop"), "{out}");
    assert_eq!(mock.actions.lock().unwrap().len(), 2, "a Busy call must never reach the backend");
    computer::clear_control();

    // 6. Suspended: an open permission Ask for this EXACT (thread, dir)
    // blocks input actions — checked before the control lock/throttle are
    // even touched (see `bus::computer_srv::input_gate`'s ordering).
    let (ask_id, _rx) = asks_handle.request(thread, dir, "some_tool", "summary", "detail", RiskLevel::Unknown, "[]");
    let out = rpc(
        &base,
        thread,
        dir,
        json!({"jsonrpc":"2.0","id":7,"method":"tools/call",
            "params":{"name":"computer","arguments":{"action":"left_click","window":"notes","coordinate":[1,1]}}}),
    )
    .await;
    assert!(out.contains("permission card"), "{out}");
    assert_eq!(mock.actions.lock().unwrap().len(), 2, "a suspended call must never reach the backend");
    // Clean up this section's manually-opened ask so it can't leak into the
    // M3-B sections below, which poll `asks.open().first()` and assume the
    // registry starts empty each time.
    assert!(asks_handle.answer(ask_id, Answer::Deny));

    // —— M3-B: engine-gated MCP image content + Ask-card preview (issue #160) ——
    //
    // Reuses the SAME MockBackend/"Notes" window as M1/M2 above — a NEW
    // `MockBackend` would silently no-op (`backend::_set_backend_override` is
    // a `OnceLock`, settable once per process) and a NEW `WEFT_HOME` would
    // race this file's own single-setter rule (see this function's own doc
    // comment) — so every M3-B scenario lives in THIS test too, continuing
    // the M1/M2 sections' numbering. What varies per scenario is the
    // `direction.tool` / `thread.lead_tool` driving each (thread, dir), which
    // is all `engine_accepts_mcp_image` actually looks at.

    // 7. Worker-lane engine gating: `claude` and an ACP backend (`omp`) both
    // get an MCP `image` content block on top of the text confirmation;
    // `codex` gets text only.
    let ws = repo::create_workspace(&db_handle, "ws").await.unwrap();
    let repo_tmp = tempfile::tempdir().unwrap();
    let r = repo::add_repo_ref(&db_handle, ws.id, "r", &repo_tmp.path().to_string_lossy(), "main", "", true)
        .await
        .unwrap();

    async fn worker_dir(db: &Db, ws_id: i32, repo_id: i32, tool: &str) -> (i32, i32) {
        let t = repo::create_thread(db, ws_id, &format!("{tool} issue"), "issue", tool)
            .await
            .unwrap();
        let d = repo::create_direction(db, t.id, "task", tool, repo_id, "why", "impl-only", "main")
            .await
            .unwrap();
        // A plain path (not `tempfile::tempdir()`, whose `TempDir` guard would
        // delete this the instant `worker_dir` returns — the screenshot write
        // this path is FOR happens later, via a separate RPC call) — mirrors
        // this file's own `WEFT_HOME` setup above (unique-per-process-id,
        // wiped first in case a prior run left it behind).
        let wt_path = std::env::temp_dir().join(format!(
            "weft-computer-mcp-wt-{tool}-{}-{}",
            std::process::id(),
            d.id
        ));
        let _ = std::fs::remove_dir_all(&wt_path);
        std::fs::create_dir_all(&wt_path).unwrap();
        repo::record_worktree(db, repo_id, d.id, "wt-branch", &wt_path.to_string_lossy(), true, true, "")
            .await
            .unwrap();
        (t.id, d.id)
    }

    async fn assert_screenshot_content(
        base: &str,
        thread: i32,
        dir: &str,
        rpc_id: i64,
        wants_image: bool,
        what: &str,
    ) {
        let out = rpc(
            base,
            thread,
            dir,
            json!({"jsonrpc":"2.0","id":rpc_id,"method":"tools/call",
                "params":{"name":"computer","arguments":{"action":"screenshot","window":"notes"}}}),
        )
        .await;
        let body = sse_json(&out);
        let content = body["result"]["content"].as_array().unwrap_or_else(|| panic!("{what}: {out}"));
        assert_eq!(content[0]["type"], "text", "{what}: text block must always be first — {content:?}");
        if wants_image {
            assert_eq!(content.len(), 2, "{what}: expected text + image — {content:?}");
            assert_eq!(content[1]["type"], "image");
            assert_eq!(content[1]["mimeType"], "image/jpeg");
            let data = content[1]["data"].as_str().unwrap();
            assert!(!data.starts_with("data:"), "{what}: MCP image content must be raw base64, no data: prefix");
            use base64::Engine as _;
            assert!(
                base64::engine::general_purpose::STANDARD.decode(data).is_ok(),
                "{what}: image data must be valid base64"
            );
        } else {
            assert_eq!(content.len(), 1, "{what}: expected text-only — {content:?}");
        }
    }

    // issue #160 round-2 P1: `screenshot` now passes through the same
    // approval gate as every other action, so `assert_screenshot_content`
    // below (which awaits its RPC call synchronously, with nobody spawned to
    // answer a card) needs a standing grant for the EXACT `screenshot @
    // notes` action to keep behaving the way it did before the gate existed.
    // Deliberately an Always grant scoped to that ONE action_key — NOT a
    // blanket `read_only_session` grant — so section 9 below can still
    // exercise a REAL card for a DIFFERENT action (a click) or a DIFFERENT
    // window (`screenshot @ other`) on these SAME sessions.
    //
    // issue #160 round-2 P2 §2: `action_key` grew a trailing `args_digest`
    // element (a sha256 over the call's consequential params — see that
    // function's own doc), so this pre-seeded key must be built the EXACT
    // same way `approve` itself would for `{"action":"screenshot","window":
    // "notes"}` — via the real `args_digest` helper (exposed
    // `#[doc(hidden)] pub` for exactly this) — or this grant would silently
    // stop matching and every `assert_screenshot_content` call below would
    // hang for up to an hour waiting on a card nobody is spawned to answer.
    // issue #160 round-11 P1 #B/#C: `screenshot` now ALSO resolves its window
    // and folds `id`/`app`/`title` into the key, exactly like a Write action
    // (see `approve`'s own doc) — "notes" resolves to id 1 / app "Notes" /
    // title "Untitled" per the `mock` window list above.
    let screenshot_notes_key = weft::ask::action_key(&[
        "gui",
        "screenshot",
        "notes",
        "1",
        "Notes",
        "Untitled",
        &weft::bus::computer_srv::args_digest(&json!({"action": "screenshot", "window": "notes"})),
    ]);
    let grant_screenshot_notes = |thread: i32, dir: &str| {
        asks_handle.seed_grants(GrantSnapshot {
            full: Vec::new(),
            always: vec![weft::ask::AlwaysGrant {
                thread,
                dir: dir.to_string(),
                action_key: screenshot_notes_key.clone(),
            }],
        });
    };

    let (claude_thread, claude_dir) = worker_dir(&db_handle, ws.id, r.id, "claude").await;
    grant_screenshot_notes(claude_thread, &claude_dir.to_string());
    assert_screenshot_content(&base, claude_thread, &claude_dir.to_string(), 20, true, "claude worker").await;

    let (omp_thread, omp_dir) = worker_dir(&db_handle, ws.id, r.id, "omp").await;
    grant_screenshot_notes(omp_thread, &omp_dir.to_string());
    assert_screenshot_content(&base, omp_thread, &omp_dir.to_string(), 21, true, "omp (ACP) worker").await;

    let (codex_thread, codex_dir) = worker_dir(&db_handle, ws.id, r.id, "codex").await;
    grant_screenshot_notes(codex_thread, &codex_dir.to_string());
    assert_screenshot_content(&base, codex_thread, &codex_dir.to_string(), 22, false, "codex worker").await;

    // 8. Lead-lane engine gating: the SAME rule, driven by `thread.lead_tool`
    // instead of `direction.tool` (a lead has no direction row at all).
    let claude_lead = repo::create_thread(&db_handle, ws.id, "claude lead", "issue", "claude").await.unwrap();
    grant_screenshot_notes(claude_lead.id, "lead");
    assert_screenshot_content(&base, claude_lead.id, "lead", 23, true, "claude lead").await;
    let codex_lead = repo::create_thread(&db_handle, ws.id, "codex lead", "issue", "codex").await.unwrap();
    grant_screenshot_notes(codex_lead.id, "lead");
    assert_screenshot_content(&base, codex_lead.id, "lead", 24, false, "codex lead").await;

    // 9. Ask-card preview attach rule, now owned by the server-side gate
    // itself (`bus::computer_srv::preview_for_action`, issue #160 round-2
    // P1 §2 — relocated from `bus::server::handle_ask`, which no longer
    // attaches previews at all): a GUI INPUT ask targeting the SAME window
    // id as the most recent screenshot for this (thread, dir) carries
    // `Ask::preview`; the SAME (thread, dir) targeting a DIFFERENT window
    // does not (§2's new window-match requirement); an observe-only ask
    // never carries one regardless; a (thread, dir) that never screenshotted
    // gets no preview either (isolation). Drives the REAL `tools/call`
    // endpoint directly (not a simulated hook POST) — this behavior is no
    // longer reachable through the hook path at all post-round-2.
    //
    // 9a. left_click on "notes" (id 1) — the SAME window `claude_thread`
    // just screenshotted in scenario 7 above — carries the preview.
    let call = spawn_computer_call(&base, claude_thread, claude_dir.to_string(), "left_click", "notes");
    let card = wait_for_card(&asks_handle, "left_click on the just-screenshotted window").await;
    assert!(card.preview.is_some(), "a GUI INPUT ask targeting the screenshotted window must carry the preview: {card:?}");
    assert!(
        card.preview.as_deref().unwrap().starts_with("data:image/jpeg;base64,"),
        "{card:?}"
    );
    assert!(asks_handle.answer(card.id, Answer::Deny));
    call.await.unwrap();

    // 9b. §2: left_click on "other" (id 2) — a DIFFERENT window than the one
    // screenshotted — must NOT inherit that unrelated preview.
    let call = spawn_computer_call(&base, claude_thread, claude_dir.to_string(), "left_click", "other");
    let card = wait_for_card(&asks_handle, "left_click on a DIFFERENT window than the one screenshotted").await;
    assert!(
        card.preview.is_none(),
        "a click on a window OTHER than the one last screenshotted must not carry a stale preview: {card:?}"
    );
    assert!(asks_handle.answer(card.id, Answer::Deny));
    call.await.unwrap();

    // 9c. An observe-only ask (a screenshot of "other" — a DIFFERENT
    // action_key than the narrow grant above, so it surfaces a real card)
    // never carries a preview, even though one is on file for this session.
    let call = spawn_computer_call(&base, claude_thread, claude_dir.to_string(), "screenshot", "other");
    let card = wait_for_card(&asks_handle, "an observe-only screenshot ask").await;
    assert!(
        card.preview.is_none(),
        "an OBSERVE-only GUI ask must never carry a preview, even with one on file: {card:?}"
    );
    assert!(asks_handle.answer(card.id, Answer::Deny));
    call.await.unwrap();

    // 9d. Isolation: a (thread, dir) that never screenshotted at all gets no
    // preview either.
    let (never_thread, never_dir) = worker_dir(&db_handle, ws.id, r.id, "claude").await;
    let call = spawn_computer_call(&base, never_thread, never_dir.to_string(), "left_click", "notes");
    let card = wait_for_card(&asks_handle, "left_click (never screenshotted)").await;
    assert!(
        card.preview.is_none(),
        "a (thread, dir) with no prior screenshot must get no preview (isolation): {card:?}"
    );
    assert!(asks_handle.answer(card.id, Answer::Deny));
    call.await.unwrap();

    // —— round-2 P1: the server-side approval gate itself ——
    //
    // Everything above exercised OTHER behavior (screenshot pipeline, input
    // gates, engine-gated images, preview attach) against sessions the gate
    // was made to stay out of the way of (a Full or narrow Always grant).
    // These scenarios exercise the GATE'S OWN behavior instead: standing
    // grants, Always-grant reuse across two calls, and the round-2 P2
    // window-argument validation — on sessions that start UNGRANTED.
    //
    // The control lease is a SINGLE process-wide slot, not one per (thread,
    // dir) — section 9's clicks above left `claude_thread` holding it, so
    // each section below that expects to actually reach the backend starts
    // by clearing it, the same defensive reset the top of this test does.
    computer::clear_control();

    // 10. No standing grant: a card appears; Deny rejects the call and the
    // backend is never touched; a SECOND identical action after an Always
    // answer skips the card entirely and reaches the backend directly.
    let (gate_thread, gate_dir) = worker_dir(&db_handle, ws.id, r.id, "claude").await;
    let gate_dir_s = gate_dir.to_string();
    // issue #160 round-11 P1 #D: an input action now maps its coordinate
    // against a RECORDED screenshot's own dimensions (fail-closed with none
    // on file) — this session never screenshotted "notes" (id 1) itself, so
    // seed the record directly rather than driving a real `screenshot` call
    // through the gate first just to satisfy this precondition (unrelated to
    // what this section actually tests).
    computer::record_shot_dims(
        gate_thread,
        &gate_dir_s,
        None,
        1,
        width,
        height,
        &WindowInfo { id: 1, app: "Notes".into(), title: "Untitled".into(), x: 0, y: 0, width, height },
    );
    let clicks_before = mock.actions.lock().unwrap().len();

    let call = spawn_computer_call(&base, gate_thread, gate_dir_s.clone(), "left_click", "notes");
    let card = wait_for_card(&asks_handle, "left_click with no standing grant").await;
    assert_eq!(card.tool, "computer", "the gate's own card must self-identify as \"computer\", not an engine name");
    assert_eq!(card.summary, "computer: left_click @ notes");
    // issue #160 round-4 P1 §1: only `action == "type"` ever carries a
    // `detail_redacted` — a `left_click`'s detail (a coordinate) has nothing
    // this module considers secret, so the IM bridge falls back to the full
    // `detail` unchanged.
    assert!(card.detail_redacted.is_none(), "{card:?}");
    assert!(asks_handle.answer(card.id, Answer::Deny));
    let out = call.await.unwrap();
    assert!(out.contains("denied"), "a Denied gate card must reject the call: {out}");
    assert_eq!(
        mock.actions.lock().unwrap().len(),
        clicks_before,
        "a Denied call must never reach the backend"
    );

    tokio::time::sleep(Duration::from_millis(600)).await; // clear the throttle window

    let call = spawn_computer_call(&base, gate_thread, gate_dir_s.clone(), "left_click", "notes");
    let card = wait_for_card(&asks_handle, "left_click, second time, to grant Always").await;
    assert!(asks_handle.answer(card.id, Answer::Always));
    let out = call.await.unwrap();
    assert!(out.contains("left_click") && out.contains("done"), "{out}");
    assert_eq!(
        mock.actions.lock().unwrap().len(),
        // issue #160 round-4 P1 §2: this Allow is an Interactive approval (a
        // real card just appeared) — the click family now activates the
        // target window first, so this call records TWO backend actions
        // (`activate`, then `click`), not one.
        clicks_before + 2,
        "the Allowed call must reach the backend exactly once (activate + click): {:?}",
        mock.actions.lock().unwrap()
    );

    tokio::time::sleep(Duration::from_millis(600)).await;

    // The THIRD, identical action_key (same action + same window) must skip
    // the card entirely now — the Always grant from the previous answer
    // auto-decides it, mirroring `bus::server::handle_ask`'s own Always
    // semantics for every other tool. issue #160 round-5 review P1 §6: an
    // Auto approval now activates the target window too, exactly like an
    // Interactive one — so this STILL adds TWO more actions (`activate`,
    // then the click), not just one.
    let out = rpc(
        &base,
        gate_thread,
        &gate_dir_s,
        json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
            "params":{"name":"computer","arguments":{"action":"left_click","window":"notes","coordinate":[1,1]}}}),
    )
    .await;
    assert!(out.contains("left_click") && out.contains("done"), "{out}");
    assert!(asks_handle.open().is_empty(), "an Always-covered action must never surface a card");
    assert_eq!(
        mock.actions.lock().unwrap().len(),
        clicks_before + 4,
        "the Always-covered call must still reach the backend, WITH an activate call too \
         (round-5 review P1 §6: Auto approvals activate too now): {:?}",
        mock.actions.lock().unwrap()
    );

    // 11. §4: a missing/blank `window` on a window-scoped action is rejected
    // as a missing-argument error, before ever resolving against the
    // backend's window list — never silently swallowed into matching the
    // sole visible window. Exercised on the Full-granted primary session
    // (thread=1) so this is purely about the window-argument validation,
    // not the approval gate (already covered above). `clear_control` first:
    // `gate_thread` above still holds the single process-wide control lease.
    computer::clear_control();
    tokio::time::sleep(Duration::from_millis(600)).await;
    let clicks_before = mock.actions.lock().unwrap().len();
    // `input_gate`'s throttle check runs BEFORE `required_window` — even a
    // call that's about to be rejected for a missing window still consumes
    // the throttle window on its way there, so each iteration below waits
    // out `computer::THROTTLE_MS` first (matching this file's own
    // discipline before every other input action).
    for window_arg in [Value::Null, json!(""), json!("   ")] {
        tokio::time::sleep(Duration::from_millis(600)).await;
        let mut args = json!({"action":"left_click","coordinate":[1,1]});
        args["window"] = window_arg.clone();
        let out = rpc(
            &base,
            1,
            "lead",
            json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
                "params":{"name":"computer","arguments": args}}),
        )
        .await;
        assert!(out.contains("missing required 'window'"), "window={window_arg:?}: {out}");
    }
    // A request that OMITS `window` entirely must be rejected the same way.
    tokio::time::sleep(Duration::from_millis(600)).await;
    let out = rpc(
        &base,
        1,
        "lead",
        json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
            "params":{"name":"computer","arguments":{"action":"left_click","coordinate":[1,1]}}}),
    )
    .await;
    assert!(out.contains("missing required 'window'"), "{out}");
    assert_eq!(
        mock.actions.lock().unwrap().len(),
        clicks_before,
        "a rejected missing-window call must never reach the backend"
    );

    // 12. Focus-freshness gate (issue #160 round-2 P1 addendum): `type`
    // rejects with no prior click on the target window; succeeds within 15s
    // of a click on that SAME window; rejects again for a DIFFERENT window
    // even with a fresh click on file (for the OTHER one). A brand-new
    // (thread, dir), Full-granted so this section is purely about the focus
    // gate, not the approval gate.
    let (focus_thread, focus_dir) = worker_dir(&db_handle, ws.id, r.id, "claude").await;
    let focus_dir_s = focus_dir.to_string();
    asks_handle.seed_grants(GrantSnapshot {
        full: vec![FullGrant { thread: focus_thread, dir: focus_dir_s.clone() }],
        always: Vec::new(),
    });
    // issue #160 round-11 P1 #D: 12b's `left_click` below reaches the real
    // backend (Full-granted) — seed this session's "notes" (id 1) shot dims
    // directly, same reasoning as section 10 above.
    computer::record_shot_dims(
        focus_thread,
        &focus_dir_s,
        None,
        1,
        width,
        height,
        &WindowInfo { id: 1, app: "Notes".into(), title: "Untitled".into(), x: 0, y: 0, width, height },
    );
    // `clear_control` first: thread=1 (section 11) still holds the single
    // process-wide control lease; the sleep clears the throttle window.
    computer::clear_control();
    tokio::time::sleep(Duration::from_millis(600)).await;

    // 12a. No click at all yet on this (thread, dir) — `type` is rejected.
    let out = rpc(
        &base,
        focus_thread,
        &focus_dir_s,
        json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
            "params":{"name":"computer","arguments":{"action":"type","window":"notes","text":"hi"}}}),
    )
    .await;
    assert!(out.to_lowercase().contains("focus"), "{out}");
    assert_eq!(
        mock.actions.lock().unwrap().iter().filter(|a| a.starts_with("type")).count(),
        0,
        "a focus-rejected type must never reach the backend"
    );
    tokio::time::sleep(Duration::from_millis(600)).await;

    // 12b. Click "notes" (id 1), then `type` into "notes" within 15s — passes.
    let out = rpc(
        &base,
        focus_thread,
        &focus_dir_s,
        json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
            "params":{"name":"computer","arguments":{"action":"left_click","window":"notes","coordinate":[1,1]}}}),
    )
    .await;
    assert!(out.contains("done"), "{out}");
    tokio::time::sleep(Duration::from_millis(600)).await;
    let out = rpc(
        &base,
        focus_thread,
        &focus_dir_s,
        json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
            "params":{"name":"computer","arguments":{"action":"type","window":"notes","text":"hi"}}}),
    )
    .await;
    assert!(out.contains("typed") && out.contains("done"), "{out}");

    tokio::time::sleep(Duration::from_millis(600)).await;

    // 12c. That SAME click does NOT satisfy a `type` into "other" (id 2) —
    // the freshness record is scoped to the exact window that was clicked.
    let out = rpc(
        &base,
        focus_thread,
        &focus_dir_s,
        json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
            "params":{"name":"computer","arguments":{"action":"type","window":"other","text":"hi"}}}),
    )
    .await;
    assert!(out.to_lowercase().contains("focus"), "{out}");

    // —— round-4 P1 §2 (broadened round-5 review P1 §6): reactivating the target window ——
    //
    // The real hazard: click target window (foreground -> target) -> `approve`
    // cards the FOLLOWING input action -> a human answers it by clicking
    // Weft's own UI (foreground -> Weft) -> without a fix, the action goes to
    // Weft, not the target (or an absolute-coordinate click lands on Weft's
    // own card). A brand-new, deliberately UNGRANTED (thread, dir) so both
    // the click and the first `type` each surface a REAL card (an
    // `Interactive` approval); a THIRD call pre-seeds an Always grant so
    // `auto_decision` decides it silently (an `Auto` approval, no card) —
    // issue #160 round-5 review P1 §6: `activate_target` is now called
    // UNCONDITIONALLY, so this Auto path activates too, exactly like the
    // Interactive ones above it; a FOURTH simulates the backend having no
    // activation API at all (`Unsupported`) and must fail closed, never
    // letting the real action through.
    //
    // "notes" always resolves to window id 1 (see the `WindowInfo` list this
    // whole test installed above) — every `activate N` assertion below names
    // that id.

    let (refocus_thread, refocus_dir) = worker_dir(&db_handle, ws.id, r.id, "claude").await;
    let refocus_dir_s = refocus_dir.to_string();
    // issue #160 round-11 P1 #D: every click/type below in this section
    // reaches the real backend for "notes" (id 1) — seed this session's shot
    // dims once, up front, same reasoning as sections 10/12 above (13b/13c's
    // `type` calls don't need this — only coordinate-taking actions do — but
    // 13a/13c's own `left_click`s do).
    computer::record_shot_dims(
        refocus_thread,
        &refocus_dir_s,
        None,
        1,
        width,
        height,
        &WindowInfo { id: 1, app: "Notes".into(), title: "Untitled".into(), x: 0, y: 0, width, height },
    );
    computer::clear_control();
    tokio::time::sleep(Duration::from_millis(600)).await;

    // 13a. left_click on "notes" — no standing grant, so a real card
    // appears; a plain Allow (not Always) is an ordinary Interactive
    // approval, exactly like any other first-time click. The click family
    // itself is now activate-gated too (issue #160 round-4 P1 §2: an
    // absolute-coordinate click can land on Weft's own card if it now covers
    // the target's on-screen position), so `activate 1` must appear
    // immediately before the click.
    let call = spawn_computer_call(&base, refocus_thread, refocus_dir_s.clone(), "left_click", "notes");
    let card = wait_for_card(&asks_handle, "activate scenario: initial left_click").await;
    assert!(asks_handle.answer(card.id, Answer::Allow));
    let out = call.await.unwrap();
    assert!(out.contains("left_click") && out.contains("done"), "{out}");
    let baseline = mock.actions.lock().unwrap().len();
    assert_eq!(
        mock.actions.lock().unwrap()[baseline - 2..],
        ["activate 1".to_string(), "click 1,1 Left x1".to_string()],
        "an Interactive approval must activate the target window BEFORE the click: {:?}",
        mock.actions.lock().unwrap()
    );

    tokio::time::sleep(Duration::from_millis(600)).await;

    // 13b. `type` into the SAME window — also ungranted, so a NEW card
    // appears (a different action_key than the click's). Answering Allow
    // here is an Interactive approval: the card itself took the foreground,
    // so the fix must reactivate window 1 BEFORE the actual keystrokes reach
    // the backend.
    let text_interactive = "hi-refocus".to_string();
    let call = {
        let base = base.clone();
        let dir = refocus_dir_s.clone();
        let text = text_interactive.clone();
        tokio::spawn(async move {
            rpc(
                &base,
                refocus_thread,
                &dir,
                json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
                    "params":{"name":"computer","arguments":{"action":"type","window":"notes","text":text}}}),
            )
            .await
        })
    };
    let card = wait_for_card(&asks_handle, "activate scenario: type").await;
    // issue #160 round-4 P1 §1: the LOCAL card still carries the raw typed
    // text (the human deciding must see it), but its `detail_redacted` must
    // never carry it — this is the exact leak the fix closes, since this
    // very `Ask` is what an IM bridge (if one were installed) would render
    // into an outbound card before the human ever answers.
    assert!(card.detail.contains(&text_interactive), "the local card must show the raw text: {card:?}");
    let redacted = card
        .detail_redacted
        .as_deref()
        .expect("a type action's Ask must carry detail_redacted");
    assert!(
        !redacted.contains(&text_interactive),
        "detail_redacted must never contain the raw typed text: {redacted}"
    );
    assert!(asks_handle.answer(card.id, Answer::Allow));
    let out = call.await.unwrap();
    assert!(out.contains("typed") && out.contains("done"), "{out}");

    {
        let actions = mock.actions.lock().unwrap();
        assert_eq!(
            actions.len(),
            baseline + 2,
            "an Interactive approval must activate the window BEFORE typing — expected \
             [..., activate, type]: {actions:?}"
        );
        assert_eq!(actions[baseline], "activate 1", "{actions:?}");
        assert_eq!(actions[baseline + 1], format!("type {text_interactive}"), "{actions:?}");
    }

    tokio::time::sleep(Duration::from_millis(600)).await;

    // 13c. Auto path: pre-seed an Always grant for a SECOND `type` call (a
    // DIFFERENT `text`, hence a DIFFERENT action_key from 13b's) so
    // `auto_decision` decides Allow WITHOUT a card ever appearing. issue #160
    // round-5 review P1 §6: unlike the ORIGINAL round-4 fix (which only
    // activated on an Interactive approval), `activate_target` now runs
    // UNCONDITIONALLY — an Auto approval offers no guarantee the target
    // window still holds the real OS foreground at call time either — so
    // this path STILL records an `activate` call, immediately before the
    // `type`.
    //
    // issue #160 round-10 P1 #A: `type` is Write-classified with a non-blank
    // `window` argument, so `approve` now folds the resolved window's own
    // `app`+`title` into the key too (scoping a standing Always grant to
    // that EXACT window identity, not just the query string) — this pre-seed
    // must include the SAME `"Notes"`/`"Untitled"` pair the `WindowInfo` for
    // id 1 carries (see this test's own `mock` setup above), matching
    // EXACTLY what `approve` itself would resolve for `window: "notes"`, or
    // this grant silently stops matching and the call below hangs waiting on
    // a card nobody is spawned to answer. issue #160 round-11 P1 #B: the
    // resolved window's own `id` ("1") is now ALSO folded in, right after the
    // query string.
    let text_auto = "auto-no-refocus".to_string();
    let type_always_key = weft::ask::action_key(&[
        "gui",
        "type",
        "notes",
        "1",
        "Notes",
        "Untitled",
        &weft::bus::computer_srv::args_digest(&json!({"action":"type","window":"notes","text":text_auto})),
    ]);
    asks_handle.seed_grants(GrantSnapshot {
        full: Vec::new(),
        always: vec![weft::ask::AlwaysGrant {
            thread: refocus_thread,
            dir: refocus_dir_s.clone(),
            action_key: type_always_key,
        }],
    });

    // A fresh click first (re-establishing freshness) — itself ungranted, so
    // it surfaces its own card (and its own `activate` call); its OWN
    // action_key is unrelated to the Always grant just seeded for `type`.
    let call = spawn_computer_call(&base, refocus_thread, refocus_dir_s.clone(), "left_click", "notes");
    let card = wait_for_card(&asks_handle, "activate scenario: pre-auto click").await;
    assert!(asks_handle.answer(card.id, Answer::Allow));
    call.await.unwrap();
    let baseline_auto = mock.actions.lock().unwrap().len();

    tokio::time::sleep(Duration::from_millis(600)).await;

    let out = rpc(
        &base,
        refocus_thread,
        &refocus_dir_s,
        json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
            "params":{"name":"computer","arguments":{"action":"type","window":"notes","text":text_auto}}}),
    )
    .await;
    assert!(out.contains("typed") && out.contains("done"), "{out}");
    assert!(asks_handle.open().is_empty(), "an Always-covered type must never surface a card");

    {
        let actions = mock.actions.lock().unwrap();
        assert_eq!(
            actions.len(),
            baseline_auto + 2,
            "round-5 review P1 §6: an Auto approval must ALSO activate the window now — expected \
             [..., activate, type]: {actions:?}"
        );
        assert_eq!(actions[baseline_auto], "activate 1", "{actions:?}");
        assert_eq!(actions[baseline_auto + 1], format!("type {text_auto}"), "{actions:?}");
    }

    tokio::time::sleep(Duration::from_millis(600)).await;

    // 13d. Fail-closed: the backend can't activate the window at all
    // (simulated via `MockBackend::fail_activate`, standing in for
    // `Unsupported` — `StubBackend`'s permanent case, or a real backend that
    // found no activation API). An Interactive approval must reject the
    // call outright — neither an `activate` NOR the real click may ever
    // reach the backend.
    mock.fail_activate.store(true, std::sync::atomic::Ordering::SeqCst);
    let before_fail_closed = mock.actions.lock().unwrap().len();
    let call = spawn_computer_call(&base, refocus_thread, refocus_dir_s.clone(), "left_click", "notes");
    let card = wait_for_card(&asks_handle, "activate scenario: fail-closed on Unsupported activation").await;
    assert!(asks_handle.answer(card.id, Answer::Allow));
    let out = call.await.unwrap();
    assert!(
        !out.contains("done"),
        "a broken activation must fail closed, never let the click reach the backend: {out}"
    );
    assert_eq!(
        mock.actions.lock().unwrap().len(),
        before_fail_closed,
        "a fail-closed activation must record NEITHER an activate NOR the real click"
    );
    mock.fail_activate.store(false, std::sync::atomic::Ordering::SeqCst);

    // —— round-3 P1 §2: recheck_after_guard ——
    //
    // A second input call that queues on `computer::input_flight_guard`
    // behind a long-held first call must re-check BOTH the kill switch and
    // the control-lease ownership AFTER it finally acquires the guard — not
    // just once, back when it first queued. Simulated by the TEST ITSELF
    // holding the flight guard (standing in for another call's in-flight
    // backend round trip) while a REAL RPC call queues behind it, disabling
    // the setting while it's queued, then releasing the guard.

    let (recheck_thread, recheck_dir) = worker_dir(&db_handle, ws.id, r.id, "claude").await;
    let recheck_dir_s = recheck_dir.to_string();
    asks_handle.seed_grants(GrantSnapshot {
        full: vec![FullGrant { thread: recheck_thread, dir: recheck_dir_s.clone() }],
        always: Vec::new(),
    });
    computer::clear_control();
    tokio::time::sleep(Duration::from_millis(600)).await;
    let clicks_before_recheck = mock.actions.lock().unwrap().len();

    let held_guard = computer::input_flight_guard().await;
    let call = spawn_computer_call(&base, recheck_thread, recheck_dir_s.clone(), "left_click", "notes");
    // Give the spawned call time to clear `approve`/`acquire_and_throttle`
    // and start queuing on the (held) flight guard.
    tokio::time::sleep(Duration::from_millis(200)).await;
    repo::set_setting(&db_handle, computer::K_COMPUTER_USE_ENABLED, "false")
        .await
        .unwrap();
    drop(held_guard);
    let out = call.await.unwrap();
    assert!(
        out.to_lowercase().contains("disabled"),
        "the queued call must recheck after the guard and see the disable: {out}"
    );
    assert_eq!(
        mock.actions.lock().unwrap().len(),
        clicks_before_recheck,
        "a recheck-denied call must never reach the backend"
    );

    // Restore the setting — disciplined cleanup even though this is the
    // last thing this test does.
    repo::set_setting(&db_handle, computer::K_COMPUTER_USE_ENABLED, "true")
        .await
        .unwrap();
    computer::clear_control();

    // —— issue #160 round-5 review P1 §2: recheck_after_guard also re-checks has_open ——
    //
    // The invariant `check_suspended` establishes UP FRONT — an open
    // permission card suspends this session's input — must still hold for a
    // call that already cleared that early check and is now sitting in
    // `input_flight_guard`'s queue: a DIFFERENT, brand-new ask for the SAME
    // (thread, dir) can open WHILE this call waits (e.g. the same engine
    // hitting a completely unrelated permission prompt through its own
    // hook), and the queued call must not slip its action through
    // underneath that fresh card once it finally gets the guard. Simulated
    // exactly like the scenario above: the TEST ITSELF holds the flight
    // guard, a REAL RPC call queues behind it, then — instead of disabling
    // the setting — a brand-new ask is opened directly against the SAME
    // (thread, dir) before the guard is released.

    let (has_open_thread, has_open_dir) = worker_dir(&db_handle, ws.id, r.id, "claude").await;
    let has_open_dir_s = has_open_dir.to_string();
    asks_handle.seed_grants(GrantSnapshot {
        full: vec![FullGrant { thread: has_open_thread, dir: has_open_dir_s.clone() }],
        always: Vec::new(),
    });
    computer::clear_control();
    tokio::time::sleep(Duration::from_millis(600)).await;
    let clicks_before_has_open = mock.actions.lock().unwrap().len();

    let held_guard = computer::input_flight_guard().await;
    let call = spawn_computer_call(&base, has_open_thread, has_open_dir_s.clone(), "left_click", "notes");
    // Give the spawned call time to clear `approve`/`acquire_and_throttle`
    // (both fine, thanks to the Full grant above) and start queuing on the
    // (held) flight guard.
    tokio::time::sleep(Duration::from_millis(200)).await;
    // A brand-new, UNRELATED ask opens for this EXACT (thread, dir) while the
    // queued call waits — standing in for some other tool's own permission
    // request racing in.
    let (new_ask_id, _rx) = asks_handle.request(
        has_open_thread,
        &has_open_dir_s,
        "some_other_tool",
        "an unrelated permission request",
        "detail",
        RiskLevel::Unknown,
        "[\"unrelated\"]",
    );
    drop(held_guard);
    let out = call.await.unwrap();
    assert!(
        out.contains("permission card"),
        "the queued call must recheck has_open after the guard and see the fresh ask: {out}"
    );
    assert_eq!(
        mock.actions.lock().unwrap().len(),
        clicks_before_has_open,
        "a recheck-denied (has_open) call must never reach the backend"
    );

    // Clean up the unrelated ask so it can't leak into any later section.
    assert!(asks_handle.answer(new_ask_id, Answer::Deny));
    computer::clear_control();

    // 14. issue #160 round-10 P1 #1 + P2 #2: multi-repo `wt` routing now
    // resolves into a WEFT-MANAGED namespace under `weft_home/computer/
    // <thread>/<dir>/wt-<id>` (no longer the worktree's own `.weft/`), and an
    // EXPLICIT-but-non-numeric `wt` fails the WHOLE call closed rather than
    // falling back to "first worktree" the way an ABSENT one still does.
    // Lives HERE (this test's own giant function), not as a separate
    // `#[tokio::test]`, because it now needs to know `WEFT_HOME` to predict
    // the output path — this file's own doc comment on why only ONE test
    // function may touch that env var applies just as much to this scenario
    // post-migration as it always did to the lead-scratch-dir screenshot
    // path above.
    let repo_tmp_14a = tempfile::tempdir().unwrap();
    let repo_tmp_14b = tempfile::tempdir().unwrap();
    let repo_14a = repo::add_repo_ref(
        &db_handle, ws.id, "wt14a", &repo_tmp_14a.path().to_string_lossy(), "main", "", true,
    )
    .await
    .unwrap();
    let repo_14b = repo::add_repo_ref(
        &db_handle, ws.id, "wt14b", &repo_tmp_14b.path().to_string_lossy(), "main", "", true,
    )
    .await
    .unwrap();
    let thread_14 = repo::create_thread(&db_handle, ws.id, "wt14", "feature", "claude").await.unwrap();
    let direction_14 = repo::create_direction(
        &db_handle, thread_14.id, "task", "claude", repo_14a.id, "why", "impl-only", "main",
    )
    .await
    .unwrap();

    let wt_14a_path = std::env::temp_dir().join(format!("weft-computer-mcp-wt14-a-{}", std::process::id()));
    let wt_14b_path = std::env::temp_dir().join(format!("weft-computer-mcp-wt14-b-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&wt_14a_path);
    let _ = std::fs::remove_dir_all(&wt_14b_path);
    std::fs::create_dir_all(&wt_14a_path).unwrap();
    std::fs::create_dir_all(&wt_14b_path).unwrap();
    // Worktree A is recorded FIRST — the pre-existing "first worktree"
    // fallback would always resolve here without an explicit `wt`.
    let wt_14a = repo::record_worktree(
        &db_handle, repo_14a.id, direction_14.id, "ba", &wt_14a_path.to_string_lossy(), true, true, "",
    )
    .await
    .unwrap();
    let wt_14b = repo::record_worktree(
        &db_handle, repo_14b.id, direction_14.id, "bb", &wt_14b_path.to_string_lossy(), true, true, "",
    )
    .await
    .unwrap();
    asks_handle.seed_grants(GrantSnapshot {
        full: vec![FullGrant { thread: thread_14.id, dir: direction_14.id.to_string() }],
        always: Vec::new(),
    });
    let dir_14_s = direction_14.id.to_string();
    let session_dir_for = |wt_id: i32| {
        weft::paths::weft_home()
            .unwrap()
            .join("computer")
            .join(thread_14.id.to_string())
            .join(&dir_14_s)
            .join(format!("wt-{wt_id}"))
    };
    let audit_14a = session_dir_for(wt_14a.id).join("computer-audit.jsonl");
    let audit_14b = session_dir_for(wt_14b.id).join("computer-audit.jsonl");

    // 14a. An explicit `?wt=` naming worktree B lands its audit line in B's
    // OWN weft-managed namespace, NOT A's — and never touches wt_14b_path
    // (the actual worktree CHECKOUT) at all: output no longer lives there.
    // issue #160 round-13/14 P1: the bearer is bound to the EXACT `wt` the URL
    // carries, so a `?wt=B` URL must present the key minted for `Some(B)`.
    let url = format!(
        "{base}/computer/{}/{}/mcp?wt={}&key={}",
        thread_14.id, dir_14_s, wt_14b.id,
        computer_srv::computer_session_token(thread_14.id, &dir_14_s, Some(wt_14b.id))
    );
    let out = rpc_url(
        &url,
        json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
            "params":{"name":"computer","arguments":{"action":"wait","duration_ms":1}}}),
    )
    .await;
    assert!(out.contains("waited"), "{out}");
    assert!(!audit_14a.exists(), "wt=B must never write into A's namespace: {out}");
    assert!(audit_14b.exists(), "wt=B must write its audit line into B's OWN namespace");
    assert!(
        !wt_14a_path.join(".weft").exists() && !wt_14b_path.join(".weft").exists(),
        "worker-lane computer-use output must never touch either worktree's own checkout at all"
    );

    // 14b. No `?wt=` at all: falls back to the first worktree (A)'s OWN
    // namespace — the SAME per-worktree isolation the old worktree-rooted
    // path gave for free.
    let url_no_wt = format!(
        "{base}/computer/{}/{}/mcp?{}",
        thread_14.id, dir_14_s, key_query(thread_14.id, &dir_14_s)
    );
    let out = rpc_url(
        &url_no_wt,
        json!({"jsonrpc":"2.0","id":2,"method":"tools/call",
            "params":{"name":"computer","arguments":{"action":"wait","duration_ms":1}}}),
    )
    .await;
    assert!(out.contains("waited"), "{out}");
    assert!(audit_14a.exists(), "an absent wt must fall back to the first worktree's OWN namespace");

    // 14c. A forged `wt` naming a worktree of a DIFFERENT direction must
    // FAIL CLOSED (round-8 P2 #7) — never silently fall back to A's
    // namespace, and never write into the foreign direction's own namespace
    // either.
    let other_direction_14 = repo::create_direction(
        &db_handle, thread_14.id, "task2", "claude", repo_14a.id, "why", "impl-only", "main",
    )
    .await
    .unwrap();
    let foreign_path_14 =
        std::env::temp_dir().join(format!("weft-computer-mcp-wt14-foreign-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&foreign_path_14);
    std::fs::create_dir_all(&foreign_path_14).unwrap();
    let foreign_14 = repo::record_worktree(
        &db_handle, repo_14a.id, other_direction_14.id, "bf", &foreign_path_14.to_string_lossy(), true, true, "",
    )
    .await
    .unwrap();
    let lines_before_14c = std::fs::read_to_string(&audit_14a).unwrap().lines().count();
    // Mint the key for THIS foreign `wt` so the request clears the bearer check
    // (issue #160 round-13/14 P1) and actually reaches `session_root` — the
    // point of 14c is that even a token-matching foreign-DIRECTION worktree is
    // rejected there (round-8 P2 #7), independently of the token layer. The
    // sibling-worktree hijack the token binding itself blocks is covered
    // separately by `computer_token_binds_to_the_worktree_not_just_thread_dir`.
    let url = format!(
        "{base}/computer/{}/{}/mcp?wt={}&key={}",
        thread_14.id, dir_14_s, foreign_14.id,
        computer_srv::computer_session_token(thread_14.id, &dir_14_s, Some(foreign_14.id))
    );
    let out = rpc_url(
        &url,
        json!({"jsonrpc":"2.0","id":3,"method":"tools/call",
            "params":{"name":"computer","arguments":{"action":"wait","duration_ms":1}}}),
    )
    .await;
    assert!(out.contains("waited"), "the action itself doesn't depend on a resolved worktree: {out}");
    let lines_after_14c = std::fs::read_to_string(&audit_14a).unwrap().lines().count();
    assert_eq!(
        lines_after_14c, lines_before_14c,
        "round-8 P2 #7: a forged wt from a different direction must NOT fall back to A's namespace"
    );
    assert!(
        !session_dir_for(foreign_14.id).join("computer-audit.jsonl").exists(),
        "must never write into the foreign direction's own namespace either"
    );

    // 14d. issue #160 round-10 P2 #2: an EXPLICIT but non-numeric `wt` must
    // ALSO fail the WHOLE call closed — never silently fall back to "first
    // worktree" the way an ABSENT `wt` does. issue #160 round-13/14 P1: it now
    // fails even earlier, at the bearer check — a malformed `wt` can match no
    // minted token, so `handle_computer` rejects it with a 401 before dispatch.
    let lines_before_14d = std::fs::read_to_string(&audit_14a).unwrap().lines().count();
    let url = format!(
        "{base}/computer/{}/{}/mcp?wt=abc&{}",
        thread_14.id, dir_14_s, key_query(thread_14.id, &dir_14_s)
    );
    let out = rpc_url(
        &url,
        json!({"jsonrpc":"2.0","id":4,"method":"tools/call",
            "params":{"name":"computer","arguments":{"action":"wait","duration_ms":1}}}),
    )
    .await;
    assert!(
        !out.contains("waited"),
        "a malformed wt must reject the WHOLE call, not just its audit write: {out}"
    );
    let lines_after_14d = std::fs::read_to_string(&audit_14a).unwrap().lines().count();
    assert_eq!(
        lines_after_14d, lines_before_14d,
        "a malformed wt must never fall back to writing A's namespace either"
    );

    let _ = std::fs::remove_dir_all(&wt_14a_path);
    let _ = std::fs::remove_dir_all(&wt_14b_path);
    let _ = std::fs::remove_dir_all(&foreign_path_14);
}

/// issue #160 round-2 P1: with no standing grant, an input action surfaces a
/// Needs-you card via the server-side gate — a Deny rejects the call, and
/// (unlike the giant test above, which routes every scenario through a
/// MockBackend) this never even reaches far enough to need one: `approve`
/// returning `Err` short-circuits `run_action` before any backend call. A
/// bare thread/dir with no worktree/DB rows at all is enough.
#[tokio::test]
async fn gate_denies_an_input_action_with_no_standing_grant() {
    let reg = BusRegistry::new();
    let db = Db::connect("sqlite::memory:").await.unwrap();
    repo::set_setting(&db, computer::K_COMPUTER_USE_ENABLED, "true")
        .await
        .unwrap();
    let asks = AskRegistry::new();
    let asks_handle = asks.clone();
    let (base, _h) = server::serve(reg, db, asks).await.unwrap();

    let call = spawn_computer_call(&base, 1, "10".to_string(), "left_click", "notes");
    let card = wait_for_card(&asks_handle, "left_click with no standing grant").await;
    assert_eq!(card.tool, "computer", "the gate's own card self-identifies as \"computer\": {card:?}");
    assert_eq!(card.risk, RiskLevel::Write);
    assert!(asks_handle.answer(card.id, Answer::Deny));
    let out = call.await.unwrap();
    assert!(out.contains("denied"), "a Denied gate card must reject the call: {out}");
}

/// The fail-closed twin of the test above: the ask's sender is dropped
/// instead of answered (`AskRegistry::cancel_for` — the SAME mechanism an
/// engine/model switch uses to tear down a stale ask, issue #96, and the
/// SAME simulation `tests/bus_http.rs::ask_bridge_cancel_returns_explicit_
/// deny` uses for the hook endpoint's own timeout/cancel path). The gate
/// must return an EXPLICIT deny — never hang, and never silently allow.
#[tokio::test]
async fn gate_denies_when_the_ask_is_cancelled_instead_of_answered() {
    let reg = BusRegistry::new();
    let db = Db::connect("sqlite::memory:").await.unwrap();
    repo::set_setting(&db, computer::K_COMPUTER_USE_ENABLED, "true")
        .await
        .unwrap();
    let asks = AskRegistry::new();
    let asks_handle = asks.clone();
    let (base, _h) = server::serve(reg, db, asks).await.unwrap();

    let call = spawn_computer_call(&base, 2, "20".to_string(), "left_click", "notes");
    let _card = wait_for_card(&asks_handle, "left_click awaiting cancellation").await;
    asks_handle.cancel_for(2, "20");
    let out = call.await.unwrap();
    assert!(
        out.contains("denied") || out.contains("no answer"),
        "a cancelled/timed-out gate ask must resolve to an EXPLICIT deny, never hang or silently allow: {out}"
    );
    assert!(asks_handle.open().is_empty());
}

/// Exercises the REAL platform backend end-to-end (window enumeration +
/// screenshot via `xcap`). Headless CI has no display server, so this stays
/// `#[ignore]`d — run it by hand locally, e.g.
/// `cargo test --test computer_mcp --features computer-os -- --ignored`.
///
/// Issue #160 round-2 §6: turned from a shape-only smoke check (did the text
/// merely contain `"id"` anywhere — which an ERROR string could too, e.g.
/// `ComputerError::Unsupported`'s message doesn't, but nothing enforced
/// that) into real assertions: the window list must actually be non-empty
/// and each entry must carry the fields a REAL window (not an error
/// sentence) would, then a genuine `screenshot` of the first one must
/// actually save. A Full grant stands in for the human who isn't present to
/// answer a card when this runs by hand from the command line — the
/// approval gate itself already has extensive coverage in the mock-backend
/// test above; this test exists to exercise the REAL backend, not the gate.
#[ignore]
#[tokio::test]
async fn real_backend_lists_and_screenshots_a_real_window() {
    let reg = BusRegistry::new();
    let db = Db::connect("sqlite::memory:").await.unwrap();
    repo::set_setting(&db, computer::K_COMPUTER_USE_ENABLED, "true")
        .await
        .unwrap();
    let asks = AskRegistry::new();
    asks.seed_grants(GrantSnapshot {
        full: vec![FullGrant { thread: 1, dir: "10".to_string() }],
        always: Vec::new(),
    });
    let (base, _h) = server::serve(reg, db, asks).await.unwrap();

    let out = rpc(
        &base,
        1,
        "10",
        json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
            "params":{"name":"computer","arguments":{"action":"list_windows"}}}),
    )
    .await;
    let body = sse_json(&out);
    let text = body["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("no text content in list_windows result: {out}"));
    let windows: Vec<serde_json::Value> = serde_json::from_str(text).unwrap_or_else(|e| {
        panic!("list_windows text isn't a JSON window array — looks like error text instead? {e}: {text}")
    });
    assert!(
        !windows.is_empty(),
        "expected at least one real, visible, non-excluded window on this machine: {text}"
    );
    let first = &windows[0];
    assert!(first.get("id").and_then(|v| v.as_u64()).is_some(), "each window must carry a numeric id: {first}");
    assert!(
        first.get("app").and_then(|v| v.as_str()).is_some(),
        "each window must carry an app name: {first}"
    );

    // A real screenshot of that SAME first window, by its id.
    let window_query = first["id"].as_u64().unwrap().to_string();
    let out = rpc(
        &base,
        1,
        "10",
        json!({"jsonrpc":"2.0","id":2,"method":"tools/call",
            "params":{"name":"computer","arguments":{"action":"screenshot","window": window_query}}}),
    )
    .await;
    assert!(out.contains("screenshot saved"), "expected a real screenshot to actually save: {out}");
}
