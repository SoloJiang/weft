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
//! Every `computer` call result comes back as plain text (a Needs-you card
//! per call — these are intentionally NOT in
//! `server::AUTO_APPROVED_INTERNAL_TOOLS`; this stays conservative and
//! relies on the human's own "Always grant" to converge instead of a
//! hand-picked auto-approve list).
//!
//! ONE tool, `computer`, dispatched by an `action` argument — the same shape
//! as Anthropic's `computer_20251124`, OpenAI's Responses `computer`, and
//! omp's `computer` tool, which models are already trained against.

use crate::ask::AskRegistry;
use crate::computer::{self, backend, backend::MouseButton, ComputerError};
use crate::store::{repo, Db};
use axum::{
    extract::{Path, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde_json::{json, Value};

fn text_result(s: String) -> Value {
    json!({ "content": [{ "type": "text", "text": s }] })
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
            "description": "Observe AND control the human's screen — OS-level window listing/screenshot plus mouse/keyboard input injection. `action=list_windows` lists visible on-screen windows (Weft's own window and terminal-emulator apps are excluded, so you can never see yourself or the terminal you're running inside). `action=screenshot` captures ONE window — never the whole desktop — and returns a PNG FILE PATH (not image data): open it with your own image-viewing tool. Every other action but `cursor_position`/`wait` needs `window` (same id/substring rule as screenshot) and drives that window's input: `left_click`/`right_click`/`double_click`/`triple_click`/`mouse_move` need `coordinate` [x, y]; `left_click_drag` needs `start_coordinate` AND `coordinate` (end point); `scroll` needs `coordinate` plus `scroll_direction`; `type`/`key` need `text` (literal text to type, or a combo like \"cmd+s\"/\"ctrl+shift+t\"/\"Return\"/\"f5\" for `key`). ALL coordinates are in the pixel space of the MOST RECENT screenshot of that window — take a screenshot first (or again after the window resizes) to get coordinates that still line up, since the mapping is recomputed from the window's CURRENT size on every call and an out-of-range coordinate is rejected rather than guessed at. Only one session drives the desktop at a time: an input action can come back `Busy` (another session currently has it) or fail while a permission card is still waiting on the human — retry after a moment. Input actions are also rate-limited to roughly 2 per second.",
            "inputSchema": { "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": VALID_ACTIONS,
                        "description": "screenshot | list_windows | left_click | right_click | double_click | triple_click | type | key | scroll | left_click_drag | mouse_move | cursor_position | wait" },
                    "window": { "type": "string",
                        "description": "Required for every action except cursor_position and wait: a window id from action=list_windows, or a case-insensitive substring of its app name or title." },
                    "coordinate": { "type": "array", "items": { "type": "integer" }, "minItems": 2, "maxItems": 2,
                        "description": "[x, y] in the pixel space of the most recent screenshot of `window`. Required for left_click/right_click/double_click/triple_click/mouse_move/scroll, and as the drag END point for left_click_drag." },
                    "start_coordinate": { "type": "array", "items": { "type": "integer" }, "minItems": 2, "maxItems": 2,
                        "description": "[x, y] drag START point, same pixel space as `coordinate`. Required for left_click_drag." },
                    "text": { "type": "string",
                        "description": "Literal text to type (action=type), or a key combo like \"cmd+s\" / \"ctrl+shift+t\" / \"Return\" / \"f5\" (action=key)." },
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

    let outcome = run_action(db, asks, thread, dir, name, &action, args, &mut window_id).await;
    let outcome_text = match &outcome {
        Ok(text) => text.clone(),
        Err(text) => text.clone(),
    };

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
            args,
            outcome: if outcome.is_ok() { "ok" } else { &outcome_text },
        },
    )
    .await;

    text_result(outcome_text)
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
    match action {
        "list_windows" => {
            let b = backend::backend();
            computer::visible_windows(b.as_ref())
                .map(|windows| serde_json::to_string(&windows).unwrap_or_else(|_| "[]".into()))
                .map_err(|e| e.to_string())
        }
        "screenshot" => {
            let Some(out_dir) = screenshot_out_dir(db, thread, dir).await else {
                return Err("no worktree for this session".into());
            };
            let b = backend::backend();
            computer::screenshot_window(b.as_ref(), &window_arg(args), &out_dir)
                .map(|shot| {
                    format!(
                        "screenshot saved: {} ({}x{}, scale {:.2}) — open it with your image viewing tool",
                        shot.path.display(),
                        shot.width,
                        shot.height,
                        shot.scale
                    )
                })
                .map_err(|e| e.to_string())
        }
        "left_click" | "right_click" | "double_click" | "triple_click" => {
            input_gate(asks, thread, dir)?;
            let (cx, cy) = parse_coordinate(args, "coordinate")?;
            let (window_id, px, py) = resolve_and_map(&window_arg(args), cx, cy)?;
            *window_id_out = Some(window_id);
            let button = if action == "right_click" { MouseButton::Right } else { MouseButton::Left };
            let count: u32 = match action {
                "double_click" => 2,
                "triple_click" => 3,
                _ => 1,
            };
            backend::backend()
                .click(px, py, button, count)
                .map_err(|e| e.to_string())?;
            Ok(format!(
                "{action} at ({px}, {py}) in window {window_id} done — take a screenshot to verify"
            ))
        }
        "mouse_move" => {
            input_gate(asks, thread, dir)?;
            let (cx, cy) = parse_coordinate(args, "coordinate")?;
            let (window_id, px, py) = resolve_and_map(&window_arg(args), cx, cy)?;
            *window_id_out = Some(window_id);
            backend::backend()
                .move_cursor(px, py)
                .map_err(|e| e.to_string())?;
            Ok(format!(
                "mouse_move to ({px}, {py}) in window {window_id} done — take a screenshot to verify"
            ))
        }
        "left_click_drag" => {
            input_gate(asks, thread, dir)?;
            let (sx, sy) = parse_coordinate(args, "start_coordinate")?;
            let (ex, ey) = parse_coordinate(args, "coordinate")?;
            let b = backend::backend();
            let w = computer::resolve_window(b.as_ref(), &window_arg(args)).map_err(|e| e.to_string())?;
            *window_id_out = Some(w.id);
            let from = computer::map_to_physical(&w, sx, sy).map_err(|e| e.to_string())?;
            let to = computer::map_to_physical(&w, ex, ey).map_err(|e| e.to_string())?;
            b.drag(from, to).map_err(|e| e.to_string())?;
            Ok(format!(
                "left_click_drag from ({}, {}) to ({}, {}) in window {} done — take a screenshot to verify",
                from.0, from.1, to.0, to.1, w.id
            ))
        }
        "scroll" => {
            input_gate(asks, thread, dir)?;
            let (cx, cy) = parse_coordinate(args, "coordinate")?;
            let (dx, dy) = parse_scroll(args)?;
            let (window_id, px, py) = resolve_and_map(&window_arg(args), cx, cy)?;
            *window_id_out = Some(window_id);
            backend::backend()
                .scroll(px, py, dx, dy)
                .map_err(|e| e.to_string())?;
            Ok(format!(
                "scroll at ({px}, {py}) dx={dx} dy={dy} in window {window_id} done — take a screenshot to verify"
            ))
        }
        "type" => {
            input_gate(asks, thread, dir)?;
            let text = required_text(args)?;
            let window_id = resolve_window_id(&window_arg(args))?;
            *window_id_out = Some(window_id);
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
            let combo = required_text(args)?;
            let window_id = resolve_window_id(&window_arg(args))?;
            *window_id_out = Some(window_id);
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
}
