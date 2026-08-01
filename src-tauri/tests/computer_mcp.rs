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
use weft::bus::{server, BusRegistry};
use weft::computer::{self, backend, mock::MockBackend, CapturedImage, WindowInfo};
use weft::store::{repo, Db};

async fn rpc(base: &str, thread: i32, dir: &str, body: serde_json::Value) -> String {
    let url = format!("{base}/computer/{thread}/{dir}/mcp");
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
        rpc(
            &base,
            thread,
            &dir,
            json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
                "params":{"name":"computer","arguments":{"action":action,"window":window,"coordinate":[1,1]}}}),
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
    assert_eq!(mock.actions.lock().unwrap().len(), 1, "a suspended call must never reach the backend");
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
    let screenshot_notes_key = weft::ask::action_key(&[
        "gui",
        "screenshot",
        "notes",
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
    let clicks_before = mock.actions.lock().unwrap().len();

    let call = spawn_computer_call(&base, gate_thread, gate_dir_s.clone(), "left_click", "notes");
    let card = wait_for_card(&asks_handle, "left_click with no standing grant").await;
    assert_eq!(card.tool, "computer", "the gate's own card must self-identify as \"computer\", not an engine name");
    assert_eq!(card.summary, "computer: left_click @ notes");
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
        clicks_before + 1,
        "the Allowed call must reach the backend exactly once"
    );

    tokio::time::sleep(Duration::from_millis(600)).await;

    // The THIRD, identical action_key (same action + same window) must skip
    // the card entirely now — the Always grant from the previous answer
    // auto-decides it, mirroring `bus::server::handle_ask`'s own Always
    // semantics for every other tool.
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
        clicks_before + 2,
        "the Always-covered call must still reach the backend"
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

/// issue #160 round-2 P2 §5: a direction with MORE THAN ONE worktree (a
/// multi-repo direction) routes computer-use output (here, the audit log —
/// simplest to observe without a MockBackend/WEFT_HOME setup, which this
/// binary's OTHER giant test owns exclusively per its own doc comment) to
/// the EXACT worktree the calling worker resolved, via the `?wt=` query
/// param — instead of always falling back to whichever worktree happens to
/// be first. A forged `wt` naming a worktree of a DIFFERENT direction is
/// rejected (closed-set validation) and falls back to the pre-existing
/// "first worktree of this direction" behavior, never writing into the
/// foreign worktree.
#[tokio::test]
async fn wt_query_param_routes_output_to_the_exact_worktree_in_a_multi_repo_direction() {
    let reg = BusRegistry::new();
    let db = Db::connect("sqlite::memory:").await.unwrap();
    repo::set_setting(&db, computer::K_COMPUTER_USE_ENABLED, "true")
        .await
        .unwrap();
    let asks = AskRegistry::new();
    let asks_handle = asks.clone();
    let (base, _h) = server::serve(reg, db.clone(), asks).await.unwrap();

    let ws = repo::create_workspace(&db, "ws").await.unwrap();
    let repo_tmp_a = tempfile::tempdir().unwrap();
    let repo_tmp_b = tempfile::tempdir().unwrap();
    let repo_a = repo::add_repo_ref(&db, ws.id, "a", &repo_tmp_a.path().to_string_lossy(), "main", "", true)
        .await
        .unwrap();
    let repo_b = repo::add_repo_ref(&db, ws.id, "b", &repo_tmp_b.path().to_string_lossy(), "main", "", true)
        .await
        .unwrap();
    let thread = repo::create_thread(&db, ws.id, "t", "feature", "claude").await.unwrap();
    let direction = repo::create_direction(
        &db, thread.id, "task", "claude", repo_a.id, "why", "impl-only", "main",
    )
    .await
    .unwrap();

    let wt_a_path = std::env::temp_dir().join(format!("weft-computer-mcp-wt5-a-{}", std::process::id()));
    let wt_b_path = std::env::temp_dir().join(format!("weft-computer-mcp-wt5-b-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&wt_a_path);
    let _ = std::fs::remove_dir_all(&wt_b_path);
    std::fs::create_dir_all(&wt_a_path).unwrap();
    std::fs::create_dir_all(&wt_b_path).unwrap();
    // Worktree A is recorded FIRST — the pre-existing "first worktree"
    // fallback would always resolve here without an explicit `wt`.
    let _wt_a = repo::record_worktree(&db, repo_a.id, direction.id, "ba", &wt_a_path.to_string_lossy(), true, true, "")
        .await
        .unwrap();
    let wt_b = repo::record_worktree(&db, repo_b.id, direction.id, "bb", &wt_b_path.to_string_lossy(), true, true, "")
        .await
        .unwrap();

    asks_handle.seed_grants(GrantSnapshot {
        full: vec![FullGrant { thread: thread.id, dir: direction.id.to_string() }],
        always: Vec::new(),
    });
    let dir_s = direction.id.to_string();
    let audit_a = wt_a_path.join(".weft").join("computer-audit.jsonl");
    let audit_b = wt_b_path.join(".weft").join("computer-audit.jsonl");

    // 1. An explicit `?wt=` naming worktree B lands its audit line in B, NOT A.
    let url = format!("{base}/computer/{}/{}/mcp?wt={}", thread.id, dir_s, wt_b.id);
    let out = rpc_url(
        &url,
        json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
            "params":{"name":"computer","arguments":{"action":"wait","duration_ms":1}}}),
    )
    .await;
    assert!(out.contains("waited"), "{out}");
    assert!(!audit_a.exists(), "wt=B must never write into worktree A: {out}");
    assert!(audit_b.exists(), "wt=B must write its audit line into worktree B");

    // 2. A forged `wt` naming a worktree of a DIFFERENT direction falls back
    // to the pre-existing "first worktree of THIS direction" (A) — never
    // resolving into the foreign direction's own worktree.
    let other_direction = repo::create_direction(
        &db, thread.id, "task2", "claude", repo_a.id, "why", "impl-only", "main",
    )
    .await
    .unwrap();
    let foreign_path = std::env::temp_dir().join(format!("weft-computer-mcp-wt5-foreign-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&foreign_path);
    std::fs::create_dir_all(&foreign_path).unwrap();
    let foreign = repo::record_worktree(&db, repo_a.id, other_direction.id, "bf", &foreign_path.to_string_lossy(), true, true, "")
        .await
        .unwrap();

    let url = format!("{base}/computer/{}/{}/mcp?wt={}", thread.id, dir_s, foreign.id);
    let out = rpc_url(
        &url,
        json!({"jsonrpc":"2.0","id":2,"method":"tools/call",
            "params":{"name":"computer","arguments":{"action":"wait","duration_ms":1}}}),
    )
    .await;
    assert!(out.contains("waited"), "{out}");
    assert!(
        audit_a.exists(),
        "a forged wt from a different direction must fall back to worktree A \
         (the first worktree of THIS direction): {out}"
    );
    assert!(
        !foreign_path.join(".weft").join("computer-audit.jsonl").exists(),
        "must never write into the foreign direction's worktree"
    );

    let _ = std::fs::remove_dir_all(&wt_a_path);
    let _ = std::fs::remove_dir_all(&wt_b_path);
    let _ = std::fs::remove_dir_all(&foreign_path);
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
