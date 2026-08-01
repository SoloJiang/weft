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
    extract::{Path, Query, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
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
//
// `?wt=<worktree_id>` (issue #160 round-2 P2 §5): an OPTIONAL query param a
// caller can attach when it already knows the EXACT worktree its own worker
// session materialized into — see `inject::computer_url`'s doc for who sets
// it and why. Without it (or when it fails the closed-set check further
// down, in `session_root`), the pre-existing "first worktree for this
// direction" fallback is unchanged — a bare `?wt=`-less URL behaves exactly
// as it always did.
pub async fn handle_computer(
    Path((thread, dir)): Path<(i32, String)>,
    Query(q): Query<HashMap<String, String>>,
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
    // Malformed/non-numeric `wt` is treated exactly like an absent one — the
    // closed-set check in `session_root` would reject a forged value anyway,
    // so a parse failure here just takes the same fallback path a step
    // earlier.
    let wt: Option<i32> = q.get("wt").and_then(|s| s.parse::<i32>().ok());

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
            call_computer(&db, &asks, thread, &dir, wt, name, &args).await
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
            "description": format!("Observe AND control the human's screen — OS-level window listing/screenshot plus mouse/keyboard input injection. `action=list_windows` lists visible on-screen windows (Weft's own window and terminal-emulator apps are excluded, so you can never see yourself or the terminal you're running inside). `action=screenshot` captures ONE window — never the whole desktop — and returns a PNG FILE PATH (not image data): open it with your own image-viewing tool. Every action except `list_windows`, `cursor_position`, and `wait` needs `window` (same id/substring rule as screenshot) and drives that window's input: `left_click`/`right_click`/`double_click`/`triple_click`/`mouse_move` need `coordinate` [x, y]; `left_click_drag` needs `start_coordinate` AND `coordinate` (end point); `scroll` needs `coordinate` plus `scroll_direction`; `type`/`key` need `text` (literal text to type, or a combo like \"cmd+s\"/\"ctrl+shift+t\"/\"Return\"/\"f5\" for `key`). ALL coordinates are in the pixel space of the MOST RECENT screenshot of that window — take a screenshot first (or again after the window resizes) to get coordinates that still line up, since the mapping is recomputed from the window's CURRENT size on every call and an out-of-range coordinate is rejected rather than guessed at. `type`/`key` additionally require a `left_click`/`right_click`/`double_click`/`triple_click` on that SAME window within the last {FOCUS_FRESHNESS_SECS}s — click inside the target window first to focus it, then type/key within {FOCUS_FRESHNESS_SECS}s, or the call is rejected. Every call — observation or input — may pause for a human's permission card (Needs-you) before it runs; an input action can additionally come back `Busy` (another session currently has it) or fail while a DIFFERENT permission card is still waiting on the human — retry after a moment. Input actions are also rate-limited to roughly 2 per second."),
            "inputSchema": { "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": VALID_ACTIONS,
                        "description": "screenshot | list_windows | left_click | right_click | double_click | triple_click | type | key | scroll | left_click_drag | mouse_move | cursor_position | wait" },
                    "window": { "type": "string",
                        "description": "Required (non-empty) for every action except list_windows, cursor_position, and wait: a window id from action=list_windows, or a case-insensitive substring of its app name or title." },
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

async fn call_computer(
    db: &Db,
    asks: &AskRegistry,
    thread: i32,
    dir: &str,
    wt: Option<i32>,
    name: &str,
    args: &Value,
) -> Value {
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
        wt,
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
        wt,
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
    wt: Option<i32>,
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
    //
    // issue #160 round-8 P1 #4: `approved` is the window identity `approve`
    // itself resolved AUTHORITATIVELY at the moment it authorized this call,
    // for a Write (input) action with a window argument — `None` for every
    // observe action and for `wait`. Every input arm below threads this
    // through its OWN later, fresh `resolve_window` via
    // `verify_approved_target`, right before it activates/injects — see that
    // function's own doc for the "approve one window, dispatch to a
    // different one" gap this closes.
    let approved = approve(asks, thread, dir, action, args).await?;
    // issue #160 round-2 P1 §1: re-check the kill switch AFTER the approval
    // await returns — NOT just once, up top, before that (potentially very
    // long, up to `bus::server::ASK_WAIT`) wait began. A human can hit Stop
    // (disabling the setting, or tripping the emergency-stop latch —  both
    // are exactly what `computer::enabled` reads) WHILE this exact card is
    // still sitting in Needs-you; if a DIFFERENT click then answers that
    // stale card Allow, the check at the top of this function already ran
    // and would never see the disable. Without this second check, that Allow
    // would sail straight through to dispatch below as if the kill switch had
    // never fired. Cheap (one more `enabled` read) and correct: this is the
    // exact race the round-2 review flagged, not a hypothetical.
    if !computer::enabled(db).await {
        return Err(ComputerError::Disabled.to_string());
    }
    match action {
        "list_windows" => {
            let b = backend::backend();
            computer::visible_windows(b.as_ref())
                .map(|windows| serde_json::to_string(&windows).unwrap_or_else(|_| "[]".into()))
                .map_err(|e| e.to_string())
        }
        "screenshot" => {
            let window_query = required_window(args)?;
            let out_dir = screenshot_out_dir(db, thread, dir, wt).await?;
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
            // pixels — read straight off `shot.pixels`, the SAME in-memory
            // RGBA `screenshot_window` itself scaled and saved (issue #160
            // round-7 P1), never re-opened from `shot.path`. This used to
            // re-read the just-saved PNG back off disk (`read_captured_image`,
            // now deleted): a worker-writable `out_dir` is repository-
            // controlled content, and the gap between that PNG's own save and
            // this re-open was an open TOCTOU/symlink window — a sandboxed
            // background process could swap the freshly-saved file for a
            // symlink to an arbitrary user-readable image in that gap, and
            // Weft would follow it with its own permissions, inlining the
            // substituted pixels to the model/human as if they were the real
            // capture. Reading `shot.pixels` instead closes that window
            // entirely rather than narrowing it — there is no second
            // filesystem access left to race. `encode_jpeg_data_uri` can
            // still fail on `shot.pixels` (encoding, not decoding, can
            // theoretically error) — best-effort, same as before: a failure
            // here must never fail a screenshot that already saved
            // successfully, it just means no preview/image this call.
            //
            // Keyed to `shot.window_id` — the id `computer::screenshot_window`
            // ITSELF already resolved and captured against (issue #160
            // round-6 review P2 #4) — rather than re-resolving `window_query`
            // a second time here: a second resolution can land on a DIFFERENT
            // window than the one actually captured if it closed, was
            // renamed, or its id got reused in the gap between the two calls,
            // silently mis-keying the preview (and any input approval card
            // that later attaches it) to the WRONG window.
            // `computer::Screenshot::window_id` closes that gap by
            // construction — there is no second resolution left to drift.
            if let Ok(preview) =
                computer::encode_jpeg_data_uri(&shot.pixels, PREVIEW_LONG_EDGE, PREVIEW_QUALITY)
            {
                store_screenshot_preview(thread, dir, preview, shot.window_id);
            }
            // The MCP `image` content block is engine-gated — see
            // `engine_accepts_mcp_image`'s doc table.
            if engine_accepts_mcp_image(db, thread, dir).await {
                if let Ok(uri) = computer::encode_jpeg_data_uri(
                    &shot.pixels,
                    MCP_IMAGE_LONG_EDGE,
                    MCP_IMAGE_QUALITY,
                ) {
                    *screenshot_image_b64_out = strip_data_uri_prefix(&uri).map(str::to_string);
                }
            }
            Ok(text)
        }
        "left_click" | "right_click" | "double_click" | "triple_click" => {
            // issue #160 round-2 P2 §4: every PURELY-argument-shaped check
            // for this action (the window argument being non-empty, the
            // coordinate's shape) runs BEFORE the control lease/throttle are
            // touched — see this section's own "input gates" doc comment
            // further down for the full ordering rationale. issue #160
            // round-6 review P1 #2+#3: the window's actual RESOLUTION and
            // the coordinate's mapping against it are NOT purely-argument
            // checks (they depend on the live desktop's current state) and
            // now run AFTER `input_flight_guard`/the first
            // `recheck_after_guard` instead — see the comment right below.
            let window_query = required_window(args)?;
            let (cx, cy) = parse_coordinate(args, "coordinate")?;
            check_suspended(asks, thread, dir)?;
            let button = if action == "right_click" { MouseButton::Right } else { MouseButton::Left };
            let count: u32 = match action {
                "double_click" => 2,
                "triple_click" => 3,
                _ => 1,
            };
            acquire_and_throttle(thread, dir)?;
            // Held for the FULL duration of the backend call, per the
            // cross-module contract on `computer::input_flight_guard`'s own
            // doc — a second `tools/call` for the SAME (thread, dir) racing
            // in concurrently must serialize here rather than interleave its
            // own click on the human's real desktop (issue #160 review R1
            // P2).
            let _flight = computer::input_flight_guard().await;
            recheck_after_guard(db, asks, thread, dir).await?;
            // issue #160 round-6 review P1 #3: resolve the window AND map
            // the coordinate FRESH here, after the flight guard — not
            // before it, as this used to. A call that queued on the guard
            // behind another session's in-flight action could sit there for
            // as long as that action's own backend round trip takes; a
            // window resolved/mapped BEFORE the guard could have since
            // moved, resized, closed, or had its id reused by the time this
            // call actually gets to inject, landing the click outside the
            // (now stale) coordinates were computed against — or on an
            // entirely different window that reused the same id.
            let w = computer::resolve_window(backend::backend().as_ref(), window_query)
                .map_err(|e| e.to_string())?;
            *window_id_out = Some(w.id);
            // issue #160 round-8 P1 #4: this window must still be the EXACT
            // one `approve` bound at authorization time — see
            // `verify_approved_target`'s own doc. Checked BEFORE this window
            // is ever activated or clicked.
            verify_approved_target(&approved, &w)?;
            let (px, py) = computer::map_to_physical(&w, cx, cy).map_err(|e| e.to_string())?;
            // issue #160 round-4 P1 §2 (broadened round-5 review P1 §6): reclaim
            // the foreground BEFORE this click reaches the OS, not after — see
            // `activate_target`'s own doc for why even the click family (not
            // just type/key) needs this, UNCONDITIONALLY (Auto approvals
            // included): an Interactive approval card can cover the target
            // window's real on-screen position, so an ABSOLUTE-coordinate
            // click risks landing on Weft's own card instead of the target —
            // and an Auto approval offers no guarantee the target still holds
            // the real OS foreground either.
            activate_and_recheck(db, asks, thread, dir, w.id).await?;
            backend::backend()
                .click(px, py, button, count)
                .map_err(|e| e.to_string())?;
            // A click that actually reached the OS is presumed to have
            // handed this window OS focus — see `recent_clicks`'s doc. Only
            // AFTER the backend call succeeds: a rejected/failed click never
            // touched the real window and must not seed a false freshness
            // record for a later `type`/`key`.
            record_click_focus(thread, dir, w.id);
            Ok(format!(
                "{action} at ({px}, {py}) in window {} done — take a screenshot to verify",
                w.id
            ))
        }
        "mouse_move" => {
            let window_query = required_window(args)?;
            let (cx, cy) = parse_coordinate(args, "coordinate")?;
            check_suspended(asks, thread, dir)?;
            acquire_and_throttle(thread, dir)?;
            // See the click-family arm above for why this guard is held
            // across the backend call itself, and why window resolution/
            // coordinate mapping now happen AFTER it (issue #160 round-6
            // review P1 #3).
            let _flight = computer::input_flight_guard().await;
            recheck_after_guard(db, asks, thread, dir).await?;
            let w = computer::resolve_window(backend::backend().as_ref(), window_query)
                .map_err(|e| e.to_string())?;
            *window_id_out = Some(w.id);
            // issue #160 round-8 P1 #4: see the click-family arm above.
            verify_approved_target(&approved, &w)?;
            let (px, py) = computer::map_to_physical(&w, cx, cy).map_err(|e| e.to_string())?;
            activate_and_recheck(db, asks, thread, dir, w.id).await?;
            backend::backend()
                .move_cursor(px, py)
                .map_err(|e| e.to_string())?;
            Ok(format!(
                "mouse_move to ({px}, {py}) in window {} done — take a screenshot to verify",
                w.id
            ))
        }
        "left_click_drag" => {
            let window_query = required_window(args)?;
            let (sx, sy) = parse_coordinate(args, "start_coordinate")?;
            let (ex, ey) = parse_coordinate(args, "coordinate")?;
            check_suspended(asks, thread, dir)?;
            acquire_and_throttle(thread, dir)?;
            let _flight = computer::input_flight_guard().await;
            recheck_after_guard(db, asks, thread, dir).await?;
            // issue #160 round-6 review P1 #3: BOTH endpoints are remapped
            // against the SAME freshly-resolved `w` — a drag has two
            // coordinates, but only one window to go stale.
            let b = backend::backend();
            let w = computer::resolve_window(b.as_ref(), window_query).map_err(|e| e.to_string())?;
            *window_id_out = Some(w.id);
            // issue #160 round-8 P1 #4: see the click-family arm above.
            verify_approved_target(&approved, &w)?;
            let from = computer::map_to_physical(&w, sx, sy).map_err(|e| e.to_string())?;
            let to = computer::map_to_physical(&w, ex, ey).map_err(|e| e.to_string())?;
            activate_and_recheck(db, asks, thread, dir, w.id).await?;
            b.drag(from, to).map_err(|e| e.to_string())?;
            Ok(format!(
                "left_click_drag from ({}, {}) to ({}, {}) in window {} done — take a screenshot to verify",
                from.0, from.1, to.0, to.1, w.id
            ))
        }
        "scroll" => {
            let window_query = required_window(args)?;
            let (cx, cy) = parse_coordinate(args, "coordinate")?;
            let (dx, dy) = parse_scroll(args)?;
            check_suspended(asks, thread, dir)?;
            acquire_and_throttle(thread, dir)?;
            let _flight = computer::input_flight_guard().await;
            recheck_after_guard(db, asks, thread, dir).await?;
            let w = computer::resolve_window(backend::backend().as_ref(), window_query)
                .map_err(|e| e.to_string())?;
            *window_id_out = Some(w.id);
            // issue #160 round-8 P1 #4: see the click-family arm above.
            verify_approved_target(&approved, &w)?;
            let (px, py) = computer::map_to_physical(&w, cx, cy).map_err(|e| e.to_string())?;
            activate_and_recheck(db, asks, thread, dir, w.id).await?;
            backend::backend()
                .scroll(px, py, dx, dy)
                .map_err(|e| e.to_string())?;
            Ok(format!(
                "scroll at ({px}, {py}) dx={dx} dy={dy} in window {} done — take a screenshot to verify",
                w.id
            ))
        }
        "type" => {
            let window_query = required_window(args)?;
            let text = required_text(args)?;
            // issue #160 round-5 review P2 §3: a hard length ceiling, checked
            // right after `required_text` and well before any lease/throttle
            // is touched — see `check_type_length`'s own doc for why.
            check_type_length(text)?;
            check_suspended(asks, thread, dir)?;
            acquire_and_throttle(thread, dir)?;
            let _flight = computer::input_flight_guard().await;
            recheck_after_guard(db, asks, thread, dir).await?;
            // issue #160 round-6 review P1 #2+#3: resolve the window (and,
            // right below, check focus-freshness against it) AFTER the
            // flight guard now too — a queued `type` used to resolve the
            // window BEFORE the guard, so a stale id from a closed/reused
            // window could reach `require_recent_focus`/`activate_target`
            // for the wrong target. Focus-freshness gate itself (issue #160
            // round-2 P1 addendum) is unchanged in SPIRIT — see
            // `require_recent_focus`'s doc — just now checked against a
            // freshly-resolved id rather than a possibly-stale one.
            let w = computer::resolve_window(backend::backend().as_ref(), window_query)
                .map_err(|e| e.to_string())?;
            *window_id_out = Some(w.id);
            // issue #160 round-8 P1 #4: see the click-family arm above.
            verify_approved_target(&approved, &w)?;
            require_recent_focus(thread, dir, w.id)?;
            activate_and_recheck(db, asks, thread, dir, w.id).await?;
            backend::backend()
                .type_text(text)
                .map_err(|e| e.to_string())?;
            Ok(format!(
                "typed {} char(s) in window {} done — take a screenshot to verify",
                text.chars().count(),
                w.id
            ))
        }
        "key" => {
            let window_query = required_window(args)?;
            let combo = required_text(args)?;
            check_suspended(asks, thread, dir)?;
            acquire_and_throttle(thread, dir)?;
            let _flight = computer::input_flight_guard().await;
            recheck_after_guard(db, asks, thread, dir).await?;
            // See the matching comment in the "type" arm above.
            let w = computer::resolve_window(backend::backend().as_ref(), window_query)
                .map_err(|e| e.to_string())?;
            *window_id_out = Some(w.id);
            // issue #160 round-8 P1 #4: see the click-family arm above.
            verify_approved_target(&approved, &w)?;
            require_recent_focus(thread, dir, w.id)?;
            activate_and_recheck(db, asks, thread, dir, w.id).await?;
            backend::backend().key(combo).map_err(|e| e.to_string())?;
            Ok(format!(
                "key {combo} in window {} done — take a screenshot to verify",
                w.id
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
///     window, args_digest]` (issue #160 round-2 P2 §2 added the trailing
///     digest — see [`args_digest`]'s own doc for why), a namespace of its
///     own (distinct from `bus::server::summarize`'s `["mcp", tool_name,
///     args]` for the SAME tool, since this is a different, more precise
///     identity: it never depends on which engine-specific tool-name shape
///     happened to reach this endpoint). `detail` is the FULL, unredacted
///     params JSON — see the inline comment below for why that must never be
///     redacted here even though the PERSISTED audit line
///     (`redact_audit_args`) is.
///  2. A standing grant (`dangerous` mode / Full / Always for this EXACT
///     `action_key`) decides silently via [`crate::ask::AskRegistry::
///     auto_decision_exact`] — issue #160 round-5 review P1 §1: unlike
///     `handle_ask`'s own `auto_decision` call, a GUI action is deliberately
///     NEVER swept in by issue #103's coarse read-only batch-or-issue grant
///     — see that method's own doc for why a `screenshot`/`list_windows`
///     call (always `RiskLevel::ReadOnly`, regardless of which window it
///     targets) cannot be treated the same as an ordinary read-only tool
///     call for this purpose.
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
///
/// Used to return `()` alone, not a distinction between how the `Ok` was
/// reached: issue #160 round-4 P1 §2 originally returned an
/// `Approval::{Auto,Interactive}` value here so every input arm of
/// [`run_action`] could call `activate_target` ONLY for an Interactive
/// approval (a card that actually rendered — a human clicking Weft's own UI
/// to answer it takes the foreground away from the target). Round-5 review
/// P1 §6 (issue #160 #3) removed that distinction: `activate_target` is now
/// called UNCONDITIONALLY by every input arm regardless of how this call
/// approved — see that function's own doc for why an Auto approval needs the
/// SAME reactivation an Interactive one does. With no caller left that
/// needed to tell the two apart, the `Approval` enum this used to return was
/// deleted entirely rather than kept as unused plumbing.
///
/// issue #160 round-8 P1 #4: now returns `Result<Option<ApprovedWindow>,
/// String>` instead — see [`ApprovedWindow`]'s own doc for the identity-
/// binding problem this closes. `Ok(None)` for every OBSERVE action
/// (`screenshot`/`list_windows`/`cursor_position`) and for any Write action
/// with no window argument at all (`wait`) — nothing to bind. For a Write
/// (input) action WITH a window argument, this now resolves `window_query`
/// AUTHORITATIVELY, right here, the INSTANT authorization actually lands
/// (whether via a standing grant or a human's card) — `Ok(Some(w))` on
/// success, or `Err` (fail-closed, never `Ok(None)`) if THAT resolution
/// itself fails: an input action must never be allowed to proceed at all
/// once its own approval step couldn't even pin down a window identity.
/// [`run_action`] threads the `Some(w)` through to its own later, FRESH
/// `resolve_window` call for that same action and refuses to inject if the
/// two disagree — see [`verify_approved_target`].
async fn approve(
    asks: &AskRegistry,
    thread: i32,
    dir: &str,
    action: &str,
    args: &Value,
) -> Result<Option<ApprovedWindow>, String> {
    let window_query = window_arg(args);
    let risk = crate::ask::classify_gui_action(action);
    let summary = if window_query.is_empty() {
        format!("computer: {action}")
    } else {
        format!("computer: {action} @ {window_query}")
    };
    let digest = args_digest(args);
    let action_key = crate::ask::action_key(&["gui", action, &window_query, &digest]);
    // UNREDACTED, even for `action == "type"`: the human approving THIS card
    // needs to see exactly what is about to be typed to judge whether to
    // allow it — a card that hid the text and only said "N characters" would
    // ask for a decision without the information the decision depends on.
    // Only the PERSISTED audit line redacts it, after the fact, once the
    // human has already made that call — see `redact_audit_args`'s doc for
    // the full symmetric point. This is the LOCAL desktop card's own detail —
    // see `detail_redacted`'s doc comment immediately below for the DIFFERENT,
    // EARLIER leak this alone does not close.
    let detail = args.to_string();
    // issue #160 round-4 P1 §1: `AskEvent::Opened` (fired inside
    // `request_with_preview` below) hands this ENTIRE `Ask` — `detail`
    // included — straight to the IM bridge, which renders it into an
    // outbound Lark card BEFORE the human ever answers anything (see
    // `im::outbound::perm_card`). For `action == "type"`, `detail` carries
    // the literal keystrokes about to be typed — a secret typed into a
    // password field would otherwise reach a third party the instant this
    // card opens, regardless of whether the human ever approves it. Reuses
    // the EXACT SAME redaction `redact_audit_args` already applies to the
    // durable audit line (`text` → `{"text_redacted":true,"text_chars":N}`)
    // rather than inventing a second shape — the IM-facing view and the
    // audit-log view have the identical "not the raw text, just its length"
    // requirement, so one function serves both. Every other action's detail
    // carries nothing this module considers secret (a coordinate, a window
    // name, a key combo LABEL like "cmd+s" — never what was typed), so this
    // is `None` for everything except `type`; `im::outbound::perm_card` falls
    // back to the unredacted `detail` in that case (see its own doc). Passed
    // to the ONLY production caller of `request_with_preview` — see that
    // method's own doc on why it grew this parameter directly rather than a
    // separate `_redacted` variant.
    let detail_redacted = (action == "type").then(|| redact_audit_args(action, args).to_string());

    // issue #160 round-5 review P1 §1: GUI actions — observation AND input
    // alike — go through `auto_decision_exact`, NOT the ordinary
    // `auto_decision` every other tool's ask-creation path uses. The
    // difference is deliberate: `auto_decision` also honors issue #103's
    // coarse "release all read-only" batch/issue grant for any
    // `RiskLevel::ReadOnly` ask, but `screenshot`/`list_windows` are
    // `ReadOnly` by `classify_gui_action`'s own construction REGARDLESS OF
    // WHICH WINDOW they target — so a session that once released "all
    // read-only" would otherwise silently auto-approve screenshotting or
    // enumerating ANY window on the human's desktop (mail, a browser tab, a
    // password manager, …) with no computer-specific card and no chance for
    // the human to see WHICH window before the pixels are captured. That is
    // a materially larger disclosure than the coarse grant was ever built to
    // cover (issue #103's own "skip the card for `git status`" scope). See
    // `AskRegistry::auto_decision_exact`'s own doc for the full rationale —
    // it still honors `dangerous` mode, a Full grant, and an EXACT
    // Always-grant `action_key` match, exactly like `auto_decision` does for
    // those same three cases; it just never falls through to the read-only
    // batch/issue grant underneath them.
    match asks.auto_decision_exact(thread, dir, &action_key) {
        // round-8 P1 #4: bind the window identity HERE, the instant this
        // standing grant authorizes the call — not left for `run_action`'s
        // OWN later resolution to discover on its own, which is exactly the
        // "approve one window, dispatch to a different one" gap this closes.
        Some(Decision::Allow) => return bind_approved_window(risk, &window_query),
        // `auto_decision_exact` never actually returns `Deny` today (only
        // Allow-only standing grants exist) — this arm keeps the gate
        // correct regardless, mirroring `handle_ask`'s own defensive shape,
        // rather than silently falling through to a redundant card for it.
        Some(Decision::Deny) => return Err("denied by a standing weft rule".to_string()),
        None => {}
    }

    let preview = preview_for_action(thread, dir, risk, &window_query);
    let (id, rx) = asks.request_with_preview(
        thread, dir, "computer", &summary, &detail, detail_redacted.as_deref(), risk, &action_key, preview,
    );

    match tokio::time::timeout(crate::bus::server::ASK_WAIT, rx).await {
        // round-8 P1 #4: same binding, taken the instant the human's card
        // itself resolves Allow — the window the card's own preview showed
        // may since have closed/been replaced by the time this returns, so
        // this resolves FRESH, right here, rather than trusting whatever
        // `preview_for_action` looked at when the card was first built.
        Ok(Ok(Decision::Allow)) => bind_approved_window(risk, &window_query),
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

/// The window identity [`approve`] itself resolved AUTHORITATIVELY, at the
/// instant it authorized ONE particular input (Write-classified) action —
/// issue #160 round-8 P1 #4. Threaded through to [`run_action`]'s own later,
/// FRESH `resolve_window` call for that SAME action (via
/// [`verify_approved_target`]) so a window that closed, was renamed, or got
/// its id reused by ANOTHER window in the gap between approval and dispatch
/// can never let that action land on the wrong target: the human's card (or
/// standing grant) showed/covered ONE window, so only that exact window may
/// receive the actual click/type/key/etc.
///
/// All three fields are checked (not just `id`): a closed window's id CAN be
/// reused by the OS/window manager for an entirely unrelated window, so `id`
/// alone is not sufficient to prove "this is still the window that was
/// approved" — `app`+`title` must also still match.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ApprovedWindow {
    id: u32,
    app: String,
    title: String,
}

/// issue #160 round-8 P1 #4: resolve `window_query` into an [`ApprovedWindow`]
/// binding, for an action `approve` just authorized — called from BOTH places
/// `approve` can return `Ok` (a standing grant, or a human's card). Observe
/// actions (`risk != Write`) and any Write action with no window argument at
/// all (`wait`) have no target to bind — `Ok(None)`, unchanged behavior. A
/// Write action WITH a window argument resolves it right here,
/// AUTHORITATIVELY: success binds `Some(ApprovedWindow)`; failure (not
/// found, or ambiguous) is `Err` — fail-CLOSED, never `Ok(None)` — since an
/// input action must not be allowed to proceed at all once even the approval
/// step's own resolution couldn't pin down a single window.
fn bind_approved_window(
    risk: crate::ask::RiskLevel,
    window_query: &str,
) -> Result<Option<ApprovedWindow>, String> {
    if risk != crate::ask::RiskLevel::Write || window_query.trim().is_empty() {
        return Ok(None);
    }
    let b = backend::backend();
    computer::resolve_window(b.as_ref(), window_query)
        .map(|w| Some(ApprovedWindow { id: w.id, app: w.app, title: w.title }))
        .map_err(|e| e.to_string())
}

/// issue #160 round-8 P1 #4: the execution-time check every input arm of
/// [`run_action`] runs right after its OWN fresh `resolve_window` (and right
/// BEFORE `activate_and_recheck`/the actual backend injection) — the target
/// this call is about to drive must be BYTE-FOR-BYTE the same window
/// [`approve`] itself bound at authorization time (`id`+`app`+`title` all
/// three — see [`ApprovedWindow`]'s own doc for why `id` alone isn't
/// sufficient). `approved: &None` (an observe action, or a Write action with
/// no window arg at all) always passes — nothing was bound, so there's
/// nothing to verify. A mismatch fails CLOSED with a message aimed at the
/// calling AGENT (MCP result text, not an i18n-routed UI string) telling it
/// to re-run so the human re-approves whatever the CURRENT target actually
/// is, rather than silently proceeding against a replaced window.
fn verify_approved_target(
    approved: &Option<ApprovedWindow>,
    w: &computer::WindowInfo,
) -> Result<(), String> {
    let Some(ap) = approved else {
        return Ok(());
    };
    if w.id != ap.id || w.app != ap.app || w.title != ap.title {
        return Err(
            "the target window changed since this action was approved — re-run so the human \
             re-approves the current target"
                .into(),
        );
    }
    Ok(())
}

/// Fixed key order for the "consequential parameters" digest folded into the
/// Always-grant `action_key` (issue #160 round-2 P2 §2) — every argument
/// across every action this server implements that actually determines WHAT
/// an action does to the human's screen. `text` carries `type`/`key`'s own
/// content; the rest are the coordinate/scroll/duration shapes the other
/// actions use. Anything NOT in this list (today: `action`, `window` — both
/// already folded into `action_key` as their own, separate parts — and
/// nothing else this server's schema defines) never affects the digest.
const DIGEST_PARAM_KEYS: &[&str] = &[
    "coordinate",
    "start_coordinate",
    "text",
    "scroll_direction",
    "scroll_amount",
    "duration_ms",
];

/// A stable sha256 hex digest of `args`'s consequential parameters (issue
/// #160 round-2 P2 §2), folded as the trailing element of the Always-grant
/// `action_key` so a standing grant is scoped to the EXACT parameters it was
/// granted for — not just `action` + `window`. Before this, `action_key =
/// ["gui", action, window]` meant a SINGLE Always answer to one `type` call
/// covered EVERY future `type` into that window with ANY text: an
/// Always-grant earned by approving "type your name into the search box"
/// would silently also auto-approve "type your password into the search
/// box" the next time an agent decided to send that, since the key never
/// captured what was actually typed. Same hazard, smaller blast radius, for
/// every other action's own consequential params (a click's coordinate, a
/// scroll's direction/amount, a drag's endpoints).
///
/// Builds a JSON object over [`DIGEST_PARAM_KEYS`] ONLY (never the raw
/// `args`, which also carries `action`/`window` — already their own, separate
/// `action_key` parts, so folding them in again here would be redundant, not
/// wrong, but also not needed), omitting any key `args` doesn't carry (a
/// `left_click`'s absent `text` must not hash the same as an explicit
/// `text: null`, and must never appear at all for an action that has no
/// `text`), then sha256-hexdigests that JSON's serialized bytes.
/// `serde_json::Map` has no `preserve_order` feature enabled in this crate
/// (see `Cargo.toml`), so its `Value::Object` serialization is ALREADY
/// stable key-sorted regardless of insertion order — this still inserts via
/// the fixed `DIGEST_PARAM_KEYS` order for readability, not because
/// correctness depends on it.
///
/// The DIGEST — never the raw JSON — is what actually lands in the
/// persisted `action_key`: unlike `detail` (the full unredacted params
/// `approve` shows the human on the card itself, which is ephemeral and gone
/// the instant the card resolves — see the inline comment at its own
/// definition above), an Always-grant's `action_key` is retained in memory
/// for the life of the session, so a literal `type` payload sitting in that
/// key would persist exactly the plaintext `redact_audit_args` exists to
/// keep OUT of the durable audit log, just relocated to a different
/// in-memory table. The digest gets the identical narrowing power (the same
/// call reuses the grant; ANY changed consequential param mints a new key,
/// forcing a fresh card) with none of that exposure — a card built from the
/// digest alone cannot be reversed back into the text that produced it.
///
/// `#[doc(hidden)] pub` (not `pub(crate)`): `tests/computer_mcp.rs` is a
/// separate integration-test crate that needs to pre-seed an `AlwaysGrant`
/// whose `action_key` matches EXACTLY what [`approve`] would itself compute
/// for a given call — mirrors `computer::mock::MockBackend`'s own doc
/// comment on why a test-only helper is `#[doc(hidden)] pub` rather than
/// `#[cfg(test)]`.
#[doc(hidden)]
pub fn args_digest(args: &Value) -> String {
    let mut canonical = serde_json::Map::new();
    for key in DIGEST_PARAM_KEYS {
        if let Some(v) = args.get(*key) {
            canonical.insert((*key).to_string(), v.clone());
        }
    }
    let canonical_json = Value::Object(canonical).to_string();
    let digest = Sha256::digest(canonical_json.as_bytes());
    format!("{digest:x}")
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
//
// issue #160 round-3 P1 §1 previously patched a SECOND hole here — an
// Interactive approval card dragging OS focus to Weft between the click and
// the keystrokes — by replaying the exact last-click coordinate right before
// `type`/`key`. Round-4 P1 §2 REMOVES that replay hack: it only ever helped
// `type`/`key` (a `left_click`/`scroll`/`drag`/`mouse_move` itself was still
// exposed — an absolute-coordinate action can land on Weft's own card if the
// card now covers the target window's on-screen position, not just a stale
// focus target), and replaying a synthetic click is itself not side-effect-
// free (it can collapse a double-click text selection, or re-toggle a
// checkbox/button the agent never asked to click again). See
// `activate_target`'s own doc, right below this section, for the actual fix:
// reactivating the TARGET window through `backend::ComputerBackend::
// activate_window` before ANY input action reaches the OS, not just
// `type`/`key` — and, since round-5 review P1 §6, before an AUTO-approved
// input action too, not only one that actually surfaced an Interactive card.

/// How long a click on a window is trusted to still hold that window's OS
/// focus for a subsequent `type`/`key` — see this section's own doc comment.
const FOCUS_FRESHNESS_MS: u64 = 15_000;
const FOCUS_FRESHNESS_SECS: u64 = FOCUS_FRESHNESS_MS / 1000;

/// Process-level "last window this `(thread, dir)` actually clicked, and
/// when" registry — see this section's own doc comment. `now_ms()`-based
/// (wall clock), consistent with every other timestamp this module already
/// records (the audit log's own `ts_ms`); a system clock adjustment
/// mid-session is not a hazard this heuristic needs to defend against.
/// issue #160 round-4 P1 §2: no longer carries the click's physical
/// `(px, py)` — that existed ONLY to feed the round-3 P1 §1 replay-click
/// hack this round removes (see this section's own top doc comment); the
/// window id + timestamp pair is all [`require_recent_focus`] itself has
/// ever needed.
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

// —— reclaiming the foreground before every input action (issue #160 round-4 P1 §2, broadened round-5 review P1 §6) ——

/// Reactivates the target window before the backend ever touches the OS —
/// click family, `mouse_move`, `left_click_drag`, `scroll`, `type`, `key`
/// (issue #160 round-4 P1 §2, replacing round-3 P1 §1's own click-replay
/// hack — see the focus-freshness section's own doc comment above for why
/// that hack was unsafe and insufficient). Called via [`activate_and_recheck`]
/// — NOT this function directly — from every input arm of [`run_action`]:
/// AFTER the FIRST [`recheck_after_guard`] (right after acquiring
/// `input_flight_guard`) and the arm's own fresh window resolution (issue
/// #160 round-6 review P1 #2+#3), and immediately followed by a SECOND
/// `recheck_after_guard` call before the action-specific backend call
/// itself — see [`activate_and_recheck`]'s own doc for why THIS call, being
/// a potentially slow, blocking OS call (`osascript`/`wmctrl`/`xdotool`),
/// needed its own dedicated recheck rather than trusting the one already
/// taken before it started.
///
/// Round-4 P1 §2 shipped this ONLY for an Interactive approval (a card that
/// actually rendered, so a human clicking Weft's own UI to answer it just
/// took the foreground away from the target). Round-5 review P1 §6 (issue
/// #160 #3) broadens it to EVERY input action, Auto-approved ones included:
/// Codex's own finding was that a standing grant deciding silently does NOT
/// mean the target window still holds the real OS foreground/focus at the
/// moment this call finally runs — a human sitting at the machine can switch
/// windows, alt-tab, or bring some OTHER app forward between an agent's calls
/// for reasons that have nothing to do with weft at all, and an Auto approval
/// (unlike an Interactive one) has no way to know whether that happened. So
/// this is now called UNCONDITIONALLY for every input action rather than
/// branching on how the call was approved — the (former) `Approval`
/// Auto/Interactive split existed ONLY to gate this call, and reactivating an
/// already-frontmost window is a cheap, idempotent no-op on every backend
/// this module drives, so there is no real cost to doing it every time
/// instead of only when a card is known to have stolen focus.
///
/// FAILS CLOSED, never falls through to the real action: `Unsupported` (no
/// `computer-os` feature, or a real backend that couldn't find a window-
/// activation API at all — see `backend::ComputerBackend::activate_window`'s
/// own doc) or any other backend error both propagate as this function's own
/// `Err`, naming why (the target window couldn't be reactivated) and the way
/// around it: answer/grant from weft's own desktop UI, where the foreground
/// never has anywhere else to go.
///
/// KNOWN, ACCEPTED residual (recorded here and in issue #160 §9, not
/// eliminated this round): even after this call succeeds, there is no
/// cross-platform primitive this module can call to VERIFY the target window
/// is truly frontmost at the exact instant the backend call right after this
/// one actually injects — neither `xcap` nor `enigo` exposes a "is this
/// window frontmost right now" query, so a real THIRD-PARTY focus-stealing
/// race (some other app/window grabbing focus back) in the gap between this
/// call returning `Ok` and the very next backend call could still, in
/// principle, land the input elsewhere. This closes the ORDINARY case (an
/// agent that never re-activates at all, or one that only did so for a
/// card-driven Interactive approval) — it is a floor, not a ceiling, exactly
/// like [`require_recent_focus`]'s own doc says about the freshness
/// heuristic it complements. This residual is scoped to third-party focus
/// theft specifically — a human hitting Stop DURING this call is a
/// DIFFERENT, now-closed hazard: see [`activate_and_recheck`]'s own doc for
/// the second `recheck_after_guard` that closes it (issue #160 round-6
/// review P1 #2).
fn activate_target(target_id: u32) -> Result<(), String> {
    backend::backend().activate_window(target_id).map_err(|e| {
        format!(
            "window {target_id} couldn't be activated before this input action ({e}) — answer \
             from weft's own desktop UI instead, where the foreground never has anywhere else to go"
        )
    })
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
///
/// issue #160 round-2 P2 §7: this used to grow UNBOUNDED for the life of the
/// process — one entry per (thread, dir) that ever screenshotted, never
/// evicted. [`MAX_PREVIEWS`] caps it; the value's third element is the
/// insertion timestamp [`store_screenshot_preview`] needs to find (and evict)
/// the single oldest entry once the map is full — see [`evict_oldest_if_full`].
fn screenshot_previews() -> &'static Mutex<HashMap<(i32, String), (String, u32, u64)>> {
    static PREVIEWS: OnceLock<Mutex<HashMap<(i32, String), (String, u32, u64)>>> = OnceLock::new();
    PREVIEWS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The registry's capacity (issue #160 round-2 P2 §7). Chosen generously
/// above any realistic number of SIMULTANEOUSLY ACTIVE computer-use sessions
/// this process would ever host at once (each entry is one small JPEG
/// thumbnail, refreshed per-session on its own next screenshot — this is not
/// a per-call log, just the latest preview per live session) — no LRU crate
/// needed for a bound this generous; a plain "find the one oldest entry and
/// drop it" scan is more than fast enough at this size.
const MAX_PREVIEWS: usize = 32;

/// Evict the single oldest entry (by insertion timestamp) from `map` if it is
/// already AT capacity — split out from [`store_screenshot_preview`] purely
/// so it's unit-testable against a `HashMap` with explicit, deterministic
/// timestamps: real wall-clock inserts in a tight test loop can land on the
/// SAME millisecond, which would make "the oldest" ambiguous for a test
/// relying on `now_ms()` alone. A no-op when `map` has room, or when the key
/// about to be written already exists (an UPDATE never grows the map, so it
/// never needs to evict anything to make room for itself) — see this
/// function's one caller for that guard.
fn evict_oldest_if_full(map: &mut HashMap<(i32, String), (String, u32, u64)>) {
    if map.len() < MAX_PREVIEWS {
        return;
    }
    if let Some(oldest_key) = map
        .iter()
        .min_by_key(|(_, (_, _, ts))| *ts)
        .map(|(k, _)| k.clone())
    {
        map.remove(&oldest_key);
    }
}

fn store_screenshot_preview(thread: i32, dir: &str, preview: String, window_id: u32) {
    let mut g = screenshot_previews().lock().unwrap_or_else(|e| e.into_inner());
    let key = (thread, dir.to_string());
    // Only evict to make room for a genuinely NEW key — refreshing an
    // EXISTING (thread, dir)'s preview (the common case: a session that
    // screenshots repeatedly) must not count against capacity or trigger an
    // eviction of some unrelated session's entry.
    if !g.contains_key(&key) {
        evict_oldest_if_full(&mut g);
    }
    g.insert(key, (preview, window_id, now_ms()));
}

/// The most recent screenshot preview (and the window id it came from) for
/// `(thread, dir)`, if any — see [`screenshot_previews`]'s doc. Read only
/// from [`preview_for_action`] within this same module now (the round-2 P1
/// server-side gate owns preview attachment; `bus::server::handle_ask` no
/// longer does — see this module's own top doc comment).
fn last_screenshot_preview(thread: i32, dir: &str) -> Option<(String, u32)> {
    let g = screenshot_previews().lock().unwrap_or_else(|e| e.into_inner());
    g.get(&(thread, dir.to_string()))
        .map(|(preview, window_id, _ts)| (preview.clone(), *window_id))
}

fn window_arg(args: &Value) -> String {
    args.get("window").and_then(|v| v.as_str()).unwrap_or("").to_string()
}

/// The ordered gate every INPUT action (the click family, `type`, `key`,
/// `scroll`, `left_click_drag`, `mouse_move` — never `screenshot`,
/// `list_windows`, `cursor_position`, or `wait`) must clear before it's
/// allowed to touch the backend at all:
/// 1. is a permission card already blocking this (thread, dir) —
///    [`ComputerError::SuspendedPendingAsk`] — see [`check_suspended`];
/// 2. does someone else hold the control lease — [`ComputerError::Busy`] —
///    see [`acquire_and_throttle`];
/// 3. are we going faster than the global input throttle allows —
///    [`ComputerError::RateLimited`] — also [`acquire_and_throttle`].
///
/// issue #160 round-2 P2 §4 split this single gate in two and moved the
/// MUTATING half ([`acquire_and_throttle`] — it actually takes the 30s
/// control lease and consumes a throttle slot) to run AFTER every
/// action-specific, purely-argument-shaped check for that action (the window
/// argument's presence, the coordinate/text/scroll shape) has already
/// passed, right before `input_flight_guard`. Before this split, the
/// mutating half ran FIRST, so a malformed call (e.g. a `left_click` missing
/// `coordinate`) still occupied the 30s lease and a throttle slot — and lit
/// the desktop-control banner in Settings — for a call that was always going
/// to be rejected anyway. [`check_suspended`] (the non-mutating half) still
/// runs early, right after `approve`: it's about a DIFFERENT, unrelated ask
/// still waiting on the human (so an agent can't click through/at the
/// permission UI while it's up), and doesn't itself acquire or consume
/// anything, so there's no cost to checking it before argument parsing.
///
/// issue #160 round-6 review P1 #2+#3: the window's actual RESOLUTION (and,
/// for the mouse family, the coordinate's mapping against it; for `type`/
/// `key`, the focus-freshness check against it) are NOT purely-argument
/// checks — they depend on the live desktop's current state, which can have
/// changed while this call sat queued on `input_flight_guard` behind another
/// session's in-flight action. Those now run AFTER `input_flight_guard`/the
/// first [`recheck_after_guard`] instead, immediately followed by
/// [`activate_and_recheck`] — see that function's own doc, and each input
/// arm of `run_action`, for the full ordering this section now describes.
///
/// Both halves run AFTER [`approve`] in [`run_action`]'s dispatch, never
/// before: `has_open` here is a completely separate concern from "is THIS
/// call authorized at all", which `approve` alone decides — checking them in
/// the other order would mean an action already suspended behind someone
/// else's card could still open (and block on) its OWN card underneath it.
fn check_suspended(asks: &AskRegistry, thread: i32, dir: &str) -> Result<(), String> {
    if asks.has_open(thread, dir) {
        return Err(ComputerError::SuspendedPendingAsk.to_string());
    }
    Ok(())
}

/// The MUTATING half of the input gate — see [`check_suspended`]'s doc
/// comment for the full round-2 P2 §4 ordering rationale. Called ONLY once
/// every fallible, non-mutating check for this specific action has already
/// passed, immediately before the backend call itself: a call that was
/// always going to be rejected for a bad argument never reaches this, so it
/// never occupies the control lease or a throttle slot.
fn acquire_and_throttle(thread: i32, dir: &str) -> Result<(), String> {
    computer::acquire_control(thread, dir).map_err(|e| e.to_string())?;
    computer::throttle_input().map_err(|e| e.to_string())?;
    Ok(())
}

/// A gate every input branch of [`run_action`] clears TWICE now (issue #160
/// round-6 review P1 #2): once immediately after acquiring
/// `computer::input_flight_guard()` (before that branch's own fresh window
/// resolution/coordinate mapping/focus check), and again via
/// [`activate_and_recheck`] right after `activate_target` — see that
/// function's own doc for why a second call is needed on top of the first.
/// Either call re-verifies the kill switch AND that the control lease this
/// call took in [`acquire_and_throttle`] is STILL held by THIS EXACT
/// `(thread, dir)`.
///
/// Why this is needed on top of every earlier check: `input_flight_guard` is
/// a single process-wide mutex a SECOND `tools/call` for the SAME session
/// can queue on for as long as the FIRST call's own backend round trip takes
/// (see that function's own doc comment on why it's held for the whole
/// backend call). While a caller sits in that queue, a human can hit Stop —
/// disabling the setting and clearing the control lease
/// (`computer::emergency_stop`) — or, once the lease self-heals on expiry, a
/// completely different `(thread, dir)` could acquire it first. Neither
/// `approve`'s own post-await recheck (issue #160 round-2 P1 §1, which only
/// re-runs once, right after the approval gate, long before the guard is
/// even requested) nor `acquire_and_throttle`'s own `acquire_control` call
/// (which already ran, successfully, before this caller ever started
/// queuing) catch either of those — this is the ONE checkpoint positioned
/// AFTER the queue itself, so a call that waited behind someone else's long
/// hold sees the world as it is NOW, not as it was when it first queued.
///
/// `Ok` requires ALL THREE: no OTHER ask now open for this `(thread, dir)`
/// (issue #160 round-5 review P1 §2 — see the paragraph below),
/// [`computer::enabled`] still true, AND [`computer::control_state`] naming
/// this EXACT `(thread, dir)` as the current holder — a DIFFERENT holder, or
/// no holder at all (an expired or force-cleared lease), both fail closed
/// rather than let a call that no longer holds the lease it thinks it does
/// reach the backend anyway.
///
/// issue #160 round-5 review P1 §2: this used to check only enabled+lease —
/// Codex's finding was that a call queued on `input_flight_guard` can have a
/// BRAND-NEW ask open for its SAME `(thread, dir)` while it waits (a
/// completely unrelated permission request from the same engine, racing in
/// through its own hook), and once such a call finally acquires the guard it
/// went straight to dispatch, exactly the "click through the card" hazard
/// [`check_suspended`]'s own up-front check exists to prevent — just arriving
/// at it from behind the queue instead of before it. `asks.has_open` here can
/// only ever be seeing a DIFFERENT ask than the one THIS call itself went
/// through in [`approve`]: that one is guaranteed resolved (no longer open)
/// by the time `approve` returns, since it only returns once its own
/// `oneshot::Receiver` has actually been answered (or the call was denied/
/// cancelled, which returns `Err` before ever reaching this function at all).
/// Same fail-closed shape and the SAME `ComputerError::SuspendedPendingAsk`
/// text [`check_suspended`] itself returns — from the calling agent's point
/// of view, this is indistinguishable from having queued behind that check
/// in the first place.
async fn recheck_after_guard(db: &Db, asks: &AskRegistry, thread: i32, dir: &str) -> Result<(), String> {
    if asks.has_open(thread, dir) {
        return Err(ComputerError::SuspendedPendingAsk.to_string());
    }
    if !computer::enabled(db).await {
        return Err(ComputerError::Disabled.to_string());
    }
    match computer::control_state() {
        Some(holder) if holder.thread == thread && holder.dir == dir => Ok(()),
        Some(holder) => Err(ComputerError::Busy { thread: holder.thread, dir: holder.dir }.to_string()),
        None => Err(
            "the control lease was lost while this call was queued (it may have expired, or been \
             cleared by a kill switch) — retry"
                .to_string(),
        ),
    }
}

/// The shared "reactivate, then recheck the kill switch/lease a SECOND
/// time" tail every input branch of [`run_action`] runs, immediately after
/// its own branch-specific fresh window resolution (and, for the mouse
/// family, coordinate remap / for `type`/`key`, focus-freshness check) and
/// right before the actual backend call (issue #160 round-6 review P1 #2).
///
/// `activate_target` shells out to a blocking OS call (`osascript`/
/// `wmctrl`/`xdotool` — see its own doc) that can itself take a real amount
/// of wall-clock time. The FIRST `recheck_after_guard`, right after
/// acquiring `input_flight_guard`, only proves the kill switch/lease were
/// still fine at the INSTANT the guard was acquired — a human hitting Stop
/// DURING the activation call that follows would otherwise go unnoticed,
/// and the backend call right after `activate_target` returns would inject
/// input anyway even though the lease is gone and the latch is tripped.
/// This re-runs the identical check one more time, right after activation
/// returns, so a Stop that lands mid-activation is still honored before the
/// backend ever sees the injection — the caller must `?` this and never
/// fall through to its own backend call on an `Err` here.
async fn activate_and_recheck(
    db: &Db,
    asks: &AskRegistry,
    thread: i32,
    dir: &str,
    window_id: u32,
) -> Result<(), String> {
    activate_target(window_id)?;
    recheck_after_guard(db, asks, thread, dir).await
}

/// `arr[0]`/`arr[1]` must each fit `u32` — issue #160 round-3 P2 §3: this
/// used to read each as `i64` then cast `as u32`, which silently WRAPS an
/// in-range-for-i64-but->u32::MAX value (e.g. `4294967296` — `2^32` — casts
/// straight to `0`), letting an absurd JSON integer sail past the "must be
/// non-negative" filter and land on a real, in-bounds-looking coordinate
/// instead of being rejected. `u32::try_from` fails (rather than wrapping)
/// for anything outside `0..=u32::MAX`, including negatives, so the single
/// `.and_then` below both replaces the old separate `>= 0` filter and closes
/// the overflow hole with the SAME existing error text — no behavior change
/// for any value that was already being accepted or rejected correctly.
fn parse_coordinate(args: &Value, key: &str) -> Result<(u32, u32), String> {
    let Some(arr) = args.get(key).and_then(|v| v.as_array()) else {
        return Err(format!("missing required '{key}': [x, y]"));
    };
    if arr.len() != 2 {
        return Err(format!("'{key}' must be exactly [x, y]"));
    }
    let x = arr[0].as_i64().and_then(|v| u32::try_from(v).ok());
    let y = arr[1].as_i64().and_then(|v| u32::try_from(v).ok());
    match (x, y) {
        (Some(x), Some(y)) => Ok((x, y)),
        _ => Err(format!("'{key}' must be two non-negative integers [x, y]")),
    }
}

fn required_text(args: &Value) -> Result<&str, String> {
    args.get("text")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "missing required 'text'".to_string())
}

/// Hard ceiling on `type`'s payload length (issue #160 round-5 review P2
/// §3): a single approved `type` call is meant to be a bounded UI
/// interaction — a form field, a search box, a short reply — not an
/// HTTP-body-sized blob. Under a Full/Always grant, `enigo.type_text` runs
/// completely SYNCHRONOUSLY inside [`computer::input_flight_guard`] — held
/// for this whole call, per that function's own doc — with no way to CANCEL
/// an in-flight injection partway through: an unbounded string could keep
/// injecting well past `CONTROL_LEASE_MS` (the 30s control lease this same
/// call already holds), well past however long the OS-level global Escape
/// shortcut takes to register/unregister around a lease
/// (`computer::sync_shortcut_state`), and well past anything else this
/// module's other fixed-duration gates implicitly assume a single input
/// action finishes within.
///
/// `MAX_TYPE_CHARS` is a DETERMINISTIC bound, not a full fix: chunking a
/// long `type` into multiple shorter injections with a lease renewal between
/// chunks (so the FIRST chunk's own cancellation window would actually cover
/// the rest) is the more complete answer, but it needs a change at the
/// `enigo` layer this round doesn't touch (there is no partial/cancellable
/// `type_text` this module could call into today) — tracked as a follow-up,
/// not required to close the bound this ceiling already provides
/// unconditionally. `5000` chars is generous for any single interactive
/// field a human would plausibly want an agent to fill in one call, while
/// comfortably ruling out something HTTP-body-shaped.
///
/// `#[doc(hidden)] pub` (not `pub(crate)`): `tests/computer_mcp.rs` is a
/// separate integration-test crate that needs the EXACT cap this module
/// enforces to build an over-limit payload, rather than hardcoding a
/// duplicate literal that could silently drift from this one — mirrors
/// `args_digest`'s own doc comment on why a test-only-consumed constant is
/// `#[doc(hidden)] pub` here rather than `#[cfg(test)]`.
#[doc(hidden)]
pub const MAX_TYPE_CHARS: usize = 5_000;

/// Reject a `type` payload over [`MAX_TYPE_CHARS`] — checked right after
/// [`required_text`] and before the control lease/throttle are ever touched
/// (the same "fail on a bad argument before it costs anything" ordering
/// discipline every other action's argument validation already follows —
/// see `check_suspended`'s own doc comment), and well before
/// `backend::ComputerBackend::type_text` ever sees the string.
fn check_type_length(text: &str) -> Result<(), String> {
    let len = text.chars().count();
    if len > MAX_TYPE_CHARS {
        return Err(format!(
            "text is too long: {len} characters (max {MAX_TYPE_CHARS}) — split it into multiple \
             shorter type calls, or shorten it"
        ));
    }
    Ok(())
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
/// a resolution or write failure here (including a refused symlink — see
/// [`refuse_symlinks`], or an `O_NOFOLLOW`-refused leaf — see
/// [`open_audit_file_for_append`]) never affects the actual tool result, it
/// just means this one call goes unlogged.
async fn append_audit(db: &Db, thread: i32, dir: &str, wt: Option<i32>, entry: &AuditEntry<'_>) {
    use tokio::io::AsyncWriteExt;
    let Some(path) = audit_log_path(db, thread, dir, wt).await else {
        return;
    };
    let Some(parent) = path.parent() else { return };
    if tokio::fs::create_dir_all(parent).await.is_err() {
        return;
    }
    let Ok(line) = audit_line(entry) else { return };
    let Ok(mut file) = open_audit_file_for_append(&path).await else {
        return;
    };
    let _ = file.write_all(line.as_bytes()).await;
}

/// The actual `open(2)` call [`append_audit`] makes — factored out so its own
/// `O_NOFOLLOW` defense (issue #160 round-4 P2 §3) is unit-testable in
/// isolation from [`refuse_symlinks`]'s EARLIER, per-component check.
///
/// [`refuse_symlinks`] (via [`audit_log_path`]) already refuses a symlink
/// that's ALREADY sitting at any path component — including the leaf file
/// itself — by the time it runs its `symlink_metadata` check. What it can
/// NOT close is the window AFTER that check returns and BEFORE this exact
/// `open` call executes: a worktree is repository-controlled content, so
/// anything with write access to the checkout (an agent's own earlier
/// approved write, or a background process) can swap the leaf
/// `computer-audit.jsonl` for a symlink to an arbitrary path in that
/// instant, and a plain `open` would silently follow it — this weft
/// PROCESS, not the sandboxed agent, would then be the one creating/
/// appending to a file outside the worktree.
///
/// `#[cfg(unix)]` adds `O_NOFOLLOW`: when the FINAL path component is a
/// symlink, the kernel's own `open(2)` fails outright (`ELOOP`) instead of
/// following it — this closes the TOCTOU window at the exact leaf
/// component, atomically, with no separate stat call of our own to race
/// against. It complements (does not replace) `refuse_symlinks`'s own
/// per-component walk, which still guards every PARENT directory
/// (`.weft` itself) — `O_NOFOLLOW` only ever inspects the LAST component of
/// the path being opened, never the ones leading up to it.
///
/// Windows keeps the pre-existing `refuse_symlinks`-only behavior for now —
/// a real per-component atomic open (an `openat`-style walk with
/// `FILE_FLAG_OPEN_REPARSE_POINT` at each step) is a follow-up, not required
/// to close THIS specific leaf race, which is what this flag targets.
async fn open_audit_file_for_append(path: &std::path::Path) -> std::io::Result<tokio::fs::File> {
    let mut options = tokio::fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW);
    options.open(path).await
}

/// The session's own working directory for `(thread, dir)`, before any
/// output-specific suffix is appended — the shared base for
/// [`screenshot_out_dir`] (which appends a DIFFERENT suffix per lane; see
/// its own doc comment) and [`audit_log_path`] (which appends the SAME
/// `.weft/computer-audit.jsonl` suffix for both lanes).
///
///  - worker lane (`dir` a direction id): a worktree of that direction's —
///    see `wt`'s own doc below for exactly which one.
///  - lead lane (`dir == bus::LEAD`): the lead's own scratch cwd,
///    `<weft_home>/leads/<thread>` — same formula as
///    `lead_chat::commands::ensure_lead_cwd`, duplicated here since that
///    helper is private to its own module (see `builtin_allow.rs`'s doc
///    comment on why a lead's cwd is this scratch dir and not one of its
///    workspace's repos).
///
/// `wt` (issue #160 round-2 P2 §5): the CALLER's own worktree id, when it
/// could resolve one — see `inject::computer_url`'s doc for who sets this
/// and why (a multi-repo direction has MORE THAN ONE worktree row, and
/// without this every worker sharing that direction fell back to whichever
/// one happened to be first — screenshots/audit for a worker in repo B could
/// silently land inside repo A's checkout instead). CLOSED-SET validated
/// here: `Some(id)` is only honored when `id` names a worktree that actually
/// belongs to THIS direction (which is itself already confirmed to belong to
/// THIS thread, below) — a forged/foreign worktree id can never redirect
/// output into another direction's (or another thread's) worktree.
///
/// issue #160 round-8 P2 #7: an EXPLICIT `wt` that fails that check is now
/// FAIL-CLOSED — `None` — rather than silently falling back to "the first
/// worktree of this direction" as it used to. That old fallback was fine for
/// an ABSENT `wt` (there was never a specific worktree pinned to begin with),
/// but for an EXPLICIT-but-invalid one (deleted, or naming a worktree of a
/// DIFFERENT direction/repo) it actively misdirected output: in a multi-repo
/// direction, a worker session that explicitly pinned worktree B and lost the
/// race against a deletion (or was handed a forged/stale id) would have had
/// its screenshots and audit log silently written into worktree A's checkout
/// instead — exposing that OTHER repo's on-screen pixels and activity to the
/// wrong worker. Only a genuinely ABSENT `wt` (`None`) still falls back to
/// "first worktree for this direction"; an explicit id that doesn't resolve
/// now fails the whole call closed instead of picking a different worktree
/// on its own.
///
/// `None` on any failure (DB error, unresolvable path, deleted worktree row,
/// a numeric `dir` that doesn't resolve to a direction belonging to THIS
/// thread, or — per round-8 P2 #7 above — an EXPLICIT `wt` that doesn't name
/// a worktree of this direction) — callers turn that into their own
/// soft-failure text rather than a 500.
async fn session_root(db: &Db, thread: i32, dir: &str, wt: Option<i32>) -> Option<std::path::PathBuf> {
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
    match wt {
        // round-8 P2 #7: an EXPLICIT pin must hit an actual worktree of THIS
        // direction, or the whole call fails closed — silently falling back
        // to the first worktree would write this session's screenshots/audit
        // into a DIFFERENT repo's checkout in a multi-repo direction.
        Some(id) => worktrees.iter().find(|w| w.id == id).map(|w| std::path::PathBuf::from(&w.path)),
        None => worktrees.into_iter().next().map(|w| std::path::PathBuf::from(w.path)),
    }
}

/// Build `base/components[0]/components[1]/...`, refusing if ANY existing
/// path component along the way is a symlink (issue #160 round-2 P2 §3): a
/// worktree is repository-controlled content, so if anything with write
/// access to the checkout (an agent's own earlier approved write, or the
/// repo itself) replaces `.weft`, `.weft/computer-audit.jsonl`, or
/// `.weft/screenshots` with a symlink, this WEFT PROCESS — not the sandboxed
/// agent — would otherwise happily create/append/write straight through it
/// to an arbitrary path OUTSIDE the worktree.
///
/// Checked component-by-component from `base` via `symlink_metadata` (never
/// plain `metadata`, so a symlink is caught even when it points at something
/// real rather than dangling): a component that doesn't exist yet is fine —
/// it hasn't been substituted for anything, and the caller's own
/// `create_dir_all`/`OpenOptions::create` will make it fresh — while one that
/// exists and IS a symlink is refused outright, named in the error so the
/// audit/screenshot caller can report which exact segment was compromised.
/// Mirrors `skills::inject::builtin_write_path_is_safe`'s own
/// component-walk shape (a different module, same hazard).
fn refuse_symlinks(base: &std::path::Path, components: &[&str]) -> Result<std::path::PathBuf, String> {
    let mut current = base.to_path_buf();
    for seg in components {
        current.push(seg);
        if let Ok(meta) = std::fs::symlink_metadata(&current) {
            if meta.file_type().is_symlink() {
                return Err(format!(
                    "refusing to write through a symlink at {}",
                    current.display()
                ));
            }
        }
    }
    Ok(current)
}

/// Resolve the screenshot output directory for `(thread, dir[, wt])`:
///  - worker lane: `<worktree>/.weft/screenshots` — excluded from git via
///    `git::git_exclude` so weft's own screenshots never show up in `git
///    status`/diffs.
///  - lead lane: the lead's scratch cwd itself, `<weft_home>/leads/<thread>`
///    (no extra `.weft/` layer — that whole directory is already
///    weft-private).
///
/// `Err` (not silently `None`) on a resolution failure OR a refused symlink
/// (issue #160 round-2 P2 §3, via [`refuse_symlinks`]) — callers surface the
/// SPECIFIC reason (missing worktree vs. a compromised `.weft` path) to the
/// calling agent rather than one flattened "no worktree" text for both.
async fn screenshot_out_dir(db: &Db, thread: i32, dir: &str, wt: Option<i32>) -> Result<std::path::PathBuf, String> {
    let root = session_root(db, thread, dir, wt)
        .await
        .ok_or_else(|| "no worktree for this session".to_string())?;
    if dir == crate::bus::LEAD {
        return Ok(root);
    }
    crate::git::git_exclude(&root, ".weft/");
    refuse_symlinks(&root, &[".weft", "screenshots"])
}

/// Resolve the audit log path for `(thread, dir[, wt])`: `<session_root>/
/// .weft/computer-audit.jsonl` for BOTH lanes (unlike [`screenshot_out_dir`],
/// which skips the `.weft/` layer for the lead lane — the audit log always
/// gets it, so it never collides with anything else a lead's scratch cwd
/// might hold). Covered by the same `git::git_exclude(".weft/")` call as
/// screenshots for the worker lane; a no-op, harmless call for the
/// (non-git) lead scratch dir. `None` (best-effort, per [`append_audit`]'s
/// own doc) on a resolution failure OR a refused symlink (issue #160 round-2
/// P2 §3, via [`refuse_symlinks`]) — a compromised `.weft` just means this
/// one call goes unlogged, same as any other audit-write failure.
async fn audit_log_path(db: &Db, thread: i32, dir: &str, wt: Option<i32>) -> Option<std::path::PathBuf> {
    let root = session_root(db, thread, dir, wt).await?;
    crate::git::git_exclude(&root, ".weft/");
    refuse_symlinks(&root, &[".weft", "computer-audit.jsonl"]).ok()
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

    /// issue #160 round-3 P2 §3: a JSON integer that fits `i64` but overflows
    /// `u32` (here `2^32` exactly) used to wrap `as u32` straight to `0` —
    /// silently passing the "non-negative" check and landing on a real,
    /// in-bounds-looking coordinate instead of being rejected. Covers both
    /// `x` and `y`, and both `coordinate` and `start_coordinate` share this
    /// same helper (`key` is just a label), so one fix covers both call
    /// sites in `run_action`.
    #[test]
    fn parse_coordinate_rejects_values_above_u32_max_without_wrapping() {
        let err = parse_coordinate(&json!({"coordinate": [4_294_967_296i64, 0]}), "coordinate").unwrap_err();
        assert!(err.contains("non-negative"), "{err}");
        let err_y = parse_coordinate(&json!({"coordinate": [0, 4_294_967_296i64]}), "coordinate").unwrap_err();
        assert!(err_y.contains("non-negative"), "{err_y}");
        let err_start = parse_coordinate(&json!({"start_coordinate": [4_294_967_296i64, 0]}), "start_coordinate")
            .unwrap_err();
        assert!(err_start.contains("non-negative"), "{err_start}");
        // The last value that DOES fit must still be accepted.
        assert_eq!(
            parse_coordinate(&json!({"coordinate": [u32::MAX as i64, 0]}), "coordinate").unwrap(),
            (u32::MAX, 0)
        );
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

    // —— issue #160 round-4 P1 §1: detail_redacted (the IM-bridge leak) ——

    /// The end-to-end property the fix exists for: a `type` action's Ask
    /// keeps the RAW text in `detail` (the LOCAL desktop card still needs it
    /// to judge the approval) but ALSO carries a `detail_redacted` (what
    /// `im::outbound::perm_card` must show instead) that never contains the
    /// raw text — the same shape `redact_audit_args` already gives the
    /// durable audit line.
    #[tokio::test]
    async fn approve_sets_detail_redacted_for_type_but_keeps_the_local_detail_raw() {
        let asks = AskRegistry::new();
        let thread = 907_001;
        let dir = "lead";
        let args = json!({"action": "type", "window": "notes", "text": "hunter2"});

        let asks_bg = asks.clone();
        let handle = tokio::spawn(async move { approve(&asks_bg, thread, dir, "type", &args).await });

        let mut card = None;
        for _ in 0..200 {
            if let Some(a) = asks.open().into_iter().find(|a| a.thread == thread && a.dir == dir) {
                card = Some(a);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let card = card.expect("a Needs-you card must appear for a type action");

        assert!(card.detail.contains("hunter2"), "the LOCAL card must keep the raw text: {card:?}");
        let redacted = card
            .detail_redacted
            .as_deref()
            .expect("a type action's Ask must carry detail_redacted");
        assert!(
            !redacted.contains("hunter2"),
            "detail_redacted must never contain the raw typed text: {redacted}"
        );
        assert!(redacted.contains("text_redacted"), "{redacted}");

        assert!(asks.answer(card.id, crate::ask::Answer::Deny));
        let _ = handle.await.unwrap();
    }

    /// Every action other than `type` must carry NO `detail_redacted` at
    /// all — nothing else this server's schema defines is treated as secret
    /// content, so `im::outbound::perm_card` falls back to the (already
    /// harmless) full `detail` for these.
    #[tokio::test]
    async fn approve_leaves_detail_redacted_none_for_a_non_type_action() {
        let asks = AskRegistry::new();
        let thread = 907_002;
        let dir = "lead";
        let args = json!({"action": "left_click", "window": "notes", "coordinate": [1, 1]});

        let asks_bg = asks.clone();
        let handle = tokio::spawn(async move { approve(&asks_bg, thread, dir, "left_click", &args).await });

        let mut card = None;
        for _ in 0..200 {
            if let Some(a) = asks.open().into_iter().find(|a| a.thread == thread && a.dir == dir) {
                card = Some(a);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let card = card.expect("a Needs-you card must appear for a left_click action");
        assert!(card.detail_redacted.is_none(), "{card:?}");

        assert!(asks.answer(card.id, crate::ask::Answer::Deny));
        let _ = handle.await.unwrap();
    }

    // —— issue #160 round-5 review P1 §1: GUI actions never sweep in the
    // generic read-only batch/issue grant ——

    /// The end-to-end property the fix exists for: a session already holding
    /// issue #103's "release all read-only for this session" grant — the
    /// GENERIC, cross-tool batch grant, not anything computer-specific — must
    /// still see a real Needs-you card for a `screenshot`/`list_windows`
    /// call, because those are `RiskLevel::ReadOnly` by `classify_gui_action`'s
    /// own construction REGARDLESS of which window they target. Before this
    /// fix, `approve` called the ordinary `auto_decision` (which DOES honor
    /// that grant for any ReadOnly ask) and would have silently captured
    /// pixels/enumerated windows with no card and no computer-specific
    /// Always ever granted.
    #[tokio::test]
    async fn screenshot_still_cards_despite_a_read_only_session_grant_with_no_exact_always() {
        let asks = AskRegistry::new();
        let thread = 908_001;
        let dir = "lead";
        asks.grant_read_only_session(thread, dir);

        let args = json!({"action": "screenshot", "window": "notes"});
        let asks_bg = asks.clone();
        let handle = tokio::spawn(async move { approve(&asks_bg, thread, dir, "screenshot", &args).await });

        let mut card = None;
        for _ in 0..200 {
            if let Some(a) = asks.open().into_iter().find(|a| a.thread == thread && a.dir == dir) {
                card = Some(a);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let card = card.expect(
            "a screenshot must still surface a Needs-you card despite the read-only-session grant \
             — it must never be swept in by the generic read-only batch grant",
        );
        assert_eq!(card.risk, crate::ask::RiskLevel::ReadOnly, "{card:?}");

        assert!(asks.answer(card.id, crate::ask::Answer::Deny));
        let err = handle.await.unwrap().unwrap_err();
        assert!(err.contains("denied"), "{err}");
    }

    /// Same property for the ISSUE-wide read-only grant (issue #103's
    /// dispatch-approval propagation) — `list_windows` must still card too.
    #[tokio::test]
    async fn list_windows_still_cards_despite_a_read_only_issue_grant() {
        let asks = AskRegistry::new();
        let thread = 908_002;
        let dir = "lead";
        asks.grant_read_only_issue(thread);

        let args = json!({"action": "list_windows"});
        let asks_bg = asks.clone();
        let handle = tokio::spawn(async move { approve(&asks_bg, thread, dir, "list_windows", &args).await });

        let mut card = None;
        for _ in 0..200 {
            if let Some(a) = asks.open().into_iter().find(|a| a.thread == thread && a.dir == dir) {
                card = Some(a);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let card = card.expect(
            "list_windows must still surface a Needs-you card despite the read-only-issue grant",
        );
        assert_eq!(card.risk, crate::ask::RiskLevel::ReadOnly, "{card:?}");

        assert!(asks.answer(card.id, crate::ask::Answer::Deny));
        let err = handle.await.unwrap().unwrap_err();
        assert!(err.contains("denied"), "{err}");
    }

    /// The other half of the property: a session with NO grant at all except
    /// a precise, EXACT `action_key` Always grant for this SAME
    /// `screenshot @ notes` call is still auto-approved without a card —
    /// `auto_decision_exact` keeps honoring Full/Always exactly like
    /// `auto_decision` does, it just drops the read-only batch/issue
    /// fallback. Proves the fix is a narrowing, not a blanket "GUI actions
    /// never auto-approve".
    #[tokio::test]
    async fn screenshot_auto_approves_with_an_exact_always_grant_and_no_read_only_batch() {
        let asks = AskRegistry::new();
        let thread = 908_003;
        let dir = "lead";
        let args = json!({"action": "screenshot", "window": "notes"});
        let action_key = crate::ask::action_key(&["gui", "screenshot", "notes", &args_digest(&args)]);
        asks.seed_grants(crate::ask::GrantSnapshot {
            full: Vec::new(),
            always: vec![crate::ask::AlwaysGrant { thread, dir: dir.to_string(), action_key }],
        });

        // No card ever appears — this must resolve on its own, promptly.
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            approve(&asks, thread, dir, "screenshot", &args),
        )
        .await
        .expect("an exact Always grant must auto-approve without ever needing a human answer");
        assert!(result.is_ok(), "{result:?}");
        assert!(asks.open().is_empty(), "an auto-approved call must never surface a card");
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

    // —— issue #160 round-4 P1 §2, broadened round-5 review P1 §6: activate_target ——

    /// The ONE shared `MockBackend` every test in this module that needs to
    /// drive `run_action` through the process-wide `backend::backend()`
    /// singleton installs — `backend::_set_backend_override` is a
    /// set-ONCE-per-process `OnceLock` (see its own doc comment), so a
    /// second test calling it with a DIFFERENT `MockBackend` instance would
    /// just silently keep using whichever one happened to install first.
    /// Every such test calls THIS helper instead of installing its own, then
    /// configures the SAME instance's interior-mutable fields
    /// (`windows_override`, `actions`, `fail_activate`, `on_activate`) for
    /// its own scenario — resetting whatever it depends on at the START of
    /// its own test body (never relying on a previous test's cleanup), and
    /// under `computer::process_state_test_lock` like every other test in
    /// this file that touches shared, un-keyed process state (the mock
    /// backend override is exactly that kind of state once installed).
    fn shared_mock() -> std::sync::Arc<computer::mock::MockBackend> {
        static MOCK: OnceLock<std::sync::Arc<computer::mock::MockBackend>> = OnceLock::new();
        MOCK.get_or_init(|| {
            let mock = std::sync::Arc::new(computer::mock::MockBackend::default());
            backend::_set_backend_override(mock.clone());
            mock
        })
        .clone()
    }

    /// The end-to-end property this fix exists for, post round-5 review P1
    /// §6: `activate_target` reactivates the target window
    /// (`backend.activate_window`) UNCONDITIONALLY, every time it's called —
    /// there is no longer an Auto/Interactive distinction to skip it for (see
    /// this function's own doc for why Codex's round-5 finding removed that
    /// distinction). Also covers the fail-closed path: when activation itself
    /// is broken (`Unsupported`), this must propagate an `Err` naming the
    /// window, never fall through and let the real action reach the OS
    /// anyway.
    #[test]
    fn activate_target_always_activates_and_fails_closed_when_unsupported() {
        let _guard = computer::process_state_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        let mock = shared_mock();
        mock.fail_activate.store(false, std::sync::atomic::Ordering::SeqCst);
        *mock.on_activate.lock().unwrap_or_else(|e| e.into_inner()) = None;
        mock.actions.lock().unwrap_or_else(|e| e.into_inner()).clear();

        // Every call activates the EXACT target window id, unconditionally.
        assert!(activate_target(7).is_ok());
        {
            let actions = mock.actions.lock().unwrap();
            assert_eq!(actions.len(), 1, "{actions:?}");
            assert_eq!(actions[0], "activate 7", "{actions:?}");
        }

        // A second call activates again — this is no longer gated on
        // "did a card actually appear", so repeated calls each activate.
        assert!(activate_target(7).is_ok());
        assert_eq!(mock.actions.lock().unwrap().len(), 2);

        // Fail-closed: the backend can't activate the window at all
        // (`Unsupported`) — must propagate an `Err` naming the window,
        // never silently proceed. No NEW action is recorded (the count
        // stays at 2, from the two successful activations above).
        mock.fail_activate.store(true, std::sync::atomic::Ordering::SeqCst);
        let err = activate_target(7).unwrap_err();
        assert!(err.contains('7'), "{err}");
        assert_eq!(
            mock.actions.lock().unwrap().len(),
            2,
            "a failed activation must never itself be recorded as a successful action"
        );

        // Leave clean for the next test sharing this mock instance.
        mock.fail_activate.store(false, std::sync::atomic::Ordering::SeqCst);
    }

    // —— issue #160 round-6 review P1 #2+#3: input branches re-resolve/re-activate AFTER the flight guard ——

    /// issue #160 round-6 review P1 #3: a `left_click` that queues on
    /// `computer::input_flight_guard` behind another in-flight action must
    /// use the window's geometry AS IT IS once it actually gets to inject —
    /// not whatever `resolve_window`/`map_to_physical` would have computed
    /// before it ever reached the guard. This test holds the guard itself
    /// (standing in for "another session's own in-flight call"), spawns the
    /// click, moves the mock window's origin while the click sits queued
    /// behind it, then releases the guard and asserts the injected
    /// coordinates reflect the NEW origin, never the stale one that was
    /// current when the call started.
    #[tokio::test]
    async fn left_click_uses_the_windows_geometry_as_of_after_the_flight_guard_not_before() {
        let _guard = computer::process_state_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        computer::clear_emergency_stop(computer::stop_generation());
        computer::clear_control();
        let mock = shared_mock();
        mock.fail_activate.store(false, std::sync::atomic::Ordering::SeqCst);
        *mock.on_activate.lock().unwrap_or_else(|e| e.into_inner()) = None;
        mock.actions.lock().unwrap_or_else(|e| e.into_inner()).clear();
        *mock.windows_override.lock().unwrap_or_else(|e| e.into_inner()) = Some(vec![computer::WindowInfo {
            id: 906_301,
            app: "Baz".into(),
            title: "Baz".into(),
            x: 0,
            y: 0,
            width: 800,
            height: 600,
        }]);

        let db = crate::store::Db::connect("sqlite::memory:").await.unwrap();
        repo::set_setting(&db, computer::K_COMPUTER_USE_ENABLED, "true").await.unwrap();
        let asks = AskRegistry::new();
        let thread = 906_301;
        let dir = "lead";
        asks.seed_grants(crate::ask::GrantSnapshot {
            full: vec![crate::ask::FullGrant { thread, dir: dir.to_string() }],
            always: Vec::new(),
        });

        // Prime the global input throttle well ahead of time so the real
        // click below isn't itself rejected as rate-limited.
        let _ = computer::throttle_input();
        tokio::time::sleep(std::time::Duration::from_millis(600)).await;

        // Hold the flight guard OURSELVES — the click below must queue
        // behind it, exactly like a second `tools/call` racing an in-flight
        // one would.
        let held = computer::input_flight_guard().await;

        let db_bg = db.clone();
        let asks_bg = asks.clone();
        let handle = tokio::spawn(async move {
            let mut window_id_out = None;
            let mut image_out = None;
            run_action(
                &db_bg, &asks_bg, thread, dir, None, "computer", "left_click",
                &json!({"window": "Baz", "coordinate": [100, 50]}),
                &mut window_id_out, &mut image_out,
            )
            .await
        });

        // Give the spawned call a chance to run its own pre-guard work
        // (argument parsing, `check_suspended`, `acquire_and_throttle`) and
        // reach — and block on — the flight guard THIS test still holds.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // The window "moves" while the click sits queued.
        *mock.windows_override.lock().unwrap_or_else(|e| e.into_inner()) = Some(vec![computer::WindowInfo {
            id: 906_301,
            app: "Baz".into(),
            title: "Baz".into(),
            x: 500,
            y: 300,
            width: 800,
            height: 600,
        }]);

        drop(held);

        let result = handle.await.unwrap();
        assert!(result.is_ok(), "{result:?}");

        let actions = mock.actions.lock().unwrap_or_else(|e| e.into_inner());
        assert!(
            actions.iter().any(|a| a == "click 600,350 Left x1"),
            "the click must land at the window's NEW origin (500+100, 300+50), not a stale one \
             computed before the flight guard: {actions:?}"
        );
        assert!(
            !actions.iter().any(|a| a == "click 100,50 Left x1"),
            "must never use the STALE pre-guard coordinates: {actions:?}"
        );
        drop(actions);

        computer::clear_control();
    }

    /// issue #160 round-6 review P1 #2: `activate_target` shells out to a
    /// (potentially slow, blocking) OS call. A Stop that lands WHILE that
    /// call is running must still be honored: the SECOND
    /// `recheck_after_guard` (run via `activate_and_recheck`, right after
    /// activation) must reject, and the backend must NEVER receive the
    /// `type_text` call. Reproduced deterministically via
    /// `MockBackend::on_activate`, which runs synchronously from INSIDE the
    /// mock's own `activate_window` — standing in for "a human's Stop
    /// finished processing while the real, blocking activation call was
    /// still running" — so by the time `activate_window` returns, its
    /// effect (here: `computer::clear_control()`, exactly what a real
    /// `emergency_stop` also does) has already happened, without needing
    /// genuine OS-thread concurrency to "catch" the race.
    #[tokio::test]
    async fn type_is_rejected_by_the_second_recheck_if_the_control_lease_is_lost_during_activation() {
        let _guard = computer::process_state_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        computer::clear_emergency_stop(computer::stop_generation());
        computer::clear_control();
        let mock = shared_mock();
        mock.fail_activate.store(false, std::sync::atomic::Ordering::SeqCst);
        mock.actions.lock().unwrap_or_else(|e| e.into_inner()).clear();
        *mock.windows_override.lock().unwrap_or_else(|e| e.into_inner()) = Some(vec![computer::WindowInfo {
            id: 906_201,
            app: "Bar".into(),
            title: "Bar".into(),
            x: 0,
            y: 0,
            width: 800,
            height: 600,
        }]);

        let db = crate::store::Db::connect("sqlite::memory:").await.unwrap();
        repo::set_setting(&db, computer::K_COMPUTER_USE_ENABLED, "true").await.unwrap();
        let asks = AskRegistry::new();
        let thread = 906_201;
        let dir = "lead";
        asks.seed_grants(crate::ask::GrantSnapshot {
            full: vec![crate::ask::FullGrant { thread, dir: dir.to_string() }],
            always: Vec::new(),
        });

        // Focus-freshness: `type` requires a recent click on the SAME
        // window first (see `require_recent_focus`) — seed it directly
        // rather than driving a real `left_click` through the whole gate a
        // second time just to satisfy this precondition.
        record_click_focus(thread, dir, 906_201);

        // Prime + clear the global input throttle so the real call below
        // isn't itself rejected as rate-limited.
        let _ = computer::throttle_input();
        tokio::time::sleep(std::time::Duration::from_millis(600)).await;

        // The hook: simulate a Stop landing DURING activation by clearing
        // the control lease from inside `activate_window` itself.
        *mock.on_activate.lock().unwrap_or_else(|e| e.into_inner()) = Some(Box::new(computer::clear_control));

        let mut window_id_out = None;
        let mut image_out = None;
        let err = run_action(
            &db, &asks, thread, dir, None, "computer", "type",
            &json!({"window": "Bar", "text": "hello"}),
            &mut window_id_out, &mut image_out,
        )
        .await
        .unwrap_err();

        assert!(
            err.contains("lost") || err.to_lowercase().contains("busy"),
            "expected a lost-lease/busy rejection from the SECOND recheck: {err}"
        );
        let actions = mock.actions.lock().unwrap_or_else(|e| e.into_inner());
        assert!(actions.iter().any(|a| a == "activate 906201"), "{actions:?}");
        assert!(
            !actions.iter().any(|a| a.starts_with("type ")),
            "the backend must never receive the type call once the lease is lost mid-activation: {actions:?}"
        );
        drop(actions);

        // Leave clean for the next test sharing this mock instance.
        *mock.on_activate.lock().unwrap_or_else(|e| e.into_inner()) = None;
        computer::clear_control();
    }

    // —— issue #160 round-8 P1 #4: approval bound to the resolved window identity ——

    /// The end-to-end property this fix exists for: `approve` resolves the
    /// target window (query "shifty", matching by title substring) and binds
    /// THAT window's identity at authorization time. If the SAME query
    /// resolves to a DIFFERENT window (different id AND app — simulating the
    /// original window closing and an unrelated one since taking the same
    /// title) by the time `run_action`'s own "left_click" arm reaches its OWN
    /// fresh resolve, the call must be rejected — fail-closed — and the
    /// backend must NEVER receive the click. Reproduced deterministically via
    /// the same "hold the flight guard ourselves" technique
    /// `left_click_uses_the_windows_geometry_as_of_after_the_flight_guard_not_before`
    /// uses: the window swap lands in the gap between `approve`'s own bind
    /// and the arm's later re-resolve.
    #[tokio::test]
    async fn input_action_fails_closed_when_the_approved_window_is_replaced_before_dispatch() {
        let _guard = computer::process_state_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        computer::clear_emergency_stop(computer::stop_generation());
        computer::clear_control();
        let mock = shared_mock();
        mock.fail_activate.store(false, std::sync::atomic::Ordering::SeqCst);
        *mock.on_activate.lock().unwrap_or_else(|e| e.into_inner()) = None;
        mock.actions.lock().unwrap_or_else(|e| e.into_inner()).clear();
        *mock.windows_override.lock().unwrap_or_else(|e| e.into_inner()) = Some(vec![computer::WindowInfo {
            id: 907_401,
            app: "Shifty".into(),
            title: "shifty window".into(),
            x: 0,
            y: 0,
            width: 800,
            height: 600,
        }]);

        let db = crate::store::Db::connect("sqlite::memory:").await.unwrap();
        repo::set_setting(&db, computer::K_COMPUTER_USE_ENABLED, "true").await.unwrap();
        let asks = AskRegistry::new();
        let thread = 907_401;
        let dir = "lead";
        asks.seed_grants(crate::ask::GrantSnapshot {
            full: vec![crate::ask::FullGrant { thread, dir: dir.to_string() }],
            always: Vec::new(),
        });

        // Prime the global input throttle well ahead of time so the real
        // click below isn't itself rejected as rate-limited.
        let _ = computer::throttle_input();
        tokio::time::sleep(std::time::Duration::from_millis(600)).await;

        // Hold the flight guard OURSELVES — `approve`'s own bind runs
        // (synchronously, no await point of its own) BEFORE the spawned call
        // ever reaches this guard, so it captures the window as it is RIGHT
        // NOW; the swap below then lands strictly after that bind.
        let held = computer::input_flight_guard().await;

        let db_bg = db.clone();
        let asks_bg = asks.clone();
        let handle = tokio::spawn(async move {
            let mut window_id_out = None;
            let mut image_out = None;
            run_action(
                &db_bg, &asks_bg, thread, dir, None, "computer", "left_click",
                &json!({"window": "shifty", "coordinate": [10, 10]}),
                &mut window_id_out, &mut image_out,
            )
            .await
        });

        // Give the spawned call a chance to run `approve` (which binds the
        // CURRENT window) and its own pre-guard argument checks, then reach
        // — and block on — the flight guard this test still holds.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // The window is REPLACED: the SAME query still matches (same title
        // substring), but a DIFFERENT id and app — standing in for the
        // original window closing and an unrelated one happening to share
        // its name.
        *mock.windows_override.lock().unwrap_or_else(|e| e.into_inner()) = Some(vec![computer::WindowInfo {
            id: 907_402,
            app: "Different App".into(),
            title: "shifty window".into(),
            x: 0,
            y: 0,
            width: 800,
            height: 600,
        }]);

        drop(held);

        let err = handle.await.unwrap().unwrap_err();
        assert!(
            err.contains("changed since this action was approved"),
            "must fail closed with the re-approve message: {err}"
        );
        assert!(
            mock.actions.lock().unwrap_or_else(|e| e.into_inner()).is_empty(),
            "the replaced window must never receive the click"
        );

        computer::clear_control();
    }

    /// The mirror image: the query resolves to the SAME window both at
    /// approval time and at dispatch time (the common, no-race case) — the
    /// action must still succeed exactly as it did before round-8 P1 #4.
    #[tokio::test]
    async fn input_action_succeeds_when_the_approved_window_is_unchanged() {
        let _guard = computer::process_state_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        computer::clear_emergency_stop(computer::stop_generation());
        computer::clear_control();
        let mock = shared_mock();
        mock.fail_activate.store(false, std::sync::atomic::Ordering::SeqCst);
        *mock.on_activate.lock().unwrap_or_else(|e| e.into_inner()) = None;
        mock.actions.lock().unwrap_or_else(|e| e.into_inner()).clear();
        *mock.windows_override.lock().unwrap_or_else(|e| e.into_inner()) = Some(vec![computer::WindowInfo {
            id: 907_501,
            app: "Steady".into(),
            title: "steady window".into(),
            x: 0,
            y: 0,
            width: 800,
            height: 600,
        }]);

        let db = crate::store::Db::connect("sqlite::memory:").await.unwrap();
        repo::set_setting(&db, computer::K_COMPUTER_USE_ENABLED, "true").await.unwrap();
        let asks = AskRegistry::new();
        let thread = 907_501;
        let dir = "lead";
        asks.seed_grants(crate::ask::GrantSnapshot {
            full: vec![crate::ask::FullGrant { thread, dir: dir.to_string() }],
            always: Vec::new(),
        });

        let _ = computer::throttle_input();
        tokio::time::sleep(std::time::Duration::from_millis(600)).await;

        let mut window_id_out = None;
        let mut image_out = None;
        let result = run_action(
            &db, &asks, thread, dir, None, "computer", "left_click",
            &json!({"window": "steady", "coordinate": [5, 5]}),
            &mut window_id_out, &mut image_out,
        )
        .await;

        assert!(result.is_ok(), "{result:?}");
        assert_eq!(window_id_out, Some(907_501));
        assert!(
            mock.actions
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .any(|a| a.starts_with("click")),
            "the unchanged window must still receive the click"
        );

        computer::clear_control();
    }

    /// Observe-class actions (`screenshot`, `list_windows`) — and any
    /// Write-classified action with NO window argument at all (`wait`) —
    /// never bind a window at all: `approve` returns `Ok(None)` for them (see
    /// `bind_approved_window`'s own doc), so they're entirely unaffected by
    /// round-8 P1 #4 — there is nothing for `verify_approved_target` to ever
    /// check against for these.
    #[tokio::test]
    async fn observe_and_windowless_actions_never_bind_a_window() {
        let asks = AskRegistry::new();
        let thread = 907_601;
        let dir = "lead";
        asks.seed_grants(crate::ask::GrantSnapshot {
            full: vec![crate::ask::FullGrant { thread, dir: dir.to_string() }],
            always: Vec::new(),
        });

        let screenshot_args = json!({"action": "screenshot", "window": "anything"});
        let approved = approve(&asks, thread, dir, "screenshot", &screenshot_args).await.unwrap();
        assert!(
            approved.is_none(),
            "an observe action must never bind a window, even with a window arg present: {approved:?}"
        );

        let list_windows_args = json!({"action": "list_windows"});
        let approved_lw = approve(&asks, thread, dir, "list_windows", &list_windows_args).await.unwrap();
        assert!(approved_lw.is_none());

        let wait_args = json!({"action": "wait", "duration_ms": 1});
        let approved_wait = approve(&asks, thread, dir, "wait", &wait_args).await.unwrap();
        assert!(
            approved_wait.is_none(),
            "wait is Write-classified but has no window argument to bind: {approved_wait:?}"
        );
    }

    // —— issue #160 round-3 P1 §2 (extended round-5 review P1 §2): recheck_after_guard ——

    /// One test exercises `recheck_after_guard`'s whole matrix sequentially,
    /// mirroring `computer::tests::control_lock_busy_expiry_release_and_clear`'s
    /// own reasoning: this touches the SAME process-wide control-lease
    /// static as every other `computer::acquire_control`-touching test in
    /// this binary, so splitting these scenarios across separate `#[test]`s
    /// would let `cargo test`'s default parallel threads race each other's
    /// lease state. issue #160 round-5 review P1 §2's own `has_open` scenario
    /// lives HERE too, for the exact same reason — a separate `#[tokio::test]`
    /// that also calls `computer::acquire_control` raced this one and the
    /// OTHER lease-touching tests in this file/binary under `cargo test`'s
    /// default parallelism (confirmed: it flaked in CI-style runs).
    /// `computer::process_state_test_lock` closes that — see its own doc.
    #[tokio::test]
    async fn recheck_after_guard_covers_disabled_foreign_lease_missing_lease_and_the_happy_path() {
        let _guard = computer::process_state_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        let db = crate::store::Db::connect("sqlite::memory:").await.unwrap();
        let asks = AskRegistry::new();
        let thread = 905_001;
        let dir = "lead";

        // Disabled setting denies regardless of the lease.
        repo::set_setting(&db, computer::K_COMPUTER_USE_ENABLED, "false").await.unwrap();
        computer::clear_control();
        let err = recheck_after_guard(&db, &asks, thread, dir).await.unwrap_err();
        assert!(err.to_lowercase().contains("disabled"), "{err}");

        // Enabled, but nobody holds the lease at all (it expired, or was
        // cleared by an emergency stop while this call was queued) — denied,
        // not silently allowed just because the setting itself reads true.
        repo::set_setting(&db, computer::K_COMPUTER_USE_ENABLED, "true").await.unwrap();
        computer::clear_control();
        let err = recheck_after_guard(&db, &asks, thread, dir).await.unwrap_err();
        assert!(!err.to_lowercase().contains("disabled"), "{err}");

        // Enabled, but a DIFFERENT (thread, dir) now holds the lease
        // (preempted while this call was queued behind the flight guard) —
        // denied.
        computer::acquire_control(999_999, "someone-else").unwrap();
        let err = recheck_after_guard(&db, &asks, thread, dir).await.unwrap_err();
        assert!(err.contains("999999") || err.contains("someone-else"), "{err}");
        computer::clear_control();

        // Enabled AND this exact (thread, dir) still holds the lease —
        // passes.
        computer::acquire_control(thread, dir).unwrap();
        assert!(recheck_after_guard(&db, &asks, thread, dir).await.is_ok());

        // issue #160 round-5 review P1 §2: a brand-new, unrelated ask opening
        // for this EXACT (thread, dir) — simulating one that opened WHILE this
        // call sat queued on `input_flight_guard` — must now deny with the
        // SAME `SuspendedPendingAsk` text `check_suspended`'s own up-front
        // check returns, even though the lease and the enabled setting are
        // both still fine (still held from the scenario right above). A
        // DIFFERENT (thread, dir)'s own open ask must not leak into this one.
        let (other_id, _rx) =
            asks.request(thread, "some-other-dir", "tool", "summary", "detail", crate::ask::RiskLevel::Unknown, "[]");
        assert!(
            recheck_after_guard(&db, &asks, thread, dir).await.is_ok(),
            "a DIFFERENT (thread, dir)'s open ask must not affect this one"
        );
        assert!(asks.answer(other_id, crate::ask::Answer::Deny));

        let (id, _rx) = asks.request(thread, dir, "tool", "summary", "detail", crate::ask::RiskLevel::Unknown, "[]");
        let err = recheck_after_guard(&db, &asks, thread, dir).await.unwrap_err();
        assert!(err.contains("permission card"), "{err}");

        // Once answered, the recheck passes again.
        assert!(asks.answer(id, crate::ask::Answer::Deny));
        assert!(recheck_after_guard(&db, &asks, thread, dir).await.is_ok());

        computer::clear_control();
    }

    // —— issue #160 round-2 P1 §1: re-check the kill switch AFTER approval ——

    /// The human hits Stop (disabling the setting) WHILE a card is still
    /// open; a stale Allow answers that SAME card anyway (e.g. a click that
    /// raced the Stop). The re-check inside `run_action`, right after
    /// `approve` returns, must still deny — and never reach dispatch.
    ///
    /// issue #160 round-8 P1 #4 note: `approve` itself now resolves the
    /// target window authoritatively as soon as the human's Allow lands (see
    /// `bind_approved_window`) — this needs a resolvable "notes" window
    /// (`shared_mock`, under `process_state_test_lock`) so that resolve
    /// itself succeeds and control actually reaches the kill-switch re-check
    /// this test exists to exercise, rather than failing earlier on an
    /// unrelated `WindowNotFound`/`Unsupported` from whatever backend state
    /// another concurrently-running test happens to have left in place.
    #[tokio::test]
    async fn disabling_the_setting_while_a_card_is_open_still_denies_a_later_allow() {
        let _guard = computer::process_state_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        let mock = shared_mock();
        mock.fail_activate.store(false, std::sync::atomic::Ordering::SeqCst);
        *mock.on_activate.lock().unwrap_or_else(|e| e.into_inner()) = None;
        mock.actions.lock().unwrap_or_else(|e| e.into_inner()).clear();
        *mock.windows_override.lock().unwrap_or_else(|e| e.into_inner()) = Some(vec![computer::WindowInfo {
            id: 901_101,
            app: "Notes".into(),
            title: "notes".into(),
            x: 0,
            y: 0,
            width: 800,
            height: 600,
        }]);

        let db = crate::store::Db::connect("sqlite::memory:").await.unwrap();
        repo::set_setting(&db, computer::K_COMPUTER_USE_ENABLED, "true")
            .await
            .unwrap();
        let asks = AskRegistry::new();
        let asks_bg = asks.clone();
        let db_bg = db.clone();
        let thread = 901_101;
        let dir = "lead";
        let args = json!({"action": "left_click", "window": "notes", "coordinate": [1, 1]});

        let handle = tokio::spawn(async move {
            let mut window_id_out = None;
            let mut image_out = None;
            run_action(
                &db_bg, &asks_bg, thread, dir, None, "computer", "left_click", &args,
                &mut window_id_out, &mut image_out,
            )
            .await
        });

        let mut card = None;
        for _ in 0..200 {
            if let Some(a) = asks.open().into_iter().find(|a| a.thread == thread && a.dir == dir) {
                card = Some(a);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let card = card.expect("a Needs-you card must appear for this call");

        // The human hits Stop: disable the setting WHILE the card is open.
        repo::set_setting(&db, computer::K_COMPUTER_USE_ENABLED, "false")
            .await
            .unwrap();

        // ...then a stale Allow answers the SAME card anyway.
        assert!(asks.answer(card.id, crate::ask::Answer::Allow));

        let err = handle.await.unwrap().unwrap_err();
        assert!(
            err.to_lowercase().contains("disabled"),
            "the re-check must deny with the disabled message, not proceed to dispatch: {err}"
        );
    }

    // —— issue #160 round-2 P2 §2: args_digest / Always-grant action_key ——

    #[test]
    fn args_digest_is_order_independent_and_stable() {
        let a = json!({"action": "left_click", "window": "notes", "coordinate": [10, 20]});
        let b = json!({"window": "notes", "coordinate": [10, 20], "action": "left_click"});
        assert_eq!(args_digest(&a), args_digest(&b));
    }

    #[test]
    fn args_digest_changes_when_typed_text_changes() {
        let a = json!({"action": "type", "window": "notes", "text": "hello"});
        let b = json!({"action": "type", "window": "notes", "text": "goodbye"});
        assert_ne!(args_digest(&a), args_digest(&b));
    }

    #[test]
    fn args_digest_never_contains_the_raw_text_and_is_a_sha256_hex_string() {
        let args = json!({"action": "type", "window": "notes", "text": "hunter2"});
        let digest = args_digest(&args);
        assert!(!digest.contains("hunter2"), "the digest must never carry the raw text: {digest}");
        assert_eq!(digest.len(), 64, "sha256 hex digest is 64 chars: {digest}");
        assert!(digest.chars().all(|c| c.is_ascii_hexdigit()), "{digest}");
    }

    #[test]
    fn args_digest_ignores_action_and_window_and_absent_keys() {
        // `action`/`window` already have their OWN slots in the action_key
        // tuple (`["gui", action, window, digest]`) — the digest itself only
        // covers `DIGEST_PARAM_KEYS`, so two calls differing ONLY in
        // action/window still produce the same digest.
        let a = json!({"action": "left_click", "window": "notes", "coordinate": [1, 2]});
        let b = json!({"action": "right_click", "window": "other", "coordinate": [1, 2]});
        assert_eq!(args_digest(&a), args_digest(&b));
    }

    /// The end-to-end property the round-2 P2 §2 fix exists for: the SAME
    /// action+window+params produce the SAME `action_key` (so an Always
    /// grant is reused); a CHANGED consequential param (here, `text`) mints a
    /// DIFFERENT key (so it must re-card); and the key itself never contains
    /// the raw typed text.
    #[test]
    fn action_key_is_stable_for_repeats_and_changes_with_text_without_leaking_it() {
        let base = json!({"action": "type", "window": "notes", "text": "hello"});
        let same_different_order = json!({"window": "notes", "text": "hello", "action": "type"});
        let changed = json!({"action": "type", "window": "notes", "text": "goodbye"});

        let key1 = crate::ask::action_key(&["gui", "type", "notes", &args_digest(&base)]);
        let key2 = crate::ask::action_key(&["gui", "type", "notes", &args_digest(&same_different_order)]);
        let key3 = crate::ask::action_key(&["gui", "type", "notes", &args_digest(&changed)]);

        assert_eq!(key1, key2, "the SAME action+window+params must reuse the SAME action_key");
        assert_ne!(key1, key3, "changed text must mint a DIFFERENT action_key");
        assert!(!key1.contains("hello"), "the action_key must never contain the raw typed text: {key1}");
        assert!(!key3.contains("goodbye"), "{key3}");
    }

    // —— issue #160 round-2 P2 §3: refuse_symlinks ——

    #[test]
    fn refuse_symlinks_accepts_an_ordinary_directory() {
        let base = std::env::temp_dir().join(format!("weft-refuse-sym-ok-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join(".weft")).unwrap();

        let out = refuse_symlinks(&base, &[".weft", "screenshots"]).unwrap();
        assert_eq!(out, base.join(".weft").join("screenshots"));

        let _ = std::fs::remove_dir_all(&base);
    }

    #[cfg(unix)]
    #[test]
    fn refuse_symlinks_rejects_a_symlinked_weft_directory() {
        let base = std::env::temp_dir().join(format!("weft-refuse-sym-dir-{}", std::process::id()));
        let outside = std::env::temp_dir().join(format!("weft-refuse-sym-dir-outside-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::remove_dir_all(&outside);
        std::fs::create_dir_all(&base).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, base.join(".weft")).unwrap();

        let err = refuse_symlinks(&base, &[".weft", "screenshots"]).unwrap_err();
        assert!(err.contains("symlink"), "{err}");
        assert!(
            !outside.join("screenshots").exists(),
            "must never write through the symlinked .weft directory"
        );

        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[cfg(unix)]
    #[test]
    fn refuse_symlinks_rejects_a_symlinked_leaf_file() {
        let base = std::env::temp_dir().join(format!("weft-refuse-sym-leaf-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join(".weft")).unwrap();
        let secret = std::env::temp_dir().join(format!("weft-refuse-sym-secret-{}", std::process::id()));
        std::fs::write(&secret, b"outside content").unwrap();
        std::os::unix::fs::symlink(&secret, base.join(".weft").join("computer-audit.jsonl")).unwrap();

        let err = refuse_symlinks(&base, &[".weft", "computer-audit.jsonl"]).unwrap_err();
        assert!(err.contains("symlink"), "{err}");

        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::remove_file(&secret);
    }

    // —— issue #160 round-4 P2 §3: O_NOFOLLOW closes the leaf open TOCTOU ——

    /// Proves the NEW defense in isolation from `refuse_symlinks`'s own,
    /// EARLIER check: the leaf path is ALREADY a symlink to an outside file
    /// by the time this open call runs (standing in for a symlink planted in
    /// the TOCTOU window between `refuse_symlinks`'s check and the real
    /// `open` — a window `refuse_symlinks` alone cannot close, since it only
    /// ever runs once, before that call). Calling
    /// [`open_audit_file_for_append`] DIRECTLY (bypassing `audit_log_path`/
    /// `refuse_symlinks` entirely) isolates that this specific open call
    /// refuses the symlink on its own — without `O_NOFOLLOW`, this exact
    /// call would silently succeed and write into `outside`.
    #[cfg(unix)]
    #[tokio::test]
    async fn open_audit_file_for_append_refuses_a_symlinked_leaf() {
        let base = std::env::temp_dir().join(format!("weft-audit-nofollow-{}", std::process::id()));
        let outside = std::env::temp_dir().join(format!("weft-audit-nofollow-outside-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::remove_file(&outside);
        std::fs::create_dir_all(&base).unwrap();
        let leaf = base.join("computer-audit.jsonl");
        std::os::unix::fs::symlink(&outside, &leaf).unwrap();

        let result = open_audit_file_for_append(&leaf).await;
        assert!(result.is_err(), "O_NOFOLLOW must refuse to open a symlinked leaf");
        assert!(
            !outside.exists(),
            "a refused open must never create/write the symlink's target"
        );

        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::remove_file(&outside);
    }

    /// The ordinary, non-symlinked case is unaffected: `O_NOFOLLOW` only
    /// rejects a symlinked LAST component, never a plain file.
    #[cfg(unix)]
    #[tokio::test]
    async fn open_audit_file_for_append_succeeds_for_an_ordinary_file() {
        let base = std::env::temp_dir().join(format!("weft-audit-nofollow-ok-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let leaf = base.join("computer-audit.jsonl");

        assert!(open_audit_file_for_append(&leaf).await.is_ok());
        assert!(leaf.exists());

        let _ = std::fs::remove_dir_all(&base);
    }

    /// End-to-end through the async path a real worker's `screenshot`/audit
    /// call takes (`session_root` → `screenshot_out_dir`/`audit_log_path`): a
    /// symlinked `.weft` inside a MATERIALIZED WORKTREE is refused by BOTH,
    /// and neither writes through it. Deliberately a WORKER (not the lead
    /// lane): `screenshot_out_dir` skips the `.weft/` layer entirely for the
    /// lead's own scratch cwd (see that function's own doc), so only the
    /// worker path actually exercises the screenshot half of this check —
    /// the lead lane still gets the audit-log half (covered structurally by
    /// `audit_log_path` running the SAME `refuse_symlinks` call for both
    /// lanes, exercised directly in the leaf/dir tests above). Doesn't touch
    /// `WEFT_HOME` at all, avoiding that env var's own process-wide hazard
    /// (see `tests/computer_mcp.rs`'s own note on it).
    #[cfg(unix)]
    #[tokio::test]
    async fn screenshot_and_audit_paths_both_refuse_a_symlinked_weft_dir_in_a_worktree() {
        let db = crate::store::Db::connect("sqlite::memory:").await.unwrap();
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        let repo_tmp = tempfile::tempdir().unwrap();
        let r = repo::add_repo_ref(&db, ws.id, "r", &repo_tmp.path().to_string_lossy(), "main", "", true)
            .await
            .unwrap();
        let thread = repo::create_thread(&db, ws.id, "t", "feature", "claude").await.unwrap();
        let direction = repo::create_direction(
            &db, thread.id, "task", "claude", r.id, "why", "impl-only", "main",
        )
        .await
        .unwrap();

        let wt_path = std::env::temp_dir().join(format!("weft-refuse-sym-wt-{}", std::process::id()));
        let outside = std::env::temp_dir().join(format!("weft-refuse-sym-wt-outside-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&wt_path);
        let _ = std::fs::remove_dir_all(&outside);
        std::fs::create_dir_all(&wt_path).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, wt_path.join(".weft")).unwrap();
        repo::record_worktree(&db, r.id, direction.id, "b", &wt_path.to_string_lossy(), true, true, "")
            .await
            .unwrap();

        let dir_s = direction.id.to_string();
        let screenshot_err = screenshot_out_dir(&db, thread.id, &dir_s, None).await.unwrap_err();
        assert!(screenshot_err.contains("symlink"), "{screenshot_err}");
        let audit_path = audit_log_path(&db, thread.id, &dir_s, None).await;
        assert!(audit_path.is_none(), "a symlinked .weft must refuse the audit path too");
        assert!(
            !outside.join("computer-audit.jsonl").exists() && !outside.join("screenshots").exists(),
            "must never write through the symlink"
        );

        let _ = std::fs::remove_dir_all(&wt_path);
        let _ = std::fs::remove_dir_all(&outside);
    }

    // —— issue #160 round-2 P2 §4: parameter validation before the lease ——

    /// A `left_click` missing `coordinate`, already Full-granted (so `approve`
    /// decides silently), must be rejected on the missing argument WITHOUT
    /// ever touching the control lease.
    ///
    /// issue #160 round-8 P1 #4 note: `approve` itself now resolves the
    /// target window authoritatively as part of authorizing this call (see
    /// `bind_approved_window`), independent of whether `coordinate` is even
    /// present — so this test installs `shared_mock` with a resolvable
    /// "notes" window (under `process_state_test_lock`) to keep that
    /// resolve itself from being the thing that fails, so the assertion
    /// below still isolates the ACTUAL property under test: the missing-
    /// `coordinate` rejection inside the "left_click" arm itself.
    #[tokio::test]
    async fn missing_coordinate_click_never_touches_the_control_lease() {
        let db = crate::store::Db::connect("sqlite::memory:").await.unwrap();
        repo::set_setting(&db, computer::K_COMPUTER_USE_ENABLED, "true")
            .await
            .unwrap();
        let asks = AskRegistry::new();
        let thread = 904_001;
        let dir = "lead";
        asks.seed_grants(crate::ask::GrantSnapshot {
            full: vec![crate::ask::FullGrant { thread, dir: dir.to_string() }],
            always: Vec::new(),
        });
        // Shares the same process-wide control-lease static as every other
        // `computer::acquire_control`-touching test in this binary (see
        // `computer::mod`'s own tests' notes on the same hazard) — cleared
        // immediately before and after so this test's own assertion isn't
        // muddied by a lease some other test happened to leave behind.
        // issue #160 round-5 review: that comment previously described the
        // hazard without actually preventing it — `process_state_test_lock`
        // does, by serializing this against every other test that touches
        // the same family of globals.
        let _guard = computer::process_state_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        computer::clear_control();
        let mock = shared_mock();
        mock.fail_activate.store(false, std::sync::atomic::Ordering::SeqCst);
        *mock.on_activate.lock().unwrap_or_else(|e| e.into_inner()) = None;
        mock.actions.lock().unwrap_or_else(|e| e.into_inner()).clear();
        *mock.windows_override.lock().unwrap_or_else(|e| e.into_inner()) = Some(vec![computer::WindowInfo {
            id: 904_001,
            app: "Notes".into(),
            title: "notes".into(),
            x: 0,
            y: 0,
            width: 800,
            height: 600,
        }]);

        let mut window_id_out = None;
        let mut image_out = None;
        let err = run_action(
            &db, &asks, thread, dir, None, "computer", "left_click",
            &json!({"window": "notes"}),
            &mut window_id_out, &mut image_out,
        )
        .await
        .unwrap_err();

        assert!(
            err.contains("coordinate"),
            "must fail on the missing coordinate, not a lease/busy error: {err}"
        );
        assert!(
            computer::control_state().is_none(),
            "a rejected missing-argument call must never touch the control lease"
        );
        computer::clear_control();
    }

    // —— issue #160 round-5 review P2 §3: MAX_TYPE_CHARS ——

    #[test]
    fn check_type_length_accepts_up_to_the_cap_and_rejects_over_it() {
        assert!(check_type_length(&"a".repeat(MAX_TYPE_CHARS)).is_ok());
        let err = check_type_length(&"a".repeat(MAX_TYPE_CHARS + 1)).unwrap_err();
        assert!(err.contains("too long"), "{err}");
        assert!(err.contains(&(MAX_TYPE_CHARS + 1).to_string()), "{err}");
    }

    #[test]
    fn check_type_length_counts_unicode_scalars_not_bytes() {
        // Mirrors `redact_audit_args_counts_unicode_scalars_not_bytes`'s own
        // reasoning: a multi-byte character must count as ONE char, not
        // however many bytes it takes in UTF-8, or a string well under the
        // human-meaningful cap could be rejected on byte length alone.
        let text = "é".repeat(MAX_TYPE_CHARS); // 2 bytes/char in UTF-8, 1 char each
        assert_eq!(text.chars().count(), MAX_TYPE_CHARS);
        assert!(check_type_length(&text).is_ok());
    }

    /// End-to-end through `run_action`'s own "type" arm (Full-granted, so
    /// `approve` decides silently): an over-limit `type` is rejected before
    /// EVER reaching the "type" arm's OWN window resolution — the one that
    /// records `window_id_out` and would go on to touch the focus-freshness
    /// gate, the control lease, and the backend call itself.
    ///
    /// issue #160 round-8 P1 #4 note: `approve` itself now ALSO resolves the
    /// window authoritatively, unconditionally, for any Write action with a
    /// non-blank window argument — including THIS one — to bind an
    /// `ApprovedWindow` before this call ever reaches its own arm (see
    /// `bind_approved_window`'s doc). That resolve happens regardless of
    /// whether the LATER, action-specific arguments (like this call's
    /// over-limit `text`) turn out to be invalid — so this test now installs
    /// `shared_mock` with a resolvable "notes" window (unlike before round-8,
    /// when this test deliberately used NO backend at all and relied on the
    /// guaranteed-`Unsupported` `StubBackend` to prove ordering). What this
    /// test still proves: `check_type_length` rejects before the ARM's own
    /// resolve/backend call — `window_id_out` stays `None` and no INPUT
    /// method (`click`/`type_text`/`key`/...) ever reaches the mock, even
    /// though `approve`'s own (list-only, unrecorded) resolve already
    /// succeeded. The integration test in `tests/computer_mcp.rs` covers the
    /// same "never reaches the backend" property end-to-end.
    #[tokio::test]
    async fn run_action_rejects_an_over_limit_type_before_resolving_the_window() {
        let _guard = computer::process_state_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        let mock = shared_mock();
        mock.fail_activate.store(false, std::sync::atomic::Ordering::SeqCst);
        *mock.on_activate.lock().unwrap_or_else(|e| e.into_inner()) = None;
        mock.actions.lock().unwrap_or_else(|e| e.into_inner()).clear();
        *mock.windows_override.lock().unwrap_or_else(|e| e.into_inner()) = Some(vec![computer::WindowInfo {
            id: 904_101,
            app: "Notes".into(),
            title: "notes".into(),
            x: 0,
            y: 0,
            width: 800,
            height: 600,
        }]);

        let db = crate::store::Db::connect("sqlite::memory:").await.unwrap();
        repo::set_setting(&db, computer::K_COMPUTER_USE_ENABLED, "true")
            .await
            .unwrap();
        let asks = AskRegistry::new();
        let thread = 904_101;
        let dir = "lead";
        asks.seed_grants(crate::ask::GrantSnapshot {
            full: vec![crate::ask::FullGrant { thread, dir: dir.to_string() }],
            always: Vec::new(),
        });

        let over_limit = "a".repeat(MAX_TYPE_CHARS + 1);
        let mut window_id_out = None;
        let mut image_out = None;
        let err = run_action(
            &db, &asks, thread, dir, None, "computer", "type",
            &json!({"window": "notes", "text": over_limit}),
            &mut window_id_out, &mut image_out,
        )
        .await
        .unwrap_err();

        assert!(err.contains("too long"), "{err}");
        assert!(
            window_id_out.is_none(),
            "the call must fail before the arm's OWN resolve ever records a window id: {window_id_out:?}"
        );
        assert!(
            mock.actions.lock().unwrap_or_else(|e| e.into_inner()).is_empty(),
            "an over-limit type must never reach the backend as an input action"
        );
    }

    // —— issue #160 round-2 P2 §5: multi-worktree `wt` routing ——

    #[tokio::test]
    async fn session_root_wt_pins_the_exact_worktree_and_a_foreign_one_fails_closed() {
        let db = crate::store::Db::connect("sqlite::memory:").await.unwrap();
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        let tmp_a = tempfile::tempdir().unwrap();
        let tmp_b = tempfile::tempdir().unwrap();
        let repo_a = repo::add_repo_ref(&db, ws.id, "a", &tmp_a.path().to_string_lossy(), "main", "", true)
            .await
            .unwrap();
        let repo_b = repo::add_repo_ref(&db, ws.id, "b", &tmp_b.path().to_string_lossy(), "main", "", true)
            .await
            .unwrap();
        let thread = repo::create_thread(&db, ws.id, "t", "feature", "claude").await.unwrap();
        let direction = repo::create_direction(
            &db, thread.id, "task", "claude", repo_a.id, "why", "impl-only", "main",
        )
        .await
        .unwrap();

        // Two worktrees for the SAME (multi-repo) direction — the exact
        // shape issue #160 round-2 P2 §5 fixes: `.next()` alone would always
        // resolve to whichever of these was inserted first, regardless of
        // which worker session actually asked.
        let wt_a =
            repo::record_worktree(&db, repo_a.id, direction.id, "b1", "/tmp/weft-wt-a", true, true, "")
                .await
                .unwrap();
        let wt_b =
            repo::record_worktree(&db, repo_b.id, direction.id, "b2", "/tmp/weft-wt-b", true, true, "")
                .await
                .unwrap();
        let dir_s = direction.id.to_string();

        // An explicit `wt` pins the EXACT worktree, even though it was
        // inserted SECOND.
        let root = session_root(&db, thread.id, &dir_s, Some(wt_b.id)).await.unwrap();
        assert_eq!(root, std::path::PathBuf::from(&wt_b.path));

        // No `wt` at all: unchanged pre-existing "first worktree" fallback.
        let no_wt = session_root(&db, thread.id, &dir_s, None).await.unwrap();
        assert_eq!(no_wt, std::path::PathBuf::from(&wt_a.path));

        // A `wt` naming a worktree of a DIFFERENT direction is rejected
        // (closed-set validation) — round-8 P2 #7: this must now FAIL CLOSED
        // (`None`), never silently fall back to "first worktree of THIS
        // direction" as it used to. A worker session that explicitly pinned a
        // worktree that's since become invalid must never have its
        // screenshots/audit quietly redirected into a DIFFERENT repo's
        // checkout in this multi-repo direction.
        let other_direction = repo::create_direction(
            &db, thread.id, "task2", "claude", repo_a.id, "why", "impl-only", "main",
        )
        .await
        .unwrap();
        let foreign = repo::record_worktree(
            &db, repo_a.id, other_direction.id, "b3", "/tmp/weft-wt-foreign", true, true, "",
        )
        .await
        .unwrap();

        let fails_closed = session_root(&db, thread.id, &dir_s, Some(foreign.id)).await;
        assert_eq!(
            fails_closed, None,
            "an explicit wt naming a worktree of a DIFFERENT direction must fail closed, \
             never silently resolve to (or fall back onto) any worktree at all"
        );

        // A wt naming a worktree that's simply been deleted (never recorded
        // at all, here) must fail closed the exact same way.
        let deleted = session_root(&db, thread.id, &dir_s, Some(999_999)).await;
        assert_eq!(deleted, None, "an explicit wt with no matching worktree row must fail closed");
    }

    // —— issue #160 round-2 P2 §7: bounded preview registry ——

    #[test]
    fn evict_oldest_if_full_removes_only_the_single_oldest_entry_at_capacity() {
        let mut map: HashMap<(i32, String), (String, u32, u64)> = HashMap::new();
        for i in 0..MAX_PREVIEWS as i32 {
            map.insert((i, "d".to_string()), (format!("p{i}"), i as u32, i as u64));
        }
        assert_eq!(map.len(), MAX_PREVIEWS);

        // Simulate the (MAX_PREVIEWS + 1)th write: evict, then insert.
        evict_oldest_if_full(&mut map);
        map.insert(
            (MAX_PREVIEWS as i32, "d".to_string()),
            (format!("p{MAX_PREVIEWS}"), MAX_PREVIEWS as u32, MAX_PREVIEWS as u64),
        );

        assert_eq!(map.len(), MAX_PREVIEWS, "capacity must stay bounded");
        assert!(
            !map.contains_key(&(0, "d".to_string())),
            "the OLDEST entry (ts=0) must be evicted"
        );
        for i in 1..=MAX_PREVIEWS as i32 {
            assert!(
                map.contains_key(&(i, "d".to_string())),
                "every entry newer than the evicted one must remain: missing {i}"
            );
        }
    }

    #[test]
    fn evict_oldest_if_full_is_a_no_op_below_capacity() {
        let mut map: HashMap<(i32, String), (String, u32, u64)> = HashMap::new();
        map.insert((1, "d".to_string()), ("p".to_string(), 1, 100));
        evict_oldest_if_full(&mut map);
        assert_eq!(map.len(), 1, "must not evict anything below capacity");
    }

    #[test]
    fn store_screenshot_preview_updating_an_existing_key_never_evicts_anyone() {
        // Fill to capacity with distinct (thread, dir) keys, then refresh the
        // FIRST one again (same key) — this must never trigger an eviction,
        // since it doesn't grow the map.
        for i in 0..MAX_PREVIEWS as i32 {
            store_screenshot_preview(910_000 + i, "lead", format!("p{i}"), i as u32);
        }
        store_screenshot_preview(910_000, "lead", "refreshed".to_string(), 999);

        let g = screenshot_previews().lock().unwrap();
        assert!(
            g.len() <= MAX_PREVIEWS,
            "refreshing an existing key must never grow past capacity: {}",
            g.len()
        );
        assert_eq!(
            g.get(&(910_000, "lead".to_string())).map(|(p, ..)| p.clone()),
            Some("refreshed".to_string()),
            "the refreshed value must actually be stored"
        );
    }
}
