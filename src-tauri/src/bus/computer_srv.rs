//! `weft_computer` MCP server (issue #160). M1 shipped observation only
//! (window enumeration + screenshot); M2 adds input injection — click/type/
//! key/scroll/drag/move — gated behind the SAME `computer_use_enabled` app
//! setting (default OFF, fails closed — see `computer::enabled`), PLUS a
//! process-wide control lock, an open-permission-card check, and an input
//! throttle, so at most one session drives the human's real mouse/keyboard
//! at a time. This does NOT touch `inject.rs` / `commands.rs` / `engine.rs`
//! — those back a DIFFERENT feature (an engine's own PTY input).
//!
//! Wiring mirrors `bus::server::handle` (JSON-RPC over one SSE
//! `event: message` per POST). Deliberately does NOT reach into
//! `bus::server`'s private helpers across modules — `text_result`/`sse` are
//! copied here instead, matching `bus::global`'s existing precedent for a
//! new MCP server module.
//!
//! Every `computer` call result comes back as plain text.
//!
//! Issue #160 round-2 P1 (architecture fix): `/computer/:thread/:dir/mcp`'s
//! identity comes from the URL path alone (same guarantee `bus::server::
//! handle` relies on) — but unlike the OTHER bus MCP servers, this one used
//! to have NO approval gate of its own at all: authorization lived entirely
//! in the calling engine's PreToolUse hook, a layer ANY local process
//! (including the agent itself, e.g. a raw `curl` straight into this
//! endpoint) can simply skip. [`run_action`]'s new [`approve`] step closes
//! that gap: it gates EVERY `tools/call` here, server-side, before it can
//! reach a backend — a Needs-you card the human answers through the exact
//! same `AskRegistry` the hook path uses, just via a route the calling
//! engine cannot route around. `bus::server::AUTO_APPROVED_INTERNAL_TOOLS`
//! now auto-approves `weft_computer`'s hook ask (see its own doc) so a
//! well-behaved engine's hook doesn't ALSO card the same call a second time.
//!
//! ONE tool, `computer`, dispatched by an `action` argument — the same shape
//! as Anthropic's `computer_20251124`, OpenAI's Responses `computer`, and
//! omp's `computer` tool, which models are already trained against.

use crate::ask::{AskRegistry, Decision};
use crate::computer::{self, backend, backend::MouseButton, ComputerError};
use crate::store::{repo, Db};
use axum::{
    extract::{Path, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

fn text_result(s: String) -> Value {
    json!({ "content": [{ "type": "text", "text": s }] })
}

/// A `screenshot` result whose owning session's engine is known to accept an
/// inline MCP `image` content block (see [`engine_accepts_mcp_image`]) —
/// `s` is the SAME confirmation text [`text_result`] alone would carry,
/// `image_b64` is the screenshot re-encoded as JPEG (no `data:` prefix — raw
/// base64, the shape the MCP `image` content type wants) via
/// [`computer::encode_jpeg_data_uri`]. The text path is NEVER dropped even
/// when the image is attached (issue #160 M3-B spec): every engine, image-
/// capable or not, still gets the file path as a fallback for its own
/// image-viewing tool.
fn text_and_image_result(s: String, image_b64: String) -> Value {
    json!({ "content": [
        { "type": "text", "text": s },
        { "type": "image", "data": image_b64, "mimeType": "image/jpeg" },
    ] })
}

fn sse(value: Value) -> Response {
    let body = format!("event: message\ndata: {}\n\n", value);
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/event-stream")],
        body,
    )
        .into_response()
}

// `thread`/`dir` come from the URL path (same identity-can't-be-spoofed
// guarantee `bus::server::handle` relies on for `/bus/:thread/:dir/mcp`).
pub async fn handle_computer(
    Path((thread, dir)): Path<(i32, String)>,
    State(db): State<Db>,
    State(asks): State<AskRegistry>,
    Json(req): Json<Value>,
) -> Response {
    // Notifications (no id) get a bare 202, same as the other bus handlers.
    let id = match req.get("id") {
        Some(v) => v.clone(),
        None => return StatusCode::ACCEPTED.into_response(),
    };
    let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");

    let result: Value = match method {
        "initialize" => json!({
            "protocolVersion": "2024-11-05",
            "capabilities": { "tools": { "listChanged": false } },
            "serverInfo": { "name": "weft_computer", "version": "1.0.0" }
        }),
        "tools/list" => json!({ "tools": computer_tool_specs() }),
        "tools/call" => {
            let name = req
                .pointer("/params/name")
                .and_then(|n| n.as_str())
                .unwrap_or("");
            let args = req
                .pointer("/params/arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            call_computer(&db, &asks, thread, &dir, name, &args).await
        }
        _ => json!({}),
    };

    sse(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
}

/// The `action`s this server implements. Kept as ONE closed list feeding
/// BOTH the JSON Schema `enum` below and the "unknown action" error text, so
/// the schema can never advertise an action this dispatcher doesn't handle
/// (or silently drift from it) — the same closed-list-as-single-source
/// discipline `EXCLUDED_TERMINAL_APPS` in `computer/mod.rs` uses.
const VALID_ACTIONS: &[&str] = &[
    "screenshot",
    "list_windows",
    "left_click",
    "right_click",
    "double_click",
    "triple_click",
    "type",
    "key",
    "scroll",
    "left_click_drag",
    "mouse_move",
    "cursor_position",
    "wait",
];

fn computer_tool_specs() -> Value {
    json!([
        {
            "name": "computer",
            "description": format!("Observe AND control the human's screen — OS-level window listing/screenshot plus mouse/keyboard input injection. `action=list_windows` lists visible on-screen windows (Weft's own window and terminal-emulator apps are excluded, so you can never see yourself or the terminal you're running inside). `action=screenshot` captures ONE window — never the whole desktop — and returns a PNG FILE PATH (not image data): open it with your own image-viewing tool. Every other action but `cursor_position`/`wait` needs `window` (same id/substring rule as screenshot) and drives that window's input: `left_click`/`right_click`/`double_click`/`triple_click`/`mouse_move` need `coordinate` [x, y]; `left_click_drag` needs `start_coordinate` AND `coordinate` (end point); `scroll` needs `coordinate` plus `scroll_direction`; `type`/`key` need `text` (literal text to type, or a combo like \"cmd+s\"/\"ctrl+shift+t\"/\"Return\"/\"f5\" for `key`). ALL coordinates are in the pixel space of the MOST RECENT screenshot of that window — take a screenshot first (or again after the window resizes) to get coordinates that still line up, since the mapping is recomputed from the window's CURRENT size on every call and an out-of-range coordinate is rejected rather than guessed at. `type`/`key` additionally require a `left_click`/`right_click`/`double_click`/`triple_click` on that SAME window within the last {FOCUS_FRESHNESS_SECS}s — click inside the target window first to focus it, then type/key within {FOCUS_FRESHNESS_SECS}s, or the call is rejected. Every call — observation or input — may pause for a human's permission card (Needs-you) before it runs; an input action can additionally come back `Busy` (another session currently has it) or fail while a DIFFERENT permission card is still waiting on the human — retry after a moment. Input actions are also rate-limited to roughly 2 per second."),
            "inputSchema": { "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": VALID_ACTIONS,
                        "description": "screenshot | list_windows | left_click | right_click | double_click | triple_click | type | key | scroll | left_click_drag | mouse_move | cursor_position | wait" },
                    "window": { "type": "string",
                        "description": "Required (non-empty) for every action except cursor_position and wait: a window id from action=list_windows, or a case-insensitive substring of its app name or title." },
                    "coordinate": { "type": "array", "items": { "type": "integer" }, "minItems": 2, "maxItems": 2,
                        "description": "[x, y] in the pixel space of the most recent screenshot of `window`. Required for left_click/right_click/double_click/triple_click/mouse_move/scroll, and as the drag END point for left_click_drag." },
                    "start_coordinate": { "type": "array", "items": { "type": "integer" }, "minItems": 2, "maxItems": 2,
                        "description": "[x, y] drag START point, same pixel space as `coordinate`. Required for left_click_drag." },
                    "text": { "type": "string",
                        "description": format!("Literal text to type (action=type), or a key combo like \"cmd+s\" / \"ctrl+shift+t\" / \"Return\" / \"f5\" (action=key). Both actions require a click on the SAME `window` within the last {FOCUS_FRESHNESS_SECS}s first — see this tool's own description.") },
                    "scroll_direction": { "type": "string", "enum": ["up", "down", "left", "right"],
                        "description": "Required for action=scroll." },
                    "scroll_amount": { "type": "integer",
                        "description": "Scroll distance for action=scroll — default 3, capped at 30." },
                    "duration_ms": { "type": "integer",
                        "description": "Milliseconds to sleep for action=wait — capped at 5000." }
                },
                "required": ["action"] }
        }
    ])
}

async fn call_computer(db: &Db, asks: &AskRegistry, thread: i32, dir: &str, name: &str, args: &Value) -> Value {
    let action = args.get("action").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let window_query = args.get("window").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let mut window_id: Option<u32> = None;
    // Set ONLY by the "screenshot" arm of `run_action`, and ONLY when the
    // engine driving `(thread, dir)` is one `engine_accepts_mcp_image` allows
    // — see that function's doc table (issue #160 M3-B).
    let mut screenshot_image_b64: Option<String> = None;

    let outcome = run_action(
        db,
        asks,
        thread,
        dir,
        name,
        &action,
        args,
        &mut window_id,
        &mut screenshot_image_b64,
    )
    .await;
    let outcome_text = match &outcome {
        Ok(text) => text.clone(),
        Err(text) => text.clone(),
    };

    // The DURABLE audit line's own args payload — NOT the same `args` the
    // approval card in `approve` showed the human — see `redact_audit_args`'s
    // doc for why `action == "type"`'s literal keystrokes are redacted here
    // (issue #160 round-2 P2) but never on the card the human approved BEFORE
    // this call ran.
    let audit_args = redact_audit_args(&action, args);

    append_audit(
        db,
        thread,
        dir,
        &AuditEntry {
            ts_ms: now_ms(),
            thread,
            dir,
            action: &action,
            window_query: &window_query,
            window_id,
            args: &audit_args,
            outcome: if outcome.is_ok() { "ok" } else { &outcome_text },
        },
    )
    .await;

    // The image block only ever gets set on the Ok path of a "screenshot"
    // call (see the out-param's own doc above), so `outcome.is_ok()` is
    // implied whenever `screenshot_image_b64` is `Some` — but checking both
    // here rather than relying on that invariant keeps this call site correct
    // even if that ever stops being true.
    match (outcome.is_ok(), screenshot_image_b64) {
        (true, Some(image_b64)) => text_and_image_result(outcome_text, image_b64),
        _ => text_result(outcome_text),
    }
}

/// Every action's dispatch, `Ok(confirmation text)` or `Err(error text)` —
/// the SAME `Value` shape either way is what the calling agent sees (this
/// server never sets an MCP `isError` flag, matching M1); the `Result` split
/// exists so [`call_computer`] can report an honest `"ok"`-or-not outcome to
/// the audit log without re-deriving it from the text.
async fn run_action(
    db: &Db,
    asks: &AskRegistry,
    thread: i32,
    dir: &str,
    name: &str,
    action: &str,
    args: &Value,
    window_id_out: &mut Option<u32>,
    screenshot_image_b64_out: &mut Option<String>,
) -> Result<String, String> {
    // Fail-closed gate BEFORE touching any backend or even validating the
    // tool/action names: every call here either observes or drives the
    // human's actual screen, so an unset/disabled setting blocks EVERYTHING,
    // not just a well-formed request.
    if !computer::enabled(db).await {
        return Err(ComputerError::Disabled.to_string());
    }
    if name != "computer" {
        return Err(format!("unknown tool: {name}"));
    }
    // The server-side approval gate (issue #160 round-2 P1) — see this
    // module's own doc comment and `approve`'s. Runs for EVERY action,
    // observation or input, before any action-specific argument is even
    // looked at: a standing grant decides silently, otherwise this blocks on
    // a Needs-you card exactly like `bus::server::handle_ask` does for every
    // other tool call in this crate.
    approve(asks, thread, dir, action, args).await?;
    match action {
        "list_windows" => {
            let b = backend::backend();
            computer::visible_windows(b.as_ref())
                .map(|windows| serde_json::to_string(&windows).unwrap_or_else(|_| "[]".into()))
                .map_err(|e| e.to_string())
        }
        "screenshot" => {
            let window_query = required_window(args)?;
            let Some(out_dir) = screenshot_out_dir(db, thread, dir).await else {
                return Err("no worktree for this session".into());
            };
            let b = backend::backend();
            let shot = computer::screenshot_window(b.as_ref(), window_query, &out_dir)
                .map_err(|e| e.to_string())?;
            let text = format!(
                "screenshot saved: {} ({}x{}, scale {:.2}) — open it with your image viewing tool",
                shot.path.display(),
                shot.width,
                shot.height,
                shot.scale
            );
            // Both the preview registry and the MCP image block need the raw
            // pixels; `screenshot_window` only hands back a `Screenshot`
            // (path + dims), so this re-reads the PNG it just wrote — see
            // `read_captured_image`'s doc for why (M3-B is scoped to add
            // exactly one new function to `computer/mod.rs`, not to change
            // `screenshot_window`'s own return shape). Best-effort: a read/
            // decode failure here must not fail a screenshot that already
            // saved successfully — it just means no preview/image this call.
            if let Some(captured) = read_captured_image(&shot.path) {
                // ALWAYS refresh the Ask-card preview, regardless of engine —
                // see `store_screenshot_preview`'s doc. Keyed to the window
                // it actually came from (issue #160 round-2 P1 §2) — re-
                // resolve `window_query` (cheap, no capture; the same kind of
                // fresh re-resolve every OTHER action makes) rather than
                // threading `screenshot_window`'s own internal resolution
                // out a second way. Best-effort: if the window vanished in
                // the instant since `screenshot_window` captured it, just
                // skip storing this preview rather than fail an already-
                // successful screenshot.
                if let Ok(preview) =
                    computer::encode_jpeg_data_uri(&captured, PREVIEW_LONG_EDGE, PREVIEW_QUALITY)
                {
                    if let Ok(w) = computer::resolve_window(b.as_ref(), window_query) {
                        store_screenshot_preview(thread, dir, preview, w.id);
                    }
                }
                // The MCP `image` content block is engine-gated — see
                // `engine_accepts_mcp_image`'s doc table.
                if engine_accepts_mcp_image(db, thread, dir).await {
                    if let Ok(uri) = computer::encode_jpeg_data_uri(
                        &captured,
                        MCP_IMAGE_LONG_EDGE,
                        MCP_IMAGE_QUALITY,
                    ) {
                        *screenshot_image_b64_out = strip_data_uri_prefix(&uri).map(str::to_string);
                    }
                }
            }
            Ok(text)
        }
        "left_click" | "right_click" | "double_click" | "triple_click" => {
            input_gate(asks, thread, dir)?;
            let window_query = required_window(args)?;
            let (cx, cy) = parse_coordinate(args, "coordinate")?;
            let (window_id, px, py) = resolve_and_map(window_query, cx, cy)?;
            *window_id_out = Some(window_id);
            let button = if action == "right_click" { MouseButton::Right } else { MouseButton::Left };
            let count: u32 = match action {
                "double_click" => 2,
                "triple_click" => 3,
                _ => 1,
            };
            // Held for the FULL duration of the backend call, per the
            // cross-module contract on `computer::input_flight_guard`'s own
            // doc — a second `tools/call` for the SAME (thread, dir) racing
            // in concurrently must serialize here rather than interleave its
            // own click on the human's real desktop (issue #160 review R1
            // P2).
            let _flight = computer::input_flight_guard().await;
            backend::backend()
                .click(px, py, button, count)
                .map_err(|e| e.to_string())?;
            // A click that actually reached the OS is presumed to have
            // handed this window OS focus — see `recent_clicks`'s doc. Only
            // AFTER the backend call succeeds: a rejected/failed click never
            // touched the real window and must not seed a false freshness
            // record for a later `type`/`key`.
            record_click_focus(thread, dir, window_id);
            Ok(format!(
                "{action} at ({px}, {py}) in window {window_id} done — take a screenshot to verify"
            ))
        }
        "mouse_move" => {
            input_gate(asks, thread, dir)?;
            let window_query = required_window(args)?;
            let (cx, cy) = parse_coordinate(args, "coordinate")?;
            let (window_id, px, py) = resolve_and_map(window_query, cx, cy)?;
            *window_id_out = Some(window_id);
            // See the click-family arm above for why this guard is held
            // across the backend call itself.
            let _flight = computer::input_flight_guard().await;
            backend::backend()
                .move_cursor(px, py)
                .map_err(|e| e.to_string())?;
            Ok(format!(
                "mouse_move to ({px}, {py}) in window {window_id} done — take a screenshot to verify"
            ))
        }
        "left_click_drag" => {
            input_gate(asks, thread, dir)?;
            let window_query = required_window(args)?;
            let (sx, sy) = parse_coordinate(args, "start_coordinate")?;
            let (ex, ey) = parse_coordinate(args, "coordinate")?;
            let b = backend::backend();
            let w = computer::resolve_window(b.as_ref(), window_query).map_err(|e| e.to_string())?;
            *window_id_out = Some(w.id);
            let from = computer::map_to_physical(&w, sx, sy).map_err(|e| e.to_string())?;
            let to = computer::map_to_physical(&w, ex, ey).map_err(|e| e.to_string())?;
            let _flight = computer::input_flight_guard().await;
            b.drag(from, to).map_err(|e| e.to_string())?;
            Ok(format!(
                "left_click_drag from ({}, {}) to ({}, {}) in window {} done — take a screenshot to verify",
                from.0, from.1, to.0, to.1, w.id
            ))
        }
        "scroll" => {
            input_gate(asks, thread, dir)?;
            let window_query = required_window(args)?;
            let (cx, cy) = parse_coordinate(args, "coordinate")?;
            let (dx, dy) = parse_scroll(args)?;
            let (window_id, px, py) = resolve_and_map(window_query, cx, cy)?;
            *window_id_out = Some(window_id);
            let _flight = computer::input_flight_guard().await;
            backend::backend()
                .scroll(px, py, dx, dy)
                .map_err(|e| e.to_string())?;
            Ok(format!(
                "scroll at ({px}, {py}) dx={dx} dy={dy} in window {window_id} done — take a screenshot to verify"
            ))
        }
        "type" => {
            input_gate(asks, thread, dir)?;
            let window_query = required_window(args)?;
            let text = required_text(args)?;
            let window_id = resolve_window_id(window_query)?;
            *window_id_out = Some(window_id);
            // Focus-freshness gate (issue #160 round-2 P1 addendum) — see
            // `require_recent_focus`'s doc. Checked AFTER resolving the
            // window (so the error names the SAME window id every other
            // error/confirmation for this call names) but BEFORE the backend
            // ever sees the keystrokes.
            require_recent_focus(thread, dir, window_id)?;
            let _flight = computer::input_flight_guard().await;
            backend::backend()
                .type_text(text)
                .map_err(|e| e.to_string())?;
            Ok(format!(
                "typed {} char(s) in window {window_id} done — take a screenshot to verify",
                text.chars().count()
            ))
        }
        "key" => {
            input_gate(asks, thread, dir)?;
            let window_query = required_window(args)?;
            let combo = required_text(args)?;
            let window_id = resolve_window_id(window_query)?;
            *window_id_out = Some(window_id);
            // See the matching comment in the "type" arm above.
            require_recent_focus(thread, dir, window_id)?;
            let _flight = computer::input_flight_guard().await;
            backend::backend().key(combo).map_err(|e| e.to_string())?;
            Ok(format!(
                "key {combo} in window {window_id} done — take a screenshot to verify"
            ))
        }
        // No window, no control lock, no throttle — this reads the cursor's
        // current position without touching input devices.
        "cursor_position" => backend::backend()
            .cursor_position()
            .map(|(x, y)| format!("cursor at ({x}, {y})"))
            .map_err(|e| e.to_string()),
        // No window, no control lock, no throttle — a pure timer.
        "wait" => {
            let ms = parse_duration_ms(args)?;
            tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
            Ok(format!("waited {ms}ms"))
        }
        // Fail-closed, not fail-open: an unrecognized/missing action is
        // rejected with the valid list, never silently treated as one of
        // the known ones.
        _ => Err(format!(
            "unknown action '{action}'; valid actions: {}",
            VALID_ACTIONS.join(", ")
        )),
    }
}

// —— server-side approval gate (issue #160 round-2 P1) ——

/// Gate EVERY `tools/call` here, server-side, before [`run_action`]'s dispatch
/// looks at any action-specific argument — see this module's own top doc
/// comment for the full architecture rationale (this closes the "any local
/// process can just POST past the engine's hook" gap the round-1 review
/// found).
///
/// Mirrors `bus::server::handle_ask`'s own shape closely on purpose, so the
/// two Ask-bridge entry points behave identically wherever they overlap:
///  1. Classify (`risk` via [`crate::ask::classify_gui_action`]), build the
///     display `summary` and the canonical `action_key` — `["gui", action,
///     window]`, a namespace of its own (distinct from `bus::server::
///     summarize`'s `["mcp", tool_name, args]` for the SAME tool, since this
///     is a different, more precise identity: it never depends on which
///     engine-specific tool-name shape happened to reach this endpoint).
///     `detail` is the FULL, unredacted params JSON — see the inline comment
///     below for why that must never be redacted here even though the
///     PERSISTED audit line (`redact_audit_args`) is.
///  2. A standing grant (Full / Always for this EXACT `action_key` / a
///     read-only batch-or-issue grant, gated on `risk` exactly like every
///     other tool) decides silently via [`crate::ask::AskRegistry::
///     auto_decision`] — the SAME method/semantics `handle_ask` uses.
///  3. Otherwise, a Needs-you card ([`crate::ask::AskRegistry::
///     request_with_preview`], `tool` = the literal `"computer"` — this
///     endpoint has no engine identity of its own the way a hook's `?tool=`
///     query does) blocks until answered, with `bus::server::ASK_WAIT`'s own
///     ceiling and the SAME fail-closed deny `handle_ask` returns on a
///     timeout or a cancelled ask (never an ambiguous default).
///
/// [`run_action`] only proceeds to `has_open`/the control lease/the throttle
/// — checks for a DIFFERENT, unrelated open ask, or for someone else driving
/// the desktop right now — AFTER this gate returns `Ok`; those are a
/// completely separate concern (issue #160 M2) from "is this call
/// authorized at all".
async fn approve(asks: &AskRegistry, thread: i32, dir: &str, action: &str, args: &Value) -> Result<(), String> {
    let window_query = window_arg(args);
    let risk = crate::ask::classify_gui_action(action);
    let summary = if window_query.is_empty() {
        format!("computer: {action}")
    } else {
        format!("computer: {action} @ {window_query}")
    };
    let action_key = crate::ask::action_key(&["gui", action, &window_query]);
    // UNREDACTED, even for `action == "type"`: the human approving THIS card
    // needs to see exactly what is about to be typed to judge whether to
    // allow it — a card that hid the text and only said "N characters" would
    // ask for a decision without the information the decision depends on.
    // Only the PERSISTED audit line redacts it, after the fact, once the
    // human has already made that call — see `redact_audit_args`'s doc for
    // the full symmetric point.
    let detail = args.to_string();

    match asks.auto_decision(thread, dir, risk, &action_key) {
        Some(Decision::Allow) => return Ok(()),
        // `auto_decision` never actually returns `Deny` today (only Allow-
        // only standing grants exist) — this arm keeps the gate correct
        // regardless, mirroring `handle_ask`'s own defensive shape, rather
        // than silently falling through to a redundant card for it.
        Some(Decision::Deny) => return Err("denied by a standing weft rule".to_string()),
        None => {}
    }

    let preview = preview_for_action(thread, dir, risk, &window_query);
    let (id, rx) = asks.request_with_preview(
        thread, dir, "computer", &summary, &detail, risk, &action_key, preview,
    );

    match tokio::time::timeout(crate::bus::server::ASK_WAIT, rx).await {
        Ok(Ok(Decision::Allow)) => Ok(()),
        Ok(Ok(Decision::Deny)) => Err("denied in weft".to_string()),
        // Timed out, or the sender was dropped (`AskRegistry::cancel`/
        // `cancel_for` — e.g. an engine/model switch tearing this session
        // down mid-ask): the SAME explicit, fail-closed deny `bus::server::
        // handle_ask` returns for either case, never an ambiguous default.
        _ => {
            asks.cancel(id);
            Err("no answer in time — denied by default (weft ask bridge)".to_string())
        }
    }
}

/// The screenshot preview to attach to a NEW Ask card for `action` targeting
/// `window_query`, if any (issue #160 round-2 P1 §2). Observe-only actions
/// (`risk != Write`) never attach one, unchanged from the M3-B rule this
/// replaces (relocated here from `bus::server::handle_ask` — see this
/// module's own doc comment): the agent that just took a screenshot already
/// has it, attaching it again is pure payload with no new context for the
/// human. For a Write-classified action, the preview registry now carries
/// the window id the LAST screenshot for this `(thread, dir)` actually
/// resolved to (see [`store_screenshot_preview`] / [`last_screenshot_
/// preview`]) — this only attaches it when THIS action's own target window
/// resolves to that SAME id, so an input action on a DIFFERENT window than
/// the one last screenshotted never shows a stale, unrelated preview.
/// Resolution failure (an empty/missing query, no match, an ambiguous
/// match) is silently treated as "no preview to attach" — the real error for
/// a malformed/unresolvable window surfaces later, at dispatch time
/// ([`required_window`] / `resolve_window`), once the human has already
/// answered this card.
fn preview_for_action(
    thread: i32,
    dir: &str,
    risk: crate::ask::RiskLevel,
    window_query: &str,
) -> Option<String> {
    if risk != crate::ask::RiskLevel::Write {
        return None;
    }
    let target = resolve_target_window_id(window_query)?;
    let (data_uri, preview_window_id) = last_screenshot_preview(thread, dir)?;
    (preview_window_id == target).then_some(data_uri)
}

/// Best-effort window resolution for [`preview_for_action`]'s id match ONLY
/// — `None` for an empty/blank query or anything `resolve_window` itself
/// can't resolve to exactly one window, never an error: this is purely a
/// "should we attach a preview" decision, not the real validation
/// ([`required_window`] / the per-action dispatch) that actually rejects a
/// malformed call.
fn resolve_target_window_id(window_query: &str) -> Option<u32> {
    if window_query.trim().is_empty() {
        return None;
    }
    let b = backend::backend();
    computer::resolve_window(b.as_ref(), window_query).ok().map(|w| w.id)
}

/// A window-scoped action's `window` argument, validated BEFORE it ever
/// reaches [`computer::resolve_window`] (issue #160 round-2 P2): missing,
/// non-string, empty, or all-whitespace all fail here with an explicit
/// missing-argument error instead of falling through to resolution. Without
/// this check, an empty string would still reach `resolve_window`'s
/// substring match, where an empty needle's `str::contains("")` is trivially
/// true for every window — silently resolving to whichever ONE window
/// happens to be visible (rather than reporting the missing argument, or
/// erroring honestly as `ComputerError::AmbiguousWindow` would whenever more
/// than one window is visible). Applies to every window-scoped action —
/// `screenshot` and every input action except `cursor_position`/`wait`,
/// which take no `window` at all and never call this.
fn required_window(args: &Value) -> Result<&str, String> {
    match args.get("window").and_then(|v| v.as_str()) {
        Some(w) if !w.trim().is_empty() => Ok(w),
        _ => Err(
            "missing required 'window': a window id from list_windows, or a case-insensitive \
             substring of its app name or title"
                .to_string(),
        ),
    }
}

/// The audit log's OWN args payload for this call — identical to the raw
/// call args for every action except `type`, whose literal keystrokes must
/// NEVER be written to `.weft/computer-audit.jsonl` (issue #160 round-2 P2):
/// that file persists on disk indefinitely and is readable by anyone with
/// filesystem access to the worktree, unlike the Ask card's `detail` (built
/// in [`approve`], from this SAME raw `args`) — which is ephemeral, shown
/// only to the human deciding whether to allow it, and gone the moment the
/// card resolves. The SAME raw text serves two different purposes with two
/// different retention needs, so it gets two different treatments: the card
/// needs it to let the human judge WHAT is about to be typed; the durable
/// log does not need to carry it forward forever just to record THAT a type
/// action happened, how many characters, and whether it succeeded. `text`
/// becomes `{"text_redacted": true, "text_chars": N}` in place of the
/// literal string; every other key (`action`, `window`, …) is untouched.
fn redact_audit_args(action: &str, args: &Value) -> Value {
    if action != "type" {
        return args.clone();
    }
    let mut redacted = args.clone();
    if let Some(obj) = redacted.as_object_mut() {
        // Only redact when `text` is ACTUALLY present as a string — a
        // malformed call missing it entirely (rejected by `required_text`
        // before it ever reaches the backend, see the "type" arm of
        // `run_action`) must not have a synthetic `text` key manufactured
        // into its audit record that was never in the real request.
        if let Some(text) = obj.get("text").and_then(|v| v.as_str()) {
            let chars = text.chars().count();
            obj.insert("text".to_string(), json!({ "text_redacted": true, "text_chars": chars }));
        }
    }
    redacted
}

// —— type/key focus-freshness gate (issue #160 round-2 P1 addendum) ——
//
// Neither backend this module drives exposes a real "which window currently
// holds OS keyboard focus" query (`xcap` has none; a genuine focus API is a
// real-machine follow-up, issue #160 §9), so `type`/`key` cannot verify focus
// directly. Without SOME check here, they would blindly inject keystrokes
// into whatever happens to be focused — which could be a credential prompt,
// or any other window the agent never intended, if it never actually clicked
// its target first. This substitutes a WEFT-tracked freshness heuristic
// instead of real focus verification: a `left_click`/`right_click`/
// `double_click`/`triple_click` that actually reached the OS is PRESUMED to
// have handed that window OS focus (an OS-level guarantee this module does
// not itself re-verify — a third party can still steal focus back after the
// click, and this module would have no way to know), and `type`/`key`
// require ONE such click, on the SAME resolved window, for the SAME
// `(thread, dir)`, within the last `FOCUS_FRESHNESS_MS`. This is not a
// substitute for real focus verification — it is a floor, not a ceiling —
// but it closes the main accident surface: typing into a target window that
// was never clicked at all, this session, ever.

/// How long a click on a window is trusted to still hold that window's OS
/// focus for a subsequent `type`/`key` — see this section's own doc comment.
const FOCUS_FRESHNESS_MS: u64 = 15_000;
const FOCUS_FRESHNESS_SECS: u64 = FOCUS_FRESHNESS_MS / 1000;

/// Process-level "last window this `(thread, dir)` actually clicked, and
/// when" registry — see this section's own doc comment. `now_ms()`-based
/// (wall clock), consistent with every other timestamp this module already
/// records (the audit log's own `ts_ms`); a system clock adjustment
/// mid-session is not a hazard this heuristic needs to defend against.
fn recent_clicks() -> &'static Mutex<HashMap<(i32, String), (u32, u64)>> {
    static CLICKS: OnceLock<Mutex<HashMap<(i32, String), (u32, u64)>>> = OnceLock::new();
    CLICKS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Record a SUCCESSFUL click on `window_id` for `(thread, dir)` — called
/// ONLY from the click-family arm of [`run_action`], and ONLY after the
/// backend call itself returned `Ok`: a rejected/failed click never actually
/// touched the real window, so it must not seed a false freshness record for
/// a later `type`/`key`.
fn record_click_focus(thread: i32, dir: &str, window_id: u32) {
    let mut g = recent_clicks().lock().unwrap_or_else(|e| e.into_inner());
    g.insert((thread, dir.to_string()), (window_id, now_ms()));
}

/// `type`/`key`'s pre-execution gate: reject unless a click on THIS EXACT
/// resolved `window_id`, for THIS `(thread, dir)`, landed within the last
/// [`FOCUS_FRESHNESS_MS`] — see this section's own doc comment for what this
/// is (and is not) verifying.
fn require_recent_focus(thread: i32, dir: &str, window_id: u32) -> Result<(), String> {
    let g = recent_clicks().lock().unwrap_or_else(|e| e.into_inner());
    let fresh = matches!(
        g.get(&(thread, dir.to_string())),
        Some((clicked, ts)) if *clicked == window_id && now_ms().saturating_sub(*ts) <= FOCUS_FRESHNESS_MS
    );
    if fresh {
        return Ok(());
    }
    Err(format!(
        "window {window_id} doesn't appear to have OS focus yet — click inside the target window \
         first to focus it, then type/key within {FOCUS_FRESHNESS_SECS}s"
    ))
}

// —— screenshot → MCP image content + Ask-card preview registry (issue #160 M3-B) ——

/// Long edge / JPEG quality for the MCP `image` content block a `screenshot`
/// result gets, for the engines [`engine_accepts_mcp_image`] allows — a full
/// working-context image for the MODEL to reason over, so it stays at the
/// SAME long edge `screenshot_window`'s own on-disk downscale already caps
/// at (`computer::display_scale`'s `MAX_LONG_EDGE`) — this only ever shrinks
/// further on a window that somehow still exceeds it after that.
const MCP_IMAGE_LONG_EDGE: u32 = 1280;
const MCP_IMAGE_QUALITY: u8 = 75;

/// Long edge / JPEG quality for the Ask-card preview registry's thumbnail —
/// deliberately smaller and lower-quality than the MCP image block above:
/// this one is glance-level visual context for a HUMAN triaging a permission
/// card, not something a model reasons over pixel-by-pixel, so a smaller
/// payload is the right tradeoff.
const PREVIEW_LONG_EDGE: u32 = 640;
const PREVIEW_QUALITY: u8 = 60;

/// Read a just-saved screenshot PNG back into raw RGBA pixels, for
/// [`computer::encode_jpeg_data_uri`] (both the preview registry and,
/// engine-permitting, the MCP image content block) — `screenshot_window`
/// only returns a [`computer::Screenshot`] (path + dims), not the pixels it
/// already wrote to disk, and this milestone is scoped to add exactly ONE
/// new function to `computer/mod.rs` (`encode_jpeg_data_uri` itself, not a
/// change to `screenshot_window`'s own return shape) — so this decodes the
/// file back rather than plumbing the raw capture out a second way.
/// `None` on any read/decode failure: best-effort, since a screenshot that
/// already saved successfully must not fail the whole call just because this
/// second, purely-additive step couldn't re-read its own output.
fn read_captured_image(path: &std::path::Path) -> Option<computer::CapturedImage> {
    let img = image::open(path).ok()?.to_rgba8();
    let (width, height) = (img.width(), img.height());
    Some(computer::CapturedImage {
        rgba: img.into_raw(),
        width,
        height,
    })
}

/// Strip the `data:image/jpeg;base64,` prefix `encode_jpeg_data_uri` always
/// adds — the MCP `image` content type wants the RAW base64 payload with no
/// prefix (`{"type":"image","data":"<base64>","mimeType":"image/jpeg"}`),
/// unlike a `data:` URI meant to sit directly in something like `<img src>`.
fn strip_data_uri_prefix(uri: &str) -> Option<&str> {
    uri.strip_prefix("data:image/jpeg;base64,")
}

/// The `tool` currently driving `(thread, dir)` — the SAME durable field
/// `commands::session_meta` reads (`direction.tool`; see that entity field's
/// own doc comment on why it — not `session.tool` — is the per-task engine
/// choice of record), so a worker mid-engine-switch reports the tool that
/// will actually see this screenshot. `dir == bus::LEAD` has no direction row
/// at all (a lead joins the bus under the `LEAD` sentinel, not a numeric
/// direction id — mirrors [`session_root`]'s own dir=="lead"-vs-numeric
/// split), so it reads the thread's `lead_tool` instead. `None` on any lookup
/// failure — a `dir` that fails to parse, a numeric `dir` that doesn't
/// resolve to a direction belonging to THIS thread (a stale/forged route,
/// same check [`session_root`] makes), a missing row, or a DB error — never a
/// guess.
async fn session_tool(db: &Db, thread: i32, dir: &str) -> Option<String> {
    if dir == crate::bus::LEAD {
        return repo::get_thread(db, thread).await.ok().flatten().map(|t| t.lead_tool);
    }
    let direction_id = dir.parse::<i32>().ok()?;
    match repo::get_direction(db, direction_id).await {
        Ok(Some(d)) if d.thread_id == thread => Some(d.tool),
        _ => None,
    }
}

/// Which weft_computer callers get a screenshot's pixels as an inline MCP
/// `image` content block (issue #160 M3-B), on top of the text confirmation
/// EVERY caller always gets regardless (this module's own doc comment: the
/// agent's own image-viewing tool is the universal fallback):
///
/// | `session_tool` result           | MCP image block? |
/// |----------------------------------|-------------------|
/// | `"claude"`                       | yes — claude feeds an MCP `image` content block straight to the model |
/// | any ACP backend (`acp::backend_for(tool).is_some()`, today just `"omp"`) | yes — same reasoning as claude |
/// | `"codex"` / `"opencode"`         | no — MCP image-content support isn't confirmed from this repo's own code for either, so this stays fail-safe |
/// | lookup failure (`None`)          | no — same fail-safe default |
///
/// The text path is NEVER dropped for the "no" rows — see
/// [`text_and_image_result`]'s doc.
async fn engine_accepts_mcp_image(db: &Db, thread: i32, dir: &str) -> bool {
    match session_tool(db, thread, dir).await {
        Some(tool) => tool == "claude" || crate::acp::backend_for(&tool).is_some(),
        None => false,
    }
}

/// Process-level "most recent screenshot" registry (issue #160 M3-B, value
/// shape extended in round-2 P1 §2): one small preview thumbnail PLUS the
/// window id it actually came from, per (thread, dir), refreshed on EVERY
/// successful `screenshot` action regardless of which engine is asking
/// (unlike the MCP image content block above, which is engine-gated) —
/// [`preview_for_action`] attaches it to a `weft_computer` GUI INPUT Ask
/// card ONLY when that action's own target window resolves to the SAME id
/// (never an observe-only ask, and never a DIFFERENT window's stale preview)
/// so the human has visual context before allowing/denying a click/type/
/// key/etc without opening the saved PNG themselves. A process-wide
/// `OnceLock`, not per-request state, so it survives across the many
/// separate MCP calls a session makes — mirrors `computer::control_mutex`'s
/// own process-level-static shape for the same reason (issue #160 M2).
/// In-memory only: a stale/missing preview is harmless (the Ask card just
/// renders without one), so a restart starting empty is fine — no
/// durability needed.
fn screenshot_previews() -> &'static Mutex<HashMap<(i32, String), (String, u32)>> {
    static PREVIEWS: OnceLock<Mutex<HashMap<(i32, String), (String, u32)>>> = OnceLock::new();
    PREVIEWS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn store_screenshot_preview(thread: i32, dir: &str, preview: String, window_id: u32) {
    let mut g = screenshot_previews().lock().unwrap_or_else(|e| e.into_inner());
    g.insert((thread, dir.to_string()), (preview, window_id));
}

/// The most recent screenshot preview (and the window id it came from) for
/// `(thread, dir)`, if any — see [`screenshot_previews`]'s doc. Read only
/// from [`preview_for_action`] within this same module now (the round-2 P1
/// server-side gate owns preview attachment; `bus::server::handle_ask` no
/// longer does — see this module's own top doc comment).
fn last_screenshot_preview(thread: i32, dir: &str) -> Option<(String, u32)> {
    let g = screenshot_previews().lock().unwrap_or_else(|e| e.into_inner());
    g.get(&(thread, dir.to_string())).cloned()
}

fn window_arg(args: &Value) -> String {
    args.get("window").and_then(|v| v.as_str()).unwrap_or("").to_string()
}

/// The ordered gate every INPUT action (the click family, `type`, `key`,
/// `scroll`, `left_click_drag`, `mouse_move` — never `screenshot`,
/// `list_windows`, `cursor_position`, or `wait`) must clear before it's
/// allowed to touch the backend at all, checked BEFORE any action-specific
/// argument parsing since none of these three care whether the call is
/// otherwise well-formed:
/// 1. is a permission card already blocking this (thread, dir) —
///    [`ComputerError::SuspendedPendingAsk`];
/// 2. does someone else hold the control lease — [`ComputerError::Busy`];
/// 3. are we going faster than the global input throttle allows —
///    [`ComputerError::RateLimited`].
///
/// Runs AFTER [`approve`] in [`run_action`]'s dispatch, never before: THIS
/// gate's `has_open` check is about a DIFFERENT, unrelated ask still waiting
/// on the human (so an agent can't click through/at the permission UI while
/// it's up) — a completely separate concern from "is THIS call authorized at
/// all", which `approve` alone decides. Checking them in the other order
/// would mean an action already suspended behind someone else's card could
/// still open (and block on) its OWN card underneath it.
fn input_gate(asks: &AskRegistry, thread: i32, dir: &str) -> Result<(), String> {
    if asks.has_open(thread, dir) {
        return Err(ComputerError::SuspendedPendingAsk.to_string());
    }
    computer::acquire_control(thread, dir).map_err(|e| e.to_string())?;
    computer::throttle_input().map_err(|e| e.to_string())?;
    Ok(())
}

/// Resolve `window_query` to exactly one window and map `(cx, cy)` — a
/// screenshot-space coordinate — to that window's current physical position.
/// Re-resolves the window FRESH every call (this turn's `list_windows`, not
/// anything cached), matching [`computer::map_to_physical`]'s own contract.
fn resolve_and_map(window_query: &str, cx: u32, cy: u32) -> Result<(u32, i32, i32), String> {
    let b = backend::backend();
    let w = computer::resolve_window(b.as_ref(), window_query).map_err(|e| e.to_string())?;
    let (px, py) = computer::map_to_physical(&w, cx, cy).map_err(|e| e.to_string())?;
    Ok((w.id, px, py))
}

/// Resolve `window_query` to exactly one window's id, with NO coordinate
/// mapping — `type`/`key` don't need a position, just the existence check
/// (and the id, for the audit log).
fn resolve_window_id(window_query: &str) -> Result<u32, String> {
    let b = backend::backend();
    computer::resolve_window(b.as_ref(), window_query)
        .map(|w| w.id)
        .map_err(|e| e.to_string())
}

fn parse_coordinate(args: &Value, key: &str) -> Result<(u32, u32), String> {
    let Some(arr) = args.get(key).and_then(|v| v.as_array()) else {
        return Err(format!("missing required '{key}': [x, y]"));
    };
    if arr.len() != 2 {
        return Err(format!("'{key}' must be exactly [x, y]"));
    }
    let x = arr[0].as_i64().filter(|v| *v >= 0);
    let y = arr[1].as_i64().filter(|v| *v >= 0);
    match (x, y) {
        (Some(x), Some(y)) => Ok((x as u32, y as u32)),
        _ => Err(format!("'{key}' must be two non-negative integers [x, y]")),
    }
}

fn required_text(args: &Value) -> Result<&str, String> {
    args.get("text")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "missing required 'text'".to_string())
}

/// `scroll_direction` (required) + `scroll_amount` (optional, default 3,
/// capped at 30 — never rejected for being too large, just clamped) into a
/// `(dx, dy)` delta `backend::ComputerBackend::scroll` understands.
fn parse_scroll(args: &Value) -> Result<(i32, i32), String> {
    let direction = args.get("scroll_direction").and_then(|v| v.as_str()).unwrap_or("");
    let amount = args
        .get("scroll_amount")
        .and_then(|v| v.as_i64())
        .unwrap_or(3)
        .clamp(0, 30) as i32;
    match direction {
        "up" => Ok((0, -amount)),
        "down" => Ok((0, amount)),
        "left" => Ok((-amount, 0)),
        "right" => Ok((amount, 0)),
        other => Err(format!(
            "'scroll_direction' must be one of up|down|left|right, got '{other}'"
        )),
    }
}

/// `duration_ms` (required) for `action=wait`, capped at 5000 rather than
/// rejected when the agent asks for longer — matches `scroll_amount`'s
/// clamp-not-reject policy above, so the two capped numeric params in this
/// tool behave the same way.
fn parse_duration_ms(args: &Value) -> Result<u64, String> {
    match args.get("duration_ms").and_then(|v| v.as_u64()) {
        Some(ms) => Ok(ms.min(5000)),
        None => Err("missing required 'duration_ms'".to_string()),
    }
}

fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// One `.weft/computer-audit.jsonl` line — see [`append_audit`].
#[derive(serde::Serialize)]
struct AuditEntry<'a> {
    ts_ms: u64,
    thread: i32,
    dir: &'a str,
    action: &'a str,
    window_query: &'a str,
    window_id: Option<u32>,
    args: &'a Value,
    outcome: &'a str,
}

/// Pure JSON-line rendering, split out from [`append_audit`] specifically so
/// it's unit-testable without touching the filesystem or the DB — see the
/// `tests` module below.
fn audit_line(entry: &AuditEntry<'_>) -> Result<String, serde_json::Error> {
    let mut line = serde_json::to_string(entry)?;
    line.push('\n');
    Ok(line)
}

/// Append one audit line for EVERY `tools/call` (every action, success or
/// failure) to the session's own `.weft/computer-audit.jsonl` — best-effort:
/// a resolution or write failure here never affects the actual tool result,
/// it just means this one call goes unlogged.
async fn append_audit(db: &Db, thread: i32, dir: &str, entry: &AuditEntry<'_>) {
    use tokio::io::AsyncWriteExt;
    let Some(path) = audit_log_path(db, thread, dir).await else {
        return;
    };
    let Some(parent) = path.parent() else { return };
    if tokio::fs::create_dir_all(parent).await.is_err() {
        return;
    }
    let Ok(line) = audit_line(entry) else { return };
    let Ok(mut file) = tokio::fs::OpenOptions::new().create(true).append(true).open(&path).await else {
        return;
    };
    let _ = file.write_all(line.as_bytes()).await;
}

/// The session's own working directory for `(thread, dir)`, before any
/// output-specific suffix is appended — the shared base for
/// [`screenshot_out_dir`] (which appends a DIFFERENT suffix per lane; see
/// its own doc comment) and [`audit_log_path`] (which appends the SAME
/// `.weft/computer-audit.jsonl` suffix for both lanes).
///
///  - worker lane (`dir` a direction id): that direction's (first)
///    worktree's root.
///  - lead lane (`dir == bus::LEAD`): the lead's own scratch cwd,
///    `<weft_home>/leads/<thread>` — same formula as
///    `lead_chat::commands::ensure_lead_cwd`, duplicated here since that
///    helper is private to its own module (see `builtin_allow.rs`'s doc
///    comment on why a lead's cwd is this scratch dir and not one of its
///    workspace's repos).
///
/// `None` on any failure (DB error, unresolvable path, deleted worktree row,
/// a numeric `dir` that doesn't resolve to a direction belonging to THIS
/// thread — a stale/forged route) — callers turn that into their own
/// soft-failure text rather than a 500.
async fn session_root(db: &Db, thread: i32, dir: &str) -> Option<std::path::PathBuf> {
    if dir == crate::bus::LEAD {
        let home = crate::paths::weft_home().ok()?;
        return Some(home.join("leads").join(thread.to_string()));
    }
    let direction_id = dir.parse::<i32>().ok()?;
    match repo::get_direction(db, direction_id).await {
        Ok(Some(d)) if d.thread_id == thread => {}
        _ => return None,
    }
    let worktrees = repo::list_worktrees(db, Some(direction_id)).await.ok()?;
    let wt = worktrees.into_iter().next()?;
    Some(std::path::PathBuf::from(wt.path))
}

/// Resolve the screenshot output directory for `(thread, dir)`:
///  - worker lane: `<worktree>/.weft/screenshots` — excluded from git via
///    `git::git_exclude` so weft's own screenshots never show up in `git
///    status`/diffs.
///  - lead lane: the lead's scratch cwd itself, `<weft_home>/leads/<thread>`
///    (no extra `.weft/` layer — that whole directory is already
///    weft-private).
async fn screenshot_out_dir(db: &Db, thread: i32, dir: &str) -> Option<std::path::PathBuf> {
    let root = session_root(db, thread, dir).await?;
    if dir == crate::bus::LEAD {
        return Some(root);
    }
    crate::git::git_exclude(&root, ".weft/");
    Some(root.join(".weft").join("screenshots"))
}

/// Resolve the audit log path for `(thread, dir)`: `<session_root>/.weft/
/// computer-audit.jsonl` for BOTH lanes (unlike [`screenshot_out_dir`],
/// which skips the `.weft/` layer for the lead lane — the audit log always
/// gets it, so it never collides with anything else a lead's scratch cwd
/// might hold). Covered by the same `git::git_exclude(".weft/")` call as
/// screenshots for the worker lane; a no-op, harmless call for the
/// (non-git) lead scratch dir.
async fn audit_log_path(db: &Db, thread: i32, dir: &str) -> Option<std::path::PathBuf> {
    let root = session_root(db, thread, dir).await?;
    crate::git::git_exclude(&root, ".weft/");
    Some(root.join(".weft").join("computer-audit.jsonl"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audit_line_is_one_json_object_per_line() {
        let args = json!({"action": "left_click", "coordinate": [1, 2]});
        let entry = AuditEntry {
            ts_ms: 12345,
            thread: 1,
            dir: "lead",
            action: "left_click",
            window_query: "notes",
            window_id: Some(9),
            args: &args,
            outcome: "ok",
        };
        let line = audit_line(&entry).unwrap();
        assert!(line.ends_with('\n'));
        assert_eq!(line.matches('\n').count(), 1, "exactly one line, not pretty-printed");
        let parsed: Value = serde_json::from_str(line.trim_end()).unwrap();
        assert_eq!(parsed["ts_ms"], 12345);
        assert_eq!(parsed["thread"], 1);
        assert_eq!(parsed["dir"], "lead");
        assert_eq!(parsed["action"], "left_click");
        assert_eq!(parsed["window_query"], "notes");
        assert_eq!(parsed["window_id"], 9);
        assert_eq!(parsed["outcome"], "ok");
        assert_eq!(parsed["args"]["action"], "left_click");
    }

    #[test]
    fn audit_line_serializes_a_missing_window_id_as_null() {
        let args = json!({"action": "wait"});
        let entry = AuditEntry {
            ts_ms: 1,
            thread: 1,
            dir: "lead",
            action: "wait",
            window_query: "",
            window_id: None,
            args: &args,
            outcome: "ok",
        };
        let parsed: Value = serde_json::from_str(audit_line(&entry).unwrap().trim_end()).unwrap();
        assert!(parsed["window_id"].is_null());
    }

    #[test]
    fn parse_scroll_maps_direction_to_signed_delta_and_clamps_amount() {
        assert_eq!(parse_scroll(&json!({"scroll_direction": "up"})).unwrap(), (0, -3));
        assert_eq!(parse_scroll(&json!({"scroll_direction": "down"})).unwrap(), (0, 3));
        assert_eq!(parse_scroll(&json!({"scroll_direction": "left"})).unwrap(), (-3, 0));
        assert_eq!(parse_scroll(&json!({"scroll_direction": "right"})).unwrap(), (3, 0));
        assert_eq!(
            parse_scroll(&json!({"scroll_direction": "down", "scroll_amount": 999})).unwrap(),
            (0, 30),
            "scroll_amount must clamp at 30, not error"
        );
        assert!(parse_scroll(&json!({"scroll_direction": "sideways"})).is_err());
        assert!(parse_scroll(&json!({})).is_err());
    }

    #[test]
    fn parse_duration_ms_clamps_at_5000_and_requires_the_param() {
        assert_eq!(parse_duration_ms(&json!({"duration_ms": 200})).unwrap(), 200);
        assert_eq!(
            parse_duration_ms(&json!({"duration_ms": 999_999})).unwrap(),
            5000,
            "duration_ms must clamp at 5000, not error"
        );
        assert!(parse_duration_ms(&json!({})).is_err());
    }

    #[test]
    fn parse_coordinate_requires_two_non_negative_integers() {
        assert_eq!(parse_coordinate(&json!({"coordinate": [10, 20]}), "coordinate").unwrap(), (10, 20));
        assert!(parse_coordinate(&json!({}), "coordinate").is_err());
        assert!(parse_coordinate(&json!({"coordinate": [10]}), "coordinate").is_err());
        assert!(parse_coordinate(&json!({"coordinate": [-1, 20]}), "coordinate").is_err());
    }

    // —— issue #160 round-2 P2: `required_window` ——

    #[test]
    fn required_window_accepts_a_non_blank_string() {
        assert_eq!(required_window(&json!({"window": "notes"})).unwrap(), "notes");
        // Leading/trailing whitespace around otherwise-real content is fine —
        // only ALL-whitespace (or absence) is rejected.
        assert_eq!(required_window(&json!({"window": " notes "})).unwrap(), " notes ");
    }

    #[test]
    fn required_window_rejects_missing_non_string_empty_and_blank() {
        assert!(required_window(&json!({})).is_err(), "missing key");
        assert!(required_window(&json!({"window": 5})).is_err(), "non-string");
        assert!(required_window(&json!({"window": ""})).is_err(), "empty string");
        assert!(required_window(&json!({"window": "   "})).is_err(), "all-whitespace string");
        assert!(required_window(&json!({"window": null})).is_err(), "null");
    }

    // —— issue #160 round-2 P3 (audit redaction) ——

    #[test]
    fn redact_audit_args_replaces_type_text_with_a_char_count_only() {
        let args = json!({"action": "type", "window": "notes", "text": "hunter2"});
        let redacted = redact_audit_args("type", &args);
        assert_eq!(redacted["window"], "notes", "non-text keys pass through untouched");
        assert_eq!(redacted["text"]["text_redacted"], true);
        assert_eq!(redacted["text"]["text_chars"], 7);
        assert!(
            redacted.to_string().contains("hunter2") == false,
            "the raw text must never appear anywhere in the redacted payload: {redacted}"
        );
    }

    #[test]
    fn redact_audit_args_counts_unicode_scalars_not_bytes() {
        // "héllo" — the "é" is 2 bytes in UTF-8 but 1 char; the redacted count
        // must match what a human reading "N characters" would expect, not
        // the raw byte length.
        let args = json!({"action": "type", "text": "héllo"});
        let redacted = redact_audit_args("type", &args);
        assert_eq!(redacted["text"]["text_chars"], 5);
    }

    #[test]
    fn redact_audit_args_leaves_every_other_action_untouched() {
        let args = json!({"action": "key", "window": "notes", "text": "cmd+s"});
        let redacted = redact_audit_args("key", &args);
        assert_eq!(redacted, args, "only action==\"type\" redacts — key's text is a combo, not content");
    }

    #[test]
    fn redact_audit_args_leaves_a_missing_text_key_missing() {
        // A malformed `type` call missing `text` entirely never reaches the
        // backend (`required_text` rejects it first) — its audit record must
        // reflect the REAL request, not manufacture a `text` key that was
        // never actually sent.
        let args = json!({"action": "type", "window": "notes"});
        let redacted = redact_audit_args("type", &args);
        assert_eq!(redacted, args);
        assert!(redacted.get("text").is_none());
    }

    // —— issue #160 round-2 P1 addendum: type/key focus-freshness gate ——
    //
    // Each test below uses a UNIQUE synthetic thread id so they can run in
    // parallel (the default for `cargo test`) without racing each other on
    // the shared process-level `recent_clicks()` registry.

    #[test]
    fn require_recent_focus_passes_right_after_a_click_on_the_same_window() {
        let thread = 900_001;
        record_click_focus(thread, "lead", 7);
        assert!(require_recent_focus(thread, "lead", 7).is_ok());
    }

    #[test]
    fn require_recent_focus_rejects_with_no_prior_click_at_all() {
        let thread = 900_002;
        let err = require_recent_focus(thread, "lead", 7).unwrap_err();
        assert!(err.contains("focus"), "{err}");
        assert!(err.contains("click"), "{err}");
    }

    #[test]
    fn require_recent_focus_rejects_a_click_on_a_different_window() {
        let thread = 900_003;
        record_click_focus(thread, "lead", 7); // clicked window A (id 7)
        let err = require_recent_focus(thread, "lead", 8).unwrap_err(); // typing into B (id 8)
        assert!(err.contains("8"), "error should name the window that lacks focus: {err}");
    }

    #[test]
    fn require_recent_focus_is_scoped_per_thread_dir() {
        let thread_a = 900_004;
        let thread_b = 900_005;
        record_click_focus(thread_a, "lead", 7);
        // A click recorded for a DIFFERENT (thread, dir) must not satisfy
        // this one's focus check — the registry is per-session, not global.
        assert!(require_recent_focus(thread_b, "lead", 7).is_err());
        assert!(require_recent_focus(thread_a, "10", 7).is_err());
    }

    #[test]
    fn require_recent_focus_rejects_once_the_freshness_window_has_expired() {
        let thread = 900_006;
        // Seed a click stamped older than `FOCUS_FRESHNESS_MS` directly,
        // rather than sleeping 15s in a test — same "no fake clock needed"
        // approach the coordinator's spec calls for, just expressed as a
        // pre-expired timestamp instead of a real-time wait.
        {
            let mut g = recent_clicks().lock().unwrap();
            g.insert((thread, "lead".to_string()), (7, now_ms() - FOCUS_FRESHNESS_MS - 1));
        }
        assert!(require_recent_focus(thread, "lead", 7).is_err());
    }
}
