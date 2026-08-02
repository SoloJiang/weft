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
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use hmac::{Hmac, Mac};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

// —— issue #160 round-16 P1 (Codex computer_srv.rs:605): move every synchronous OS/encode step off the async runtime ——

/// issue #160 round-16 P1 (Codex 605): run one synchronous OS/encode step on
/// tokio's blocking pool instead of directly on the async worker that called
/// it. Before this round, EVERY OS-touching call this module makes —
/// `computer::visible_windows`/`resolve_window` (xcap window enumeration),
/// `screenshot_window` (capture + PNG encode), `encode_jpeg_data_uri` (JPEG
/// re-encode, preview and MCP image alike), `activate_target` (a shell-out
/// that can block for a real amount of wall-clock time), and every input
/// backend call (`click`/`type_text`/`key`/`scroll`/`drag`/`move_cursor`, all
/// backed by `enigo`) — ran straight on whichever Tokio worker thread picked
/// up the request. A session holding a standing Full/Always grant could fire
/// enough concurrent `list_windows`/input calls to occupy every worker in the
/// runtime's pool at once; worse, since the Tauri `Stop` command and the
/// global-Escape callback ALSO spawn onto that same pool, a single slow or
/// wedged OS call anywhere on this path could leave the kill switch itself
/// unable to get scheduled — exactly the failure mode a kill switch exists to
/// never have. Routing each such step through `spawn_blocking` keeps the
/// async workers free for Stop/Escape regardless of how long, how many, or
/// how stuck the OS-facing calls get; the dedicated blocking pool is sized to
/// grow for exactly this kind of workload. Failure here (`JoinError`, in
/// practice only ever a panic inside `f` or the runtime shutting down) is
/// mapped to a plain error string and propagated — fail closed, never a
/// panic that could unwind past this boundary.
async fn on_blocking<T: Send + 'static>(
    f: impl FnOnce() -> T + Send + 'static,
) -> Result<T, String> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| format!("desktop backend task failed: {e}"))
}

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

/// The `?wt=` query param's three distinguishable states (issue #160
/// round-10 P2 #2, Codex 107) — CLAUDE.md: derive ONE discriminated value,
/// map it exhaustively, rather than re-deriving "is this absent, valid, or
/// garbage" ad hoc. Before this type existed, `q.get("wt").and_then(|s|
/// s.parse::<i32>().ok())` collapsed BOTH "no `?wt=` at all" and "a `?wt=`
/// present but not a number" onto the identical `None` — so a malformed/
/// forged non-numeric `wt` on a multi-repo direction took the SAME "first
/// worktree" fallback an honestly-absent one gets, silently misdirecting a
/// worker's screenshots/audit into a DIFFERENT repo's checkout. Round-8 P2 #7
/// already closed this for an explicit NUMERIC id that doesn't resolve to a
/// worktree of this direction (fail closed, never fall back to first); this
/// closes the identical gap one parse step earlier, for a `wt` that isn't
/// even numeric to begin with.
enum WtParam {
    /// No `?wt=` in the URL at all — the pre-existing "first worktree for
    /// this direction" fallback in `session_root` is unchanged.
    Absent,
    /// `?wt=<n>` parsed to a valid i32 — may still fail closed later, in
    /// `session_root`, if `n` doesn't name a worktree of THIS direction
    /// (round-8 P2 #7); this variant only proves the STRING was numeric.
    Explicit(i32),
    /// `?wt=<garbage>` present but NOT a valid i32 — an EXPLICIT pin that is
    /// simply malformed. Must reject the call outright, never fall back to
    /// "first worktree": whatever worktree the caller meant to pin, a
    /// non-numeric value manifestly isn't "no preference at all", so
    /// silently guessing on its behalf reopens the exact misdirection
    /// round-8 P2 #7 already refuses for a well-formed-but-wrong id.
    Invalid,
}

impl WtParam {
    fn parse(q: &HashMap<String, String>) -> WtParam {
        match q.get("wt") {
            None => WtParam::Absent,
            Some(s) => match s.parse::<i32>() {
                Ok(id) => WtParam::Explicit(id),
                Err(_) => WtParam::Invalid,
            },
        }
    }

    /// Collapse to what `session_root` already knows how to resolve
    /// (`Ok(Option<i32>)`, honoring round-8 P2 #7's own closed-set fail-
    /// closed rule), or a fixed rejection for `Invalid` — the exhaustive map
    /// CLAUDE.md asks for, kept in ONE place rather than an `if` scattered at
    /// the call site.
    fn resolve(&self) -> Result<Option<i32>, String> {
        match self {
            WtParam::Absent => Ok(None),
            WtParam::Explicit(id) => Ok(Some(*id)),
            WtParam::Invalid => Err(
                "invalid wt query parameter — it must be a worktree id (a plain integer); omit it \
                 entirely to fall back to this direction's first worktree"
                    .to_string(),
            ),
        }
    }
}

// —— issue #160 round-11 P1 #A: per-session bearer for this ONE privileged
// endpoint ——
//
// Every OTHER bus MCP endpoint (`/bus/:thread/:dir/mcp`, `/planner/:thread/
// mcp`, …) relies on the URL path alone for identity — an accepted, deliberate
// local-first tradeoff for tools that only ever READ/steer a chat session (see
// `bus::server`'s own top-of-file doc comment). `/computer/:thread/:dir/mcp` is
// different in kind, not just degree: a POST that reaches [`call_computer`]
// can capture the human's real screen and inject real mouse/keyboard input —
// and unlike the bus endpoints, this one had NO caller-side authentication of
// its own at all. The URL this route lives at is itself injected into a
// worker's OWN MCP config/launch args (`inject::computer_url`) — readable by
// that same worker's process (or, same-uid, by ANY other local process that
// can read that worker's own config/environment) — so ANY local process that
// can read (or simply guess: the path is just `/computer/<thread>/<dir>/mcp`,
// two small integers/strings) could POST directly into this endpoint,
// impersonating an arbitrary `(thread, dir)` identity. If that identity
// happens to already hold a Full/exact-Always grant for some GUI action,
// `auto_decision_exact` in [`approve`] would silently wave the forged call
// through with no card at all — Weft would screenshot/click/type with its own
// desktop permissions on a forged caller's behalf.
//
// The fix: a per-session, unguessable bearer token bound to the EXACT
// `(thread, dir)` in the URL path, checked BEFORE the request's `method`/`id`
// are even inspected and before ANY authorization logic runs — see
// [`verify_computer_token`]. Deliberately NOT a persisted per-session secret
// (no new store table, no migration): the token is a keyed HMAC of the path's
// own `(thread, dir)` under a process-lifetime random secret, so verifying it
// needs nothing more than recomputing the SAME HMAC from the SAME path values
// — no lookup table to keep in sync, no secret to ever write to disk.
//
// KNOWN, ACCEPTED residual (not eliminated this round): a SAME-UID local
// process can still read a legitimate worker's own MCP config file / process
// environment / launch args and recover this SAME token from there — this
// closes "any local process, any uid, can forge the path or guess a URL",
// not "no same-uid process can ever recover this session's own credential".
// That is the same-uid isolation ceiling this repo's other residuals already
// live with (nothing here is weaker than the rest of the process's own trust
// boundary) — full closure needs OS-level uid/sandbox isolation between
// weft and the tool processes it launches, tracked (like the other real-
// machine residuals in this file) as issue #160 §9 follow-up work, not
// something a single-process HMAC can fix on its own.

/// The process-wide HMAC key, generated ONCE per process from a CSPRNG
/// (`rand::rngs::OsRng` — already in this crate's dependency tree, see
/// `Cargo.toml`; this reuses it rather than adding a `getrandom`-direct
/// dependency) the first time this is called, and NEVER regenerated,
/// persisted, logged, or placed into any injected config — it lives ONLY in
/// this process's memory for its own lifetime. A restart mints a brand-new
/// secret (invalidating every previously-issued token, which is fine: a
/// fresh process re-injects fresh URLs with fresh tokens for every session it
/// spawns — nothing here needs to survive a restart).
fn computer_endpoint_secret() -> &'static [u8; 32] {
    static SECRET: OnceLock<[u8; 32]> = OnceLock::new();
    SECRET.get_or_init(|| {
        use rand::RngCore;
        let mut buf = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut buf);
        buf
    })
}

type HmacSha256 = Hmac<Sha256>;

/// The ONE place this module's HMAC key material gets constructed — shared by
/// both [`computer_session_token`] (mint) and [`verify_computer_token`]
/// (verify), so the two can never drift onto two different derivations of
/// "the MAC for this (thread, dir)". Returns `Err` only if `HmacSha256::
/// new_from_slice` itself rejects the key — HMAC accepts a key of ANY length
/// (including this fixed 32-byte CSPRNG buffer), so this is a can't-happen
/// path in practice; matched explicitly (never `.expect()`/`.unwrap()`) per
/// CLAUDE.md's ban on panicking in a production path.
fn computer_token_mac(
    thread: i32,
    dir: &str,
    wt: Option<i32>,
) -> Result<HmacSha256, hmac::digest::InvalidLength> {
    let mut mac = <HmacSha256 as Mac>::new_from_slice(computer_endpoint_secret())?;
    // issue #160 round-13/14 P1 (Codex computer_srv.rs:214 + inject.rs:483): the
    // MAC binds the EXACT worktree this URL carries, not just `(thread, dir)`.
    // Sibling worker sessions of one multi-repo direction share a single
    // `(thread, dir)` but differ only by `wt`; binding just `(thread, dir)`
    // gave every one of them the SAME bearer, so any of them could swap its
    // URL's `?wt=` to a sibling's id and route screenshots/audit into that
    // sibling's namespace under its own otherwise-valid token and shared
    // grants. Folding `wt` into the MAC means a swapped `?wt=` no longer matches
    // the token the worker was actually issued. The `none` marker for the
    // absent/lead case (no worktree at all) is a DISTINCT representation that
    // can never collide with any explicit `wt<id>` — the separate lead/absent
    // encoding Codex's finding calls for.
    let wt_repr = match wt {
        Some(id) => format!("wt{id}"),
        None => "none".to_string(),
    };
    mac.update(format!("{thread}/{dir}/{wt_repr}").as_bytes());
    Ok(mac)
}

/// The per-session token [`inject::computer_url`] appends as `&key=<token>`
/// (issue #160 round-11 P1 #A) — hex(HMAC-SHA256(process secret,
/// "{thread}/{dir}")). Deterministic for the SAME `(thread, dir)` (so a
/// worker's own injected URL keeps working for the life of the process — this
/// is never re-minted per-call), but unforgeable without either the process
/// secret (never leaves memory) or reading it back off a legitimate worker's
/// own already-injected config (the same-uid residual — see this module's own
/// top-of-file doc comment).
///
/// `#[doc(hidden)] pub` (not `pub(crate)`): `bus::inject` is a SIBLING module
/// (not a descendant of this one) that mints this into the URL it injects,
/// and `tests/computer_mcp.rs` is a separate integration-test crate that needs
/// to build the SAME token to drive the real endpoint — mirrors `args_digest`/
/// `MAX_TYPE_CHARS`'s own doc comments on why a cross-module/cross-crate
/// test-and-production-shared item is exposed this way. On the can't-happen
/// HMAC-construction failure (see [`computer_token_mac`]'s own doc), this
/// returns a fixed sentinel string that is not valid hex and therefore can
/// NEVER equal a legitimately hex-encoded `key=` a caller could ever supply —
/// keeping mint/verify symmetric (both go through the identical fallback) and
/// fail-closed even in this can't-happen case, rather than silently minting
/// (or accepting) an empty/predictable token.
#[doc(hidden)]
pub fn computer_session_token(thread: i32, dir: &str, wt: Option<i32>) -> String {
    match computer_token_mac(thread, dir, wt) {
        Ok(mac) => hex::encode(mac.finalize().into_bytes()),
        Err(_) => "hmac-init-failed-not-valid-hex".to_string(),
    }
}

/// Constant-time verification of a caller-supplied `key` against the token
/// [`computer_session_token`] would mint for THIS EXACT path `(thread, dir)`
/// (issue #160 round-11 P1 #A) — used by [`handle_computer`] before anything
/// else runs. Goes through `hmac`'s own `Mac::verify_slice` (backed by
/// `subtle`'s constant-time equality) rather than decoding+comparing hex
/// strings with `==`, which would short-circuit on the first mismatching
/// byte and leak timing information about how much of the token the caller
/// got right. A `supplied` that isn't even valid hex fails immediately
/// (`hex::decode` error) — there is no valid token shape it could be
/// mistaken for.
fn verify_computer_token(thread: i32, dir: &str, wt: Option<i32>, supplied: &str) -> bool {
    let Ok(supplied_bytes) = hex::decode(supplied) else {
        return false;
    };
    match computer_token_mac(thread, dir, wt) {
        Ok(mac) => mac.verify_slice(&supplied_bytes).is_ok(),
        Err(_) => false,
    }
}

// `thread`/`dir` come from the URL path (same identity-can't-be-spoofed
// guarantee `bus::server::handle` relies on for `/bus/:thread/:dir/mcp`).
//
// `?wt=<worktree_id>` (issue #160 round-2 P2 §5): an OPTIONAL query param a
// caller can attach when it already knows the EXACT worktree its own worker
// session materialized into — see `inject::computer_url`'s doc for who sets
// it and why. Without it, the pre-existing "first worktree for this
// direction" fallback is unchanged — a bare `?wt=`-less URL behaves exactly
// as it always did. A malformed/non-numeric `wt` (issue #160 round-10 P2 #2)
// is now REJECTED outright — see [`WtParam`] — rather than silently taking
// that same fallback.
pub async fn handle_computer(
    Path((thread, dir)): Path<(i32, String)>,
    Query(q): Query<HashMap<String, String>>,
    State(db): State<Db>,
    State(asks): State<AskRegistry>,
    headers: HeaderMap,
    Json(req): Json<Value>,
) -> Response {
    // issue #160 round-11 P1 #A: reject BEFORE the request's `method`/`id`
    // are even inspected, and before ANY authorization logic (`approve`'s own
    // gate included) ever runs — see this module's own top-of-section doc
    // comment for the full rationale. A caller with no `key=`, an empty one,
    // or one that doesn't match THIS EXACT path's `(thread, dir)` gets a bare
    // 401 — no JSON-RPC envelope, no hint about method/id/tool shape, nothing
    // that would help a guessing caller narrow down the real token.
    //
    // issue #160 round-15 P1 (Codex inject.rs:364): the SAME token is now also
    // accepted as `Authorization: Bearer <token>` — codex carries its MCP
    // config on argv (`-c` flags), where a `?key=` URL would be world-readable
    // through process listings, so its injection names an env var instead and
    // codex's own `bearer_token_env_var` support turns that into this header
    // (see `inject::inject_computer`'s codex arm). One verification path for
    // both channels: whichever the caller supplied is checked against the ONE
    // `(thread, dir, wt)` MAC — a query key, when present, wins (every other
    // engine still uses it), and an empty/absent one falls through to the
    // header.
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");
    let supplied_key = match q.get("key").map(String::as_str) {
        Some(k) if !k.is_empty() => k,
        _ => bearer,
    };
    // issue #160 round-13/14 P1 (Codex computer_srv.rs:214 + inject.rs:483): the
    // per-session bearer is bound to the EXACT worktree this URL carries, not
    // just `(thread, dir)` — see [`computer_token_mac`]'s own doc for the
    // sibling-worktree hijack this closes. Resolve `?wt=` FIRST: a malformed one
    // can correspond to no minted token, so it fails auth CLOSED here (a bare
    // 401, indistinguishable from any other bad key — it never reveals that the
    // path or a worktree exists), while a well-formed absent/explicit `wt` is
    // exactly what the token is then verified against.
    let wt = match WtParam::parse(&q).resolve() {
        Ok(w) => w,
        Err(_) => return StatusCode::UNAUTHORIZED.into_response(),
    };
    if !verify_computer_token(thread, &dir, wt, supplied_key) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    // issue #160 round-19 P1 (Codex computer_srv.rs:403): a still-valid token is
    // refused once its owning thread has been deleted — `delete_thread` revokes
    // the route. Fast path is a lock-only set lookup (live sessions and the
    // synthetic-identity tests pay nothing); only a revoked thread pays one
    // `session_is_live` DB check, which also lets a REUSED thread id back in.
    if computer_routes_revoked(thread) && !session_is_live(&db, thread, &dir, wt).await {
        return StatusCode::UNAUTHORIZED.into_response();
    }
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

/// The ONE rendering of the "unknown action" rejection, shared by BOTH
/// [`pure_validate`] (which now rejects an unrecognized action FIRST, before
/// the approval gate — issue #160 round-13 P1) and [`run_action`]'s own
/// fail-closed `_` arm. issue #160 round-13 follow-up: these two used to
/// format the message independently — `pure_validate` emitted a bare
/// `unknown action: <a>` while `run_action` listed the valid ones — so once
/// `pure_validate` began winning the race, the caller (and the
/// `computer_mcp` integration test that asserts the valid list is named)
/// stopped seeing the action list at all. Routing both through this single
/// function keeps them from ever drifting again — same
/// closed-list-as-single-source discipline `VALID_ACTIONS` itself follows.
fn unknown_action_error(action: &str) -> String {
    format!("unknown action '{action}'; valid actions: {}", VALID_ACTIONS.join(", "))
}

fn computer_tool_specs() -> Value {
    json!([
        {
            "name": "computer",
            "description": format!("Observe AND control the human's screen — OS-level window listing/screenshot plus mouse/keyboard input injection. `action=list_windows` lists visible on-screen windows (Weft's own window and terminal-emulator apps are excluded, so you can never see yourself or the terminal you're running inside). `action=screenshot` captures ONE window — never the whole desktop — and returns a PNG FILE PATH; for clients that accept an inline MCP image (Claude, and ACP/omp sessions) the same result ALSO carries the screenshot inlined as an image block, so you can reason over it directly with no need to open the path yourself — other clients should open that file path with their own image-viewing tool. Every action except `list_windows`, `cursor_position`, and `wait` needs `window` (same id/substring rule as screenshot) and drives that window's input: `left_click`/`right_click`/`double_click`/`triple_click`/`mouse_move` need `coordinate` [x, y]; `left_click_drag` needs `start_coordinate` AND `coordinate` (end point); `scroll` needs `coordinate` plus `scroll_direction`; `type`/`key` need `text` (literal text to type, or a combo like \"cmd+s\"/\"ctrl+shift+t\"/\"Return\"/\"f5\" for `key`). ALL coordinates are in the pixel space of the MOST RECENT screenshot of that window — you MUST screenshot a window before clicking/dragging/scrolling/moving the mouse in it, or the call is rejected (there is no screenshot on file yet to map coordinates against); a coordinate maps proportionally onto the window's CURRENT position and size even if it moved or resized since that screenshot, and an out-of-range coordinate (judged against the screenshot's own dimensions) is rejected rather than guessed at. `type`/`key` additionally require a `left_click`/`right_click`/`double_click`/`triple_click` on that SAME window within the last {FOCUS_FRESHNESS_SECS}s — click inside the target window first to focus it, then type/key within {FOCUS_FRESHNESS_SECS}s, or the call is rejected. Every call — observation or input — may pause for a human's permission card (Needs-you) before it runs; an input action can additionally come back `Busy` (another session currently has it) or fail while a DIFFERENT permission card is still waiting on the human — retry after a moment. Input actions are also rate-limited to roughly 2 per second. This tool is only ever ACTIVE once the human has turned on \"Computer Use\" in weft's Settings; while it's off, every call here fails with a `disabled` result explaining that — this tool being LISTED does not by itself mean it can be used yet."),
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
    // issue #160 round-13 P1 (Codex computer_srv.rs:515): run ALL pure,
    // side-effect-free schema validation for THIS action BEFORE the approval
    // gate below ever opens a card. A malformed call (e.g. `left_click` with no
    // `coordinate`) must be rejected outright — never after a human already
    // answered a card for it, which would mint a standing Always/Full grant for
    // a request that then fails its own arm's parse and never runs. The arms
    // below still re-parse (idempotent) and still do the LIVE checks (window
    // resolution, focus freshness, control lease) that can only run post-approval.
    pure_validate(action, args)?;
    // The server-side approval gate (issue #160 round-2 P1) — see this
    // module's own doc comment and `approve`'s. Runs for EVERY action,
    // observation or input, before any action-specific argument is even
    // looked at: a standing grant decides silently, otherwise this blocks on
    // a Needs-you card exactly like `bus::server::handle_ask` does for every
    // other tool call in this crate.
    //
    // issue #160 round-10 P2 #5 (Codex 296): for a Write (input) action,
    // refuse to even OPEN a new approval card when this EXACT (thread, dir)
    // already has a DIFFERENT one open and unanswered — checked BEFORE
    // `approve` below, not just in each input arm's own later call (which
    // still runs, unchanged, to catch a card that races in DURING the
    // `approve` await itself — see `check_suspended`'s own doc). Without this
    // earlier check, a human sitting on an unrelated open card who answers
    // THIS action's brand-new one with Always/Full would record a standing
    // grant for a call that then never actually runs (the in-arm check
    // rejects it anyway, since the original card is still open) — two cards
    // opened, one of them entirely wasted, and a grant recorded for
    // authorization that was never exercised. Observation actions
    // (`screenshot`/`list_windows`/`cursor_position`) are untouched — they
    // never take the control lease and were never meant to be blocked by an
    // unrelated pending ask; `wait` IS Write-classified (see
    // `GUI_WRITE_ACTIONS`'s own doc: it's part of an input sequence) and so
    // IS now subject to this same pre-approve gate, even though its own arm
    // below never calls `check_suspended` itself.
    if crate::ask::classify_gui_action(action) == crate::ask::RiskLevel::Write {
        check_suspended(asks, thread, dir)?;
    }
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
            // issue #160 round-16 P1 (Codex 605): acquire the SAME semaphore
            // `screenshot` holds during capture (`screenshot_semaphore` — see
            // its own doc, capacity 2) before this call's own OS enumeration
            // — round-15 P2's `MAX_OPEN_OBSERVE_ASKS` only bounds how many
            // observe Ask CARDS may sit open waiting on a human; a session
            // already holding a standing Full/Always grant skips that gate
            // entirely and previously had NO cap at all on how many
            // concurrent `list_windows` calls it could fire, each one a
            // synchronous `xcap` enumeration. Sharing the capture semaphore's
            // budget gives the already-authorized path the same hard
            // concurrency ceiling, rather than inventing a second one.
            let _observe_permit = screenshot_semaphore().acquire().await.map_err(|e| e.to_string())?;
            // issue #160 round-17 P1 (Codex computer_srv.rs:647): recheck the
            // kill switch AFTER the permit queue — with both permits held, a
            // standing-granted caller can sit in `acquire().await` across a
            // Stop/disable, and the only `enabled` read it ever passed was
            // `run_action`'s top gate, long before the queue. Same shape as
            // the screenshot arm's own post-semaphore recheck (round-14 P1);
            // this is the arm's LAST await before the enumeration is
            // scheduled.
            if !computer::enabled(db).await {
                return Err(ComputerError::Disabled.to_string());
            }
            // issue #160 round-22 P1 (Codex computer_srv.rs:664): re-check the
            // session's liveness after the observe queue too. A thread deleted
            // while this `list_windows` sat waiting for a permit records a route
            // revocation but does NOT flip `enabled`, so without this a
            // standing-granted caller would still enumerate the desktop under a
            // deleted identity — the input arms' `recheck_after_guard` closes
            // the same gap for injection, this closes it for enumeration. Gated
            // on the revocation set: a never-deleted thread pays only the
            // lock-only lookup, never the `session_is_live` DB check.
            if computer_routes_revoked(thread) && !session_is_live(db, thread, dir, wt).await {
                return Err(SESSION_GONE_MSG.to_string());
            }
            let b = backend::backend();
            // issue #160 round-16 P1 (Codex 605): the enumeration itself
            // moves onto tokio's blocking pool — see `on_blocking`'s own doc
            // for why every OS-touching call here now does.
            on_blocking(move || {
                // issue #160 round-23 P2 (Codex computer_srv.rs:684): final
                // synchronous Stop-latch check on the blocking-pool thread, as
                // the first statement immediately before the OS enumeration — a
                // Stop landing while THIS closure sat QUEUED for a blocking
                // thread must fail closed here, exactly as the screenshot/input
                // closures already do via `recheck_stop_and_lease_before_backend`.
                // `list_windows` holds no control lease, so only the stop latch
                // applies (there is no thread/dir-scoped lease to re-verify).
                if computer::stop_latched() {
                    return Err(ComputerError::Disabled.to_string());
                }
                computer::visible_windows(b.as_ref()).map_err(|e| e.to_string())
            })
            .await?
            .map(|windows| serde_json::to_string(&windows).unwrap_or_else(|_| "[]".into()))
        }
        "screenshot" => {
            let window_query = required_window(args)?;
            // issue #160 round-11 P1 #C: verify the window `approve` bound at
            // authorization time (screenshot now binds one too — see that
            // function's own `resolved`/doc comment) is STILL the window
            // about to be captured — fail-closed otherwise, never silently
            // capturing whatever the query happens to resolve to NOW. The
            // read-only twin of every input arm's own
            // `resolve_and_verify_target` gate (issue #160 round-8 P1 #4),
            // applied here to an OBSERVE action for the first time: a card
            // can sit open for a long time (up to `bus::server::ASK_WAIT`)
            // before a human answers it, during which the ORIGINAL window
            // could close and a same-titled replacement take its place — an
            // exact-Always grant has the identical gap between "the window
            // it was granted for" and "whatever the query resolves to on
            // THIS call". Also records `window_id_out` for the audit line,
            // even on the failure path (matches every input arm's own
            // ordering — see that helper's doc).
            // First check: fail fast (and still record `window_id_out` for
            // the audit line — `resolve_and_verify_target`'s own doc) before
            // ever awaiting `screenshot_out_dir`/the capture semaphore below
            // for a call that was always going to be rejected anyway. Its
            // OWN `WindowInfo` is intentionally unused past this point —
            // issue #160 round-12 P1 #I re-resolves and re-verifies a SECOND
            // time, right before the capture, and THAT result is what gets
            // used for the actual capture/record below.
            // issue #160 round-18 P1 (Codex computer_srv.rs:1343): this first
            // (fail-fast) resolve now runs on the blocking pool too — one
            // single, argument-gated enumeration, but still a synchronous
            // `xcap` call that must never run inline on an async worker where
            // concurrent captures could collectively starve the Stop/Escape
            // kill-switch tasks (round-16 P1 kept it inline as "just one
            // enumeration"; round-18 closes that residual runtime-occupancy
            // gap — the same reason `approve`'s own authorization-time resolve
            // moved off the runtime). The SECOND resolve below, sitting
            // immediately next to the actual capture, already runs inside this
            // arm's single capture `on_blocking` closure.
            let _ = resolve_and_verify_target_blocking(window_query, &approved, window_id_out).await?;
            let out_dir = screenshot_out_dir(db, thread, dir, wt).await?;
            // issue #160 round-12 P1 #5: acquire the process-wide capture
            // semaphore BEFORE the synchronous capture below — see
            // `screenshot_semaphore`'s own doc for why (a Full/Always-granted
            // worker could otherwise fire arbitrarily many concurrent
            // `screenshot` calls, each holding its own full-resolution RGBA
            // buffer plus PNG/JPEG encode buffers at once, with no cap
            // anywhere on this path). Held across capture AND every encode
            // this call does; dropped when this arm's block ends (including
            // on an early `?` return from the capture itself).
            let _capture_permit = screenshot_semaphore().acquire().await.map_err(|e| e.to_string())?;
            // issue #160 round-16 P1 (Codex 605): whether this call's owning
            // engine accepts an inline MCP image is looked up now, on the
            // runtime — `engine_accepts_mcp_image` awaits the db, but
            // everything past the recheck below (the second resolve through
            // every encode/record) runs inside a SINGLE `on_blocking` closure
            // (see that helper's own doc), and a blocking-pool closure can't
            // itself `.await` anything. The bool crosses that boundary as a
            // plain owned value instead of the lookup itself moving in.
            //
            // issue #160 round-17 P1 (Codex computer_srv.rs:730): this lookup
            // sits BEFORE the enabled recheck below, not after it — round-16's
            // refactor briefly had it after, which re-opened the exact gap the
            // recheck exists to close: a Stop landing while THIS db await was
            // in flight went unseen, and the capture was scheduled anyway. The
            // recheck must be the arm's genuinely LAST await.
            let want_mcp_image = engine_accepts_mcp_image(db, thread, dir).await;
            // issue #160 round-23 P1 (Codex computer_srv.rs:766): recheck session
            // liveness after the capture queue too. The list_windows arm gained
            // this in round-22, but this screenshot arm still only rechecked
            // `enabled` below. A thread deleted while this call waited for a
            // capture permit records a route revocation but does NOT flip
            // `enabled`, so without this a standing-granted caller would capture
            // pixels after its session is gone — and `screenshot_window` writing
            // into the already-resolved `out_dir` would RECREATE the just-cleaned
            // per-thread output tree. Gated on the revocation set (a never-deleted
            // thread pays only the lock-only lookup, never the DB check).
            if computer_routes_revoked(thread) && !session_is_live(db, thread, dir, wt).await {
                return Err(SESSION_GONE_MSG.to_string());
            }
            // issue #160 round-14 P1 (Codex computer_srv.rs:583): recheck the
            // kill switch AFTER every await this arm took since the last
            // `enabled` check up top — `screenshot_out_dir`, the capture
            // semaphore's own `acquire().await` (which can queue arbitrarily
            // long when both `SCREENSHOT_CONCURRENCY` permits are already
            // held), and the engine lookup just above. Without this, a human
            // hitting Stop / disabling Computer Use while this call sat queued
            // would still capture once a permit freed — a Full/Always-granted
            // caller could leave many captures queued to fire AFTER Stop.
            // This is the arm's LAST `.await` (round-17 P1 re-established
            // that invariant — see the lookup's comment above): nothing runs
            // between it and scheduling the blocking capture below.
            if !computer::enabled(db).await {
                return Err(ComputerError::Disabled.to_string());
            }
            // issue #160 round-12 P1 #I: re-resolve + re-verify identity
            // AGAIN, after EVERY await this arm takes since the first check
            // above (`screenshot_out_dir`, the capture semaphore itself, then
            // the engine lookup just above) — with BOTH `SCREENSHOT_
            // CONCURRENCY` permits already held by concurrent captures, the
            // `acquire().await` above can queue for arbitrarily long.
            // `screenshot_window` below only re-resolves `window_query` for
            // ITS OWN capture; it does not compare against `approved` at all.
            // Without this second check, the ORIGINAL window could close
            // during that queueing gap and a same-query REPLACEMENT window
            // take its place, and `screenshot_window`'s internal re-resolve
            // would then silently capture the REPLACEMENT's pixels under an
            // approval that was only ever shown for the original — this is
            // exactly the round-11 P1 #C gap, reopened by round-12 P1 #5's OWN
            // semaphore queue.
            //
            // issue #160 round-16 P1 (Codex 605): this resolve, the capture,
            // and every encode/record below now all run INSIDE one
            // `on_blocking` closure — moved off the async runtime for the
            // same reason `on_blocking`'s own doc gives (a slow/queued OS
            // call here must never risk starving the Stop/Escape kill
            // switch). The "no further `.await` happens between this check
            // and the capture" guarantee this comment used to make still
            // holds in spirit: the closure itself contains no `.await` at all
            // (a plain sync closure — `spawn_blocking` requires that), and
            // the resolve inside it runs immediately before the capture, on
            // the SAME blocking-pool thread, with nothing else able to run in
            // between — so the "just verified" identity guarantee (round-12
            // P1 #I) is preserved exactly, just now separated from this check
            // by a `spawn_blocking` scheduling boundary instead of sitting in
            // the same async stack frame. `resolve_and_verify_target` takes
            // `window_id_out` as `&mut Option<u32>` — a reference into THIS
            // function's own stack frame, which can't cross into a `'static`
            // blocking closure — so the closure below runs it against a
            // throwaway LOCAL `Option<u32>` instead and returns that
            // alongside its own result; written back into the real
            // `window_id_out` immediately after `on_blocking` returns (below),
            // preserving `resolve_and_verify_target`'s existing "record the
            // id even when identity verification fails" semantics exactly.
            let window_query_owned = window_query.to_string();
            let approved_for_capture = approved.clone();
            let dir_owned = dir.to_string();
            let b = backend::backend();
            let (resolved_id, capture) = on_blocking(move || {
                let mut resolved_id: Option<u32> = None;
                let w = match resolve_and_verify_target(
                    &window_query_owned,
                    &approved_for_capture,
                    &mut resolved_id,
                ) {
                    Ok(w) => w,
                    Err(e) => return (resolved_id, Err(e)),
                };
                // issue #160 round-17 P1 (Codex computer_srv.rs:793): capture
                // the EXACT window `w` just verified — `computer::screenshot_
                // resolved` takes the already-resolved `WindowInfo` instead of
                // the raw query, so there is NO third enumeration left for a
                // same-query replacement to slip into between verify and
                // capture (the old `screenshot_window(query, ..)` call
                // re-resolved internally; two OS enumerations back-to-back
                // are still two). If `w` closed in the instant since the
                // verify above, `capture_window(w.id)` fails closed rather
                // than ever falling back to a lookalike.
                // issue #160 round-20 P1 (Codex computer_srv.rs:813): recheck
                // the stop latch on THIS blocking thread, immediately before
                // the capture. The arm's last `enabled` recheck ran BEFORE this
                // closure was scheduled; a Stop landing while the closure sat
                // queued for a blocking thread — or while the final resolve
                // just above stalled — would otherwise let the capture proceed
                // AFTER Emergency Stop. `screenshot` holds no control lease
                // (it is ReadOnly), so only the stop latch is checked here (the
                // input arms' `recheck_stop_and_lease_before_backend` also
                // re-checks the lease).
                if computer::stop_latched() {
                    return (resolved_id, Err(ComputerError::Disabled.to_string()));
                }
                let shot = match computer::screenshot_resolved(b.as_ref(), &w, &out_dir) {
                    Ok(s) => s,
                    Err(e) => return (resolved_id, Err(e.to_string())),
                };
                // issue #160 round-10 P2 #H: this confirmation text is shared
                // by BOTH the plain-text-only result and `text_and_image_
                // result`'s own text block (see that function's doc) —
                // worded so it holds for either: a capable client (Claude,
                // ACP/omp) also gets this screenshot inlined as an image
                // block in the SAME result and can reason over it directly;
                // any other client opens the path.
                let text = format!(
                    "screenshot saved: {} ({}x{}, scale {:.2}) — inlined as an image in this result for \
                     capable clients (Claude, ACP/omp), otherwise open the path with your own image \
                     viewing tool",
                    shot.path.display(),
                    shot.width,
                    shot.height,
                    shot.scale
                );
                // Both the preview registry and the MCP image block need the
                // raw pixels — read straight off `shot.pixels`, the SAME
                // in-memory RGBA `screenshot_window` itself scaled and saved
                // (issue #160 round-7 P1), never re-opened from `shot.path`.
                // This used to re-read the just-saved PNG back off disk
                // (`read_captured_image`, now deleted): a worker-writable
                // `out_dir` is repository-controlled content, and the gap
                // between that PNG's own save and this re-open was an open
                // TOCTOU/symlink window — a sandboxed background process
                // could swap the freshly-saved file for a symlink to an
                // arbitrary user-readable image in that gap, and Weft would
                // follow it with its own permissions, inlining the
                // substituted pixels to the model/human as if they were the
                // real capture. Reading `shot.pixels` instead closes that
                // window entirely rather than narrowing it — there is no
                // second filesystem access left to race. `encode_jpeg_data_
                // uri` can still fail on `shot.pixels` (encoding, not
                // decoding, can theoretically error) — best-effort, same as
                // before: a failure here must never fail a screenshot that
                // already saved successfully, it just means no
                // preview/image this call.
                //
                // Keyed to `shot.window_id` — the id `computer::
                // screenshot_window` ITSELF already resolved and captured
                // against (issue #160 round-6 review P2 #4) — rather than
                // re-resolving `window_query` a second time here: a second
                // resolution can land on a DIFFERENT window than the one
                // actually captured if it closed, was renamed, or its id got
                // reused in the gap between the two calls, silently
                // mis-keying the preview (and any input approval card that
                // later attaches it) to the WRONG window. `computer::
                // Screenshot::window_id` closes that gap by construction —
                // there is no second resolution left to drift.
                if let Ok(preview) =
                    computer::encode_jpeg_data_uri(&shot.pixels, PREVIEW_LONG_EDGE, PREVIEW_QUALITY)
                {
                    // issue #160 round-14 P1 (Codex computer_srv.rs:1466):
                    // store the FULL window identity (id + app + title) this
                    // capture came from, not the numeric id alone — `w` is
                    // the already-resolved, just-verified target (its `id`
                    // equals `shot.window_id` by that verification), so this
                    // reuses it rather than re-resolving. See
                    // [`VerifiedWindowIdentity`] for the id-reuse hazard this
                    // closes.
                    store_screenshot_preview(thread, &dir_owned, preview, VerifiedWindowIdentity::from_window(&w));
                }
                // issue #160 round-11 P1 #D: record THIS capture's own saved
                // dimensions for (thread, dir, shot.window_id) — every
                // coordinate-taking input arm below maps against whatever is
                // on file here (see `computer::map_screenshot_coord`'s own
                // doc), fail-closed if nothing is. Recorded unconditionally,
                // on every successful capture, regardless of which engine is
                // asking — matches `store_screenshot_preview`'s own "refresh
                // every successful screenshot" rule right above.
                computer::record_shot_dims(thread, &dir_owned, shot.window_id, shot.width, shot.height, &w);
                // The MCP `image` content block is engine-gated — see
                // `engine_accepts_mcp_image`'s doc table; `want_mcp_image`
                // was already decided on the runtime, above, before this
                // closure started (see that round-16 P1 comment).
                let image_b64 = if want_mcp_image {
                    computer::encode_jpeg_data_uri(&shot.pixels, MCP_IMAGE_LONG_EDGE, MCP_IMAGE_QUALITY)
                        .ok()
                        .and_then(|uri| strip_data_uri_prefix(&uri).map(str::to_string))
                } else {
                    None
                };
                (resolved_id, Ok((text, image_b64)))
            })
            .await?;
            *window_id_out = resolved_id;
            let (text, image_b64) = capture?;
            if let Some(b64) = image_b64 {
                *screenshot_image_b64_out = Some(b64);
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
            //
            // issue #160 round-8 P1 #4: this window must still be the EXACT
            // one `approve` bound at authorization time — see
            // `verify_approved_target`'s own doc. Checked BEFORE this window
            // is ever activated or clicked (`resolve_and_verify_target`
            // does both: resolve, record `window_id_out`, verify).
            let w = resolve_and_verify_target_blocking(window_query, &approved, window_id_out).await?;
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
            // issue #160 round-10 P1 #B: `activate_target` (inside the call
            // above) shells out to a potentially slow, blocking OS call
            // (osascript/wmctrl/`xdotool --sync`) — the window can move,
            // resize, close, or have its id reused by an unrelated window
            // WHILE that call runs. Re-resolve/re-verify AFTER it returns,
            // and map/inject against THIS fresh state — never the
            // pre-activation `w`, which may already be stale by now.
            let w2 = resolve_and_verify_target_blocking(window_query, &approved, window_id_out).await?;
            let (px, py) = map_input_coord(thread, dir, &w2, cx, cy)?;
            // issue #160 round-16 P1 (Codex 605): the injection itself moves
            // onto tokio's blocking pool too (`enigo` is a synchronous OS
            // call — see `on_blocking`'s own doc). No extra concurrency cap
            // is needed here the way `list_windows`/`screenshot` needed one:
            // `input_flight_guard`, acquired above, already serializes the
            // ENTIRE process to one in-flight input action at a time — every
            // OTHER input arm below shares this same reasoning without
            // repeating it.
            // issue #160 round-18 P1 (Codex computer_srv.rs:967): the final
            // stop/lease recheck runs INSIDE the closure, on the blocking
            // thread, right before the backend call — see
            // `recheck_stop_and_lease_before_backend`'s own doc for the gap
            // (the `w2` resolve + the blocking-pool queue) it closes.
            let dir_owned = dir.to_string();
            let b = backend::backend();
            on_blocking(move || {
                recheck_stop_and_lease_before_backend(thread, &dir_owned)?;
                b.click(px, py, button, count).map_err(|e| e.to_string())
            })
            .await??;
            // A click that actually reached the OS is presumed to have
            // handed this window OS focus — see `recent_clicks`'s doc. Only
            // AFTER the backend call succeeds: a rejected/failed click never
            // touched the real window and must not seed a false freshness
            // record for a later `type`/`key`.
            record_click_focus(thread, dir, &w2);
            Ok(format!(
                "{action} at ({px}, {py}) in window {} done — take a screenshot to verify",
                w2.id
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
            // review P1 #3), TWICE — once before activation, once after
            // (issue #160 round-10 P1 #B).
            let _flight = computer::input_flight_guard().await;
            recheck_after_guard(db, asks, thread, dir).await?;
            let w = resolve_and_verify_target_blocking(window_query, &approved, window_id_out).await?;
            activate_and_recheck(db, asks, thread, dir, w.id).await?;
            // issue #160 round-10 P1 #B: see the click-family arm above.
            let w2 = resolve_and_verify_target_blocking(window_query, &approved, window_id_out).await?;
            let (px, py) = map_input_coord(thread, dir, &w2, cx, cy)?;
            // issue #160 round-16 P1 (Codex 605): see the click-family arm
            // above.
            // issue #160 round-18 P1: final in-closure stop/lease recheck —
            // see the click-family arm above.
            let dir_owned = dir.to_string();
            let b = backend::backend();
            on_blocking(move || {
                recheck_stop_and_lease_before_backend(thread, &dir_owned)?;
                b.move_cursor(px, py).map_err(|e| e.to_string())
            })
            .await??;
            Ok(format!(
                "mouse_move to ({px}, {py}) in window {} done — take a screenshot to verify",
                w2.id
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
            // against the SAME freshly-resolved window — a drag has two
            // coordinates, but only one window to go stale. issue #160
            // round-10 P1 #B: that resolve now happens TWICE, before and
            // after activation — see the click-family arm above — and BOTH
            // endpoints are mapped against the SECOND (post-activation) `w2`.
            let w = resolve_and_verify_target_blocking(window_query, &approved, window_id_out).await?;
            activate_and_recheck(db, asks, thread, dir, w.id).await?;
            let w2 = resolve_and_verify_target_blocking(window_query, &approved, window_id_out).await?;
            let from = map_input_coord(thread, dir, &w2, sx, sy)?;
            let to = map_input_coord(thread, dir, &w2, ex, ey)?;
            // issue #160 round-16 P1 (Codex 605): see the click-family arm
            // above.
            // issue #160 round-18 P1: final in-closure stop/lease recheck —
            // see the click-family arm above.
            let dir_owned = dir.to_string();
            let b = backend::backend();
            on_blocking(move || {
                recheck_stop_and_lease_before_backend(thread, &dir_owned)?;
                b.drag(from, to).map_err(|e| e.to_string())
            })
            .await??;
            Ok(format!(
                "left_click_drag from ({}, {}) to ({}, {}) in window {} done — take a screenshot to verify",
                from.0, from.1, to.0, to.1, w2.id
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
            let w = resolve_and_verify_target_blocking(window_query, &approved, window_id_out).await?;
            activate_and_recheck(db, asks, thread, dir, w.id).await?;
            // issue #160 round-10 P1 #B: see the click-family arm above.
            let w2 = resolve_and_verify_target_blocking(window_query, &approved, window_id_out).await?;
            let (px, py) = map_input_coord(thread, dir, &w2, cx, cy)?;
            // issue #160 round-16 P1 (Codex 605): see the click-family arm
            // above.
            // issue #160 round-18 P1: final in-closure stop/lease recheck —
            // see the click-family arm above.
            let dir_owned = dir.to_string();
            let b = backend::backend();
            on_blocking(move || {
                recheck_stop_and_lease_before_backend(thread, &dir_owned)?;
                b.scroll(px, py, dx, dy).map_err(|e| e.to_string())
            })
            .await??;
            Ok(format!(
                "scroll at ({px}, {py}) dx={dx} dy={dy} in window {} done — take a screenshot to verify",
                w2.id
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
            let w = resolve_and_verify_target_blocking(window_query, &approved, window_id_out).await?;
            activate_and_recheck(db, asks, thread, dir, w.id).await?;
            // issue #160 round-10 P1 #B: re-resolve/re-verify AFTER
            // activation, same as every other input arm — and check focus-
            // freshness against THIS fresh id (`w2.id`), not the
            // pre-activation one: the window `require_recent_focus` guards
            // is the SAME one about to receive the keystrokes.
            let w2 = resolve_and_verify_target_blocking(window_query, &approved, window_id_out).await?;
            require_recent_focus(thread, dir, &w2)?;
            let char_count = text.chars().count();
            let text_owned = text.to_string();
            // issue #160 round-16 P1 (Codex 605): see the click-family arm
            // above — `text_owned` crosses into the blocking closure since
            // `text` itself is borrowed from `args`, not `'static`.
            // issue #160 round-18 P1: final in-closure stop/lease recheck —
            // see the click-family arm above.
            let dir_owned = dir.to_string();
            let b = backend::backend();
            on_blocking(move || {
                recheck_stop_and_lease_before_backend(thread, &dir_owned)?;
                b.type_text(&text_owned).map_err(|e| e.to_string())
            })
            .await??;
            Ok(format!(
                "typed {char_count} char(s) in window {} done — take a screenshot to verify",
                w2.id
            ))
        }
        "key" => {
            let window_query = required_window(args)?;
            let combo = required_text(args)?;
            // issue #160 round-10 P2 #4 (Codex 580): validate the combo's
            // SHAPE — a pure, argument-only check, no lease/throttle/backend
            // touched — before `check_suspended`/`acquire_and_throttle`
            // below, mirroring round-2 P2 §4's "purely-argument checks run
            // first" discipline every other input arm already follows (see
            // this section's own "input gates" doc comment further down).
            // Before this, a malformed combo (e.g. "ctrl+a+b") wasn't
            // rejected until `OsBackend::key` actually ran it through
            // `parse_key_combo` a SECOND time — by which point this call had
            // already taken the 30s control lease, consumed a throttle slot,
            // and activated the target window, for a call that was always
            // going to fail. The parsed tokens are discarded here (`let _`)
            // — this exists ONLY to reject a malformed shape early; the
            // combo's ACTUAL injection still goes through `b.key(combo)`
            // below (issue #160 round-16 P1: now via `on_blocking` — see
            // that helper's own doc — but still the SAME backend call,
            // unchanged; os.rs itself is not touched here).
            let _ = computer::parse_key_combo(combo).map_err(|e| e.to_string())?;
            check_suspended(asks, thread, dir)?;
            acquire_and_throttle(thread, dir)?;
            let _flight = computer::input_flight_guard().await;
            recheck_after_guard(db, asks, thread, dir).await?;
            // See the matching comment in the "type" arm above.
            let w = resolve_and_verify_target_blocking(window_query, &approved, window_id_out).await?;
            activate_and_recheck(db, asks, thread, dir, w.id).await?;
            // issue #160 round-10 P1 #B: see the "type" arm above.
            let w2 = resolve_and_verify_target_blocking(window_query, &approved, window_id_out).await?;
            require_recent_focus(thread, dir, &w2)?;
            let combo_owned = combo.to_string();
            // issue #160 round-16 P1 (Codex 605): see the click-family arm
            // above.
            // issue #160 round-18 P1: final in-closure stop/lease recheck —
            // see the click-family arm above.
            let dir_owned = dir.to_string();
            let b = backend::backend();
            on_blocking(move || {
                recheck_stop_and_lease_before_backend(thread, &dir_owned)?;
                b.key(&combo_owned).map_err(|e| e.to_string())
            })
            .await??;
            Ok(format!(
                "key {combo} in window {} done — take a screenshot to verify",
                w2.id
            ))
        }
        // No window, no control lock, no throttle — this reads the cursor's
        // current position without touching input devices.
        "cursor_position" => {
            // issue #160 round-16 P1 (Codex 605): even this near-instant OS
            // call moves onto tokio's blocking pool (see `on_blocking`'s own
            // doc) so it never risks parking an async worker Stop/Escape
            // needs — cheap enough that it doesn't need `screenshot_
            // semaphore`'s concurrency budget (see that constant's own doc
            // for what THAT protects against: buffered capture memory, not a
            // quick position query).
            let b = backend::backend();
            on_blocking(move || b.cursor_position())
                .await?
                .map(|(x, y)| format!("cursor at ({x}, {y})"))
                .map_err(|e| e.to_string())
        }
        // No window, no control lock, no throttle — a pure timer.
        "wait" => {
            let ms = parse_duration_ms(args)?;
            tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
            Ok(format!("waited {ms}ms"))
        }
        // Fail-closed, not fail-open: an unrecognized/missing action is
        // rejected with the valid list, never silently treated as one of
        // the known ones. In practice [`pure_validate`] already rejected an
        // unknown action before dispatch ever reached here (issue #160
        // round-13 P1) — this arm stays as defense-in-depth, sharing the
        // SAME [`unknown_action_error`] rendering so the two can't diverge.
        _ => Err(unknown_action_error(action)),
    }
}

// —— server-side approval gate (issue #160 round-2 P1) ——

/// issue #160 round-15 P2 (Codex computer_srv.rs:1288): how many OBSERVE-class
/// (`RiskLevel::ReadOnly`) computer-use Ask cards may sit open at once for one
/// `(thread, dir)` before further grant-less observe calls fail closed instead
/// of opening more — see `approve`'s observe branch. Small on purpose: a human
/// can only meaningfully consider a couple of screenshot/list_windows cards at
/// a time, and each open card is a held waiter (`bus::server::ASK_WAIT` — up
/// to an hour) plus an IM-bridge push; Write (input) actions are stricter
/// still (ANY open ask suspends them — `check_suspended` /
/// `request_with_preview_unless_open`).
const MAX_OPEN_OBSERVE_ASKS: usize = 3;

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
///
/// issue #160 round-10 P1 #A: the resolve above USED TO happen only once
/// authorization already landed (`bind_approved_window`, round-8's own
/// helper, now folded directly into this function — see the `resolved`
/// binding below). That left a gap `bind_approved_window` alone could never
/// close: a standing Always grant is looked up by `action_key` ALONE, and
/// the OLD key (`["gui", action, window_query, digest]`) never captured
/// WHICH window was actually approved — just the query STRING. Once a human
/// approved one `type`-into-"notes" call as Always, that SAME key kept
/// auto-approving every FUTURE `type` into whatever window "notes" happens
/// to resolve to later, including an entirely different app/title that
/// closed the original and took its place (round-8's own approve→dispatch
/// binding only ever caught that drift AFTER approval, never at the
/// grant-lookup step itself). The fix: for a Write action with a non-blank
/// window argument, resolve `window_query` AUTHORITATIVELY right here,
/// BEFORE `action_key` is even built, and fold the resolved `app`+`title`
/// into the key itself. A standing Always grant then only ever matches the
/// EXACT window identity it was granted for — a query that now resolves to
/// a different app/title mints a DIFFERENT key, misses `auto_decision_exact`
/// entirely, and falls through to a fresh card. This same `resolved` value
/// is reused directly for the `ApprovedWindow` this function returns on
/// EITHER success path (a standing grant, or a human's card) — no second
/// resolve inside a separate `bind_approved_window` step anymore, so there
/// is no "resolve once for the key, resolve again to bind" drift left to
/// reopen.
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
    // issue #160 round-10 P1 #A: resolve the window's identity FIRST, before
    // `action_key` is even built — see this function's own doc comment above
    // for the standing-grant identity gap this closes. Every Write-classified
    // action with a non-blank window argument gets this (mirrors the OLD
    // `bind_approved_window`'s own gate exactly): `list_windows`/
    // `cursor_position` and `wait` (Write-classified but windowless) have
    // nothing to bind and keep the OLD, resolve-free key shape below.
    //
    // issue #160 round-11 P1 #C: `screenshot` now ALSO resolves here, even
    // though it's `RiskLevel::ReadOnly` — before this round, ONLY Write
    // actions bound a window at all, so a screenshot's approval card/standing
    // grant was scoped to the bare QUERY STRING alone, never the window it
    // actually resolved to. That let a card opened (or an Always grant
    // earned) for "screenshot @ notes" keep silently authorizing every FUTURE
    // capture of whatever "notes" happens to resolve to later — including a
    // DIFFERENT window that closed the original and took its place while the
    // card sat open, or after the grant was earned. Folding the resolved
    // identity into a screenshot's own key too closes that the same way
    // round-10 P1 #A already closed it for input actions: a standing grant
    // only ever matches the EXACT window it was granted against, and this
    // same `resolved` value is what the screenshot arm below verifies the
    // window it's ABOUT TO CAPTURE against (see `verify_approved_target`'s
    // new call site there) — fail-closed if a human/Always-approved ONE
    // window and a later capture would land on a DIFFERENT one.
    // `list_windows`/`cursor_position` (no specific window target at all)
    // are unaffected: `window_query` is always blank for them.
    let resolved = if !window_query.trim().is_empty()
        && (risk == crate::ask::RiskLevel::Write || action == "screenshot")
    {
        // issue #160 round-18 P1 (Codex computer_srv.rs:1343): this
        // authorization-time window resolution is a synchronous `xcap`
        // enumeration that runs for EVERY windowed Write action and every
        // `screenshot`, BEFORE the observe-ask bound or the capture
        // semaphore. Run it on the blocking pool, never inline on an async
        // worker — otherwise concurrent authenticated `tools/call`s (grant-
        // less or standing-granted alike) could each park a tokio worker in
        // enumeration and collectively starve the Stop/Escape kill-switch
        // tasks that MUST keep scheduling. Every other OS-touching call on
        // this server already routes through `on_blocking` for exactly this
        // reason; this approval-time resolve was the one that still ran
        // inline.
        let b = backend::backend();
        let wq = window_query.to_string();
        // issue #160 round-23 P1 (Codex computer_srv.rs:1428): this resolve runs
        // at AUTHORIZATION time — before `auto_decision_exact` and before any
        // permission card. `ComputerError::AmbiguousWindow`'s Display lists every
        // matching app name, window title, and id; returning it here would hand
        // that desktop metadata to the agent with NO human approval — exactly the
        // enumeration the `list_windows` card exists to gate, reconstructable via
        // repeated broad-query probes. Redact the candidate list to a generic
        // narrow-your-query message. Other resolution errors (e.g. WindowNotFound)
        // disclose nothing about OTHER windows and pass through unchanged; the
        // full candidate list still reaches the human on the approval card.
        let window = match on_blocking(move || computer::resolve_window(b.as_ref(), &wq)).await? {
            Ok(w) => w,
            Err(ComputerError::AmbiguousWindow { .. }) => {
                return Err(
                    "the window query matched more than one window — narrow it to a unique \
                     application name or window title"
                        .to_string(),
                );
            }
            Err(other) => return Err(other.to_string()),
        };
        Some(window)
    } else {
        None
    };
    // Granularity tradeoff (documented, not a behavior bug): folding
    // `app`+`title` into an input action's key makes a standing Always grant
    // scoped to that EXACT window TITLE, not just the app — a title change
    // (a browser tab navigating, a document renaming itself in its title
    // bar) mints a new key and re-cards even though a human would likely
    // consider it "the same window". That is the safe default this round
    // ships; loosening it to app-only scoping is a legitimate, separately-
    // discussable product tradeoff for later, not something this round
    // changes.
    //
    // issue #160 round-11 P1 #B: the resolved window's own `id` is now ALSO
    // folded in — right after `window_query`, before `app`/`title` — for
    // every action this branch covers (both Write actions with a window, and
    // now `screenshot`). Before this, the key carried `app`+`title` but NOT
    // `id`: if the ORIGINAL window closed and a NEW window opened with the
    // exact same app+title (a relaunched app, a reopened document with an
    // identical name), the key was IDENTICAL to the one a standing Always
    // grant was earned against — silently authorizing input into (or a
    // capture of) the REPLACEMENT window instance, even though
    // `verify_approved_target`'s execution-time check (which DOES compare
    // `id`) would have caught the SAME mismatch had the grant lookup itself
    // been scoped that tightly to begin with. Folding `id` into the key
    // closes that at the SOURCE: a new window instance (new id) mints a
    // DIFFERENT key regardless of how closely its app/title happen to match
    // the old one, missing `auto_decision_exact` and falling through to a
    // fresh card — the safer default for input/capture actions, at the cost
    // of re-prompting once per window instance rather than once per
    // app+title (documented tradeoff, not a bug: see the top-of-file doc for
    // the a `type` Always-grant's own note on this same shape).
    // `always_key_is_persistable` (ask.rs) only inspects `parts[0]`/`parts[1]`
    // (`"gui"`/the action name) to decide whether a `type` grant is safe to
    // persist — inserting `id` at position 3 (0-indexed) doesn't touch either
    // of those, so that gate is unaffected by this shape change.
    let action_key = match &resolved {
        Some(w) => crate::ask::action_key(&[
            "gui",
            action,
            &window_query,
            &w.id.to_string(),
            &w.app,
            &w.title,
            &digest,
        ]),
        None => crate::ask::action_key(&["gui", action, &window_query, &digest]),
    };
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
        // issue #160 round-10 P1 #A: `resolved` was already computed above,
        // BEFORE this grant was even looked up (the key itself depends on
        // it) — reused directly here rather than resolved a second time, so
        // there is no "resolve once for the key, resolve again to bind" gap
        // left for a window swap to land in.
        Some(Decision::Allow) => return Ok(resolved.map(ApprovedWindow::from)),
        // `auto_decision_exact` never actually returns `Deny` today (only
        // Allow-only standing grants exist) — this arm keeps the gate
        // correct regardless, mirroring `handle_ask`'s own defensive shape,
        // rather than silently falling through to a redundant card for it.
        Some(Decision::Deny) => return Err("denied by a standing weft rule".to_string()),
        None => {}
    }

    let preview = preview_for_action(thread, dir, risk, resolved.as_ref());
    // issue #160 round-14 P1 (Codex computer_srv.rs:515): for an input (Write)
    // action, open the card ATOMICALLY with the "no other ask is already open
    // for this (thread, dir)" check — `check_suspended` above and this insert
    // were two SEPARATE lock acquisitions, so two concurrent Write calls for
    // the same session could both pass that pre-check and each open a card. The
    // atomic variant returns `None` (opening nothing) if another card raced in
    // first, which we surface as the SAME `SuspendedPendingAsk` the pre-check
    // and every input arm's own recheck already return. Observe actions
    // (`RiskLevel::ReadOnly` — `screenshot`/`list_windows`/`cursor_position`)
    // are never suspended (see `check_suspended`'s Write-only gate) and keep
    // the plain, always-inserts path.
    let (id, rx) = if risk == crate::ask::RiskLevel::Write {
        match asks.request_with_preview_unless_open(
            thread, dir, "computer", &summary, &detail, detail_redacted.as_deref(), risk, &action_key, preview,
        ) {
            Some(pair) => pair,
            None => return Err(ComputerError::SuspendedPendingAsk.to_string()),
        }
    } else {
        // issue #160 round-15 P2 (Codex computer_srv.rs:1288): observe actions
        // (`screenshot`/`list_windows`/`cursor_position`) are deliberately NOT
        // suspended by an unrelated open card the way Write actions are — but
        // that must not mean UNBOUNDED: without a cap, a worker looping
        // grant-less observe calls minted one Ask + waiter per call (each held
        // up to `bus::server::ASK_WAIT`), flooding the registry/UI/IM bridge,
        // and a later Full answer released the whole backlog at once. The
        // bounded variant counts THIS session's open GUI asks and inserts
        // atomically under [`MAX_OPEN_OBSERVE_ASKS`] — over the cap, the call
        // fails closed with a retry hint instead of opening yet another card.
        match asks.request_with_preview_gui_bounded(
            thread, dir, "computer", &summary, &detail, detail_redacted.as_deref(), risk, &action_key, preview,
            MAX_OPEN_OBSERVE_ASKS,
        ) {
            Some(pair) => pair,
            None => {
                return Err(format!(
                    "too many computer-use approvals are already waiting on the human for this \
                     session (max {MAX_OPEN_OBSERVE_ASKS} open at once) — wait for the open cards \
                     to be answered, then retry"
                ))
            }
        }
    };

    // issue #160 round-15 P1 (Codex commands.rs:1619): re-check the SYNCHRONOUS
    // stop latch immediately AFTER publishing the card, and self-cancel it if a
    // stop/disable landed. `run_action`'s top `computer::enabled` gate can be
    // passed by a call that then straddles the disable transition — window
    // resolution above is a real OS call, so the gap between "enabled read
    // true" and "card inserted" is unbounded. Both disable paths (Emergency
    // Stop AND, as of round-15, the Settings toggle) run trip_stop_latch →
    // cancel_gui_asks: a card inserted BEFORE the sweep is killed by the sweep;
    // one inserted AFTER it is exactly this straddler — the sweep missed it,
    // but the latch was already visible when we get here (this check is after
    // the insert in program order), so we cancel our OWN card and fail closed.
    // Either interleaving leaves NO surviving card a human could later answer
    // Always/Full into a post-disable grant. `stop_latched` is latch-only and
    // sync (no await between insert and check — nothing can wedge in).
    if computer::stop_latched() {
        asks.cancel(id);
        return Err(ComputerError::Disabled.to_string());
    }

    match tokio::time::timeout(crate::bus::server::ASK_WAIT, rx).await {
        // issue #160 round-10 P1 #A: same `resolved` value, reused here too —
        // see this function's own top doc comment for why this round
        // deliberately no longer re-resolves a second time at the moment the
        // human's card answers Allow (round-8's own `bind_approved_window`
        // used to): a standing Always grant this call might mint is keyed
        // off THIS identity regardless, and `run_action`'s own later, fresh
        // `resolve_window`/`verify_approved_target` pair (issue #160 round-10
        // P1 #B, run both before AND after activation) is what actually
        // guards the approve→dispatch gap a long human wait can open, not
        // this return value.
        Ok(Ok(Decision::Allow)) => Ok(resolved.map(ApprovedWindow::from)),
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
///
/// issue #160 round-10 P1 #A: this is now ALSO the exact identity folded
/// into the input action's own `action_key` (see `approve`'s `resolved`
/// binding) — the same three fields serve both jobs (scoping a standing
/// Always grant to one window, and verifying dispatch lands on that same
/// window) since they are, by construction, the identical question: "is
/// this still the window that was approved".
#[derive(Debug, Clone, PartialEq, Eq)]
struct ApprovedWindow {
    id: u32,
    app: String,
    title: String,
}

/// issue #160 round-10 P1 #A: `approve` resolves a Write action's window
/// EXACTLY ONCE (see its own `resolved` binding) and reuses that SAME
/// `WindowInfo` both to build `action_key` and — via this trivial,
/// infallible conversion — to construct the `ApprovedWindow` it returns.
/// Replaces round-8's own `bind_approved_window`, which used to re-resolve
/// `window_query` a SECOND time at the moment authorization landed; folding
/// window identity into the key itself (this round's fix) means that second
/// resolve no longer serves any purpose a plain field-copy can't.
impl From<computer::WindowInfo> for ApprovedWindow {
    fn from(w: computer::WindowInfo) -> Self {
        ApprovedWindow { id: w.id, app: w.app, title: w.title }
    }
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

/// issue #160 round-10 P1 #B: resolve `window_query` fresh against the
/// process-wide backend, record its id into `window_id_out` — done even when
/// the very next [`verify_approved_target`] check fails, so the audit log
/// still names which window was TARGETED, not only ones that actually
/// received input — then verify it against `approved`. Every input arm of
/// [`run_action`] calls this TWICE now: once before [`activate_and_recheck`]
/// (purely to get an id to activate, plus an early identity check), and
/// again immediately after it returns.
///
/// Why twice: `activate_target` (inside `activate_and_recheck`) shells out to
/// a potentially slow, blocking OS call (`osascript`/`wmctrl`/`xdotool
/// --sync`) — the window can move, resize, close, or have its id reused by an
/// entirely unrelated window WHILE that call is in flight. A coordinate
/// mapped, or a focus-freshness check made, against the PRE-activation
/// resolve could then land outside the window that's actually there once
/// activation finishes, or silently target a replacement window that reused
/// the same id — with no re-check that it's still the one `approve` bound at
/// authorization time. The SECOND call's `WindowInfo` is what every arm
/// actually maps coordinates against / checks focus-freshness against /
/// injects into; the first call exists only to obtain an id for activation
/// and an early fail-fast identity check.
fn resolve_and_verify_target(
    window_query: &str,
    approved: &Option<ApprovedWindow>,
    window_id_out: &mut Option<u32>,
) -> Result<computer::WindowInfo, String> {
    let w = computer::resolve_window(backend::backend().as_ref(), window_query).map_err(|e| e.to_string())?;
    *window_id_out = Some(w.id);
    verify_approved_target(approved, &w)?;
    Ok(w)
}

/// issue #160 round-16 P1 (Codex 605): the on-blocking-pool wrapper every
/// input arm of [`run_action`] calls (instead of [`resolve_and_verify_
/// target`] directly) both times it resolves a target window — window
/// enumeration (`computer::resolve_window`, inside the wrapped call) is a
/// synchronous OS call that must not run straight on the async worker (see
/// [`on_blocking`]'s own doc for why). [`resolve_and_verify_target`] itself
/// takes `window_id_out` as `&mut Option<u32>` — a reference into the
/// CALLER's own stack frame, which can't cross into a `'static` blocking
/// closure — so this runs it against a throwaway LOCAL `Option<u32>` inside
/// the closure instead, and writes that back into the real `window_id_out`
/// itself once `on_blocking` returns, preserving `resolve_and_verify_
/// target`'s existing "record the id even when identity verification fails"
/// semantics exactly. A drop-in replacement for every existing call site —
/// same arguments, same `Result`, just `async` now.
async fn resolve_and_verify_target_blocking(
    window_query: &str,
    approved: &Option<ApprovedWindow>,
    window_id_out: &mut Option<u32>,
) -> Result<computer::WindowInfo, String> {
    let window_query = window_query.to_string();
    let approved = approved.clone();
    let (id, result) = on_blocking(move || {
        let mut id = None;
        let result = resolve_and_verify_target(&window_query, &approved, &mut id);
        (id, result)
    })
    .await?;
    *window_id_out = id;
    result
}

/// Map an agent-given screenshot-space coordinate for `w` — the FRESHLY
/// resolved (post-activation) window every coordinate-taking input arm
/// already has on hand — to a physical on-screen point, using `(thread,
/// dir, w.id)`'s most recently recorded screenshot dimensions (issue #160
/// round-11 P1 #D) rather than re-deriving a scale from `w`'s CURRENT size —
/// see `computer::map_screenshot_coord`'s own doc for the resize-drift bug
/// this replaces `computer::map_to_physical` to close.
///
/// Fails CLOSED — a clear, agent-facing error, never a silent fallback to
/// "current size" — when NOTHING is on file for this exact `(thread, dir,
/// w.id)`: an agent that never screenshotted `w.id` this session (or whose
/// last screenshot fell out of `computer::MAX_SHOT_DIMS`'s bound) has no
/// screenshot-space coordinate system to interpret its own `coordinate`
/// against at all — this ALSO happens to enforce "screenshot before you
/// click", a good practice this round is happy to require outright rather
/// than merely encourage.
fn map_input_coord(thread: i32, dir: &str, w: &computer::WindowInfo, cx: u32, cy: u32) -> Result<(i32, i32), String> {
    // issue #160 round-12 P1 #2: `shot_dims_for` (not the old id-only
    // `shot_dims`) also verifies `w`'s CURRENT app+title against whatever was
    // recorded at capture time — an id the OS reused for a different window
    // since that screenshot now reads as no record at all, the same
    // fail-closed message below, rather than a stale hit against the OLD
    // window's saved geometry. See that function's own doc.
    let (shot_w, shot_h) = computer::shot_dims_for(thread, dir, w).ok_or_else(|| {
        format!(
            "no recent screenshot of window {} to map this coordinate against — take a screenshot of \
             it first, then read coordinates off THAT screenshot",
            w.id
        )
    })?;
    computer::map_screenshot_coord(w, shot_w, shot_h, cx, cy).map_err(|e| e.to_string())
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
    resolved: Option<&computer::WindowInfo>,
) -> Option<String> {
    if risk != crate::ask::RiskLevel::Write {
        return None;
    }
    // issue #160 round-20 P1 (Codex computer_srv.rs:1889): REUSE the identity
    // `approve` already resolved once (on the blocking pool — see its
    // `resolved` binding) instead of enumerating windows a SECOND time inline
    // on the async runtime. Under several concurrent grant-less input requests
    // that extra synchronous `xcap` enumeration could occupy every tokio worker
    // before the first card was even admitted, starving the Stop/Escape tasks;
    // reusing the already-resolved window removes it (and closes the last
    // "resolve twice, window swapped in between" gap for the preview). issue
    // #160 round-14 P1: match on the FULL window identity (id + app + title),
    // not the numeric id alone — see [`VerifiedWindowIdentity`]'s own doc for
    // the id-reuse hazard an id-only comparison left open.
    let target = VerifiedWindowIdentity::from_window(resolved?);
    let (data_uri, stored) = last_screenshot_preview(thread, dir)?;
    (stored == target).then_some(data_uri)
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
    if action != "type" && action != "key" {
        return args.clone();
    }
    let mut redacted = args.clone();
    let Some(obj) = redacted.as_object_mut() else {
        return redacted;
    };
    // Only redact when `text` is ACTUALLY present as a string — a malformed
    // call missing it entirely (rejected by `required_text` before it ever
    // reaches the backend, see the "type"/"key" arms of `run_action`) must not
    // have a synthetic `text` key manufactured into its audit record that was
    // never in the real request.
    let Some(text) = obj.get("text").and_then(|v| v.as_str()) else {
        return redacted;
    };
    // `type` always redacts (bulk keystrokes are content). issue #160 round-20
    // (Codex computer_srv.rs:1475): `key` redacts ONLY a BARE single printable
    // character — the sensitive char-by-char case `pure_validate`/
    // `reject_unsafe_key_combo` reject; redacting it HERE too means even the
    // rejected attempt's audit line never records the raw character. A real
    // combo (`cmd+s`, `ctrl+c`, `enter`) is NOT content and stays in the audit
    // for forensics.
    let redact = action == "type"
        || matches!(
            computer::parse_key_combo(text).as_deref(),
            Ok([computer::KeyToken::Unicode(_)])
        );
    if redact {
        let chars = text.chars().count();
        obj.insert("text".to_string(), json!({ "text_redacted": true, "text_chars": chars }));
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
/// when" registry — see this section's own doc comment.
/// issue #160 round-4 P1 §2: no longer carries the click's physical
/// `(px, py)` — that existed ONLY to feed the round-3 P1 §1 replay-click
/// hack this round removes (see this section's own top doc comment); the
/// window id + timestamp pair is all [`require_recent_focus`] itself has
/// ever needed.
///
/// issue #160 round-15 P2 (Codex computer_srv.rs:1707): the timestamp is a
/// monotonic [`std::time::Instant`] now, NOT wall-clock `now_ms()`. This used
/// to reason "a system clock adjustment mid-session is not a hazard this
/// heuristic needs to defend against" — wrong for a PERMISSION-adjacent gate:
/// with wall clock, a backward clock step (manual correction, NTP sync) made
/// `now_ms().saturating_sub(ts)` read 0, so ONE old click stayed "fresh" for
/// as long as the rollback was large — letting `type`/`key` bypass the
/// documented 15-second click requirement for minutes or hours. `Instant` is
/// process-local and monotonic, which is exactly what this check is: a
/// process-local freshness heuristic. (The audit log's `ts_ms` stays wall
/// clock — that's a human-readable record, not a gate.)
/// issue #160 round-17 P2 (Codex computer_srv.rs:1943): the value carries the
/// clicked window's FULL [`VerifiedWindowIdentity`] now, not its bare id — an
/// id reused within the 15s freshness window used to let the REPLACEMENT
/// window read as "recently clicked", and under a Full grant a `type`/`key`
/// would then activate and inject into it with neither a card nor a genuine
/// click. Identity comparison fails that closed, same as the preview and
/// shot-dims registries already do.
fn recent_clicks() -> &'static Mutex<HashMap<(i32, String), (VerifiedWindowIdentity, std::time::Instant)>> {
    static CLICKS: OnceLock<
        Mutex<HashMap<(i32, String), (VerifiedWindowIdentity, std::time::Instant)>>,
    > = OnceLock::new();
    CLICKS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Record a SUCCESSFUL click on `window_id` for `(thread, dir)` — called
/// ONLY from the click-family arm of [`run_action`], and ONLY after the
/// backend call itself returned `Ok`: a rejected/failed click never actually
/// touched the real window, so it must not seed a false freshness record for
/// a later `type`/`key`.
fn record_click_focus(thread: i32, dir: &str, w: &computer::WindowInfo) {
    let mut g = recent_clicks().lock().unwrap_or_else(|e| e.into_inner());
    g.insert(
        (thread, dir.to_string()),
        (VerifiedWindowIdentity::from_window(w), std::time::Instant::now()),
    );
}

/// `type`/`key`'s pre-execution gate: reject unless a click on THIS EXACT
/// resolved `window_id`, for THIS `(thread, dir)`, landed within the last
/// [`FOCUS_FRESHNESS_MS`] — see this section's own doc comment for what this
/// is (and is not) verifying.
fn require_recent_focus(thread: i32, dir: &str, w: &computer::WindowInfo) -> Result<(), String> {
    let g = recent_clicks().lock().unwrap_or_else(|e| e.into_inner());
    let target = VerifiedWindowIdentity::from_window(w);
    // issue #160 round-15 P2: `Instant::elapsed` is monotonic — a wall-clock
    // rollback can no longer stretch one click's freshness window. issue #160
    // round-17 P2: the FULL identity must match, not just the id (see
    // [`recent_clicks`]'s own doc for the id-reuse hazard).
    let fresh = matches!(
        g.get(&(thread, dir.to_string())),
        Some((clicked, ts)) if *clicked == target
            && ts.elapsed() <= std::time::Duration::from_millis(FOCUS_FRESHNESS_MS)
    );
    if fresh {
        return Ok(());
    }
    let window_id = w.id;
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

/// issue #160 round-12 P1 #5: process-wide cap on CONCURRENT screenshot
/// capture+encode. A `screenshot` call synchronously captures a full RGBA
/// frame, PNG-encodes it to disk, then may ALSO JPEG-encode it up to twice
/// more (the preview thumbnail above, and — engine-gated — the MCP inline
/// image) — with no throttle/semaphore/flight-guard anywhere on this path
/// before this round. Full access or a matching Always grant lets a worker
/// fire arbitrarily many concurrent `screenshot` calls, each one holding its
/// own full-resolution RGBA buffer (tens of MB for a 4K display) plus
/// PNG/JPEG encode buffers, all resident in memory at once — a
/// straightforward memory/thread-exhaustion vector.
///
/// `N = 2`: generous enough that one legitimate in-flight capture never
/// queues behind an unrelated session's for long (this is an occasional,
/// human-paced action, not a hot loop), small enough that the worst case is
/// bounded to a couple of full-frame buffers rather than however many
/// concurrent MCP calls a worker cares to fire. A queued caller beyond the
/// cap waits its turn rather than allocating its own buffers alongside the
/// others.
const SCREENSHOT_CONCURRENCY: usize = 2;

/// The semaphore [`SCREENSHOT_CONCURRENCY`] backs — acquired by the
/// `screenshot` arm of `run_action` immediately before the synchronous
/// capture, held across every encode that same call does, released when
/// that arm's block ends (including on an early `?` return from a failed
/// capture — Rust drops the permit as part of normal scope unwinding, no
/// manual guard-drop needed).
///
/// issue #160 round-16 P1 (Codex 605): the `list_windows` arm of `run_action`
/// now ALSO acquires this same semaphore, before its own enumeration —
/// deliberately sharing this one "OS observation" budget rather than minting
/// a second, separate one. Round-15 P2's `MAX_OPEN_OBSERVE_ASKS` only bounds
/// how many observe Ask CARDS may sit open waiting on a human; a session
/// already holding a standing Full/Always grant skips card-opening entirely,
/// and until this round had NO concurrency cap at all on the OS calls that
/// path could fire. This is the hard ceiling for that already-authorized
/// case — `MAX_OPEN_OBSERVE_ASKS` and this semaphore guard two different
/// stages of the same lifecycle (before a grant exists, and after one does).
fn screenshot_semaphore() -> &'static tokio::sync::Semaphore {
    static SEM: OnceLock<tokio::sync::Semaphore> = OnceLock::new();
    SEM.get_or_init(|| tokio::sync::Semaphore::new(SCREENSHOT_CONCURRENCY))
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
///
/// issue #160 round-2 P2 §7: this used to grow UNBOUNDED for the life of the
/// process — one entry per (thread, dir) that ever screenshotted, never
/// evicted. [`MAX_PREVIEWS`] caps it; the value's third element is the
/// insertion timestamp [`store_screenshot_preview`] needs to find (and evict)
/// the single oldest entry once the map is full — see [`evict_oldest_if_full`].
/// The FULL identity of a window one of this module's per-session registries
/// recorded — `id` ALONE is never enough: an OS window id is reusable, so a
/// closed window's number can be handed to a DIFFERENT window (different
/// app/title) moments later, and an id-only match would silently transfer
/// whatever the registry vouches for onto that replacement. Comparing
/// `app`+`title` alongside `id` fails such a match closed instead — the SAME
/// id-reuse defense `computer::shot_dims_for` applies to recorded screenshot
/// geometry. Two registries key off this:
///  - the Ask-card preview registry (issue #160 round-14 P1, Codex
///    computer_srv.rs:1466) — so a stale preview can't be attached to a card
///    for a window that merely reused the captured window's number;
///  - the click-focus registry (issue #160 round-17 P2, Codex
///    computer_srv.rs:1943) — so a `type`/`key` within the freshness window
///    can't ride a click recorded against a window that closed and had its
///    id reused (under a Full grant that would have meant injecting
///    keystrokes into the replacement with neither a card nor a real click
///    on it).
#[derive(Clone, Debug, PartialEq, Eq)]
struct VerifiedWindowIdentity {
    id: u32,
    app: String,
    title: String,
}

impl VerifiedWindowIdentity {
    fn from_window(w: &computer::WindowInfo) -> Self {
        Self { id: w.id, app: w.app.clone(), title: w.title.clone() }
    }
}

fn screenshot_previews() -> &'static Mutex<HashMap<(i32, String), (String, VerifiedWindowIdentity, u64)>> {
    static PREVIEWS: OnceLock<Mutex<HashMap<(i32, String), (String, VerifiedWindowIdentity, u64)>>> =
        OnceLock::new();
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
fn evict_oldest_if_full(map: &mut HashMap<(i32, String), (String, VerifiedWindowIdentity, u64)>) {
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

fn store_screenshot_preview(thread: i32, dir: &str, preview: String, identity: VerifiedWindowIdentity) {
    let mut g = screenshot_previews().lock().unwrap_or_else(|e| e.into_inner());
    let key = (thread, dir.to_string());
    // Only evict to make room for a genuinely NEW key — refreshing an
    // EXISTING (thread, dir)'s preview (the common case: a session that
    // screenshots repeatedly) must not count against capacity or trigger an
    // eviction of some unrelated session's entry.
    if !g.contains_key(&key) {
        evict_oldest_if_full(&mut g);
    }
    g.insert(key, (preview, identity, now_ms()));
}

/// The most recent screenshot preview (and the window id it came from) for
/// `(thread, dir)`, if any — see [`screenshot_previews`]'s doc. Read only
/// from [`preview_for_action`] within this same module now (the round-2 P1
/// server-side gate owns preview attachment; `bus::server::handle_ask` no
/// longer does — see this module's own top doc comment).
fn last_screenshot_preview(thread: i32, dir: &str) -> Option<(String, VerifiedWindowIdentity)> {
    let g = screenshot_previews().lock().unwrap_or_else(|e| e.into_inner());
    g.get(&(thread, dir.to_string()))
        .map(|(preview, identity, _ts)| (preview.clone(), identity.clone()))
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
/// 2. are we going faster than the global input throttle allows —
///    [`ComputerError::RateLimited`] — see [`acquire_and_throttle`];
/// 3. does someone else hold the control lease — [`ComputerError::Busy`] —
///    also [`acquire_and_throttle`], checked AFTER the throttle (issue #160
///    round-12 P2 #G — see that function's own doc for why the order
///    flipped from the reverse it used to be).
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
///
/// issue #160 round-12 P2 #G: throttle is now checked BEFORE the control
/// lease is (re)acquired — the reverse of this function's own pre-round-12
/// order. Before this, a same-session input call faster than
/// `computer::THROTTLE_MS` apart still ran `acquire_control` FIRST, which
/// unconditionally RENEWS the 30s sliding-window lease (a live same-holder
/// re-acquire renews `expires_at_ms`, per that function's own doc) — only
/// THEN did `throttle_input` reject it. A caller spamming calls faster than
/// the throttle window — each one individually rejected — kept renewing its
/// own lease on every single rejected attempt, so the control lease (and the
/// "an agent is controlling the desktop" banner/OS-level Escape shortcut it
/// keeps registered) could be held open INDEFINITELY by a loop of calls that
/// never actually got to inject anything. `computer::throttle_input` has no
/// side effect on its OWN `Err` path (it only records `now` on success — see
/// that function's own doc), so checking it FIRST costs nothing when it
/// passes, and means a rejected, rate-limited call no longer touches
/// `acquire_control`/the lease at all.
fn acquire_and_throttle(thread: i32, dir: &str) -> Result<(), String> {
    // issue #160 round-13 P2 (Codex computer_srv.rs:1955): a call that is about
    // to be rejected as `Busy` (a DIFFERENT, still-live session holds the
    // control lease) must NOT consume a throttle slot on the way out. The
    // throttle is process-wide and single-slotted; `throttle_input` records
    // `now` on success, so a foreign session polling roughly every
    // `THROTTLE_MS` would pass the throttle, bump that global timestamp, and
    // only THEN hit `Busy` from `acquire_control` — repeatedly, starving the
    // ACTUAL holder's own paced calls into `RateLimited` even though they are
    // ≥ `THROTTLE_MS` apart. Peek the holder first: only a free lease or one
    // this SAME `(thread, dir)` already holds proceeds to the throttle. This
    // keeps round-12 P2 #G intact — a same-holder call that is itself rate-
    // limited still rejects at `throttle_input` WITHOUT renewing its lease,
    // because `acquire_control` only runs after the throttle passes.
    //
    // Residual (accepted, not a starvation vector): the peek and the
    // acquire below are two separate lock acquisitions, so if the lease is free
    // at the peek but a foreign holder wins the race to acquire it in the gap,
    // this one call consumes a throttle slot and then still gets `Busy`. That
    // can happen at most ONCE per lease-free window (the very next call peeks
    // the new foreign holder and bails before the throttle), so it can never
    // become the repeatable starvation the peek closes.
    if let Some(holder) = computer::control_state() {
        if !(holder.thread == thread && holder.dir == dir) {
            return Err(ComputerError::Busy { thread: holder.thread, dir: holder.dir }.to_string());
        }
    }
    computer::throttle_input().map_err(|e| e.to_string())?;
    computer::acquire_control(thread, dir).map_err(|e| e.to_string())?;
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
    // issue #160 round-19 P1 (Codex computer_srv.rs:403): revalidate the session
    // at this post-queue checkpoint, immediately before injection — a thread
    // deleted WHILE this call sat queued on `input_flight_guard` (behind another
    // session's in-flight action) must be caught here, not only at the
    // `handle_computer` entry gate. Gated on the revocation set: a thread that
    // was never deleted (every synthetic-identity test, and normal operation)
    // pays only the lock-only lookup, never the `session_is_live` DB check.
    if computer_routes_revoked(thread) && !session_is_live(db, thread, dir, None).await {
        return Err(SESSION_GONE_MSG.to_string());
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

/// issue #160 round-18 P1 (Codex computer_srv.rs:967): the FINAL, purely
/// SYNCHRONOUS kill-switch + control-lease recheck, run INSIDE an input arm's
/// [`on_blocking`] closure — on the very blocking-pool thread that is about to
/// call the OS injection backend, as its first statement immediately before
/// the backend call.
///
/// Every input arm already clears [`recheck_after_guard`] twice (after the
/// flight guard, and again inside [`activate_and_recheck`] right after
/// activation). But BOTH of those run BEFORE this arm's final
/// `resolve_and_verify_target_blocking` — itself an awaited OS enumeration
/// scheduled onto the blocking pool — and before the injection closure below
/// waits its own turn for a blocking-pool thread. A human hitting Emergency
/// Stop (which trips [`computer::stop_latched`]) or global Escape (which
/// clears the control lease) DURING that final resolve, or while the
/// injection closure sits queued for a blocking thread, would otherwise go
/// unseen and the click/type/key would still reach their real desktop AFTER
/// the kill switch fired. Reading the stop latch and the control-lease holder
/// are both lock-only, no `.await`/no db, so this can run at the last possible
/// instant on the same thread as the backend call, closing that residual gap
/// completely. Callers `?` this at the head of the closure and never fall
/// through to the backend call on an `Err`.
fn recheck_stop_and_lease_before_backend(thread: i32, dir: &str) -> Result<(), String> {
    if computer::stop_latched() {
        return Err(ComputerError::Disabled.to_string());
    }
    // issue #160 round-23 P1 (Codex computer_srv.rs:2608): the "session deleted
    // after recheck_after_guard, while the final resolve/injection was queued"
    // gap is closed at ITS ROOT rather than here — the delete paths now CLEAR
    // the control lease for any route they tear down (see `commands`'
    // `clear_control_if_doomed`), so the lease check below already fails closed
    // for a deleted route. A blanket revocation check HERE would be wrong: this
    // helper is `dir`-blind at the thread level, and `delete_repo` revokes a
    // SURVIVING thread (only one of its directions is gone), so a thread-level
    // refuse would permanently break computer-use for that thread's OTHER
    // directions. Lease-clearing is direction-precise (the lease names exactly
    // one `(thread, dir)`), so it refuses only the torn-down route.
    match computer::control_state() {
        Some(holder) if holder.thread == thread && holder.dir == dir => Ok(()),
        Some(holder) => Err(ComputerError::Busy { thread: holder.thread, dir: holder.dir }.to_string()),
        None => Err(
            "the control lease was lost just before injection (it may have expired, or been \
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
///
/// issue #160 round-16 P1 (Codex 605): `activate_target` — the blocking
/// shell-out this function's own doc above describes — now runs via
/// [`on_blocking`] rather than directly on the async worker (see that
/// helper's own doc for why: a slow/wedged activation call must never risk
/// starving the Stop/Escape kill switch's own scheduling). `recheck_after_
/// guard` stays a plain `.await` on the runtime, unchanged — it only touches
/// `db`/in-memory registries, nothing OS-facing.
async fn activate_and_recheck(
    db: &Db,
    asks: &AskRegistry,
    thread: i32,
    dir: &str,
    window_id: u32,
) -> Result<(), String> {
    on_blocking(move || activate_target(window_id)).await??;
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
    // issue #160 round-14 P2 (Codex computer_srv.rs:2147): distinguish an
    // ABSENT `scroll_amount` (→ the documented default of 3) from one that is
    // PRESENT but not representable as an `i64` (a JSON string like `"30"`, or a
    // number outside `i64`'s range). The old `.and_then(as_i64).unwrap_or(3)`
    // collapsed both into 3, so a malformed value passed `pure_validate`, the
    // approval card rendered the raw requested value, and the backend then
    // scrolled a DIFFERENT distance after the human allowed it. A present-but-
    // invalid value is now rejected outright, before approval.
    let amount = match args.get("scroll_amount") {
        None | Some(Value::Null) => 3,
        Some(v) => match v.as_i64() {
            Some(n) => n,
            None => {
                return Err(
                    "'scroll_amount' must be an integer (a plain JSON number); omit it to use the \
                     default of 3"
                        .to_string(),
                )
            }
        },
    };
    let amount = amount.clamp(0, 30) as i32;
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

/// Every pure, side-effect-free schema check for `action`'s arguments,
/// factored out so [`run_action`] can run them ALL before the approval gate
/// (`approve`) ever opens a Needs-you card (issue #160 round-13 P1, Codex
/// computer_srv.rs:515). A malformed call is rejected here, up front — never
/// after a human answered a card for it, which would mint a standing
/// Always/Full grant for a request that then fails its own arm's parse and
/// never executes. This mirrors, EXACTLY, the pure checks each arm in
/// `run_action`'s `match` already does; the arms keep doing them (idempotent)
/// and additionally do the LIVE checks — window resolution, focus freshness,
/// the control lease — that can only run after approval. An unknown `action`
/// is rejected here too, so it can never reach `approve` and open a card
/// either.
fn pure_validate(action: &str, args: &Value) -> Result<(), String> {
    if !VALID_ACTIONS.iter().any(|a| *a == action) {
        return Err(unknown_action_error(action));
    }
    match action {
        "left_click" | "right_click" | "double_click" | "triple_click" | "mouse_move" => {
            required_window(args)?;
            parse_coordinate(args, "coordinate")?;
        }
        "left_click_drag" => {
            required_window(args)?;
            parse_coordinate(args, "start_coordinate")?;
            parse_coordinate(args, "coordinate")?;
        }
        "scroll" => {
            required_window(args)?;
            parse_coordinate(args, "coordinate")?;
            parse_scroll(args)?;
        }
        "type" => {
            required_window(args)?;
            check_type_length(required_text(args)?)?;
        }
        "key" => {
            required_window(args)?;
            let tokens = computer::parse_key_combo(required_text(args)?).map_err(|e| e.to_string())?;
            reject_unsafe_key_combo(&tokens)?;
        }
        "screenshot" => {
            required_window(args)?;
        }
        "wait" => {
            parse_duration_ms(args)?;
        }
        // list_windows / cursor_position take no arguments to validate.
        _ => {}
    }
    Ok(())
}

/// issue #160 round-20 (Codex computer_srv.rs:1189 + :1475): reject two `key`
/// payloads outright, from [`pure_validate`] — BEFORE the approval card is ever
/// built (so neither reaches the IM bridge) and before any backend work:
///
///  - a BARE printable character (a lone `Unicode` token, no modifier): this is
///    char-by-char TEXT entry, which `type` — not `key` — exists for. Unlike
///    `type`, a `key` payload is NOT redacted on the outbound Lark card or in
///    the durable audit, so routing sensitive text through `key` one character
///    at a time would disclose each character; forcing it onto `type` (which
///    redacts) closes that (round-20 P1, Codex ...:1475).
///  - a BARE `Escape` (no modifier): the process-wide global Escape shortcut is
///    the kill switch's OS-level layer whenever a control lease is held, so an
///    injected bare Escape can be swallowed as Emergency Stop instead of
///    reaching the target window — disabling Computer Use rather than acting on
///    it (round-20 P2, Codex ...:1189). A MODIFIED chord (e.g. `shift+escape`)
///    does not match the bare-Escape shortcut and is deliberately left alone.
fn reject_unsafe_key_combo(tokens: &[computer::KeyToken]) -> Result<(), String> {
    match tokens {
        [computer::KeyToken::Unicode(_)] => Err(
            "send a single printable character with the `type` action, not `key` — `key` is for \
             named keys and modifier shortcuts (e.g. `enter`, `tab`, `ctrl+c`)"
                .to_string(),
        ),
        [computer::KeyToken::Named(computer::NamedKey::Escape)] => Err(
            "`escape` can't be injected through the `key` action — a bare Escape collides with \
             weft's global emergency-stop shortcut and could trip the kill switch instead of \
             reaching the target window"
                .to_string(),
        ),
        _ => Ok(()),
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
    let Some(path) = audit_log_path(db, thread, dir, wt).await else {
        return;
    };
    let Some(parent) = path.parent() else { return };
    if tokio::fs::create_dir_all(parent).await.is_err() {
        return;
    }
    let Ok(line) = audit_line(entry) else { return };
    write_audit_line_locked(&path, &line).await;
}

/// issue #160 round-12 P1 #E: process-wide async lock serializing every
/// "check size → (maybe) rotate → open → append" sequence
/// [`write_audit_line_locked`] performs — the concurrent-writer race this
/// closes: two `tools/call`s finishing at nearly the same moment each
/// independently read the file's size (via [`rotate_audit_if_needed`]) as
/// still under [`MAX_AUDIT_BYTES`] — true at the instant EACH checked, since
/// neither call's check-then-act was synchronized against the other — so
/// BOTH skip rotation and both append; worse, both could decide TO rotate,
/// racing each other's `rename` and losing whichever line landed between
/// the loser's stale size-check and its own now-superseded open. The
/// aggregate can land arbitrarily far past the cap, or a rotation can
/// clobber/lose lines, before any single caller's own check would have
/// caught it.
///
/// A single GLOBAL lock (not keyed per-path) is deliberate: audit logging is
/// best-effort and each write is small (one JSON line), so there is no
/// throughput concern serializing every session's own audit append behind
/// one mutex — and a single lock can never deadlock across two DIFFERENT
/// sessions' paths the way a per-path lock keyed by a fallible hash/pool
/// could. `tokio::sync::Mutex` (not `std::sync::Mutex`): held ACROSS the
/// `.await`s inside [`write_audit_line_locked`] (the file open/write), which
/// a `std::sync::Mutex` guard cannot survive across an `.await` point at
/// all — `tokio::sync::Mutex::lock` never poisons (unlike `std::sync::
/// Mutex`, there is no fallible `.unwrap_or_else(|e| e.into_inner())` needed
/// here).
fn audit_write_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

/// The actual critical section [`append_audit`] serializes on (issue #160
/// round-12 P1 #E) — split out from `append_audit` so a test can drive
/// genuine concurrent calls against a raw path directly, without needing a
/// `Db`/workspace resolution just to reach the race this closes. Holds
/// [`audit_write_lock`] across the WHOLE size-check → rotate → open →
/// append sequence, so two concurrent callers can never interleave their
/// own rotate decisions or race an open against a rotation still in
/// flight — see that lock's own doc for the exact corruption this
/// prevents. Best-effort like every other step here: never blocks or fails
/// the actual tool call; a lock/rotate/open/write failure here just means
/// this one line goes unlogged.
async fn write_audit_line_locked(path: &std::path::Path, line: &str) {
    use tokio::io::AsyncWriteExt;
    let _guard = audit_write_lock().lock().await;
    // issue #160 round-10 P1 #F: rotate BEFORE opening for append — see
    // `rotate_audit_if_needed`'s own doc for the unbounded-growth hazard
    // this closes (a Full/exact-Always-granted agent looping
    // `cursor_position`/`list_windows` with no throttle of its own would
    // otherwise append forever).
    rotate_audit_if_needed(path);
    let Ok(mut file) = open_audit_file_for_append(path).await else {
        return;
    };
    if file.write_all(line.as_bytes()).await.is_ok() {
        // `tokio::fs::File` buffers writes and does NOT flush on drop:
        // dropping the handle right after `write_all` (as the guard releases
        // at the end of this function) can lose the just-written line when its
        // background write op hasn't completed yet — an intermittent
        // single-line loss under concurrent appends (observed as a flaky
        // failure of `concurrent_appends_serialize_through_rotation_...`).
        // Flush inside the critical section so every audit line is durable
        // before the lock releases and the next writer's rotate/size-check
        // runs against the fully-written file.
        let _ = file.flush().await;
    }
}

/// Single-file rotation cap for `computer-audit.jsonl` (issue #160 round-10
/// P1 #F): unlike screenshots ([`prune_old_screenshots`]/
/// [`MAX_RETAINED_SCREENSHOTS`] equivalents live in `computer` — see
/// `computer::screenshot_window`'s own retention cap), the audit log had NO
/// rotation/quota/cleanup at all — every `tools/call`, Full/exact-Always
/// granted or not, appends one more line forever. A looping
/// `cursor_position`/`list_windows` agent (both `ReadOnly`, both throttled
/// only by the general input rate limit which doesn't even apply to
/// observation actions) could grow this file without bound until the
/// human's disk fills up.
const MAX_AUDIT_BYTES: u64 = 5 * 1024 * 1024;

/// Rotate `path` — the audit log's own file — to `<path>.1` (overwriting
/// whatever was previously there) if it is already at/over
/// [`MAX_AUDIT_BYTES`], via [`rotate_audit_at_size`]. Total on-disk usage for
/// this one file stays bounded to roughly `2 * MAX_AUDIT_BYTES`: the live
/// file plus, at most, one rotated `.1`. A more elaborate multi-generation
/// rotation, or a per-session quota, is a follow-up — this round ships the
/// simplest scheme that actually bounds growth.
fn rotate_audit_if_needed(path: &std::path::Path) {
    rotate_audit_at_size(path, MAX_AUDIT_BYTES);
}

/// The actual rotation logic, parameterized on `max_bytes` (issue #160
/// round-10 P1 #F) so a test can drive it against a real, small file without
/// having to write out multiple real megabytes just to exercise the
/// threshold — [`rotate_audit_if_needed`] is the one production caller,
/// always with [`MAX_AUDIT_BYTES`].
///
/// Symlink safety: `path` has ALREADY been walked component-by-component by
/// [`refuse_symlinks`] (via [`audit_log_path`]) before [`append_audit`] ever
/// calls this — but a worktree is repository-controlled content, so the
/// window between that check and THIS call is still open. Re-checks via
/// `symlink_metadata` (never plain `metadata`) right here, immediately
/// before the `rename`: if `path` is no longer a REGULAR file at all (a
/// symlink swapped into place in that gap) rotation is skipped outright
/// rather than renaming something that may not mean what the size check
/// assumed; the `.1` destination gets the identical check before anything is
/// renamed onto it, so this never renames a legitimate file over — or reads
/// the size of — a symlink planted at either path.
///
/// Best-effort, like every other step [`append_audit`] takes: ANY failure
/// here (a metadata read, the rename itself) silently skips rotation for
/// this one call — audit logging is best-effort end to end, and a rotation
/// hiccup must never turn into a dropped/corrupted audit line, let alone a
/// failed tool call.
fn rotate_audit_at_size(path: &std::path::Path, max_bytes: u64) {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return; // doesn't exist yet, or unreadable — nothing to rotate
    };
    if !meta.file_type().is_file() || meta.len() < max_bytes {
        return;
    }
    let mut rotated_name = path.as_os_str().to_os_string();
    rotated_name.push(".1");
    let rotated = std::path::PathBuf::from(rotated_name);
    if let Ok(rotated_meta) = std::fs::symlink_metadata(&rotated) {
        if rotated_meta.file_type().is_symlink() {
            return; // never rename onto a symlinked destination
        }
        // issue #160 round-16 P2 (Codex computer_srv.rs:2631): remove the
        // previous generation EXPLICITLY before the rename. On unix, `rename`
        // replaces an existing destination atomically — but on Windows it
        // FAILS instead, and since rotation is best-effort (the error was
        // ignored), every rotation after the first silently no-op'd and the
        // live log grew without bound. Removing first is portable; the cost is
        // giving up unix's atomic replace, which is fine here: the whole
        // check→rotate→append sequence is already serialized under
        // [`audit_write_lock`], so no concurrent writer can observe (or race)
        // the tiny remove→rename gap, and a crash inside it merely loses the
        // OLD generation — the live log survives, which is the right
        // best-effort trade for an audit trail. The symlink check above stays:
        // a symlink at the destination is never removed OR renamed over.
        let _ = std::fs::remove_file(&rotated);
    }
    let _ = std::fs::rename(path, &rotated);
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
///
/// issue #160 round-10 P2 #3 (Codex 1868): `#[cfg(unix)]` also sets the
/// create mode to `0o600` (owner read/write only). Before this, a fresh
/// `computer-audit.jsonl` was created with whatever `open(2)`'s own default
/// (`0o666`) survives the process umask — `0o644` under the common `022`
/// umask — leaving desktop-activity metadata (target window queries, action
/// arguments, timestamps, outcomes) world/group-readable on a shared
/// machine. The screenshot files themselves were already `0o600` (see
/// `computer::screenshot_window`); this brings the audit log to the SAME
/// owner-only bar. Mode is only consulted by `open(2)` when it actually
/// CREATES a new file (`O_CREAT` with no existing inode) — appending to an
/// already-existing, already-lenient file from before this round keeps
/// whatever mode it already had; only fresh files (and rotated-away originals
/// — see `rotate_if_full`) get the tightened default.
async fn open_audit_file_for_append(path: &std::path::Path) -> std::io::Result<tokio::fs::File> {
    let mut options = tokio::fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW);
    #[cfg(unix)]
    options.mode(0o600);
    // issue #160 round-21 P2 (Codex computer_srv.rs:3052): on Windows, a freshly
    // CREATED audit file (the first append, or the new live file after each
    // rotation) must get the same owner-only protection unix gets from `0o600`
    // above — otherwise it inherits a permissive `WEFT_HOME` directory ACL and
    // exposes window titles/actions/coordinates/outcomes to other local
    // accounts. Detect creation up front so the ACL is applied ONCE (re-stamping
    // it on every append would be pure overhead); on a `try_exists` error, treat
    // it as new and apply anyway (fail toward protecting).
    #[cfg(windows)]
    let is_new = !tokio::fs::try_exists(path).await.unwrap_or(false);
    let file = options.open(path).await?;
    #[cfg(windows)]
    if is_new {
        use std::os::windows::io::AsRawHandle;
        // Best-effort, matching this function's own contract: a failure to lock
        // the file down does not fail the audit append (unlike the secret-config
        // writer, which fails closed — an audit line is not itself a secret).
        let _ = crate::bus::inject::restrict_handle_to_owner(file.as_raw_handle());
    }
    Ok(file)
}

/// The session's own Weft-managed output root for `(thread, dir[, wt])`,
/// before any output-specific suffix is appended — the shared base for
/// [`screenshot_out_dir`] (which appends a DIFFERENT suffix per lane; see
/// its own doc comment) and [`audit_log_path`] (which appends a per-lane
/// suffix too).
///
///  - worker lane (`dir` a direction id): `<weft_home>/computer/<thread>/
///    <dir>/wt-<id>`, `id` being the RESOLVED worktree id (see `wt`'s own doc
///    below for exactly which one). issue #160 round-10 P1 #1 (Codex 1992 +
///    672) moved this OFF the worktree entirely and into Weft's own managed
///    storage — see [`crate::paths::computer_output_root`]'s doc for why: in
///    a LINKED worktree, `git rev-parse --git-path info/exclude` resolves to
///    the CANONICAL repo's own SHARED `.git/info/exclude` (worktrees share
///    one gitdir's `info/` directory), so the OLD `<worktree>/.weft/...`
///    location — once "excluded" via `git::git_exclude(".weft/")` — silently
///    wrote a `.weft/` entry into the user's REAL repo metadata on the very
///    first screenshot/audit line: a hard violation of "never wire Weft's
///    own bookkeeping into a canonical repo". Nothing computer-use writes for
///    the worker lane touches the worktree at all anymore, so that
///    `git_exclude` call is gone too (see [`screenshot_out_dir`]/
///    [`audit_log_path`]) — there is no longer anything to exclude FROM. The
///    `wt-<id>` suffix keeps the SAME per-worktree isolation the old
///    worktree-rooted path got for free (round-2 P2 §5): two worktrees of the
///    same multi-repo direction still never share an output directory,
///    whether `id` came from an EXPLICIT pin or the first-worktree fallback
///    below.
///  - lead lane (`dir == bus::LEAD`): `<weft_home>/computer/<thread>/lead` —
///    issue #160 round-16 P1 (Codex computer_srv.rs:2812). This used to be
///    the lead's own scratch cwd (`<weft_home>/leads/<thread>`), but that
///    directory is a real working dir the LEAD AGENT ITSELF writes into: an
///    agent (or any background process with the same access) could swap the
///    `.weft`/`screenshots` PARENT directories for symlinks in the window
///    between [`refuse_symlinks`]'s component walk and the later
///    `create_dir_all`/open — the writers only apply `O_NOFOLLOW` to the
///    LEAF, so a swapped parent would be silently followed and Weft (with its
///    own privileges, not the sandboxed agent's) would write outside the
///    session directory. Rather than re-plumbing every writer onto openat-
///    style verified directory handles, the lead's output moves under the
///    SAME Weft-managed root the worker lane already uses — a tree Weft
///    creates for itself and never hands to any agent as a working
///    directory — which removes the agent-writable-parent premise of the
///    race by construction (the same reasoning round-10 P1 #1 used to move
///    the WORKER lane off the worktree). `refuse_symlinks` stays as defense
///    in depth on both lanes. Old audit/screenshot files left in the scratch
///    cwd from before this round are simply orphaned (best-effort logs; never
///    read back).
///
/// `wt` (issue #160 round-2 P2 §5): the CALLER's own worktree id, when it
/// could resolve one — see `inject::computer_url`'s doc for who sets this
/// and why (a multi-repo direction has MORE THAN ONE worktree row, and
/// without this every worker sharing that direction fell back to whichever
/// one happened to be first — screenshots/audit for a worker in repo B could
/// silently land in repo A's namespace instead). CLOSED-SET validated here:
/// `Some(id)` is only honored when `id` names a worktree that actually
/// belongs to THIS direction (which is itself already confirmed to belong to
/// THIS thread, below) — a forged/foreign worktree id can never redirect
/// output into another direction's (or another thread's) namespace.
///
/// issue #160 round-8 P2 #7: an EXPLICIT `wt` that fails that check is now
/// FAIL-CLOSED — `None` — rather than silently falling back to "the first
/// worktree of this direction" as it used to. That old fallback was fine for
/// an ABSENT `wt` (there was never a specific worktree pinned to begin with),
/// but for an EXPLICIT-but-invalid one (deleted, or naming a worktree of a
/// DIFFERENT direction/repo) it actively misdirected output: in a multi-repo
/// direction, a worker session that explicitly pinned worktree B and lost the
/// race against a deletion (or was handed a forged/stale id) would have had
/// its screenshots and audit log silently written into worktree A's
/// namespace instead — exposing that OTHER repo's on-screen pixels and
/// activity to the wrong worker. Only a genuinely ABSENT `wt` (`None`) still
/// falls back to "first worktree for this direction"; an explicit id that
/// doesn't resolve now fails the whole call closed instead of picking a
/// different worktree on its own. issue #160 round-10 P2 #2 ([`WtParam`])
/// extends the SAME fail-closed rule one parse step earlier, for a `?wt=`
/// that isn't even numeric — that rejection happens in `handle_computer`,
/// before this function (or any of its callers) is ever reached.
///
/// `None` on any failure (DB error, unresolvable path, no worktree at all
/// for this direction, a numeric `dir` that doesn't resolve to a direction
/// belonging to THIS thread, or — per round-8 P2 #7 above — an EXPLICIT `wt`
/// that doesn't name a worktree of this direction) — callers turn that into
/// their own soft-failure text rather than a 500.
async fn session_root(db: &Db, thread: i32, dir: &str, wt: Option<i32>) -> Option<std::path::PathBuf> {
    if dir == crate::bus::LEAD {
        // issue #160 round-16 P1: Weft-managed, never agent-writable — see
        // this function's own lead-lane doc above.
        let root = crate::paths::computer_output_root()
            .ok()?
            .join(thread.to_string())
            .join(crate::bus::LEAD);
        return Some(root);
    }
    let direction_id = dir.parse::<i32>().ok()?;
    match repo::get_direction(db, direction_id).await {
        Ok(Some(d)) if d.thread_id == thread => {}
        _ => return None,
    }
    let worktrees = repo::list_worktrees(db, Some(direction_id)).await.ok()?;
    // round-8 P2 #7: an EXPLICIT pin must hit an actual worktree of THIS
    // direction, or the whole call fails closed — silently falling back to
    // the first worktree would write this session's screenshots/audit into a
    // DIFFERENT repo's namespace in a multi-repo direction. An ABSENT `wt`
    // still falls back to the first worktree, same as always.
    let resolved_id = match wt {
        Some(id) => worktrees.iter().find(|w| w.id == id).map(|w| w.id),
        None => worktrees.first().map(|w| w.id),
    }?;
    let root = crate::paths::computer_output_root()
        .ok()?
        .join(thread.to_string())
        .join(dir)
        .join(format!("wt-{resolved_id}"));
    Some(root)
}

/// issue #160 round-18 P2 (Codex paths.rs:89): remove a thread's ENTIRE
/// computer-use output subtree — `<weft_home>/computer/<thread>/`, which holds
/// every lead/worker session's screenshots and rotated audit logs (see
/// [`session_root`] for the layout underneath it) — as part of deleting the
/// thread. Without this, the persistent per-thread tree these actions write
/// outlives its thread indefinitely: the per-session retention limits only
/// bound a LIVE session's own growth, and a deleted thread's sessions never
/// run again to enforce anything, so repeatedly creating, using, and deleting
/// threads would grow this directory without bound.
///
/// Best-effort: a failed/absent delete must never block the thread cascade
/// (mirrors `materialize::cleanup_worktrees`'s own best-effort tail). Bounded
/// to this thread's own subtree only — the single `join` is on `thread`, a
/// plain integer with no path-separator/`..` component, so there is nothing to
/// escape [`crate::paths::computer_output_root`] with.
pub(crate) fn remove_computer_output_for_thread(thread: i32) {
    let Ok(root) = crate::paths::computer_output_root() else {
        return;
    };
    let dir = root.join(thread.to_string());
    if dir.exists() {
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// issue #160 round-19 P1 (Codex computer_srv.rs:403): does `(thread, dir, wt)`
/// STILL denote a live session? The per-session bearer stays valid for the
/// whole process lifetime and each Axum request is independent of the engine,
/// so a thread/direction deleted AFTER the token was minted — `delete_thread_
/// cascade` purges asks/grants and stops engines, but can neither revoke an
/// already-minted token nor abort an in-flight HTTP call — must still be
/// refused before any OS work, or a stale caller could keep driving (or newly
/// submit calls against) the human's desktop under a deleted identity.
///
/// Mirrors [`session_root`]'s OWN DB validation: a worker lane's `dir` must
/// resolve to a direction of THIS exact thread and, if a worktree is pinned,
/// that worktree must belong to the direction; the lead lane requires only that
/// the thread row still exists. Fail-CLOSED — any DB error resolves to `false`
/// (not live), so a transient store hiccup can never fail OPEN into driving a
/// desktop whose owning session may already be gone.
/// The one rendering of the "session deleted" refusal (issue #160 round-19 P1),
/// used by the [`recheck_after_guard`] post-queue revalidation.
const SESSION_GONE_MSG: &str =
    "this computer-use session no longer exists (its issue or direction was deleted) — refused";

/// Process-global set of thread ids whose computer-use routes were REVOKED by a
/// thread deletion (issue #160 round-19 P1, Codex computer_srv.rs:403).
/// `commands::delete_thread` records a thread here (via [`revoke_computer_
/// routes`]) as part of its cascade; the [`handle_computer`] entry gate and the
/// [`recheck_after_guard`] pre-injection revalidation consult it.
///
/// Why a revocation SET rather than a blanket DB-liveness check on every
/// request: the per-session bearer is a process-lifetime HMAC the delete path
/// can't rescind, and the Axum request is independent of the engine it stops,
/// so a token minted before a delete stays cryptographically valid. Recording
/// the deletion here revokes those tokens WITHOUT a DB round-trip on the
/// overwhelmingly common live-session request — and without coupling every
/// request (or the many synthetic-identity tests, which deliberately never set
/// up matching DB rows) to a DB shape. Only a thread that was actually deleted
/// is ever in this set, and such a request then pays ONE [`session_is_live`]
/// check to tell a genuinely-deleted thread (refuse) from a REUSED thread id (a
/// brand-new thread that happens to reuse the number — allow), so id reuse can
/// never permanently strand a fresh session.
fn revoked_computer_threads() -> &'static std::sync::Mutex<std::collections::HashSet<i32>> {
    static SET: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<i32>>> =
        std::sync::OnceLock::new();
    SET.get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()))
}

/// Revoke every computer-use route for `thread` (issue #160 round-19 P1) — see
/// [`revoked_computer_threads`]. Called from `commands::delete_thread`.
pub(crate) fn revoke_computer_routes(thread: i32) {
    revoked_computer_threads()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(thread);
}

/// Undo a [`revoke_computer_routes`] — issue #160 round-23 P1 (Codex
/// commands.rs:756). The delete paths now publish the revocation BEFORE their
/// first destructive await; if the cascade then FAILS the owning rows still
/// exist, so the thread must be un-revoked — otherwise the final
/// [`recheck_stop_and_lease_before_backend`] guard (which refuses ANY revoked
/// thread) would fail-close that still-live session's input forever.
pub(crate) fn unrevoke_computer_routes(thread: i32) {
    revoked_computer_threads()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&thread);
}

/// Whether `thread`'s routes were revoked by a delete — a lock-only set lookup,
/// `false` for every thread that was never deleted (so live sessions and the
/// synthetic-identity tests never reach the [`session_is_live`] DB check).
fn computer_routes_revoked(thread: i32) -> bool {
    revoked_computer_threads()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains(&thread)
}

async fn session_is_live(db: &Db, thread: i32, dir: &str, wt: Option<i32>) -> bool {
    if dir == crate::bus::LEAD {
        return matches!(repo::get_thread(db, thread).await, Ok(Some(_)));
    }
    let Ok(direction_id) = dir.parse::<i32>() else {
        return false;
    };
    match repo::get_direction(db, direction_id).await {
        Ok(Some(d)) if d.thread_id == thread => {}
        _ => return false,
    }
    let Some(wt_id) = wt else {
        return true;
    };
    matches!(
        repo::list_worktrees(db, Some(direction_id)).await,
        Ok(wts) if wts.iter().any(|w| w.id == wt_id)
    )
}

/// Build `base/components[0]/components[1]/...`, refusing if ANY existing
/// path component along the way is a symlink (issue #160 round-2 P2 §3).
/// Originally guarded a worktree's own `.weft` subtree (repository-controlled
/// content a sandboxed agent's own approved writes could tamper with);
/// round-10 P1 #1 moved the worker lane's own `base` off the worktree
/// entirely and into a Weft-owned directory under `weft_home` (see
/// [`session_root`]'s own doc) — that directory is created by Weft itself,
/// never handed to a sandboxed agent to write into directly, so the
/// worktree-tampering scenario this originally guarded against is largely
/// closed by construction now. Kept anyway as defense in depth (a SEPARATE,
/// same-uid process on the human's own machine could still reach into
/// `weft_home` — a residual risk noted here, not fixed by this round) and
/// because the lead lane's `base` (`<weft_home>/leads/<thread>`) is still a
/// real, git-init'd working directory other tooling can touch.
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
///  - worker lane: `<session_root>/screenshots` — a dedicated, Weft-managed
///    directory (see [`session_root`]'s own doc for the round-10 P1 #1 move
///    off the worktree). No `git::git_exclude` call anymore: this directory
///    was never inside a git-tracked worktree to begin with, so there is
///    nothing left to exclude FROM (and nothing left that could leak a
///    `.weft/` entry into a canonical repo's `info/exclude`).
///  - lead lane: `<session_root>/screenshots` too — issue #160 round-15 P2
///    (Codex computer_srv.rs:2759) first gave leads a dedicated subdirectory
///    (so retention pruning never operates over unrelated files), and
///    round-16 P1 (Codex computer_srv.rs:2812) then moved the lead's WHOLE
///    session root under Weft-managed storage (see [`session_root`]'s
///    lead-lane doc for the agent-writable-parent race that closed) — at
///    which point the lead lane needs no special `.weft/` layer anymore and
///    both lanes share ONE shape.
///
/// `Err` (not silently `None`) on a resolution failure OR a refused symlink
/// (issue #160 round-2 P2 §3, via [`refuse_symlinks`]) — callers surface the
/// SPECIFIC reason (missing worktree vs. a compromised output path) to the
/// calling agent rather than one flattened "no worktree" text for both.
async fn screenshot_out_dir(db: &Db, thread: i32, dir: &str, wt: Option<i32>) -> Result<std::path::PathBuf, String> {
    let root = session_root(db, thread, dir, wt)
        .await
        .ok_or_else(|| "no worktree for this session".to_string())?;
    refuse_symlinks(&root, &["screenshots"])
}

/// Resolve the audit log path for `(thread, dir[, wt])`:
///  - worker lane: `<session_root>/computer-audit.jsonl` — directly under the
///    dedicated per-(thread, dir, wt) directory [`session_root`] resolves
///    (issue #160 round-10 P1 #1); no `.weft/` layer needed since the whole
///    directory is already private to this one session, and no
///    `git::git_exclude` call anymore for the SAME reason [`screenshot_out_dir`]
///    no longer needs one.
///  - lead lane: `<session_root>/computer-audit.jsonl` too — issue #160
///    round-16 P1 (Codex computer_srv.rs:2812) moved the lead's session root
///    under Weft-managed storage (see [`session_root`]'s lead-lane doc), so
///    the `.weft/` layer its scratch-cwd audit log used to hide behind is no
///    longer needed: the whole directory is private to this one session, same
///    as the worker lane, and both lanes share ONE shape.
///
/// `None` (best-effort, per [`append_audit`]'s own doc) on a resolution
/// failure OR a refused symlink (issue #160 round-2 P2 §3, via
/// [`refuse_symlinks`]) — a compromised output path just means this one call
/// goes unlogged, same as any other audit-write failure.
async fn audit_log_path(db: &Db, thread: i32, dir: &str, wt: Option<i32>) -> Option<std::path::PathBuf> {
    let root = session_root(db, thread, dir, wt).await?;
    refuse_symlinks(&root, &["computer-audit.jsonl"]).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    // —— issue #160 round-12 P2 #7: always-inject + server-side disabled gate ——

    /// The static tool description must itself say this needs enabling in
    /// Settings — since round-12 P2 #7 makes injection unconditional
    /// (`weft_computer` is now handed to every issue-lead/worker engine
    /// regardless of the setting), an agent must not have to guess why every
    /// call comes back `disabled` the first time it tries.
    #[test]
    fn tool_description_says_it_needs_enabling_in_settings() {
        let specs = computer_tool_specs();
        let desc = specs[0]["description"].as_str().unwrap_or_default();
        assert!(
            desc.to_lowercase().contains("settings"),
            "the tool description must mention enabling this in Settings: {desc}"
        );
    }

    // —— issue #160 round-10 P2 #2: `?wt=` three-state parsing ——

    #[test]
    fn wt_param_parse_distinguishes_absent_explicit_and_invalid() {
        let mut q = HashMap::new();
        assert!(matches!(WtParam::parse(&q), WtParam::Absent), "no wt key at all");

        q.insert("wt".to_string(), "42".to_string());
        assert!(matches!(WtParam::parse(&q), WtParam::Explicit(42)));

        q.insert("wt".to_string(), "abc".to_string());
        assert!(matches!(WtParam::parse(&q), WtParam::Invalid), "non-numeric");

        q.insert("wt".to_string(), String::new());
        assert!(matches!(WtParam::parse(&q), WtParam::Invalid), "present but empty");

        q.insert("wt".to_string(), "-7".to_string());
        assert!(
            matches!(WtParam::parse(&q), WtParam::Explicit(-7)),
            "a negative number still parses as i32 — session_root's own closed-set check is what \
             ultimately rejects an id that doesn't name a real worktree, not this parse step"
        );
    }

    #[test]
    fn wt_param_resolve_only_invalid_rejects_absent_and_explicit_pass_through() {
        assert_eq!(WtParam::Absent.resolve(), Ok(None));
        assert_eq!(WtParam::Explicit(5).resolve(), Ok(Some(5)));
        assert!(WtParam::Invalid.resolve().is_err());
    }

    /// issue #160 round-13/14 P1 (Codex computer_srv.rs:214 + inject.rs:483):
    /// the bearer binds the EXACT worktree, not just `(thread, dir)`. A token
    /// minted for one `wt` must NOT verify for a sibling `wt` (the hijack), for
    /// the absent case, or for a different `(thread, dir)`; the absent token is
    /// symmetric — it must not verify for any explicit `wt`.
    #[test]
    fn computer_token_binds_to_the_worktree_not_just_thread_dir() {
        let t7 = computer_session_token(1, "10", Some(7));
        assert!(verify_computer_token(1, "10", Some(7), &t7), "the exact wt must verify");
        assert!(
            !verify_computer_token(1, "10", Some(8), &t7),
            "a SIBLING worktree id must not verify a token minted for a different one"
        );
        assert!(
            !verify_computer_token(1, "10", None, &t7),
            "the absent-worktree case must not verify a token minted for an explicit wt"
        );
        assert!(!verify_computer_token(2, "10", Some(7), &t7), "still bound to thread");
        assert!(!verify_computer_token(1, "20", Some(7), &t7), "still bound to dir");

        let t_none = computer_session_token(1, "10", None);
        assert!(verify_computer_token(1, "10", None, &t_none), "the absent case must verify itself");
        assert!(
            !verify_computer_token(1, "10", Some(7), &t_none),
            "an explicit wt must not verify a token minted for the absent case"
        );
        // The `none` marker for the absent case can never collide with any
        // explicit id's `wt<id>` representation.
        assert_ne!(t7, t_none);
    }

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

    /// issue #160 round-14 P2 (Codex computer_srv.rs:2147): an ABSENT
    /// `scroll_amount` defaults to 3, but a PRESENT-but-invalid one (a JSON
    /// string, or a number outside `i64`) is REJECTED — never silently
    /// substituted with 3, which would let the approval card and the actual
    /// scroll distance disagree.
    #[test]
    fn parse_scroll_rejects_a_present_but_invalid_amount_but_defaults_when_absent() {
        // Absent → default 3 (unchanged).
        assert_eq!(parse_scroll(&json!({"scroll_direction": "down"})).unwrap(), (0, 3));
        assert_eq!(
            parse_scroll(&json!({"scroll_direction": "down", "scroll_amount": null})).unwrap(),
            (0, 3),
            "an explicit null is treated as absent → default"
        );
        // Present but not an i64 → rejected, not silently defaulted.
        assert!(
            parse_scroll(&json!({"scroll_direction": "down", "scroll_amount": "30"})).is_err(),
            "a string scroll_amount must be rejected, not defaulted to 3"
        );
        assert!(
            parse_scroll(&json!({"scroll_direction": "down", "scroll_amount": 9_999_999_999_999_999_999u64}))
                .is_err(),
            "a number above i64::MAX must be rejected, not defaulted to 3"
        );
        // A valid integer still parses (and clamps) as before.
        assert_eq!(parse_scroll(&json!({"scroll_direction": "down", "scroll_amount": 5})).unwrap(), (0, 5));
    }

    /// issue #160 round-13 P2 (Codex computer_srv.rs:1955): a call rejected as
    /// `Busy` by a DIFFERENT live holder must NOT consume a throttle slot on
    /// the way out — otherwise a foreign session polling every `THROTTLE_MS`
    /// keeps bumping the global throttle and starves the real holder.
    #[test]
    fn acquire_and_throttle_leaves_the_throttle_untouched_on_a_foreign_busy() {
        let _guard = computer::process_state_test_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        computer::clear_control();
        // Let any throttle timestamp a previous test left behind age out past
        // the window, so the assertion below is about THIS call alone.
        std::thread::sleep(std::time::Duration::from_millis(600));

        // A DIFFERENT session holds the live control lease.
        computer::acquire_control(999, "foreign").unwrap();

        // Our call is rejected Busy...
        let err = acquire_and_throttle(1, "10").unwrap_err();
        assert!(err.contains("controlling the desktop"), "expected a Busy rejection, got: {err}");

        // ...and it must have left the throttle untouched: the real holder's
        // very next input can still pace immediately (a consumed slot would
        // make this `Err(RateLimited)`).
        assert!(
            computer::throttle_input().is_ok(),
            "a foreign-holder Busy must not consume a throttle slot"
        );

        computer::clear_control();
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
    fn redact_audit_args_leaves_key_combos_and_other_actions_untouched() {
        // A real `key` COMBO is a shortcut, not content — it stays in the audit
        // for forensics (issue #160 round-20 only redacts a BARE printable key).
        let combo = json!({"action": "key", "window": "notes", "text": "cmd+s"});
        assert_eq!(redact_audit_args("key", &combo), combo, "a key combo is not redacted");
        // A non-type/non-key action is passed through wholesale.
        let click = json!({"action": "left_click", "window": "notes", "coordinate": [1, 2]});
        assert_eq!(redact_audit_args("left_click", &click), click, "other actions untouched");
    }

    /// issue #160 round-20 (Codex computer_srv.rs:1475): a BARE printable `key`
    /// payload (the char-by-char text-entry case `pure_validate` rejects) is
    /// redacted in the durable audit too, so even the rejected attempt records
    /// no raw character.
    #[test]
    fn redact_audit_args_redacts_a_bare_printable_key() {
        let args = json!({"action": "key", "window": "notes", "text": "h"});
        let redacted = redact_audit_args("key", &args);
        assert_eq!(redacted["text"]["text_redacted"], true);
        assert_eq!(redacted["text"]["text_chars"], 1);
        assert_eq!(redacted["window"], "notes", "non-text keys pass through");
    }

    /// issue #160 round-20 (Codex computer_srv.rs:1189 + :1475): the `key`
    /// action rejects a bare printable character (use `type`) and a bare Escape
    /// (kill-switch collision), but still accepts named keys and modifier chords.
    #[test]
    fn pure_validate_rejects_bare_printable_and_bare_escape_key() {
        let key = |text: &str| json!({"action": "key", "window": "notes", "text": text});
        assert!(pure_validate("key", &key("a")).is_err(), "a bare printable char is rejected");
        assert!(pure_validate("key", &key("Escape")).is_err(), "a bare Escape is rejected");
        assert!(pure_validate("key", &key("esc")).is_err(), "the `esc` alias is rejected too");
        // Named keys, modifier chords, and modified-Escape chords still pass.
        assert!(pure_validate("key", &key("enter")).is_ok());
        assert!(pure_validate("key", &key("ctrl+c")).is_ok());
        assert!(pure_validate("key", &key("shift+escape")).is_ok());
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
        // issue #160 round-10 P1 #A note: `approve` now resolves a Write
        // action's window AUTHORITATIVELY before it even checks a standing
        // grant (so it can fold the identity into `action_key`) — this needs
        // a resolvable "notes" window (`shared_mock`, under
        // `process_state_test_lock`) so that early resolve itself succeeds,
        // rather than failing before a card is ever created.
        let _guard = computer::process_state_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        let mock = shared_mock();
        mock.fail_activate.store(false, std::sync::atomic::Ordering::SeqCst);
        *mock.on_activate.lock().unwrap_or_else(|e| e.into_inner()) = None;
        mock.actions.lock().unwrap_or_else(|e| e.into_inner()).clear();
        *mock.windows_override.lock().unwrap_or_else(|e| e.into_inner()) = Some(vec![computer::WindowInfo {
            id: 907_001,
            app: "Notes".into(),
            title: "notes".into(),
            x: 0,
            y: 0,
            width: 800,
            height: 600,
        }]);

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
        // issue #160 round-10 P1 #A note: see the matching comment in
        // `approve_sets_detail_redacted_for_type_but_keeps_the_local_detail_raw`
        // above — `left_click` is Write-classified too, so it needs the same
        // resolvable-window setup for `approve`'s own early resolve.
        let _guard = computer::process_state_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        let mock = shared_mock();
        mock.fail_activate.store(false, std::sync::atomic::Ordering::SeqCst);
        *mock.on_activate.lock().unwrap_or_else(|e| e.into_inner()) = None;
        mock.actions.lock().unwrap_or_else(|e| e.into_inner()).clear();
        *mock.windows_override.lock().unwrap_or_else(|e| e.into_inner()) = Some(vec![computer::WindowInfo {
            id: 907_002,
            app: "Notes".into(),
            title: "notes".into(),
            x: 0,
            y: 0,
            width: 800,
            height: 600,
        }]);

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
        // issue #160 round-11 P1 #C note: `approve` now resolves a
        // `screenshot`'s window authoritatively too (not just Write actions),
        // even on the path to opening a card — this needs a resolvable
        // "notes" window (`shared_mock`, under `process_state_test_lock`) so
        // that resolve itself succeeds, rather than failing before a card is
        // ever created.
        let _guard = computer::process_state_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        let mock = shared_mock();
        mock.fail_activate.store(false, std::sync::atomic::Ordering::SeqCst);
        *mock.on_activate.lock().unwrap_or_else(|e| e.into_inner()) = None;
        mock.actions.lock().unwrap_or_else(|e| e.into_inner()).clear();
        *mock.windows_override.lock().unwrap_or_else(|e| e.into_inner()) = Some(vec![computer::WindowInfo {
            id: 908_001,
            app: "Notes".into(),
            title: "notes".into(),
            x: 0,
            y: 0,
            width: 800,
            height: 600,
        }]);

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
        // issue #160 round-11 P1 #C: `screenshot` now resolves its window
        // authoritatively too, folding `id`/`app`/`title` into the key
        // (issue #160 round-11 P1 #B) exactly like a Write action's key —
        // this pre-seeded Always grant must be built the SAME way `approve`
        // itself now would, or it silently misses and this call hangs
        // waiting on a card nobody is spawned to answer.
        let _guard = computer::process_state_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        let mock = shared_mock();
        mock.fail_activate.store(false, std::sync::atomic::Ordering::SeqCst);
        *mock.on_activate.lock().unwrap_or_else(|e| e.into_inner()) = None;
        mock.actions.lock().unwrap_or_else(|e| e.into_inner()).clear();
        *mock.windows_override.lock().unwrap_or_else(|e| e.into_inner()) = Some(vec![computer::WindowInfo {
            id: 908_003,
            app: "Notes".into(),
            title: "notes".into(),
            x: 0,
            y: 0,
            width: 800,
            height: 600,
        }]);

        let asks = AskRegistry::new();
        let thread = 908_003;
        let dir = "lead";
        let args = json!({"action": "screenshot", "window": "notes"});
        let action_key = crate::ask::action_key(&[
            "gui",
            "screenshot",
            "notes",
            "908003",
            "Notes",
            "notes",
            &args_digest(&args),
        ]);
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

    /// A minimal window fixture for the focus tests — round-17 P2 made the
    /// registry identity-keyed, so the tests pass full `WindowInfo`s now.
    fn focus_win(id: u32) -> computer::WindowInfo {
        computer::WindowInfo {
            id,
            app: "Notes".into(),
            title: "notes".into(),
            x: 0,
            y: 0,
            width: 800,
            height: 600,
        }
    }

    #[test]
    fn require_recent_focus_passes_right_after_a_click_on_the_same_window() {
        let thread = 900_001;
        record_click_focus(thread, "lead", &focus_win(7));
        assert!(require_recent_focus(thread, "lead", &focus_win(7)).is_ok());
    }

    #[test]
    fn require_recent_focus_rejects_with_no_prior_click_at_all() {
        let thread = 900_002;
        let err = require_recent_focus(thread, "lead", &focus_win(7)).unwrap_err();
        assert!(err.contains("focus"), "{err}");
        assert!(err.contains("click"), "{err}");
    }

    #[test]
    fn require_recent_focus_rejects_a_click_on_a_different_window() {
        let thread = 900_003;
        record_click_focus(thread, "lead", &focus_win(7)); // clicked window A (id 7)
        let err = require_recent_focus(thread, "lead", &focus_win(8)).unwrap_err(); // typing into B (id 8)
        assert!(err.contains("8"), "error should name the window that lacks focus: {err}");
    }

    /// issue #160 round-17 P2 (Codex computer_srv.rs:1943): a REUSED numeric
    /// id must not satisfy the freshness check — the clicked window closed
    /// and a different app/title took its number within the 15s window.
    #[test]
    fn require_recent_focus_rejects_a_reused_id_with_a_different_identity() {
        let thread = 900_007;
        record_click_focus(thread, "lead", &focus_win(7));
        let mut imposter = focus_win(7);
        imposter.app = "Mail".into();
        imposter.title = "inbox".into();
        assert!(
            require_recent_focus(thread, "lead", &imposter).is_err(),
            "the SAME id with a different app/title must not read as recently clicked"
        );
        // The genuine identity still passes.
        assert!(require_recent_focus(thread, "lead", &focus_win(7)).is_ok());
    }

    #[test]
    fn require_recent_focus_is_scoped_per_thread_dir() {
        let thread_a = 900_004;
        let thread_b = 900_005;
        record_click_focus(thread_a, "lead", &focus_win(7));
        // A click recorded for a DIFFERENT (thread, dir) must not satisfy
        // this one's focus check — the registry is per-session, not global.
        assert!(require_recent_focus(thread_b, "lead", &focus_win(7)).is_err());
        assert!(require_recent_focus(thread_a, "10", &focus_win(7)).is_err());
    }

    #[test]
    fn require_recent_focus_rejects_once_the_freshness_window_has_expired() {
        let thread = 900_006;
        // Seed a click stamped older than `FOCUS_FRESHNESS_MS` directly,
        // rather than sleeping 15s in a test — same "no fake clock needed"
        // approach the coordinator's spec calls for, just expressed as a
        // pre-expired `Instant` (round-15 P2: the registry is monotonic
        // Instant-based now) instead of a real-time wait. `checked_sub` can
        // only return `None` if the process somehow started less than ~15s
        // after the Instant epoch — skip (vacuously pass) rather than panic
        // in that unreachable-in-practice case.
        let Some(stale) = std::time::Instant::now()
            .checked_sub(std::time::Duration::from_millis(FOCUS_FRESHNESS_MS + 1_000))
        else {
            return;
        };
        {
            let mut g = recent_clicks().lock().unwrap();
            g.insert(
                (thread, "lead".to_string()),
                (VerifiedWindowIdentity::from_window(&focus_win(7)), stale),
            );
        }
        assert!(require_recent_focus(thread, "lead", &focus_win(7)).is_err());
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
        // issue #160 round-11 P1 #D: an input action now maps its coordinate
        // against a RECORDED screenshot's own dimensions (fail-closed with
        // none on file) rather than the window's current size — seed the
        // record directly (standing in for "this session already
        // screenshotted this window") since the CURRENT size is what this
        // test's own window origin/size started at, at 1:1 scale.
        computer::record_shot_dims(
            thread,
            dir,
            906_301,
            800,
            600,
            &computer::WindowInfo {
                id: 906_301,
                app: "Baz".into(),
                title: "Baz".into(),
                x: 0,
                y: 0,
                width: 800,
                height: 600,
            },
        );

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
        record_click_focus(thread, dir, &computer::WindowInfo {
            id: 906_201,
            app: "Bar".into(),
            title: "Bar".into(),
            x: 0,
            y: 0,
            width: 800,
            height: 600,
        });

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

    // —— issue #160 round-10 P1 #B: re-resolve/re-verify AFTER activation ——

    /// The end-to-end property this fix exists for: while `activate_window`
    /// runs (standing in for a real, slow OS activation call), the window
    /// moves to a NEW origin — SAME id/app/title, so `verify_approved_target`
    /// still passes; only the GEOMETRY changed. The click must land using the
    /// window's geometry AS OF AFTER activation, never the stale
    /// pre-activation origin.
    ///
    /// issue #160 round-12 P1 #C note: this origin change is also exactly
    /// why `computer::shot_dims_for` does NOT gate on geometry — see that
    /// function's own doc, which names this test by function name as one of
    /// the two reasons.
    #[tokio::test]
    async fn left_click_uses_the_windows_geometry_as_of_after_activation_not_before() {
        let _guard = computer::process_state_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        computer::clear_emergency_stop(computer::stop_generation());
        computer::clear_control();
        let mock = shared_mock();
        mock.fail_activate.store(false, std::sync::atomic::Ordering::SeqCst);
        mock.actions.lock().unwrap_or_else(|e| e.into_inner()).clear();
        *mock.windows_override.lock().unwrap_or_else(|e| e.into_inner()) = Some(vec![computer::WindowInfo {
            id: 910_401,
            app: "Moving".into(),
            title: "moving window".into(),
            x: 0,
            y: 0,
            width: 800,
            height: 600,
        }]);

        let db = crate::store::Db::connect("sqlite::memory:").await.unwrap();
        repo::set_setting(&db, computer::K_COMPUTER_USE_ENABLED, "true").await.unwrap();
        let asks = AskRegistry::new();
        let thread = 910_401;
        let dir = "lead";
        asks.seed_grants(crate::ask::GrantSnapshot {
            full: vec![crate::ask::FullGrant { thread, dir: dir.to_string() }],
            always: Vec::new(),
        });
        // issue #160 round-11 P1 #D: seed this window's own recorded
        // screenshot dims (its size never changes in this scenario, only its
        // origin does — see the hook below) so the click's coordinate mapping
        // doesn't fail closed for want of a screenshot on file.
        computer::record_shot_dims(
            thread,
            dir,
            910_401,
            800,
            600,
            &computer::WindowInfo {
                id: 910_401,
                app: "Moving".into(),
                title: "moving window".into(),
                x: 0,
                y: 0,
                width: 800,
                height: 600,
            },
        );

        let _ = computer::throttle_input();
        tokio::time::sleep(std::time::Duration::from_millis(600)).await;

        // The hook: while `activate_window` runs, the window moves to a NEW
        // origin — same identity, different geometry.
        let mock_for_hook = mock.clone();
        *mock.on_activate.lock().unwrap_or_else(|e| e.into_inner()) = Some(Box::new(move || {
            *mock_for_hook.windows_override.lock().unwrap_or_else(|e| e.into_inner()) = Some(vec![computer::WindowInfo {
                id: 910_401,
                app: "Moving".into(),
                title: "moving window".into(),
                x: 400,
                y: 200,
                width: 800,
                height: 600,
            }]);
        }));

        let mut window_id_out = None;
        let mut image_out = None;
        let result = run_action(
            &db, &asks, thread, dir, None, "computer", "left_click",
            &json!({"window": "moving", "coordinate": [10, 10]}),
            &mut window_id_out, &mut image_out,
        )
        .await;

        assert!(result.is_ok(), "{result:?}");
        let actions = mock.actions.lock().unwrap_or_else(|e| e.into_inner());
        assert!(
            actions.iter().any(|a| a == "click 410,210 Left x1"),
            "the click must use the window's geometry AS OF AFTER activation \
             (400+10, 200+10), not the stale pre-activation origin: {actions:?}"
        );
        assert!(
            !actions.iter().any(|a| a == "click 10,10 Left x1"),
            "must never use the stale pre-activation origin (0,0): {actions:?}"
        );
        drop(actions);

        *mock.on_activate.lock().unwrap_or_else(|e| e.into_inner()) = None;
        computer::clear_control();
    }

    /// The fail-closed half: while `activate_window` runs, the window is
    /// REPLACED — same query still matches (title substring), but a
    /// DIFFERENT id AND app — standing in for the original window closing
    /// and an unrelated one taking its place mid-activation. The action must
    /// fail closed (the SAME re-approve message `verify_approved_target`
    /// gives the approve→dispatch gap) and the backend must never receive
    /// the keystrokes.
    #[tokio::test]
    async fn type_fails_closed_when_the_window_is_replaced_during_activation() {
        let _guard = computer::process_state_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        computer::clear_emergency_stop(computer::stop_generation());
        computer::clear_control();
        let mock = shared_mock();
        mock.fail_activate.store(false, std::sync::atomic::Ordering::SeqCst);
        mock.actions.lock().unwrap_or_else(|e| e.into_inner()).clear();
        *mock.windows_override.lock().unwrap_or_else(|e| e.into_inner()) = Some(vec![computer::WindowInfo {
            id: 910_501,
            app: "Swappy".into(),
            title: "swappy window".into(),
            x: 0,
            y: 0,
            width: 800,
            height: 600,
        }]);

        let db = crate::store::Db::connect("sqlite::memory:").await.unwrap();
        repo::set_setting(&db, computer::K_COMPUTER_USE_ENABLED, "true").await.unwrap();
        let asks = AskRegistry::new();
        let thread = 910_501;
        let dir = "lead";
        asks.seed_grants(crate::ask::GrantSnapshot {
            full: vec![crate::ask::FullGrant { thread, dir: dir.to_string() }],
            always: Vec::new(),
        });
        record_click_focus(thread, dir, &computer::WindowInfo {
            id: 910_501,
            app: "Swappy".into(),
            title: "swappy window".into(),
            x: 0,
            y: 0,
            width: 800,
            height: 600,
        });

        let _ = computer::throttle_input();
        tokio::time::sleep(std::time::Duration::from_millis(600)).await;

        // The hook: while `activate_window` runs, the window is REPLACED by
        // a different id/app that happens to share the same title substring.
        let mock_for_hook = mock.clone();
        *mock.on_activate.lock().unwrap_or_else(|e| e.into_inner()) = Some(Box::new(move || {
            *mock_for_hook.windows_override.lock().unwrap_or_else(|e| e.into_inner()) = Some(vec![computer::WindowInfo {
                id: 910_502,
                app: "Different".into(),
                title: "swappy window".into(),
                x: 0,
                y: 0,
                width: 800,
                height: 600,
            }]);
        }));

        let mut window_id_out = None;
        let mut image_out = None;
        let err = run_action(
            &db, &asks, thread, dir, None, "computer", "type",
            &json!({"window": "swappy", "text": "hello"}),
            &mut window_id_out, &mut image_out,
        )
        .await
        .unwrap_err();

        assert!(
            err.contains("changed since this action was approved"),
            "must fail closed with the re-approve message: {err}"
        );
        let actions = mock.actions.lock().unwrap_or_else(|e| e.into_inner());
        assert!(
            !actions.iter().any(|a| a.starts_with("type ")),
            "the replaced window must never receive the keystrokes: {actions:?}"
        );
        drop(actions);

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
        // issue #160 round-11 P1 #D: seed this window's recorded screenshot
        // dims (unchanged for this scenario) so the click's coordinate
        // mapping doesn't fail closed for want of a screenshot on file.
        computer::record_shot_dims(
            thread,
            dir,
            907_501,
            800,
            600,
            &computer::WindowInfo {
                id: 907_501,
                app: "Steady".into(),
                title: "steady window".into(),
                x: 0,
                y: 0,
                width: 800,
                height: 600,
            },
        );

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

    // —— issue #160 round-10 P1 #A: standing Always grant scoped to the
    // approved window's own identity, not just the query string ——

    /// The end-to-end property this fix exists for: an Always grant seeded
    /// with a key that includes the CURRENT window's `app`+`title` still
    /// auto-approves the SAME query when it resolves to that SAME window —
    /// this is the "narrowing must not break the common case" half.
    #[tokio::test]
    async fn standing_always_grant_auto_approves_when_the_window_identity_is_unchanged() {
        let _guard = computer::process_state_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        let mock = shared_mock();
        mock.fail_activate.store(false, std::sync::atomic::Ordering::SeqCst);
        *mock.on_activate.lock().unwrap_or_else(|e| e.into_inner()) = None;
        mock.actions.lock().unwrap_or_else(|e| e.into_inner()).clear();
        *mock.windows_override.lock().unwrap_or_else(|e| e.into_inner()) = Some(vec![computer::WindowInfo {
            id: 909_101,
            app: "Editor".into(),
            title: "notes.txt — Editor".into(),
            x: 0,
            y: 0,
            width: 800,
            height: 600,
        }]);

        let asks = AskRegistry::new();
        let thread = 909_101;
        let dir = "lead";
        let args = json!({"action": "left_click", "window": "editor", "coordinate": [1, 1]});
        let action_key = crate::ask::action_key(&[
            "gui",
            "left_click",
            "editor",
            "909101",
            "Editor",
            "notes.txt — Editor",
            &args_digest(&args),
        ]);
        asks.seed_grants(crate::ask::GrantSnapshot {
            full: Vec::new(),
            always: vec![crate::ask::AlwaysGrant { thread, dir: dir.to_string(), action_key }],
        });

        let approved = tokio::time::timeout(std::time::Duration::from_secs(5), approve(&asks, thread, dir, "left_click", &args))
            .await
            .expect("must resolve without ever needing a human answer")
            .expect("an exact identity match must auto-approve");
        assert_eq!(
            approved,
            Some(ApprovedWindow { id: 909_101, app: "Editor".into(), title: "notes.txt — Editor".into() })
        );
        assert!(asks.open().is_empty(), "an auto-approved call must never surface a card");
    }

    /// The narrowing half: the SAME query now resolves to a DIFFERENT window
    /// (different id AND app — standing in for the original window closing
    /// and an unrelated one taking its place) — the standing Always grant
    /// seeded for the OLD identity must MISS (a different `action_key`,
    /// since `app`+`title` are folded into it), falling through to a fresh
    /// Needs-you card rather than silently auto-approving the replacement
    /// window. Before issue #160 round-10 P1 #A, the key never captured
    /// WHICH window was approved (only the query string), so this exact
    /// scenario would have auto-approved silently.
    #[tokio::test]
    async fn standing_always_grant_misses_a_different_window_identity_and_cards() {
        let _guard = computer::process_state_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        let mock = shared_mock();
        mock.fail_activate.store(false, std::sync::atomic::Ordering::SeqCst);
        *mock.on_activate.lock().unwrap_or_else(|e| e.into_inner()) = None;
        mock.actions.lock().unwrap_or_else(|e| e.into_inner()).clear();
        *mock.windows_override.lock().unwrap_or_else(|e| e.into_inner()) = Some(vec![computer::WindowInfo {
            id: 909_201,
            app: "Editor".into(),
            title: "notes.txt — Editor".into(),
            x: 0,
            y: 0,
            width: 800,
            height: 600,
        }]);

        let asks = AskRegistry::new();
        let thread = 909_201;
        let dir = "lead";
        let args = json!({"action": "left_click", "window": "editor", "coordinate": [1, 1]});
        // Seed an Always grant for the CURRENT (soon-to-be-replaced) window's
        // identity.
        let action_key = crate::ask::action_key(&[
            "gui",
            "left_click",
            "editor",
            "909201",
            "Editor",
            "notes.txt — Editor",
            &args_digest(&args),
        ]);
        asks.seed_grants(crate::ask::GrantSnapshot {
            full: Vec::new(),
            always: vec![crate::ask::AlwaysGrant { thread, dir: dir.to_string(), action_key }],
        });

        // The query now resolves to a DIFFERENT window.
        *mock.windows_override.lock().unwrap_or_else(|e| e.into_inner()) = Some(vec![computer::WindowInfo {
            id: 909_202,
            app: "OtherApp".into(),
            title: "an unrelated editor window".into(),
            x: 0,
            y: 0,
            width: 800,
            height: 600,
        }]);

        let asks_bg = asks.clone();
        let args_bg = args.clone();
        let handle = tokio::spawn(async move { approve(&asks_bg, thread, dir, "left_click", &args_bg).await });

        let mut card = None;
        for _ in 0..200 {
            if let Some(a) = asks.open().into_iter().find(|a| a.thread == thread && a.dir == dir) {
                card = Some(a);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let card = card.expect(
            "a DIFFERENT window identity must miss the standing grant's key and surface a fresh card",
        );

        assert!(asks.answer(card.id, crate::ask::Answer::Deny));
        let err = handle.await.unwrap().unwrap_err();
        assert!(err.contains("denied"), "{err}");
    }

    // —— issue #160 round-11 P1 #B: Always key now also binds the window INSTANCE (id) ——

    /// The exact gap round-11 P1 #B closes, isolated from round-10 P1 #A's
    /// own (broader) "different window entirely" scenario above: the ORIGINAL
    /// window closes and a NEW one opens with the EXACT SAME `app`+`title` —
    /// a relaunched app, a reopened document with an identical name — but a
    /// DIFFERENT `id`. Before this round, the key carried `app`+`title` but
    /// NOT `id`, so this replacement window's key was IDENTICAL to the
    /// original's — the standing Always grant would have silently kept
    /// authorizing input into the REPLACEMENT instance. With `id` folded in,
    /// the SAME query now mints a DIFFERENT key and must fall through to a
    /// fresh card instead.
    #[tokio::test]
    async fn standing_always_grant_misses_a_replaced_window_instance_with_the_same_app_and_title() {
        let _guard = computer::process_state_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        let mock = shared_mock();
        mock.fail_activate.store(false, std::sync::atomic::Ordering::SeqCst);
        *mock.on_activate.lock().unwrap_or_else(|e| e.into_inner()) = None;
        mock.actions.lock().unwrap_or_else(|e| e.into_inner()).clear();
        *mock.windows_override.lock().unwrap_or_else(|e| e.into_inner()) = Some(vec![computer::WindowInfo {
            id: 909_301,
            app: "Editor".into(),
            title: "notes.txt — Editor".into(),
            x: 0,
            y: 0,
            width: 800,
            height: 600,
        }]);

        let asks = AskRegistry::new();
        let thread = 909_301;
        let dir = "lead";
        let args = json!({"action": "left_click", "window": "editor", "coordinate": [1, 1]});
        // Seed an Always grant for the ORIGINAL window's identity (id 909_301).
        let action_key = crate::ask::action_key(&[
            "gui",
            "left_click",
            "editor",
            "909301",
            "Editor",
            "notes.txt — Editor",
            &args_digest(&args),
        ]);
        asks.seed_grants(crate::ask::GrantSnapshot {
            full: Vec::new(),
            always: vec![crate::ask::AlwaysGrant { thread, dir: dir.to_string(), action_key }],
        });

        // The ORIGINAL window closed; a REPLACEMENT opened with the SAME
        // app+title but a NEW id (909_302) — standing in for a relaunched
        // app / reopened document with an identical name.
        *mock.windows_override.lock().unwrap_or_else(|e| e.into_inner()) = Some(vec![computer::WindowInfo {
            id: 909_302,
            app: "Editor".into(),
            title: "notes.txt — Editor".into(),
            x: 0,
            y: 0,
            width: 800,
            height: 600,
        }]);

        let asks_bg = asks.clone();
        let args_bg = args.clone();
        let handle = tokio::spawn(async move { approve(&asks_bg, thread, dir, "left_click", &args_bg).await });

        let mut card = None;
        for _ in 0..200 {
            if let Some(a) = asks.open().into_iter().find(|a| a.thread == thread && a.dir == dir) {
                card = Some(a);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let card = card.expect(
            "a REPLACED window instance (new id, same app+title) must miss the standing grant's \
             key — bound to the OLD id — and surface a fresh card",
        );

        assert!(asks.answer(card.id, crate::ask::Answer::Allow));
        let approved = handle.await.unwrap().unwrap();
        assert_eq!(
            approved,
            Some(ApprovedWindow { id: 909_302, app: "Editor".into(), title: "notes.txt — Editor".into() }),
            "the fresh card's own Allow binds the NEW window instance"
        );
    }

    /// The mirror image, for completeness: the EXACT same window instance
    /// (same id+app+title) still auto-approves via the standing grant — this
    /// round's narrowing must not re-card a window that never actually
    /// changed. (`standing_always_grant_auto_approves_when_the_window_
    /// identity_is_unchanged` above already covers this end-to-end; this is
    /// a second, differently-shaped window/query pair for variety, not a
    /// duplicate of that test's own assertions.)
    #[tokio::test]
    async fn standing_always_grant_auto_approves_the_exact_same_window_instance() {
        let _guard = computer::process_state_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        let mock = shared_mock();
        mock.fail_activate.store(false, std::sync::atomic::Ordering::SeqCst);
        *mock.on_activate.lock().unwrap_or_else(|e| e.into_inner()) = None;
        mock.actions.lock().unwrap_or_else(|e| e.into_inner()).clear();
        *mock.windows_override.lock().unwrap_or_else(|e| e.into_inner()) = Some(vec![computer::WindowInfo {
            id: 909_401,
            app: "Notes".into(),
            title: "Untitled".into(),
            x: 0,
            y: 0,
            width: 800,
            height: 600,
        }]);

        let asks = AskRegistry::new();
        let thread = 909_401;
        let dir = "lead";
        let args = json!({"action": "left_click", "window": "notes", "coordinate": [2, 2]});
        let action_key = crate::ask::action_key(&[
            "gui",
            "left_click",
            "notes",
            "909401",
            "Notes",
            "Untitled",
            &args_digest(&args),
        ]);
        asks.seed_grants(crate::ask::GrantSnapshot {
            full: Vec::new(),
            always: vec![crate::ask::AlwaysGrant { thread, dir: dir.to_string(), action_key }],
        });

        let approved = tokio::time::timeout(std::time::Duration::from_secs(5), approve(&asks, thread, dir, "left_click", &args))
            .await
            .expect("must resolve without ever needing a human answer")
            .expect("an exact id+app+title match must auto-approve");
        assert_eq!(approved, Some(ApprovedWindow { id: 909_401, app: "Notes".into(), title: "Untitled".into() }));
        assert!(asks.open().is_empty(), "an auto-approved call must never surface a card");
    }

    /// `list_windows` (no window argument at all) and `wait` (Write-classified
    /// but windowless) never bind a window: `approve` returns `Ok(None)` for
    /// both, and never even attempts a resolve for either — there is nothing
    /// for `verify_approved_target` to ever check against, and their
    /// `action_key` keeps the OLD, resolve-free four-part shape.
    ///
    /// issue #160 round-11 P1 #C note: this test USED TO also cover
    /// `screenshot` here (asserting it never binds a window either) — that is
    /// no longer true: `screenshot` now resolves and binds a window exactly
    /// like a Write action does (see `approve`'s own `resolved` doc comment)
    /// — see `screenshot_binds_its_resolved_window_like_a_write_action` and
    /// `screenshot_fails_closed_when_its_window_arg_cannot_resolve` below for
    /// screenshot's OWN, now-different behavior.
    #[tokio::test]
    async fn list_windows_and_wait_never_bind_a_window_arg() {
        let asks = AskRegistry::new();
        let thread = 907_601;
        let dir = "lead";
        asks.seed_grants(crate::ask::GrantSnapshot {
            full: vec![crate::ask::FullGrant { thread, dir: dir.to_string() }],
            always: Vec::new(),
        });

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

    // —— issue #160 round-11 P1 #C: screenshot ALSO binds its resolved window ——

    /// The end-to-end property the fix exists for: a `screenshot` with a
    /// non-blank, resolvable `window` argument now binds that window's
    /// identity in `approve`'s return value, exactly like a Write action
    /// does — closing the gap where a screenshot's standing grant/card used
    /// to be scoped to the bare QUERY STRING alone (see `approve`'s own doc
    /// comment for the full rationale).
    #[tokio::test]
    async fn screenshot_binds_its_resolved_window_like_a_write_action() {
        let _guard = computer::process_state_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        let mock = shared_mock();
        mock.fail_activate.store(false, std::sync::atomic::Ordering::SeqCst);
        *mock.on_activate.lock().unwrap_or_else(|e| e.into_inner()) = None;
        mock.actions.lock().unwrap_or_else(|e| e.into_inner()).clear();
        *mock.windows_override.lock().unwrap_or_else(|e| e.into_inner()) = Some(vec![computer::WindowInfo {
            id: 907_602,
            app: "Anything".into(),
            title: "anything window".into(),
            x: 0,
            y: 0,
            width: 800,
            height: 600,
        }]);

        let asks = AskRegistry::new();
        let thread = 907_602;
        let dir = "lead";
        asks.seed_grants(crate::ask::GrantSnapshot {
            full: vec![crate::ask::FullGrant { thread, dir: dir.to_string() }],
            always: Vec::new(),
        });

        let screenshot_args = json!({"action": "screenshot", "window": "anything"});
        let approved = approve(&asks, thread, dir, "screenshot", &screenshot_args).await.unwrap();
        assert_eq!(
            approved,
            Some(ApprovedWindow { id: 907_602, app: "Anything".into(), title: "anything window".into() }),
            "a resolvable screenshot must bind the SAME window identity a Write action would"
        );
    }

    /// The fail-closed half: a `screenshot` whose `window` argument does NOT
    /// resolve to any visible window must reject the WHOLE call — never
    /// silently proceed with `Ok(None)` the way an unresolvable query used to
    /// before this round (when screenshot never attempted a resolve at all).
    #[tokio::test]
    async fn screenshot_fails_closed_when_its_window_arg_cannot_resolve() {
        let _guard = computer::process_state_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        let mock = shared_mock();
        mock.fail_activate.store(false, std::sync::atomic::Ordering::SeqCst);
        *mock.on_activate.lock().unwrap_or_else(|e| e.into_inner()) = None;
        mock.actions.lock().unwrap_or_else(|e| e.into_inner()).clear();
        // No window matches "nonexistent-window-query" at all.
        *mock.windows_override.lock().unwrap_or_else(|e| e.into_inner()) = Some(vec![computer::WindowInfo {
            id: 907_603,
            app: "SomethingElse".into(),
            title: "unrelated".into(),
            x: 0,
            y: 0,
            width: 800,
            height: 600,
        }]);

        let asks = AskRegistry::new();
        let thread = 907_603;
        let dir = "lead";
        asks.seed_grants(crate::ask::GrantSnapshot {
            full: vec![crate::ask::FullGrant { thread, dir: dir.to_string() }],
            always: Vec::new(),
        });

        let screenshot_args = json!({"action": "screenshot", "window": "nonexistent-window-query"});
        let err = approve(&asks, thread, dir, "screenshot", &screenshot_args).await.unwrap_err();
        assert!(err.to_lowercase().contains("no visible window"), "{err}");
    }

    /// The end-to-end property `screenshot`'s NEW capture-time identity check
    /// exists for (issue #160 round-11 P1 #C): a card is opened for
    /// `screenshot @ shifty`, binding window X's identity the instant it
    /// opens; while the human sits on that still-open card, the ORIGINAL
    /// window closes and a DIFFERENT one — same title substring, so the SAME
    /// query still matches — takes its place. The human's (now-stale) Allow
    /// must NOT let the capture proceed against the replacement: `run_action`'s
    /// own "screenshot" arm re-resolves fresh and must reject, exactly like
    /// every input arm's own `resolve_and_verify_target` gate — no pixels are
    /// ever captured of the wrong window.
    #[tokio::test]
    async fn screenshot_fails_closed_when_the_window_is_replaced_between_approval_and_capture() {
        let _guard = computer::process_state_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        let mock = shared_mock();
        mock.fail_activate.store(false, std::sync::atomic::Ordering::SeqCst);
        *mock.on_activate.lock().unwrap_or_else(|e| e.into_inner()) = None;
        mock.actions.lock().unwrap_or_else(|e| e.into_inner()).clear();
        *mock.windows_override.lock().unwrap_or_else(|e| e.into_inner()) = Some(vec![computer::WindowInfo {
            id: 911_101,
            app: "Shifty".into(),
            title: "shifty window".into(),
            x: 0,
            y: 0,
            width: 800,
            height: 600,
        }]);

        let db = crate::store::Db::connect("sqlite::memory:").await.unwrap();
        repo::set_setting(&db, computer::K_COMPUTER_USE_ENABLED, "true").await.unwrap();
        // Deliberately UNGRANTED — a real card must open, binding window X's
        // identity at the instant it does.
        let asks = AskRegistry::new();
        let thread = 911_101;
        let dir = "lead";

        let db_bg = db.clone();
        let asks_bg = asks.clone();
        let handle = tokio::spawn(async move {
            let mut window_id_out = None;
            let mut image_out = None;
            run_action(
                &db_bg, &asks_bg, thread, dir, None, "computer", "screenshot",
                &json!({"window": "shifty"}), &mut window_id_out, &mut image_out,
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
        let card = card.expect("a screenshot with no standing grant must surface a card");

        // The window is REPLACED while the card sits open: the SAME title
        // substring still matches, but a DIFFERENT id and app — standing in
        // for the original window closing and an unrelated one taking its
        // place before the human answers.
        *mock.windows_override.lock().unwrap_or_else(|e| e.into_inner()) = Some(vec![computer::WindowInfo {
            id: 911_102,
            app: "Different App".into(),
            title: "shifty window".into(),
            x: 0,
            y: 0,
            width: 800,
            height: 600,
        }]);

        assert!(asks.answer(card.id, crate::ask::Answer::Allow));
        let err = handle.await.unwrap().unwrap_err();
        assert!(
            err.contains("changed since this action was approved"),
            "must fail closed with the re-approve message, never silently capture the replacement: {err}"
        );
        assert!(
            computer::shot_dims_for(
                thread,
                dir,
                &computer::WindowInfo {
                    id: 911_102,
                    app: "Different App".into(),
                    title: "shifty window".into(),
                    x: 0,
                    y: 0,
                    width: 800,
                    height: 600,
                }
            )
            .is_none(),
            "a fail-closed capture must never record shot dims for the replacement window either"
        );
    }

    /// The Always-grant twin of the test above: a standing Always grant
    /// scoped to window X's identity must MISS once the query resolves to a
    /// DIFFERENT window Y — falling through to a fresh card rather than
    /// silently auto-capturing Y under a grant the human only ever earned for
    /// X. Mirrors `standing_always_grant_misses_a_different_window_identity_
    /// and_cards` (the click-family version) for `screenshot` specifically.
    #[tokio::test]
    async fn screenshot_standing_always_grant_misses_a_replaced_window_and_cards() {
        let _guard = computer::process_state_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        let mock = shared_mock();
        mock.fail_activate.store(false, std::sync::atomic::Ordering::SeqCst);
        *mock.on_activate.lock().unwrap_or_else(|e| e.into_inner()) = None;
        mock.actions.lock().unwrap_or_else(|e| e.into_inner()).clear();
        *mock.windows_override.lock().unwrap_or_else(|e| e.into_inner()) = Some(vec![computer::WindowInfo {
            id: 911_201,
            app: "Shifty".into(),
            title: "shifty window".into(),
            x: 0,
            y: 0,
            width: 800,
            height: 600,
        }]);

        let asks = AskRegistry::new();
        let thread = 911_201;
        let dir = "lead";
        let args = json!({"action": "screenshot", "window": "shifty"});
        // Seed an Always grant for the CURRENT (soon-to-be-replaced) window's
        // identity.
        let action_key = crate::ask::action_key(&[
            "gui",
            "screenshot",
            "shifty",
            "911201",
            "Shifty",
            "shifty window",
            &args_digest(&args),
        ]);
        asks.seed_grants(crate::ask::GrantSnapshot {
            full: Vec::new(),
            always: vec![crate::ask::AlwaysGrant { thread, dir: dir.to_string(), action_key }],
        });

        // The query now resolves to a DIFFERENT window.
        *mock.windows_override.lock().unwrap_or_else(|e| e.into_inner()) = Some(vec![computer::WindowInfo {
            id: 911_202,
            app: "OtherApp".into(),
            title: "an unrelated shifty window".into(),
            x: 0,
            y: 0,
            width: 800,
            height: 600,
        }]);

        let asks_bg = asks.clone();
        let args_bg = args.clone();
        let handle = tokio::spawn(async move { approve(&asks_bg, thread, dir, "screenshot", &args_bg).await });

        let mut card = None;
        for _ in 0..200 {
            if let Some(a) = asks.open().into_iter().find(|a| a.thread == thread && a.dir == dir) {
                card = Some(a);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let card = card.expect(
            "a DIFFERENT window identity must miss the screenshot's own standing grant key and \
             surface a fresh card",
        );

        assert!(asks.answer(card.id, crate::ask::Answer::Deny));
        let err = handle.await.unwrap().unwrap_err();
        assert!(err.contains("denied"), "{err}");
    }

    /// issue #160 round-18 P1 (Codex computer_srv.rs:967): the FINAL,
    /// SYNCHRONOUS stop/lease recheck each input arm runs INSIDE its
    /// `on_blocking` closure right before the backend call. Touches the SAME
    /// process-wide stop-latch/control-lease statics as every other lease test
    /// here, so it shares `process_state_test_lock`. No `.await` — the whole
    /// point is that it's a lock-only check runnable on a blocking thread.
    #[test]
    fn recheck_stop_and_lease_before_backend_covers_stop_lease_and_happy_path() {
        let _guard = computer::process_state_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        let thread = 906_301;
        let dir = "lead";

        // Clean baseline: clear any latch a prior test left (trip mints+returns
        // a generation and clears the lease, so clear it right back), then take
        // the lease for THIS (thread, dir).
        let base_gen = computer::trip_stop_latch();
        assert!(computer::clear_emergency_stop(base_gen));
        computer::acquire_control(thread, dir).unwrap();
        // Latch clear AND this exact (thread, dir) holds the lease — the only
        // combination allowed to reach the injection backend.
        assert!(recheck_stop_and_lease_before_backend(thread, dir).is_ok());

        // Emergency Stop landing after the last async recheck (during the final
        // resolve, or while the closure sat queued for a blocking thread) trips
        // the latch — deny with the disabled message. `trip_stop_latch` ALSO
        // clears the lease, so the stop check firing first is what this asserts.
        let stop_gen = computer::trip_stop_latch();
        let err = recheck_stop_and_lease_before_backend(thread, dir).unwrap_err();
        assert!(err.to_lowercase().contains("disabled"), "{err}");
        assert!(computer::clear_emergency_stop(stop_gen));

        // Latch clear, but the lease is gone (Escape cleared it / it expired in
        // that same window) — deny, and NOT with the disabled message.
        computer::clear_control();
        let err = recheck_stop_and_lease_before_backend(thread, dir).unwrap_err();
        assert!(!err.to_lowercase().contains("disabled"), "{err}");

        // A DIFFERENT (thread, dir) now holds the lease — deny (busy).
        computer::acquire_control(999_999, "someone-else").unwrap();
        let err = recheck_stop_and_lease_before_backend(thread, dir).unwrap_err();
        assert!(err.contains("999999") || err.contains("someone-else"), "{err}");
        computer::clear_control();
    }

    /// issue #160 round-18 P2 (Codex paths.rs:89): deleting a thread removes
    /// ITS computer-output subtree only, never a sibling thread's.
    #[test]
    fn remove_computer_output_for_thread_drops_only_that_threads_subtree() {
        let _env = crate::paths::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let weft_home =
            std::env::temp_dir().join(format!("weft-computer-srv-rm-out-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&weft_home);
        std::env::set_var("WEFT_HOME", weft_home.to_str().unwrap());

        let root = crate::paths::computer_output_root().unwrap();
        // Thread 42 (lead lane): a screenshot plus a rotated-style audit log.
        let shots_42 = root.join("42").join(crate::bus::LEAD).join("screenshots");
        std::fs::create_dir_all(&shots_42).unwrap();
        std::fs::write(shots_42.join("shot.png"), b"x").unwrap();
        std::fs::write(root.join("42").join("computer-audit.jsonl"), b"{}").unwrap();
        // Sibling thread 43 that must survive the delete untouched.
        let shots_43 = root.join("43").join(crate::bus::LEAD).join("screenshots");
        std::fs::create_dir_all(&shots_43).unwrap();
        std::fs::write(shots_43.join("shot.png"), b"y").unwrap();

        remove_computer_output_for_thread(42);

        assert!(!root.join("42").exists(), "thread 42's whole subtree is removed");
        assert!(root.join("43").exists(), "a sibling thread's subtree is untouched");
        assert!(shots_43.join("shot.png").exists(), "the sibling's screenshot survives");

        std::env::remove_var("WEFT_HOME");
        let _ = std::fs::remove_dir_all(&weft_home);
    }

    /// issue #160 round-19 P1 (Codex computer_srv.rs:403): a request under a
    /// deleted thread/direction is refused — `session_is_live` is the admission
    /// gate that revokes the route once the delete cascade runs.
    #[tokio::test]
    async fn session_is_live_tracks_thread_and_direction_deletion() {
        let db = crate::store::Db::connect("sqlite::memory:").await.unwrap();
        let ws = repo::create_workspace(&db, "ws").await.unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let repo_ref =
            repo::add_repo_ref(&db, ws.id, "a", &tmp.path().to_string_lossy(), "main", "", true)
                .await
                .unwrap();
        let thread = repo::create_thread(&db, ws.id, "t", "feature", "claude").await.unwrap();
        let direction = repo::create_direction(
            &db, thread.id, "task", "claude", repo_ref.id, "why", "impl-only", "main",
        )
        .await
        .unwrap();
        let wt = repo::record_worktree(&db, repo_ref.id, direction.id, "b1", "/tmp/weft-wt", true, true, "")
            .await
            .unwrap();
        let dir_s = direction.id.to_string();

        // Live: lead lane, worker lane, and a pinned worktree all resolve.
        assert!(session_is_live(&db, thread.id, crate::bus::LEAD, None).await);
        assert!(session_is_live(&db, thread.id, &dir_s, None).await);
        assert!(session_is_live(&db, thread.id, &dir_s, Some(wt.id)).await);
        // A worktree that isn't this direction's, and a direction under a
        // DIFFERENT thread, both fail closed.
        assert!(!session_is_live(&db, thread.id, &dir_s, Some(wt.id + 999)).await);
        assert!(!session_is_live(&db, thread.id + 999, &dir_s, None).await);
        assert!(!session_is_live(&db, thread.id, "not-a-number", None).await);

        // After the delete cascade, every lane is refused.
        repo::delete_thread_cascade(&db, thread.id).await.unwrap();
        assert!(!session_is_live(&db, thread.id, crate::bus::LEAD, None).await);
        assert!(!session_is_live(&db, thread.id, &dir_s, None).await);
        assert!(!session_is_live(&db, thread.id, &dir_s, Some(wt.id)).await);
    }

    /// issue #160 round-19 P1 (Codex computer_srv.rs:403): the revocation set is
    /// empty until a delete (so no live session — nor any synthetic-identity
    /// test — ever pays the `session_is_live` DB check); `revoke_computer_routes`
    /// flags exactly the deleted thread.
    #[test]
    fn revoke_computer_routes_flags_only_the_deleted_thread() {
        let t = 917_001;
        assert!(!computer_routes_revoked(t), "a never-deleted thread is not revoked");
        revoke_computer_routes(t);
        assert!(computer_routes_revoked(t), "the deleted thread is revoked");
        assert!(!computer_routes_revoked(t + 1), "revocation is per-thread");
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

    // —— issue #160 round-11 P1 #D: coordinate mapping by SAVED screenshot geometry ——

    /// The fail-closed half, end-to-end through `run_action`'s own "left_click"
    /// arm (Full-granted, so `approve` decides silently): a window that
    /// resolves fine but was NEVER screenshotted for this exact (thread, dir)
    /// has no screenshot-space coordinate system to map against at all —
    /// the click must be rejected, never silently fall back to mapping
    /// against the window's CURRENT size (the exact bug this round closes).
    #[tokio::test]
    async fn left_click_fails_closed_when_no_screenshot_is_on_file_for_the_window() {
        let _guard = computer::process_state_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        computer::clear_control();
        let mock = shared_mock();
        mock.fail_activate.store(false, std::sync::atomic::Ordering::SeqCst);
        *mock.on_activate.lock().unwrap_or_else(|e| e.into_inner()) = None;
        mock.actions.lock().unwrap_or_else(|e| e.into_inner()).clear();
        *mock.windows_override.lock().unwrap_or_else(|e| e.into_inner()) = Some(vec![computer::WindowInfo {
            id: 912_001,
            app: "Fresh".into(),
            title: "never screenshotted".into(),
            x: 0,
            y: 0,
            width: 800,
            height: 600,
        }]);

        let db = crate::store::Db::connect("sqlite::memory:").await.unwrap();
        repo::set_setting(&db, computer::K_COMPUTER_USE_ENABLED, "true").await.unwrap();
        let asks = AskRegistry::new();
        let thread = 912_001;
        let dir = "lead";
        asks.seed_grants(crate::ask::GrantSnapshot {
            full: vec![crate::ask::FullGrant { thread, dir: dir.to_string() }],
            always: Vec::new(),
        });
        // Deliberately NO `computer::record_shot_dims` call for this
        // (thread, dir, 912_001) — this window has never been screenshotted.

        // Clear the global input throttle window so this call isn't itself
        // rejected as rate-limited (which would mask the property under
        // test) — mirrors every other real-input test in this module.
        let _ = computer::throttle_input();
        tokio::time::sleep(std::time::Duration::from_millis(600)).await;

        let mut window_id_out = None;
        let mut image_out = None;
        let err = run_action(
            &db, &asks, thread, dir, None, "computer", "left_click",
            &json!({"window": "fresh", "coordinate": [1, 1]}),
            &mut window_id_out, &mut image_out,
        )
        .await
        .unwrap_err();

        assert!(err.to_lowercase().contains("screenshot"), "{err}");
        assert!(
            mock.actions.lock().unwrap_or_else(|e| e.into_inner()).iter().all(|a| !a.starts_with("click")),
            "a fail-closed coordinate mapping must never let the click itself reach the backend"
        );
        computer::clear_control();
    }

    /// The end-to-end property this whole round exists for (the review's own
    /// example: a 2000px window screenshotted at a downscaled 1280px, then
    /// resized DOWN to 1000px before the next click): a coordinate read off
    /// the SAVED screenshot must map to the SAME proportional position on the
    /// window's CURRENT rectangle — never a position derived from treating
    /// the screenshot as if it were sized to match the window's PRESENT
    /// dimensions (`map_to_physical`'s old bug this round replaces).
    #[tokio::test]
    async fn left_click_maps_the_screenshot_coordinate_proportionally_after_a_resize() {
        let _guard = computer::process_state_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        computer::clear_control();
        let mock = shared_mock();
        mock.fail_activate.store(false, std::sync::atomic::Ordering::SeqCst);
        *mock.on_activate.lock().unwrap_or_else(|e| e.into_inner()) = None;
        mock.actions.lock().unwrap_or_else(|e| e.into_inner()).clear();
        // The window's CURRENT size — already resized DOWN from whatever it
        // was at screenshot time.
        *mock.windows_override.lock().unwrap_or_else(|e| e.into_inner()) = Some(vec![computer::WindowInfo {
            id: 912_101,
            app: "Resizable".into(),
            title: "resizable window".into(),
            x: 0,
            y: 0,
            width: 1000,
            height: 600,
        }]);

        let db = crate::store::Db::connect("sqlite::memory:").await.unwrap();
        repo::set_setting(&db, computer::K_COMPUTER_USE_ENABLED, "true").await.unwrap();
        let asks = AskRegistry::new();
        let thread = 912_101;
        let dir = "lead";
        asks.seed_grants(crate::ask::GrantSnapshot {
            full: vec![crate::ask::FullGrant { thread, dir: dir.to_string() }],
            always: Vec::new(),
        });
        // The screenshot this agent is reading coordinates off of was saved
        // at 1280x800 — BEFORE the window resized down to 1000x600 above.
        // issue #160 round-12 P1 #C: geometry is now ALSO recorded here
        // (the window's own pre-resize rect), but `shot_dims_for` does not
        // gate on it — see that function's own doc for why this exact
        // resize-tolerance property (`left_click_maps_the_screenshot_
        // coordinate_proportionally_after_a_resize`, THIS test) is one of
        // the two reasons it doesn't.
        computer::record_shot_dims(
            thread,
            dir,
            912_101,
            1280,
            800,
            &computer::WindowInfo {
                id: 912_101,
                app: "Resizable".into(),
                title: "resizable window".into(),
                x: 0,
                y: 0,
                width: 1280,
                height: 800,
            },
        );

        // Clear the global input throttle window so this call isn't itself
        // rejected as rate-limited.
        let _ = computer::throttle_input();
        tokio::time::sleep(std::time::Duration::from_millis(600)).await;

        let mut window_id_out = None;
        let mut image_out = None;
        // (640, 400) is the exact midpoint of the 1280x800 screenshot.
        let result = run_action(
            &db, &asks, thread, dir, None, "computer", "left_click",
            &json!({"window": "resizable", "coordinate": [640, 400]}),
            &mut window_id_out, &mut image_out,
        )
        .await;

        assert!(result.is_ok(), "{result:?}");
        let actions = mock.actions.lock().unwrap_or_else(|e| e.into_inner());
        assert!(
            actions.iter().any(|a| a == "click 500,300 Left x1"),
            "must land at the CURRENT window's own midpoint (500, 300 of 1000x600), not a position \
             derived from the window's now-stale 1280x800 screenshot-time size: {actions:?}"
        );
        drop(actions);
        computer::clear_control();
    }

    // —— issue #160 round-12 P1 #5: screenshot capture/encode concurrency cap ——

    /// The mechanism itself: `SCREENSHOT_CONCURRENCY` permits total, so
    /// draining that many exhausts the semaphore and a further acquire must
    /// queue (not resolve) until one of the held permits is released — the
    /// exact property `run_action`'s `screenshot` arm relies on to keep at
    /// most `SCREENSHOT_CONCURRENCY` captures resident at once. Tested
    /// directly against the semaphore rather than through a full end-to-end
    /// `run_action` call: `shared_mock`'s `MockBackend.image` is fixed at
    /// construction (`None`, per its own doc — no test in this module can
    /// configure a delayed/blocking capture), so a real concurrent-capture
    /// race can't be reproduced through the mock; the semaphore this arm
    /// acquires BEFORE ever touching the backend is the actual unit
    /// enforcing the cap, and is what this test exercises.
    #[tokio::test]
    async fn screenshot_semaphore_caps_concurrent_capture_at_the_configured_limit() {
        let mut held = Vec::new();
        for _ in 0..SCREENSHOT_CONCURRENCY {
            held.push(
                screenshot_semaphore()
                    .acquire()
                    .await
                    .expect("semaphore is never closed"),
            );
        }
        assert_eq!(
            screenshot_semaphore().available_permits(),
            0,
            "every permit must be held once SCREENSHOT_CONCURRENCY captures are in flight"
        );

        // A queued (N+1)th caller must not proceed while every permit is held.
        let queued = tokio::spawn(async {
            let _permit = screenshot_semaphore()
                .acquire()
                .await
                .expect("semaphore is never closed");
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(
            !queued.is_finished(),
            "a capture beyond the concurrency cap must queue, never run alongside the others already in flight"
        );

        // Releasing ONE held permit lets the queued caller through.
        held.pop();
        queued
            .await
            .expect("the queued capture must complete once a permit frees up");
    }

    /// issue #160 round-12 P1 #I: the identity re-verification round-12 P1 #5's
    /// OWN capture semaphore reopened. With every `SCREENSHOT_CONCURRENCY`
    /// permit already held (so this call's own `screenshot_semaphore().
    /// acquire().await` must queue for a while), the ORIGINAL window can
    /// close and a same-query REPLACEMENT take its place WHILE the call sits
    /// queued — `approve` and the arm's own FIRST `resolve_and_verify_target`
    /// (both run before the permit is ever touched) only ever saw the
    /// ORIGINAL window. A Full grant (not a fresh card) isolates this from
    /// round-11 P1 #C's own pre-approval gap: authorization already landed
    /// before the call is even queued, so the ONLY window this test
    /// exercises is the post-approval, pre-capture one the semaphore reopens.
    /// Without the round-12 P1 #I re-check, `screenshot_window`'s own
    /// internal re-resolve would silently capture the REPLACEMENT the
    /// instant a permit frees up.
    #[tokio::test]
    async fn screenshot_re_verifies_after_the_capture_semaphore_queue_before_capturing() {
        let _guard = computer::process_state_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        let mock = shared_mock();
        mock.fail_activate.store(false, std::sync::atomic::Ordering::SeqCst);
        *mock.on_activate.lock().unwrap_or_else(|e| e.into_inner()) = None;
        mock.actions.lock().unwrap_or_else(|e| e.into_inner()).clear();
        *mock.windows_override.lock().unwrap_or_else(|e| e.into_inner()) = Some(vec![computer::WindowInfo {
            id: 913_301,
            app: "Queued".into(),
            title: "queued window".into(),
            x: 0,
            y: 0,
            width: 800,
            height: 600,
        }]);

        let db = crate::store::Db::connect("sqlite::memory:").await.unwrap();
        repo::set_setting(&db, computer::K_COMPUTER_USE_ENABLED, "true").await.unwrap();
        let asks = AskRegistry::new();
        let thread = 913_301;
        let dir = "lead";
        asks.seed_grants(crate::ask::GrantSnapshot {
            full: vec![crate::ask::FullGrant { thread, dir: dir.to_string() }],
            always: Vec::new(),
        });

        // Drain every capture permit so this call's own acquire must queue.
        let mut held = Vec::new();
        for _ in 0..SCREENSHOT_CONCURRENCY {
            held.push(screenshot_semaphore().acquire().await.expect("semaphore is never closed"));
        }

        let db_bg = db.clone();
        let asks_bg = asks.clone();
        let handle = tokio::spawn(async move {
            let mut window_id_out = None;
            let mut image_out = None;
            run_action(
                &db_bg, &asks_bg, thread, dir, None, "computer", "screenshot",
                &json!({"window": "queued"}), &mut window_id_out, &mut image_out,
            )
            .await
        });

        // Give the spawned call time to clear approval + its own first
        // verify and queue on the drained semaphore.
        tokio::time::sleep(std::time::Duration::from_millis(80)).await;
        assert!(!handle.is_finished(), "the call must still be queued on the drained capture semaphore");

        // The window is REPLACED while queued: the SAME query still
        // matches, but a DIFFERENT id/app — standing in for the original
        // closing and an unrelated one taking its place during the wait.
        *mock.windows_override.lock().unwrap_or_else(|e| e.into_inner()) = Some(vec![computer::WindowInfo {
            id: 913_302,
            app: "Different App".into(),
            title: "queued window".into(),
            x: 0,
            y: 0,
            width: 800,
            height: 600,
        }]);

        // Free exactly one permit so the queued call proceeds.
        held.pop();

        let err = handle.await.unwrap().unwrap_err();
        assert!(
            err.contains("changed since this action was approved"),
            "must fail closed with the re-approve message, never silently capture the replacement: {err}"
        );
        assert!(
            computer::shot_dims_for(
                thread,
                dir,
                &computer::WindowInfo {
                    id: 913_302,
                    app: "Different App".into(),
                    title: "queued window".into(),
                    x: 0,
                    y: 0,
                    width: 800,
                    height: 600,
                }
            )
            .is_none(),
            "a fail-closed capture must never record shot dims for the replacement window either"
        );

        drop(held);
    }

    // —— issue #160 round-16 P1: list_windows shares the observe semaphore ——

    /// issue #160 round-16 P1 (Codex computer_srv.rs:605): `list_windows` now
    /// acquires the SAME `screenshot_semaphore` a `screenshot` capture does —
    /// closing the "no cap at all on an already-authorized session's
    /// concurrent enumeration" gap this round exists for. `MockBackend` (see
    /// its own doc comment) has no delay-injection hook for `list_windows`
    /// itself, so a genuine concurrent race can't be forced through
    /// `run_action` the way `screenshot_re_verifies_after_the_capture_
    /// semaphore_queue_before_capturing` forces one for capture (that test's
    /// own doc explains the same limitation for `MockBackend.image`) — but
    /// draining the semaphore's permits directly, exactly like
    /// `screenshot_semaphore_caps_concurrent_capture_at_the_configured_limit`
    /// does, reproduces the SAME observable effect without needing a new mock
    /// hook: a `list_windows` call must queue behind those drained permits
    /// rather than run unbounded, and complete successfully once one frees
    /// up. Driven through the full `run_action` dispatch (not the bare
    /// semaphore), so this also covers the "the observe path still returns
    /// the right data through `on_blocking`" regression for this arm.
    #[tokio::test]
    async fn list_windows_queues_behind_the_shared_observe_semaphore_then_succeeds() {
        let _guard = computer::process_state_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        let mock = shared_mock();
        mock.fail_activate.store(false, std::sync::atomic::Ordering::SeqCst);
        *mock.on_activate.lock().unwrap_or_else(|e| e.into_inner()) = None;
        mock.actions.lock().unwrap_or_else(|e| e.into_inner()).clear();
        *mock.windows_override.lock().unwrap_or_else(|e| e.into_inner()) = Some(vec![computer::WindowInfo {
            id: 916_001,
            app: "Listed".into(),
            title: "listed window".into(),
            x: 0,
            y: 0,
            width: 800,
            height: 600,
        }]);

        let db = crate::store::Db::connect("sqlite::memory:").await.unwrap();
        repo::set_setting(&db, computer::K_COMPUTER_USE_ENABLED, "true").await.unwrap();
        let asks = AskRegistry::new();
        let thread = 916_001;
        let dir = "lead";
        asks.seed_grants(crate::ask::GrantSnapshot {
            full: vec![crate::ask::FullGrant { thread, dir: dir.to_string() }],
            always: Vec::new(),
        });

        // Drain every permit the shared observe semaphore has — standing in
        // for `SCREENSHOT_CONCURRENCY` other observe calls already in
        // flight, exactly like the capture-side semaphore test does.
        let mut held = Vec::new();
        for _ in 0..SCREENSHOT_CONCURRENCY {
            held.push(screenshot_semaphore().acquire().await.expect("semaphore is never closed"));
        }

        let db_bg = db.clone();
        let asks_bg = asks.clone();
        let handle = tokio::spawn(async move {
            let mut window_id_out = None;
            let mut image_out = None;
            run_action(
                &db_bg, &asks_bg, thread, dir, None, "computer", "list_windows",
                &json!({}), &mut window_id_out, &mut image_out,
            )
            .await
        });

        tokio::time::sleep(std::time::Duration::from_millis(80)).await;
        assert!(
            !handle.is_finished(),
            "list_windows must queue behind the shared observe semaphore like screenshot does, \
             never run unbounded against an already-authorized grant"
        );

        // Free exactly one permit so the queued call proceeds.
        held.pop();

        let result = handle.await.unwrap();
        assert!(result.is_ok(), "{result:?}");
        // `computer::WindowInfo` only derives `Serialize` (this is the ONE
        // place it's ever sent back out as JSON — see this arm's own
        // dispatch), so assert against the raw `Value` rather than round-
        // tripping through a `Deserialize` this module has no other use for.
        let windows: Value = serde_json::from_str(&result.unwrap()).unwrap();
        let windows = windows.as_array().expect("list_windows returns a JSON array");
        assert_eq!(windows.len(), 1, "{windows:?}");
        assert_eq!(windows[0]["id"], json!(916_001));

        drop(held);
    }

    // —— issue #160 round-12 P2 #G: throttle checked BEFORE the control lease
    // is (re)acquired ——

    /// The end-to-end property this fix exists for: a same-session input
    /// call faster than the global throttle window must be rejected WITHOUT
    /// renewing (extending) the control lease's own expiry. Before this
    /// round, `acquire_and_throttle` renewed the lease FIRST and only THEN
    /// checked the throttle — a loop of calls faster than the throttle
    /// window, each individually rejected, still pushed `expires_at_ms`
    /// forward on every single attempt, keeping the control lease (and the
    /// "an agent is controlling the desktop" banner/OS-level Escape shortcut
    /// it keeps registered) alive indefinitely even though no input actually
    /// ever got through.
    #[test]
    fn acquire_and_throttle_rejects_a_rate_limited_repeat_without_renewing_the_control_lease() {
        let _guard = computer::process_state_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        computer::clear_control();
        // Consume + reset the throttle window far enough in the past that
        // the FIRST call below is not itself rejected as rate-limited.
        let _ = computer::throttle_input();
        std::thread::sleep(std::time::Duration::from_millis(600));

        // First call: nobody holds the lease and the throttle window has
        // already elapsed — both checks pass, taking a fresh lease.
        acquire_and_throttle(931_001, "90").expect("the first call must succeed");
        let after_first = computer::control_state().expect("the lease must be held after the first call");

        // A SECOND call for the SAME (thread, dir), immediately after — well
        // inside the throttle window — must be rejected...
        let err = acquire_and_throttle(931_001, "90")
            .expect_err("a call inside the throttle window must be rejected");
        assert!(err.to_lowercase().contains("rate-limited"), "{err}");

        // ...and the lease's own expiry must be UNCHANGED by that rejected
        // call — proving `acquire_control` was never reached for it.
        let after_second = computer::control_state()
            .expect("the lease must still be held — unaffected by the rejected call");
        assert_eq!(
            after_first.expires_at_ms, after_second.expires_at_ms,
            "a rate-limited call must not renew (extend) the control lease's expiry"
        );

        computer::clear_control();
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

    /// issue #160 round-10 P2 #3 (Codex 1868): a freshly-created audit file
    /// must be owner-only `0600`, never the umask-`022`-survivable `0644` a
    /// bare `open(2)` with no explicit mode would leave it at. Checked via
    /// `symlink_metadata` (not `metadata`) purely out of habit-consistency
    /// with this module's other symlink-aware checks — the leaf here is an
    /// ordinary file either way.
    #[cfg(unix)]
    #[tokio::test]
    async fn open_audit_file_for_append_creates_an_owner_only_file() {
        use std::os::unix::fs::PermissionsExt;
        let base = std::env::temp_dir().join(format!("weft-audit-mode-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let leaf = base.join("computer-audit.jsonl");

        let file = open_audit_file_for_append(&leaf).await.unwrap();
        drop(file);
        let mode = std::fs::symlink_metadata(&leaf).unwrap().permissions().mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "a fresh computer-audit.jsonl must be owner-only (0600), not the umask-survivable default"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    // —— issue #160 round-10 P1 #F: bounded audit-log rotation ——

    /// The core property: a file already AT (or over) the threshold gets
    /// renamed to `<path>.1`, and the original path is free again for a
    /// fresh file (`append_audit`'s subsequent open recreates it).
    #[test]
    fn rotate_audit_at_size_renames_an_over_limit_file_to_dot_one() {
        let base = std::env::temp_dir().join(format!("weft-audit-rotate-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let path = base.join("computer-audit.jsonl");
        std::fs::write(&path, b"0123456789").unwrap(); // 10 bytes

        rotate_audit_at_size(&path, 10);

        assert!(!path.exists(), "the over-limit file must be rotated away from its original path");
        let rotated = base.join("computer-audit.jsonl.1");
        assert_eq!(std::fs::read(&rotated).unwrap(), b"0123456789");

        let _ = std::fs::remove_dir_all(&base);
    }

    /// A file already under the threshold is left completely alone.
    #[test]
    fn rotate_audit_at_size_is_a_no_op_below_the_threshold() {
        let base = std::env::temp_dir().join(format!("weft-audit-rotate-noop-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let path = base.join("computer-audit.jsonl");
        std::fs::write(&path, b"tiny").unwrap();

        rotate_audit_at_size(&path, 1_000);

        assert!(path.exists(), "an under-limit file must never be rotated");
        assert!(!base.join("computer-audit.jsonl.1").exists());

        let _ = std::fs::remove_dir_all(&base);
    }

    /// A pre-existing `.1` is overwritten by the new rotation, not appended
    /// to or left as a stray extra file — total on-disk usage stays bounded
    /// to roughly `2 * max_bytes`.
    #[test]
    fn rotate_audit_at_size_overwrites_an_existing_dot_one() {
        let base = std::env::temp_dir().join(format!("weft-audit-rotate-overwrite-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let path = base.join("computer-audit.jsonl");
        let rotated = base.join("computer-audit.jsonl.1");
        std::fs::write(&rotated, b"stale generation").unwrap();
        std::fs::write(&path, b"0123456789").unwrap();

        rotate_audit_at_size(&path, 10);

        assert_eq!(
            std::fs::read(&rotated).unwrap(),
            b"0123456789",
            "the new rotation must overwrite the stale .1, not sit alongside it"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    /// Missing/nonexistent path: best-effort no-op, never panics.
    #[test]
    fn rotate_audit_at_size_is_a_no_op_when_the_file_does_not_exist() {
        let base = std::env::temp_dir().join(format!("weft-audit-rotate-missing-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let path = base.join("computer-audit.jsonl");

        rotate_audit_at_size(&path, 0);
        assert!(!path.exists());

        let _ = std::fs::remove_dir_all(&base);
    }

    /// Symlink safety: a symlinked audit-log path must never be rotated
    /// (renamed) — `rotate_audit_at_size` refuses it outright rather than
    /// renaming a symlink (which would relocate the LINK, not its target,
    /// but is refused anyway per this function's own doc: `path` no longer
    /// being a REGULAR file at all is reason enough to skip).
    #[cfg(unix)]
    #[test]
    fn rotate_audit_at_size_refuses_a_symlinked_audit_path() {
        let base = std::env::temp_dir().join(format!("weft-audit-rotate-symlink-{}", std::process::id()));
        let outside = std::env::temp_dir().join(format!("weft-audit-rotate-symlink-outside-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::remove_file(&outside);
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(&outside, b"0123456789").unwrap();
        let path = base.join("computer-audit.jsonl");
        std::os::unix::fs::symlink(&outside, &path).unwrap();

        rotate_audit_at_size(&path, 10);

        assert!(path.exists(), "the symlink itself must be left in place");
        assert!(
            !base.join("computer-audit.jsonl.1").exists(),
            "a symlinked source must never be rotated"
        );
        assert_eq!(
            std::fs::read(&outside).unwrap(),
            b"0123456789",
            "the symlink's target must be untouched"
        );

        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::remove_file(&outside);
    }

    /// End-to-end through `append_audit` itself (not just the pure rotation
    /// helper): a pre-existing over-limit audit file rotates to `.1` and the
    /// NEW line lands in a fresh, small file at the original path — proving
    /// the wiring inside `append_audit`, not just `rotate_audit_at_size` in
    /// isolation. Uses `rotate_audit_at_size` directly with a tiny threshold
    /// (rather than writing multiple real megabytes) to stand in for what
    /// `append_audit` itself calls unconditionally before every open.
    #[tokio::test]
    async fn append_audit_rotates_an_over_limit_file_before_writing_the_new_line() {
        let base = std::env::temp_dir().join(format!("weft-audit-rotate-e2e-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let path = base.join("computer-audit.jsonl");
        std::fs::write(&path, b"old-line-1\nold-line-2\n").unwrap();

        rotate_audit_at_size(&path, 5); // well under the real file's size

        let rotated = base.join("computer-audit.jsonl.1");
        assert_eq!(std::fs::read_to_string(&rotated).unwrap(), "old-line-1\nold-line-2\n");

        let mut file = open_audit_file_for_append(&path).await.unwrap();
        use tokio::io::AsyncWriteExt;
        file.write_all(b"new-line\n").await.unwrap();
        // `tokio::fs::File::write_all` only QUEUES the write onto its own
        // background blocking task — an explicit `flush` (which `append_audit`
        // itself does not call, a separate, pre-existing characteristic this
        // test doesn't change) is what this test needs to assert the file's
        // on-disk content deterministically.
        file.flush().await.unwrap();
        drop(file);

        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "new-line\n",
            "the new file at the original path must start fresh, not append to the rotated-away content"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    /// issue #160 round-12 P1 #E: the concurrent-writer race `audit_write_lock`
    /// closes, exercised through genuine concurrency (a multi-threaded
    /// runtime, so the check/rotate/open/write sequence really can interleave
    /// across OS threads without the lock) rather than sequential calls.
    /// Pre-seeds a REAL over-limit file (`MAX_AUDIT_BYTES` + a margin) so the
    /// FIRST writer to actually acquire the lock must rotate it — then fires
    /// many concurrent appends. Without the lock, two callers could each read
    /// the pre-rotation size, both decide to rotate, and race each other's
    /// `rename`/open — losing lines or clobbering the `.1` rotation. With it,
    /// exactly one rotation happens, every line lands, and the live file
    /// never grows anywhere near the cap again for this many short lines.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_appends_serialize_through_rotation_without_losing_or_corrupting_lines() {
        let base = std::env::temp_dir().join(format!("weft-audit-concurrent-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let path = base.join("computer-audit.jsonl");
        std::fs::write(&path, vec![b'x'; (MAX_AUDIT_BYTES + 1024) as usize]).unwrap();

        const N: usize = 25;
        let mut handles = Vec::with_capacity(N);
        for i in 0..N {
            let p = path.clone();
            handles.push(tokio::spawn(async move {
                write_audit_line_locked(&p, &format!("line-{i}\n")).await;
            }));
        }
        for h in handles {
            h.await.unwrap();
        }

        let rotated = base.join("computer-audit.jsonl.1");
        assert!(rotated.exists(), "the pre-seeded over-limit original must have been rotated exactly once");
        assert!(
            std::fs::metadata(&rotated).unwrap().len() >= MAX_AUDIT_BYTES,
            "the rotated-away file must be the ORIGINAL over-limit content, never a torn/mixed one"
        );

        let live = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            live.lines().count(),
            N,
            "every concurrent append must land — none lost to an interleaved rotate/open race: {live:?}"
        );
        assert!(
            (live.len() as u64) < MAX_AUDIT_BYTES / 100,
            "the post-rotation file must stay small — no SECOND rotation was needed for {N} short \
             lines, proving no writer raced ahead of the lock to see a stale over-limit size: \
             {} bytes",
            live.len()
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    /// End-to-end through the async path a real worker's `screenshot`/audit
    /// call takes (`session_root` → `screenshot_out_dir`/`audit_log_path`).
    /// issue #160 round-10 P1 #1 moved the worker lane's output OFF the
    /// worktree entirely and into a dedicated directory under `weft_home` —
    /// so this test's hazard is no longer "a worktree-writable `.weft`"
    /// (there is no `.weft` in the worktree for this path anymore at all —
    /// asserted below) but the residual one `refuse_symlinks`'s own doc now
    /// names: a symlink planted directly at the Weft-managed output
    /// directory (by some OTHER same-uid process) is still refused, as
    /// defense in depth. Needs an ISOLATED `WEFT_HOME` (unlike this module's
    /// other worktree-path tests, which deliberately avoid touching it) —
    /// see `paths::ENV_LOCK`'s own doc for why every WEFT_HOME-touching test
    /// must hold it.
    #[cfg(unix)]
    #[tokio::test]
    async fn screenshot_and_audit_paths_both_refuse_a_symlinked_weft_managed_output_dir() {
        let _env = crate::paths::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let weft_home =
            std::env::temp_dir().join(format!("weft-computer-srv-refuse-sym-home-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&weft_home);
        std::env::set_var("WEFT_HOME", weft_home.to_str().unwrap());

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
        let wt = repo::record_worktree(&db, r.id, direction.id, "b", &wt_path.to_string_lossy(), true, true, "")
            .await
            .unwrap();

        let dir_s = direction.id.to_string();
        // Plant the symlinks at the Weft-managed output directory THIS
        // session resolves to — deliberately NOT inside the worktree, which
        // this round moves output off of entirely.
        let session_dir = crate::paths::computer_output_root()
            .unwrap()
            .join(thread.id.to_string())
            .join(&dir_s)
            .join(format!("wt-{}", wt.id));
        std::fs::create_dir_all(&session_dir).unwrap();
        std::os::unix::fs::symlink(&outside, session_dir.join("screenshots")).unwrap();
        std::os::unix::fs::symlink(&outside, session_dir.join("computer-audit.jsonl")).unwrap();

        let screenshot_err = screenshot_out_dir(&db, thread.id, &dir_s, None).await.unwrap_err();
        assert!(screenshot_err.contains("symlink"), "{screenshot_err}");
        let audit_path = audit_log_path(&db, thread.id, &dir_s, None).await;
        assert!(audit_path.is_none(), "a symlinked output dir must refuse the audit path too");
        assert!(
            !outside.join("computer-audit.jsonl").exists() && !outside.join("screenshots").exists(),
            "must never write through the symlink"
        );

        // issue #160 round-10 P1 #1: the worker lane must never touch the
        // worktree's own `.weft` at all anymore — there is nothing left to
        // `git_exclude` there either (that call is gone).
        assert!(
            !wt_path.join(".weft").exists(),
            "worker-lane computer-use output must never touch the worktree at all"
        );

        std::env::remove_var("WEFT_HOME");
        let _ = std::fs::remove_dir_all(&wt_path);
        let _ = std::fs::remove_dir_all(&outside);
        let _ = std::fs::remove_dir_all(&weft_home);
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

    // —— issue #160 round-10 P2 #4: key combo validated before the lease ——

    /// End-to-end through `run_action`'s own "key" arm (Full-granted, so
    /// `approve` decides silently): a malformed combo (`ctrl+a+b` — a
    /// non-modifier in a non-final position, see `parse_key_combo`'s own
    /// grammar) is rejected before EVER touching the control lease/throttle
    /// or activating the target window — `computer::control_state()` must
    /// stay `None` and the mock backend must never see a `key`/`activate`
    /// call, exactly like `missing_coordinate_click_never_touches_the_
    /// control_lease`'s own property for the click family.
    #[tokio::test]
    async fn run_action_rejects_a_malformed_key_combo_before_touching_the_control_lease() {
        let _guard = computer::process_state_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        computer::clear_control();
        let mock = shared_mock();
        mock.fail_activate.store(false, std::sync::atomic::Ordering::SeqCst);
        *mock.on_activate.lock().unwrap_or_else(|e| e.into_inner()) = None;
        mock.actions.lock().unwrap_or_else(|e| e.into_inner()).clear();
        *mock.windows_override.lock().unwrap_or_else(|e| e.into_inner()) = Some(vec![computer::WindowInfo {
            id: 904_201,
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
        let thread = 904_201;
        let dir = "lead";
        asks.seed_grants(crate::ask::GrantSnapshot {
            full: vec![crate::ask::FullGrant { thread, dir: dir.to_string() }],
            always: Vec::new(),
        });

        let mut window_id_out = None;
        let mut image_out = None;
        let err = run_action(
            &db, &asks, thread, dir, None, "computer", "key",
            &json!({"window": "notes", "text": "ctrl+a+b"}),
            &mut window_id_out, &mut image_out,
        )
        .await
        .unwrap_err();

        assert!(err.contains("ctrl") || err.to_lowercase().contains("key"), "{err}");
        assert!(
            mock.actions.lock().unwrap_or_else(|e| e.into_inner()).is_empty(),
            "a malformed combo must never reach the backend, not even an activate call"
        );
        assert!(
            computer::control_state().is_none(),
            "a rejected malformed combo must never touch the control lease"
        );
        computer::clear_control();
    }

    // —— issue #160 round-10 P2 #5: check_suspended before opening a NEW card ——

    /// The property this fix exists for: a session with an ALREADY-open,
    /// unanswered permission card must reject a Write action BEFORE `approve`
    /// ever opens a SECOND card for it — not just after (the pre-existing
    /// in-arm `check_suspended` calls still catch a card racing in DURING
    /// `approve`'s own await, but this call never reaches that point at all).
    /// Deliberately UNGRANTED (no standing Full/Always grant): if `approve`
    /// ran first here, it would open a real Needs-you card of its own before
    /// this test could even answer it, hanging until `bus::server::ASK_WAIT`
    /// expires. Asserts all three: the pre-existing ask is still the ONLY
    /// open one (no second card), the backend was never touched, and no
    /// grant was recorded for the rejected call.
    #[tokio::test]
    async fn run_action_rejects_a_write_action_before_opening_a_second_card() {
        let _guard = computer::process_state_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        computer::clear_control();
        let mock = shared_mock();
        mock.fail_activate.store(false, std::sync::atomic::Ordering::SeqCst);
        *mock.on_activate.lock().unwrap_or_else(|e| e.into_inner()) = None;
        mock.actions.lock().unwrap_or_else(|e| e.into_inner()).clear();
        *mock.windows_override.lock().unwrap_or_else(|e| e.into_inner()) = Some(vec![computer::WindowInfo {
            id: 904_301,
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
        let thread = 904_301;
        let dir = "lead";
        // An unrelated card is already open for this EXACT (thread, dir) —
        // standing in for some other tool's own permission request racing
        // in first.
        let (existing_ask_id, _rx) = asks.request(
            thread, dir, "some_other_tool", "an unrelated permission request", "detail",
            crate::ask::RiskLevel::Unknown, "[\"unrelated\"]",
        );

        let mut window_id_out = None;
        let mut image_out = None;
        let err = run_action(
            &db, &asks, thread, dir, None, "computer", "left_click",
            &json!({"window": "notes", "coordinate": [1, 1]}),
            &mut window_id_out, &mut image_out,
        )
        .await
        .unwrap_err();

        assert!(err.contains("permission card"), "{err}");
        assert_eq!(
            asks.open().len(),
            1,
            "no SECOND card may open while the original one is still unanswered"
        );
        assert_eq!(asks.open()[0].id, existing_ask_id, "the surviving open ask must be the ORIGINAL one");
        assert!(
            mock.actions.lock().unwrap_or_else(|e| e.into_inner()).is_empty(),
            "the backend must never be touched"
        );
        assert!(
            computer::control_state().is_none(),
            "no control lease may be taken for a call rejected before approve"
        );

        assert!(asks.answer(existing_ask_id, crate::ask::Answer::Deny));
        computer::clear_control();
    }

    #[tokio::test]
    async fn run_action_rejects_a_malformed_action_before_opening_any_card() {
        // issue #160 round-13 P1 (Codex computer_srv.rs:515): a malformed call
        // (here `left_click` with no `coordinate`) must be rejected by pure
        // schema validation BEFORE `approve` — so NO permission card is ever
        // opened for it, even with no standing grant. If validation regressed
        // to run only inside the arm (after `approve`), this call would block
        // on a Needs-you card (no grant, no answer) up to `ASK_WAIT`; the 2s
        // timeout turns that regression into a clean fast failure, never a
        // 3600s hang.
        let _guard = computer::process_state_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        computer::clear_control();
        let mock = shared_mock();
        mock.actions.lock().unwrap_or_else(|e| e.into_inner()).clear();
        *mock.windows_override.lock().unwrap_or_else(|e| e.into_inner()) = Some(vec![computer::WindowInfo {
            id: 904_401,
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
        let asks = AskRegistry::new(); // no grant, no pre-existing open ask
        let thread = 904_401;
        let dir = "lead";

        let mut window_id_out = None;
        let mut image_out = None;
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            run_action(
                &db, &asks, thread, dir, None, "computer", "left_click",
                &json!({"window": "notes"}), // no `coordinate`
                &mut window_id_out, &mut image_out,
            ),
        )
        .await
        .expect("a malformed call must reject synchronously via pure_validate, never block on a card");

        let err = result.unwrap_err();
        assert!(err.contains("coordinate"), "expected a coordinate schema error, got: {err}");
        assert!(
            asks.open().is_empty(),
            "a malformed call must open NO approval card: {:?}",
            asks.open()
        );
        assert!(
            mock.actions.lock().unwrap_or_else(|e| e.into_inner()).is_empty(),
            "the backend must never be touched by a malformed call"
        );
        computer::clear_control();
    }

    /// issue #160 round-15 P1 (Codex commands.rs:1619): a card published by a
    /// caller that straddled the disable transition (passed `run_action`'s
    /// `enabled` gate BEFORE the latch tripped, inserted its card AFTER
    /// `cancel_gui_asks`'s sweep) self-cancels via `approve`'s post-insert
    /// `stop_latched` check — leaving NO surviving card a human could answer
    /// Always/Full into a post-disable grant. Driven by calling `approve`
    /// directly with the latch already tripped (`wait` is Write-classified but
    /// windowless, so no backend is needed) — through `run_action`'s front
    /// door the top gate would already reject, which is exactly why only the
    /// straddler path can reach this insert.
    #[tokio::test]
    async fn approve_self_cancels_a_card_published_after_the_stop_latch_tripped() {
        let _guard = computer::process_state_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        computer::clear_emergency_stop(computer::stop_generation());
        let asks = AskRegistry::new(); // no grant → would otherwise open a card
        let thread = 904_402;
        let dir = "lead";

        computer::trip_stop_latch();
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            approve(&asks, thread, dir, "wait", &json!({"duration_ms": 5})),
        )
        .await
        .expect("the post-insert latch check must fail fast, never block on the card");

        let err = result.unwrap_err();
        assert!(err.contains("disabled"), "expected the disabled rejection, got: {err}");
        assert!(
            asks.open().is_empty(),
            "the straddler's card must be self-cancelled, not left open: {:?}",
            asks.open()
        );

        computer::clear_emergency_stop(computer::stop_generation());
    }

    /// issue #160 round-15 P2 (Codex computer_srv.rs:1288): grant-less OBSERVE
    /// calls stop opening cards once [`MAX_OPEN_OBSERVE_ASKS`] are already
    /// open for this (thread, dir) — the one-over call fails closed with the
    /// backlog error instead of minting an unbounded Ask + waiter per call.
    /// `list_windows` is used (ReadOnly, windowless — no backend resolution),
    /// with each under-cap call parked on its own spawned task awaiting its
    /// card.
    #[tokio::test]
    async fn observe_asks_are_capped_per_session_once_the_backlog_is_full() {
        let _guard = computer::process_state_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        computer::clear_emergency_stop(computer::stop_generation());
        let asks = AskRegistry::new();
        let thread = 904_403;
        let dir = "lead";

        let mut parked = Vec::new();
        for _ in 0..MAX_OPEN_OBSERVE_ASKS {
            let a = asks.clone();
            parked.push(tokio::spawn(async move {
                let _ = approve(&a, thread, dir, "list_windows", &json!({})).await;
            }));
        }
        // Wait (bounded) until all under-cap cards are actually open.
        for _ in 0..200 {
            if asks.open().len() == MAX_OPEN_OBSERVE_ASKS {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(asks.open().len(), MAX_OPEN_OBSERVE_ASKS, "precondition: backlog full");

        // The one-over call must reject immediately — no fourth card.
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            approve(&asks, thread, dir, "list_windows", &json!({})),
        )
        .await
        .expect("an over-cap observe call must fail fast, never open another card");
        let err = result.unwrap_err();
        assert!(err.contains("too many"), "expected the backlog rejection, got: {err}");
        assert_eq!(asks.open().len(), MAX_OPEN_OBSERVE_ASKS, "no card past the cap");

        // Unblock and reap the parked callers.
        for a in asks.open() {
            asks.cancel(a.id);
        }
        for p in parked {
            let _ = p.await;
        }
    }

    // —— issue #160 round-2 P2 §5: multi-worktree `wt` routing ——

    /// issue #160 round-10 P1 #1: `session_root` no longer returns the
    /// worktree's OWN path for the worker lane — it returns a dedicated
    /// namespace under `weft_home/computer/<thread>/<dir>/wt-<id>` (see that
    /// function's own doc). This test now needs an ISOLATED `WEFT_HOME` to
    /// assert deterministic paths — see `paths::ENV_LOCK`'s own doc for why
    /// every WEFT_HOME-touching test must hold it.
    #[tokio::test]
    async fn session_root_wt_pins_the_exact_worktree_and_a_foreign_one_fails_closed() {
        let _env = crate::paths::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let weft_home = std::env::temp_dir()
            .join(format!("weft-computer-srv-session-root-home-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&weft_home);
        std::env::set_var("WEFT_HOME", weft_home.to_str().unwrap());

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
        let expected_root_for = |wt_id: i32| {
            crate::paths::computer_output_root()
                .unwrap()
                .join(thread.id.to_string())
                .join(&dir_s)
                .join(format!("wt-{wt_id}"))
        };

        // An explicit `wt` pins the EXACT worktree's OWN weft-managed
        // namespace, even though it was inserted SECOND.
        let root = session_root(&db, thread.id, &dir_s, Some(wt_b.id)).await.unwrap();
        assert_eq!(root, expected_root_for(wt_b.id));

        // No `wt` at all: unchanged pre-existing "first worktree" fallback —
        // still resolves to wt_a's OWN namespace, keeping the SAME
        // per-worktree isolation the old worktree-rooted path got for free
        // (round-2 P2 §5): two worktrees of the same multi-repo direction
        // never share an output namespace, whether `wt` came from an
        // explicit pin or this fallback.
        let no_wt = session_root(&db, thread.id, &dir_s, None).await.unwrap();
        assert_eq!(no_wt, expected_root_for(wt_a.id));
        assert_ne!(
            root, no_wt,
            "wt_a and wt_b must never resolve to the same output namespace"
        );

        // A `wt` naming a worktree of a DIFFERENT direction is rejected
        // (closed-set validation) — round-8 P2 #7: this must now FAIL CLOSED
        // (`None`), never silently fall back to "first worktree of THIS
        // direction" as it used to. A worker session that explicitly pinned a
        // worktree that's since become invalid must never have its
        // screenshots/audit quietly redirected into a DIFFERENT repo's
        // namespace in this multi-repo direction.
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

        std::env::remove_var("WEFT_HOME");
        let _ = std::fs::remove_dir_all(&weft_home);
    }

    // —— issue #160 round-2 P2 §7: bounded preview registry ——

    /// A throwaway [`VerifiedWindowIdentity`] for tests that only exercise the
    /// registry's capacity/timestamp behavior, where the window identity itself
    /// is irrelevant (issue #160 round-14 P1 changed the stored value to carry
    /// the full identity).
    fn pid(id: u32) -> VerifiedWindowIdentity {
        VerifiedWindowIdentity { id, app: String::new(), title: String::new() }
    }

    #[test]
    fn evict_oldest_if_full_removes_only_the_single_oldest_entry_at_capacity() {
        let mut map: HashMap<(i32, String), (String, VerifiedWindowIdentity, u64)> = HashMap::new();
        for i in 0..MAX_PREVIEWS as i32 {
            map.insert((i, "d".to_string()), (format!("p{i}"), pid(i as u32), i as u64));
        }
        assert_eq!(map.len(), MAX_PREVIEWS);

        // Simulate the (MAX_PREVIEWS + 1)th write: evict, then insert.
        evict_oldest_if_full(&mut map);
        map.insert(
            (MAX_PREVIEWS as i32, "d".to_string()),
            (format!("p{MAX_PREVIEWS}"), pid(MAX_PREVIEWS as u32), MAX_PREVIEWS as u64),
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
        let mut map: HashMap<(i32, String), (String, VerifiedWindowIdentity, u64)> = HashMap::new();
        map.insert((1, "d".to_string()), ("p".to_string(), pid(1), 100));
        evict_oldest_if_full(&mut map);
        assert_eq!(map.len(), 1, "must not evict anything below capacity");
    }

    #[test]
    fn store_screenshot_preview_updating_an_existing_key_never_evicts_anyone() {
        // Fill to capacity with distinct (thread, dir) keys, then refresh the
        // FIRST one again (same key) — this must never trigger an eviction,
        // since it doesn't grow the map.
        for i in 0..MAX_PREVIEWS as i32 {
            store_screenshot_preview(910_000 + i, "lead", format!("p{i}"), pid(i as u32));
        }
        store_screenshot_preview(910_000, "lead", "refreshed".to_string(), pid(999));

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

    /// issue #160 round-14 P1 (Codex computer_srv.rs:1466): the preview registry
    /// stores and matches the FULL window identity (id + app + title), so a
    /// reused numeric id belonging to a DIFFERENT window (different app/title)
    /// no longer matches a stale preview — the id-reuse hazard an id-only
    /// comparison left open.
    #[test]
    fn screenshot_preview_stores_and_matches_the_full_window_identity() {
        let captured = VerifiedWindowIdentity { id: 5, app: "Notes".into(), title: "todo".into() };
        let id_reused = VerifiedWindowIdentity { id: 5, app: "Mail".into(), title: "inbox".into() };
        assert_ne!(
            captured, id_reused,
            "the SAME numeric id with a different app/title must NOT compare equal"
        );

        // Unique (thread, dir) key — no process-wide lock needed (see
        // `process_state_test_lock`'s doc on keyed vs unkeyed state).
        store_screenshot_preview(920_001, "lead", "PREVIEW".to_string(), captured.clone());
        let (data, stored) = last_screenshot_preview(920_001, "lead").unwrap();
        assert_eq!(data, "PREVIEW", "the preview data URI round-trips");
        assert_eq!(stored, captured, "the FULL identity round-trips, not just the id");
        assert_ne!(
            stored, id_reused,
            "a later window that merely reused the numeric id won't match the stored preview"
        );
    }
}
