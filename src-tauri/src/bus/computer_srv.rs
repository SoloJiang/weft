//! `weft_computer` MCP server (issue #160 M1): OS-level window enumeration +
//! screenshot, gated behind the `computer_use_enabled` app setting (default
//! OFF, fails closed — see `computer::enabled`). Observation only: no input
//! injection lives here — that is a separate, later task, and it does NOT
//! touch `inject.rs` / `commands.rs` / `engine.rs`.
//!
//! Wiring mirrors `bus::server::handle` (JSON-RPC over one SSE
//! `event: message` per POST). Deliberately does NOT reach into
//! `bus::server`'s private helpers across modules — `text_result`/`sse` are
//! copied here instead, matching `bus::global`'s existing precedent for a
//! new MCP server module.
//!
//! Every `computer` call result comes back as plain text (a Needs-you card
//! per call — these are intentionally NOT in
//! `server::AUTO_APPROVED_INTERNAL_TOOLS`; M1 is conservative and relies on
//! the human's own "Always grant" to converge instead of a hand-picked
//! auto-approve list).
//!
//! Revision v2 (spec update — align the tool FACE with the community shape):
//! the MCP surface is ONE tool, `computer`, dispatched by an `action`
//! argument (`"screenshot"` / `"list_windows"` for M1) — the same shape as
//! Anthropic's `computer_20251124`, OpenAI's Responses `computer`, and omp's
//! `computer` tool, which models are already trained against. This is a
//! presentation-layer change only: `computer::visible_windows` /
//! `computer::screenshot_window` and everything else under `computer/` keep
//! their names.

use crate::computer::{self, backend, ComputerError};
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
            call_computer(&db, thread, &dir, name, &args).await
        }
        _ => json!({}),
    };

    sse(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
}

/// The `action`s M1 actually implements. Kept as ONE closed list feeding
/// BOTH the JSON Schema `enum` below and the "unknown action" error text, so
/// the schema can never advertise an action this dispatcher doesn't handle
/// (or silently drift from it) — the same closed-list-as-single-source
/// discipline `EXCLUDED_TERMINAL_APPS` in `computer/mod.rs` uses.
const VALID_ACTIONS: &[&str] = &["screenshot", "list_windows"];

fn computer_tool_specs() -> Value {
    json!([
        {
            "name": "computer",
            "description": "Observe the human's screen — OS-level window listing and screenshot only, NO input injection (no click/type/key/scroll yet). `action=list_windows` lists visible on-screen windows (Weft's own window and terminal-emulator apps are excluded, so you can never see yourself or the terminal you're running inside). `action=screenshot` captures ONE window — never the whole desktop — and returns a PNG FILE PATH (not image data): open it with your own image-viewing tool (view_image / Read an image / inspect_image). Call list_windows first if you don't already have a window id. Input actions (left_click/type/key/scroll/left_click_drag/mouse_move/wait) land in a later milestone, at which point `action`'s allowed values grow to include them.",
            "inputSchema": { "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": VALID_ACTIONS,
                        "description": "screenshot | list_windows" },
                    "window": { "type": "string",
                        "description": "Required when action=screenshot: a window id from action=list_windows, or a case-insensitive substring of its app name or title." }
                },
                "required": ["action"] }
        }
    ])
}

async fn call_computer(db: &Db, thread: i32, dir: &str, name: &str, args: &Value) -> Value {
    // Fail-closed gate BEFORE touching any backend or even validating the
    // tool/action names: every call here is an observation of the human's
    // actual screen, so an unset/disabled setting blocks EVERYTHING, not
    // just a well-formed screenshot request.
    if !computer::enabled(db).await {
        return text_result(ComputerError::Disabled.to_string());
    }
    if name != "computer" {
        return text_result(format!("unknown tool: {name}"));
    }
    let action = args.get("action").and_then(|v| v.as_str()).unwrap_or("");
    match action {
        "list_windows" => {
            let b = backend::backend();
            match computer::visible_windows(b.as_ref()) {
                Ok(windows) => {
                    text_result(serde_json::to_string(&windows).unwrap_or_else(|_| "[]".into()))
                }
                Err(e) => text_result(e.to_string()),
            }
        }
        "screenshot" => {
            let window = args.get("window").and_then(|v| v.as_str()).unwrap_or("");
            let Some(out_dir) = screenshot_out_dir(db, thread, dir).await else {
                return text_result("no worktree for this session".into());
            };
            let b = backend::backend();
            match computer::screenshot_window(b.as_ref(), window, &out_dir) {
                Ok(shot) => text_result(format!(
                    "screenshot saved: {} ({}x{}, scale {:.2}) — open it with your image viewing tool",
                    shot.path.display(),
                    shot.width,
                    shot.height,
                    shot.scale
                )),
                Err(e) => text_result(e.to_string()),
            }
        }
        // Fail-closed, not fail-open: an unrecognized/missing action is
        // rejected with the valid list, never silently treated as one of
        // the known ones.
        _ => text_result(format!(
            "unknown action '{action}'; valid actions: {}",
            VALID_ACTIONS.join(", ")
        )),
    }
}

/// Resolve the screenshot output directory for `(thread, dir)`. Mirrors
/// `bus::server::session_roots`'s identity derivation, narrowed to ONE
/// directory instead of a read-allowlist:
///  - worker lane (`dir` a direction id): that direction's (first) worktree,
///    `<worktree>/.weft/screenshots` — excluded from git via
///    `git::git_exclude` so weft's own screenshots never show up in `git
///    status`/diffs. A numeric `dir` that doesn't resolve to a direction
///    belonging to THIS thread (stale/forged route — same check
///    `session_roots` makes) is treated the same as no worktree at all.
///  - lead lane (`dir == bus::LEAD`): the lead's own scratch cwd,
///    `<weft_home>/leads/<thread>` — same formula as
///    `lead_chat::commands::ensure_lead_cwd`, duplicated here since that
///    helper is private to its own module (see `builtin_allow.rs`'s doc
///    comment on why a lead's cwd is this scratch dir and not one of its
///    workspace's repos).
///
/// `None` on any failure (DB error, unresolvable path, deleted worktree row)
/// — the caller turns that into "no worktree for this session" rather than a
/// 500, matching every other bus MCP handler's soft-failure convention.
async fn screenshot_out_dir(db: &Db, thread: i32, dir: &str) -> Option<std::path::PathBuf> {
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
    let wt_path = std::path::PathBuf::from(wt.path);
    crate::git::git_exclude(&wt_path, ".weft/");
    Some(wt_path.join(".weft").join("screenshots"))
}
