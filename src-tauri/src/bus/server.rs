//! MCP-over-HTTP for the thread bus. Stateless: each POST yields one SSE
//! `event: message` carrying the JSON-RPC response. Identity is derived from
//! the URL path, never agent input — so an agent can't spoof `from` via tool
//! arguments. This does NOT stop a local process that forges the URL path
//! itself (no auth; an accepted local-first tradeoff).

use crate::ask::{AskRegistry, Decision};
use crate::bus::builtin_allow;
use crate::bus::BusRegistry;
use crate::store::Db;
use axum::{
    extract::{FromRef, Path, Query, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::time::Duration;

/// Shared state for the local server: the in-memory thread bus, the DB (the
/// planner reads the repo map and writes proposals), and the Ask registry (the
/// permission Ask Bridge).
#[derive(Clone)]
pub struct ServerState {
    pub bus: BusRegistry,
    pub db: Db,
    pub asks: AskRegistry,
}

impl FromRef<ServerState> for BusRegistry {
    fn from_ref(s: &ServerState) -> BusRegistry {
        s.bus.clone()
    }
}
impl FromRef<ServerState> for Db {
    fn from_ref(s: &ServerState) -> Db {
        s.db.clone()
    }
}
impl FromRef<ServerState> for AskRegistry {
    fn from_ref(s: &ServerState) -> AskRegistry {
        s.asks.clone()
    }
}

pub fn router(bus: BusRegistry, db: Db, asks: AskRegistry) -> Router {
    Router::new()
        .route("/bus/:thread/:dir/mcp", post(handle).get(get_not_allowed))
        .route(
            "/planner/:thread/mcp",
            post(handle_planner).get(get_not_allowed),
        )
        .route(
            "/curator/:thread/mcp",
            post(handle_curator).get(get_not_allowed),
        )
        .route(
            "/global/mcp",
            post(crate::bus::global::handle_global).get(get_not_allowed),
        )
        .route("/ask/:thread/:dir", post(handle_ask).get(get_not_allowed))
        .route("/health", get(|| async { "ok" }))
        .with_state(ServerState { bus, db, asks })
}

/// How long weft holds a permission Ask before letting the tool fall back to its
/// own prompt. Kept under the hook's own timeout so the fallback is clean.
// Hold the tool call until the human answers in Needs-you. Long by design
// (automation-first): a permission decision is the human's to make, so we wait
// rather than time out into the tool's own hidden TUI prompt. Falls back only if
// truly abandoned. Kept just under the hook/curl ceilings in inject.rs.
const ASK_WAIT: Duration = Duration::from_secs(3600);

/// The `?tool=` query parameter's fallback when a request omits it. Every
/// hook consumer this repo controls hard-codes its own literal
/// `tool=claude|codex|opencode` when it injects the `/ask` URL
/// (`inject.rs::ask_url`), so a normal request never actually hits this
/// default — but PR #146 promoted this same variable from a purely cosmetic
/// card label into the LOOKUP KEY for `builtin_allow::safe_scope`'s
/// auto-approval decision. Before that, defaulting to `"claude"` was
/// harmless; after it, a missing/unrecognized `tool` would silently inherit
/// whichever engine's row happens to sit at that string — today `claude`,
/// the most PERMISSIVE of the three engines' allowlists — rather than
/// degrading to "no scope, surface the card". `UNKNOWN_ENGINE` is guaranteed
/// to match no `SAFE_BUILTINS` row (`safe_scope_is_exact_and_engine_keyed`
/// and `dangerous_builtins_are_never_allowlisted` in `builtin_allow` check
/// every real engine string; this is deliberately none of them), so a
/// missing/forged `tool` param now always surfaces the Needs-you card
/// instead of quietly borrowing the loosest engine's grants. It also makes
/// the card's own tool label honest instead of mislabeling an unknown caller
/// as "Claude Code" (`ToolIcon`/`toolFullName` on the frontend already
/// degrade gracefully for an unrecognized string).
///
/// That guarantee used to rest on convention alone: every existing test
/// checked specific tool names, never the actual `SAFE_BUILTINS` table, so a
/// future row added there with THIS string as its engine would silently defeat
/// the sentinel and no test would notice. `unknown_engine_sentinel_matches_
/// no_safe_builtins_row_structurally` below closes that gap by iterating
/// `builtin_allow::SAFE_BUILTINS` (made `pub(super)` for exactly this) instead
/// of enumerating tool names.
const UNKNOWN_ENGINE: &str = "unknown";

/// The Ask Bridge endpoint. A tool's permission hook POSTs its PreToolUse-style
/// payload here and BLOCKS until the human answers in weft (→ allow/deny) or the
/// wait elapses (→ an explicit deny, issue #96). Every weft-spawned engine runs
/// headless (`-p`/`exec`/`run`, no TTY), so there is no "native interactive
/// prompt" for an ambiguous/empty response to fall back to — and an empty body
/// was fail-OPEN for at least two of the three engines this endpoint serves,
/// confirmed against each hook consumer's OWN code in this repo (not just
/// claude/opencode's documented behavior, which this repo has no control
/// over and could drift):
///   - opencode's hook plugin (`bus/inject.rs::inject_opencode_ask_plugin`)
///     only throws (denies) on the literal string `"deny"` — an empty `{}`
///     leaves `decision` `undefined`, which does NOT throw, i.e. silently
///     ALLOWS.
///   - codex's global hook script (`codex.rs::ensure_codex_hook_in`) is
///     `[ -n "$resp" ] && printf '%s' "$resp"; exit 0` — an empty `$resp`
///     prints nothing and exits 0, and Codex's own hook contract treats
///     "exit 0, no output" as success/continue (fail-open), matching
///     opencode's shape exactly rather than being safe by construction.
///   - claude's behavior on an empty PreToolUse response under headless
///     `-p` is NOT independently verified from this repo's own code (unlike
///     the two above, weft has no local hook-consumer script to read) — it
///     is presumed to deny (or at minimum not silently allow) based on
///     claude's documented behavior, but that is an external claim, not
///     something this file confirms the way it confirms the other two.
/// An explicit, well-formed deny (the same shape a human's real Deny answer
/// produces) removes the ambiguity for all three regardless of exactly how
/// each one's undocumented/unverified empty-response path behaves. Identity
/// (thread/dir) comes from the URL path, not the body.
async fn handle_ask(
    Path((thread, dir)): Path<(i32, String)>,
    Query(q): Query<HashMap<String, String>>,
    State(asks): State<AskRegistry>,
    State(db): State<Db>,
    Json(req): Json<Value>,
) -> Response {
    let tool = q.get("tool").map(|s| s.as_str()).unwrap_or(UNKNOWN_ENGINE);
    let tool_name = req
        .get("tool_name")
        .and_then(|v| v.as_str())
        .unwrap_or("tool");

    // weft's OWN injected MCP tools are never permission-gated: the human
    // governs them through weft's surfaces (Needs-you, the board, the
    // direction-confirm flow), so a per-call prompt to read the task or post
    // to the bus is pure interruption. Short-circuit before summarizing — but
    // ONLY when weft actually injected this server for THIS session. A repo/user
    // MCP server that reused a weft server name (e.g. its own `weft_planner`) in
    // a session where weft never injected it must still surface the card.
    if is_weft_internal_tool(tool_name) {
        if let Some((server, _)) = split_internal_tool(tool_name) {
            if session_injected(&db, thread, &dir, server).await {
                return hook_decision("allow", "weft-internal tool (auto-approved)");
            }
        }
    }

    let (summary, detail, risk, action_key) = summarize(tool_name, req.get("tool_input"));

    // A read-only BUILTIN of the engine itself (claude's Read/Grep/Glob, …) is
    // waved through, so a turn that reads twenty files doesn't cost twenty
    // human clicks (issue #96's 23-minute freeze). Closed allowlist, and every
    // condition below can only SUBTRACT approvals — see `bus::builtin_allow`
    // for why this lives here instead of in the hook's matcher.
    //
    // Distinct from issue #103's read-only grant below, and deliberately
    // narrower: that one is a HUMAN's explicit "trust every read-only action in
    // this session/issue" and then covers the whole `ReadOnly` tier (a `pwd`
    // Bash, an MCP read). This is the zero-configuration default for a handful
    // of named builtins, so it also demands containment — which is why it can
    // apply without anyone having granted anything.
    match builtin_allow::safe_scope(tool, tool_name) {
        // Nothing to point anywhere: the name alone settles it, so this needs
        // neither the risk verdict nor the session's directories.
        Some(builtin_allow::SafeScope::NoTarget) => {
            return hook_decision("allow", "read-only builtin (auto-approved)");
        }
        // The arguments decide. BOTH the independent risk verdict (which is
        // what still catches a credential-shaped file living INSIDE the
        // worktree, e.g. its own `.env`) and containment in the session's own
        // directories must agree before skipping the human.
        Some(builtin_allow::SafeScope::ReadOnlyPath) => {
            if risk == crate::ask::RiskLevel::ReadOnly {
                let roots = session_roots(&db, thread, &dir).await;
                if builtin_allow::paths_contained(req.get("tool_input"), &roots) {
                    return hook_decision("allow", "read-only builtin (auto-approved)");
                }
            }
        }
        None => {}
    }

    // A standing rule (full access / always-allow / issue #103's read-only
    // batch-or-issue grant) decides without surfacing. Matches on the
    // canonical action_key, NOT the (possibly lossy) summary; `risk` gates the
    // read-only grants (never widens Full/Always, which ignore it entirely).
    if asks.auto_decision(thread, &dir, risk, &action_key) == Some(Decision::Allow) {
        return hook_decision("allow", "Auto-approved by a weft rule");
    }

    let (id, rx) = asks.request(thread, &dir, tool, &summary, &detail, risk, &action_key);

    match tokio::time::timeout(ASK_WAIT, rx).await {
        Ok(Ok(decision)) => {
            let (d, reason) = match decision {
                Decision::Allow => ("allow", "Approved in weft"),
                Decision::Deny => ("deny", "Denied in weft"),
            };
            hook_decision(d, reason)
        }
        // Timed out, or the sender was dropped (e.g. `AskRegistry::cancel_for`
        // tearing down this ask because its engine is being switched/stopped —
        // issue #96): drop the card and return an EXPLICIT deny — see this
        // function's doc for why an empty body is the wrong fallback here.
        _ => {
            asks.cancel(id);
            hook_decision("deny", "No answer in time — denied by default (weft ask bridge)")
        }
    }
}

/// The EXACT `(server, tool)` pairs the Ask Bridge auto-approves. This is a
/// closed allowlist, deliberately not a `weft_*` server-name match:
///
/// - Provenance: a user/repo MCP server that happens to share a weft server name
///   only bypasses the card for these exact known calls; any OTHER tool it
///   exposes (e.g. `mcp__weft_planner__deploy`) still surfaces the Needs-you card.
/// - `weft_global`'s `answer_permission` is EXCLUDED on purpose: auto-approving
///   it would let an agent answer an existing Bash/file permission ask as
///   allow/always/full and erase the very boundary this bridge guards, so it
///   stays gated even though weft injected the server.
///
/// The included writes (bus posts, task status, proposals, edge calibration) are
/// governed by weft's own surfaces (Needs-you, the board, the direction-confirm
/// flow), so a per-call prompt for them is pure interruption.
const AUTO_APPROVED_INTERNAL_TOOLS: &[(&str, &str)] = &[
    // weft_bus — thread bus: reads, weft-governed posts, and ask_human (which
    // itself surfaces to the human).
    ("weft_bus", "bus_post"),
    ("weft_bus", "bus_broadcast"),
    ("weft_bus", "bus_inbox"),
    ("weft_bus", "ask_human"),
    ("weft_bus", "thread_state_get"),
    ("weft_bus", "thread_state_set"),
    ("weft_bus", "announce_interface_change"),
    ("weft_bus", "set_task_status"),
    ("weft_bus", "register_pr"),
    // weft_planner — lead read-only planning; proposals are confirmed by the
    // human downstream in the direction-confirm flow.
    ("weft_planner", "get_task"),
    ("weft_planner", "get_repo_map"),
    ("weft_planner", "get_test_cases"),
    ("weft_planner", "propose_directions"),
    ("weft_planner", "cancel_directions"),
    // weft_curator — repo-map calibration in the curator chat.
    ("weft_curator", "get_repo_map"),
    ("weft_curator", "calibrate_edges"),
    ("weft_curator", "reanalyze"),
    ("weft_curator", "set_classification"),
    // weft_global — concierge/IM helper. answer_permission is intentionally absent.
    ("weft_global", "answer_question"),
    ("weft_global", "create_issue"),
    ("weft_global", "create_issue_from_im"),
    ("weft_global", "ensure_issue_topic"),
    ("weft_global", "ensure_issue_im_topic"),
    ("weft_global", "issue_status"),
    ("weft_global", "list_issues"),
    ("weft_global", "list_workspaces"),
    ("weft_global", "message_lead"),
    ("weft_global", "pending_needs_you"),
];

/// Extract `(server, tool)` from an agent-reported MCP tool name. ONLY the
/// claude-style `mcp__<server>__<tool>` form is recognized — its `__` delimiter is
/// unambiguous. opencode flattens MCP names as `<server>_<tool>` with a single
/// `_`, and weft server names contain `_`, so a project tool such as server `weft`
/// + tool `bus_thread_state_set` is indistinguishable from `weft_bus` +
/// `thread_state_set`; parsing that by prefix could auto-approve a foreign tool.
/// So opencode-form names are NOT matched here — they surface the Needs-you card
/// (fail-safe). Auto-approving opencode internals would need an unambiguous
/// server-identity signal, which the flat tool name alone doesn't carry.
fn split_internal_tool(tool_name: &str) -> Option<(&str, &str)> {
    tool_name
        .strip_prefix("mcp__")
        .and_then(|rest| rest.split_once("__"))
}

/// A tool weft itself injected AND that is safe to auto-approve. The Ask Bridge
/// skips the permission card for these; everything else (including a weft server
/// name paired with an unknown tool, and `answer_permission`) surfaces normally.
fn is_weft_internal_tool(tool_name: &str) -> bool {
    split_internal_tool(tool_name)
        .is_some_and(|pair| AUTO_APPROVED_INTERNAL_TOOLS.contains(&pair))
}

/// The weft servers a lead-family session injects, by thread `kind`. MIRRORS the
/// injection branch in `lead_chat::commands::start_lead` — the one place that
/// decides what each session gets:
///   concierge → weft_global (never the per-thread bus)
///   curator   → weft_curator + weft_bus
///   per-issue lead (any other kind) → weft_planner + weft_bus
fn session_servers_for_kind(kind: &str) -> &'static [&'static str] {
    match kind {
        "concierge" => &["weft_global"],
        "curator" => &["weft_curator", "weft_bus"],
        _ => &["weft_planner", "weft_bus"],
    }
}

/// Whether weft injected `server` for the session identified by (thread, dir).
/// A worker lane (its ask `dir` is a direction id, not `LEAD`) injects only the
/// bus. The lead family keys off the thread kind and FAILS CLOSED: if the thread
/// can't be resolved — a deleted thread, a DB error, an engine/hook outliving its
/// thread — nothing is auto-approved and the tool surfaces the Needs-you card.
async fn session_injected(db: &Db, thread: i32, dir: &str, server: &str) -> bool {
    if dir != crate::bus::LEAD {
        // Worker lane: only the bus, and only when `dir` is a REAL direction of
        // this thread. Fail closed for a stale/deleted direction or a forged
        // route (an engine/hook outliving its direction, a leftover .weft-ask).
        if server != "weft_bus" {
            return false;
        }
        let Ok(direction_id) = dir.parse::<i32>() else {
            return false;
        };
        return matches!(
            crate::store::repo::get_direction(db, direction_id).await,
            Ok(Some(d)) if d.thread_id == thread
        );
    }
    match crate::store::repo::get_thread(db, thread).await {
        Ok(Some(t)) => session_servers_for_kind(&t.kind).contains(&server),
        _ => false,
    }
}

/// The directories a session is entitled to READ without asking — the "working
/// directory and additional directories" that claude's own permission model
/// scopes `Read`/`Grep`/`Glob` to, expressed in weft's terms.
///
/// Derived from weft's OWN database, never from the hook payload: identity is
/// the (thread, dir) pair in the URL path, and the paths come from the rows
/// weft wrote when it created the session. A payload field (`cwd`) or an
/// injected route file would both be things a repo could plant — see the
/// planted-`.weft-codex-ask-url` defense in `codex::ensure_codex_hook_in` for
/// the same threat.
///
/// - Worker lane (`dir` is a direction id): that direction's worktrees, one per
///   repo it writes. NOT the canonical repos those worktrees came from —
///   workers are isolated to their worktree by design, so reading the shared
///   checkout stays a visible decision.
/// - Lead (`dir` == `LEAD`): its scratch cwd (`<weft_home>/leads/<thread>`,
///   see `lead_chat::commands::ensure_lead_cwd`) plus the local checkouts of
///   its workspace's repos — a lead plans ACROSS those repos and reads them
///   constantly, which is precisely the traffic that froze a lead for 23
///   minutes in dogfooding.
///
/// Returns pre-canonicalized paths (`builtin_allow::contained` compares real
/// locations, and this way each root is resolved once per ask rather than once
/// per path). A root that can't be canonicalized — deleted worktree, repo moved
/// out from under weft — is DROPPED rather than compared as a raw string.
///
/// FAILS CLOSED at every step: a DB error, a deleted thread/direction, or a
/// direction belonging to a different thread all yield an empty (or narrowed)
/// list, and `builtin_allow::paths_contained` then refuses every absolute path,
/// so the call surfaces the Needs-you card exactly as it does today.
async fn session_roots(db: &Db, thread: i32, dir: &str) -> Vec<std::path::PathBuf> {
    let mut roots: Vec<std::path::PathBuf> = Vec::new();
    if dir != crate::bus::LEAD {
        let Ok(direction_id) = dir.parse::<i32>() else {
            return roots;
        };
        // Same ownership check `session_injected` makes: a direction id that
        // isn't THIS thread's is a stale or forged route, not a session.
        match crate::store::repo::get_direction(db, direction_id).await {
            Ok(Some(d)) if d.thread_id == thread => {}
            _ => return roots,
        }
        if let Ok(worktrees) = crate::store::repo::list_worktrees(db, Some(direction_id)).await {
            roots.extend(worktrees.into_iter().map(|w| std::path::PathBuf::from(w.path)));
        }
        return canonicalized(roots);
    }
    let Ok(Some(thread_row)) = crate::store::repo::get_thread(db, thread).await else {
        return roots;
    };
    if let Ok(home) = crate::paths::weft_home() {
        roots.push(home.join("leads").join(thread.to_string()));
    }
    if let Ok(repos) = crate::store::repo::list_repos(db, thread_row.workspace_id).await {
        roots.extend(
            repos
                .into_iter()
                .map(|r| std::path::PathBuf::from(r.local_git_path)),
        );
    }
    canonicalized(roots)
}

/// Resolve each root to its real location, dropping the ones that no longer
/// exist. An empty string (an unset `local_git_path` on a legacy row) would
/// canonicalize to the PROCESS's cwd and silently admit everything under it, so
/// it is filtered before the syscall.
fn canonicalized(roots: Vec<std::path::PathBuf>) -> Vec<std::path::PathBuf> {
    roots
        .into_iter()
        .filter(|p| !p.as_os_str().is_empty())
        .filter_map(|p| std::fs::canonicalize(p).ok())
        .collect()
}

/// The PreToolUse hook response carrying a permission decision.
fn hook_decision(decision: &str, reason: &str) -> Response {
    Json(json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": decision,
            "permissionDecisionReason": reason
        }
    }))
    .into_response()
}

/// A short human label + raw detail + danger tier + canonical action key for a
/// tool action. Tool-agnostic across claude (Bash / file_path) and opencode
/// (bash / filePath, lowercase names): a command reads as "Run: …", a file op
/// as "<tool> <file>".
///
/// Returns `(summary, detail, risk, action_key)`: `summary` is a compact
/// DISPLAY label that MAY truncate (a multi-line command's first line, a bare
/// MCP tool name); `detail` is the FULL raw content (untruncated — shown in
/// the detail tooltip / IM plain-text card); `risk` is the danger tier for
/// the human's one-glance triage, computed by the single shared
/// `crate::ask::classify_risk` (issue #101); `action_key` is the EXACT action
/// identity used ONLY for Always-grant matching (`auto_decision`), never
/// shown to the human — a later ask sharing `summary` but not `action_key`
/// must NOT auto-allow (issue #89).
///
/// Each branch tags its `action_key` with a fixed literal kind ("cmd" / "file" /
/// "mcp") via `crate::ask::action_key`, THEN folds in `tool_name` and the exact
/// content — never a bare `format!("{tool_name}:{content}")` join. Without the
/// kind tag, the SAME `tool_name` used across two different input shapes (e.g. a
/// tool that sometimes sends `{"command": "X"}` and other times `{"file_path":
/// "X"}`) would collide into the identical joined string whenever the content
/// matched, letting an Always for one silently cover the other (see #89's
/// round-2 finding — a fresh instance of the exact over-broad-match bug this
/// issue exists to eliminate).
// `pub(crate)` (not `pub`) — crate-internal only, but visible to
// `lead_chat::engine`'s test module for the cross-engine consistency
// regression test (issue #101 round-2 P3): the SAME command text must yield
// the SAME `RiskLevel` from both `summarize` and `codex_approval_fields`.
pub(crate) fn summarize(
    tool_name: &str,
    input: Option<&Value>,
) -> (String, String, crate::ask::RiskLevel, String) {
    let s = |k: &str| {
        input
            .and_then(|v| v.get(k))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
    };
    if let Some(cmd) = s("command") {
        let first = cmd.lines().next().unwrap_or("").to_string();
        // action_key = the full, untruncated command — a later line differing
        // (e.g. a multi-line command sharing only its first line) is a DIFFERENT
        // action even though `summary` collides. `risk` scans that SAME full
        // text, so a dangerous second line still raises the tier.
        let action_key = crate::ask::action_key(&["cmd", tool_name, &cmd]);
        let risk = crate::ask::classify_risk(crate::ask::RiskSignal::Command(&cmd));
        return (format!("Run: {first}"), cmd, risk, action_key);
    }
    if let Some(f) = s("file_path").or_else(|| s("filePath")) {
        // action_key folds in the tool name too: `Read` and `Write` on the same
        // path are different actions, even though both already show the full
        // (untruncated) path in `summary` today.
        let action_key = crate::ask::action_key(&["file", tool_name, &f]);
        let risk = crate::ask::classify_risk(crate::ask::RiskSignal::File {
            tool_name,
            path: &f,
        });
        return (format!("{tool_name} {f}"), f.clone(), risk, action_key);
    }
    let detail = input.map(|v| v.to_string()).unwrap_or_default();
    // MCP/fallback ask: `summary` is just the bare tool name (lossy for
    // display — e.g. "WebFetch"), but `action_key` folds in the full args so two
    // calls to the same tool with different args are different actions (issue
    // #89's MCP tool-name-fallback case). `risk` scans the same full args text.
    let action_key = crate::ask::action_key(&["mcp", tool_name, &detail]);
    let risk = crate::ask::classify_risk(crate::ask::RiskSignal::Other {
        tool_name,
        args_text: &detail,
    });
    (tool_name.to_string(), detail, risk, action_key)
}

async fn get_not_allowed() -> StatusCode {
    StatusCode::METHOD_NOT_ALLOWED
}

/// One SSE event carrying `value`.
fn sse(value: Value) -> Response {
    let body = format!("event: message\ndata: {}\n\n", value);
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/event-stream")],
        body,
    )
        .into_response()
}

// `thread`/`dir` come from the URL path, so an agent can't spoof its identity
// via tool arguments; it does NOT defend against a local process forging the
// path (no auth — local-first tradeoff).
async fn handle(
    Path((thread, dir)): Path<(i32, String)>,
    State(reg): State<BusRegistry>,
    State(db): State<Db>,
    Json(req): Json<Value>,
) -> Response {
    // Notifications (no id) get a bare 202.
    let id = match req.get("id") {
        Some(v) => v.clone(),
        None => return StatusCode::ACCEPTED.into_response(),
    };
    let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
    reg.join(thread, &dir);

    let result: Value = match method {
        "initialize" => json!({
            "protocolVersion": "2024-11-05",
            "capabilities": { "tools": { "listChanged": false } },
            "serverInfo": { "name": "weft_bus", "version": "1.0.0" }
        }),
        "tools/list" => json!({ "tools": tool_specs() }),
        "tools/call" => {
            let name = req
                .pointer("/params/name")
                .and_then(|n| n.as_str())
                .unwrap_or("");
            let args = req
                .pointer("/params/arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            // set_task_status and register_pr write the DB (the task is
            // `dir`); the rest are in-memory bus ops.
            if name == "set_task_status" {
                let status = args.get("status").and_then(|v| v.as_str()).unwrap_or("");
                set_task_status_tool(&db, &dir, status).await
            } else if name == "register_pr" {
                let url = args.get("url").and_then(|v| v.as_str()).unwrap_or("");
                let title = args.get("title").and_then(|v| v.as_str()).unwrap_or("");
                register_pr_tool(&db, thread, &dir, url, title).await
            } else {
                call_tool(&reg, thread, &dir, name, &args)
            }
        }
        _ => json!({}),
    };

    sse(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
}

/// Bus tool: the agent sets its own task's lifecycle status. `dir` is the
/// direction id from the URL path, so the agent can't move another task.
async fn set_task_status_tool(db: &Db, dir: &str, status: &str) -> Value {
    let allowed = ["queued", "planning", "working", "review", "done"];
    if !allowed.contains(&status) {
        return text_result(format!(
            "invalid status '{status}'; use one of: queued, planning, working, review, done"
        ));
    }
    match dir.parse::<i32>() {
        Ok(id) => match crate::store::repo::set_direction_status(db, id, status).await {
            Ok(()) => text_result(format!("status set to {status}")),
            Err(e) => text_result(format!("error: {e}")),
        },
        Err(_) => text_result("this session has no task to update".into()),
    }
}

/// Bus tool: track a just-opened PR/MR (issue #110 T1). `dir` is the
/// direction id from the URL path (same identity-can't-be-spoofed guarantee
/// `set_task_status_tool` relies on) — the task that owns this PR/MR. Only
/// `url` is required: everything else (host kind, owner, repo, number) is
/// PARSED from it (`host::parse_pr_url`) rather than trusted as separate
/// free-text args that could drift from the URL the agent actually got back
/// from `gh pr create` / a future `glab mr create`. The background monitor
/// (`host::monitor::spawn_pr_watch`) picks up newly-registered rows on its
/// own next sweep — this tool only ever writes the DB row, never calls a host
/// API itself.
async fn register_pr_tool(db: &Db, thread: i32, dir: &str, url: &str, title: &str) -> Value {
    let Some(parts) = crate::host::parse_pr_url(url) else {
        return text_result(format!(
            "could not parse a PR/MR number and repo from '{url}' — expected a GitHub pull request URL (…/pull/N) or a GitLab merge request URL (…/-/merge_requests/N)"
        ));
    };
    // Reject an unimplemented host BEFORE creating a row, not after: a row
    // for a host with no working `PrHost` backend would sweep-fail forever
    // (every ~60s) with no way to self-clear (the readiness question never
    // resolves) and no manual dismiss (a non-answerable Needs-you NOTICE has
    // no close button — see `NeedsRows.tsx`'s `AskRow`). For GitLab
    // specifically — the exact host this issue's user explicitly needs
    // supported — that would mean the FIRST time anyone tries it, they get a
    // permanently-stuck card. Fail the registration itself, honestly, instead.
    if crate::host::resolve_host(parts.host_kind).is_err() {
        return text_result(format!(
            "{} tracking isn't supported yet — weft's PR/MR automation currently only implements GitHub. This {} was NOT registered; please track it yourself for now.",
            parts.host_kind.native_noun(),
            parts.host_kind.native_noun()
        ));
    }
    let direction_id = dir.parse::<i32>().unwrap_or(0);
    let repo_id = match crate::store::repo::get_direction(db, direction_id).await {
        Ok(Some(d)) => d.repo_id,
        _ => 0,
    };
    match crate::store::repo::register_pull_request(
        db,
        thread,
        direction_id,
        repo_id,
        parts.host_kind.as_str(),
        &parts.host_base,
        &parts.owner,
        &parts.repo,
        parts.number,
        url,
        title,
    )
    .await
    {
        Ok(pr) => text_result(format!(
            "tracking {} #{} — weft will monitor its CI/review/conflict state in the background and post to Needs-you if it needs your attention",
            parts.host_kind.native_noun(),
            pr.number
        )),
        Err(e) => text_result(format!("error: {e}")),
    }
}

fn text_result(s: String) -> Value {
    json!({ "content": [{ "type": "text", "text": s }] })
}

fn call_tool(reg: &BusRegistry, thread: i32, me: &str, name: &str, args: &Value) -> Value {
    let s = |k: &str| {
        args.get(k)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    };
    match name {
        "bus_post" => {
            reg.post(thread, me, &s("to"), &s("text"), "message");
            text_result(format!("posted to {}", s("to")))
        }
        "bus_broadcast" => {
            reg.broadcast(thread, me, &s("text"), "message");
            text_result("broadcast sent".into())
        }
        "announce_interface_change" => {
            reg.broadcast(thread, me, &s("summary"), "interface");
            text_result("interface change announced".into())
        }
        "bus_inbox" => {
            let msgs = reg.inbox(thread, me);
            text_result(serde_json::to_string(&msgs).unwrap_or_else(|_| "[]".into()))
        }
        "ask_human" => {
            let id = reg.ask_human(thread, me, &s("text"));
            text_result(format!(
                "asked the human (ask #{id}); their answer will arrive in your bus_inbox — keep working and check it"
            ))
        }
        "thread_state_get" => text_result(reg.state_get(thread).to_string()),
        "thread_state_set" => {
            let patch = args.get("patch").cloned().unwrap_or_else(|| json!({}));
            reg.state_set(thread, patch);
            text_result("state updated".into())
        }
        _ => text_result(format!("unknown tool: {name}")),
    }
}

// ---- planner MCP (lead-only, per thread) ----

async fn handle_planner(
    Path(thread): Path<i32>,
    State(db): State<Db>,
    Json(req): Json<Value>,
) -> Response {
    let id = match req.get("id") {
        Some(v) => v.clone(),
        None => return StatusCode::ACCEPTED.into_response(),
    };
    let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");

    let result: Value = match method {
        "initialize" => json!({
            "protocolVersion": "2024-11-05",
            "capabilities": { "tools": { "listChanged": false } },
            "serverInfo": { "name": "weft_planner", "version": "1.0.0" }
        }),
        "tools/list" => json!({ "tools": planner_specs() }),
        "tools/call" => {
            let name = req
                .pointer("/params/name")
                .and_then(|n| n.as_str())
                .unwrap_or("");
            let args = req
                .pointer("/params/arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            call_planner(&db, thread, name, &args).await
        }
        _ => json!({}),
    };
    sse(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
}

/// MCP for the workspace curator chat: read the dependency graph and apply human
/// calibrations to it. Mirrors `handle_planner`; identity (the curator thread)
/// comes from the URL path.
async fn handle_curator(
    Path(thread): Path<i32>,
    State(db): State<Db>,
    Json(req): Json<Value>,
) -> Response {
    let id = match req.get("id") {
        Some(v) => v.clone(),
        None => return StatusCode::ACCEPTED.into_response(),
    };
    let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
    let result: Value = match method {
        "initialize" => json!({
            "protocolVersion": "2024-11-05",
            "capabilities": { "tools": { "listChanged": false } },
            "serverInfo": { "name": "weft_curator", "version": "1.0.0" }
        }),
        "tools/list" => json!({ "tools": curator_specs() }),
        "tools/call" => {
            let name = req
                .pointer("/params/name")
                .and_then(|n| n.as_str())
                .unwrap_or("");
            let args = req
                .pointer("/params/arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            call_curator(&db, thread, name, &args).await
        }
        _ => json!({}),
    };
    sse(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
}

fn curator_specs() -> Value {
    json!([
        {
            "name": "get_repo_map",
            "description": "Read the workspace repos and their current dependency edges (ids, tier, category/role, summaries, components, path) plus each repo's analysis_state (\"failed\" repos carry an analysis_error — automatic passes couldn't classify them; tell the human if they ask why a repo is unclassified). Use the ids when calling calibrate_edges or set_classification.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "calibrate_edges",
            "description": "Add or remove ONE cross-repo dependency edge after inspecting the code (READ-ONLY — never modify files). `from`/`to` are repo ids from get_repo_map and MUST differ. `kind` ∈ http|grpc|queue|infra|lib. `action` ∈ add|remove. `via` is a short evidence label (e.g. \"POST /orders\"). Human-set edges are pinned and survive automatic re-analysis; removals are remembered so the agent won't re-add them.",
            "inputSchema": { "type": "object", "properties": {
                "from": { "type": "integer" },
                "to": { "type": "integer" },
                "kind": { "type": "string" },
                "via": { "type": "string" },
                "action": { "type": "string", "enum": ["add", "remove"] }
            }, "required": ["from", "to", "kind", "action"] }
        },
        {
            "name": "reanalyze",
            "description": "Run a fresh dependency-analysis pass over the WHOLE workspace: re-classify each repo (tier/stack/summary) and re-infer cross-repo runtime/infra edges, then regenerate the map. Call this when the human asks to re-analyze / regenerate the repo map (e.g. after repos changed). Takes no arguments; returns when the pass completes, with the resulting repo/edge counts. Human-pinned edges survive the pass.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "set_classification",
            "description": "Fix ONE repo's architectural classification when get_repo_map shows it wrong (e.g. a frontend/local SDK mislabeled as a backend service). `repo` is the repo id from get_repo_map. `tier` ∈ frontend|backend. `category` is the role within that tier (free text, e.g. gateway|biz|core|common|idl|support for backend; app|sdk|web for frontend). Human/curator-set classification is PINNED and survives automatic re-analysis.",
            "inputSchema": { "type": "object", "properties": {
                "repo": { "type": "integer" },
                "tier": { "type": "string", "enum": ["frontend", "backend"] },
                "category": { "type": "string" }
            }, "required": ["repo", "tier"] }
        }
    ])
}

async fn call_curator(db: &Db, thread: i32, name: &str, args: &Value) -> Value {
    match name {
        "get_repo_map" => match curator_map_json(db, thread).await {
            Ok(v) => text_result(v),
            Err(e) => text_result(format!("error: {e}")),
        },
        "calibrate_edges" => calibrate_edges_tool(db, thread, args).await,
        "set_classification" => set_classification_tool(db, thread, args).await,
        "reanalyze" => match reanalyze_tool(db, thread).await {
            Ok(v) => text_result(v),
            Err(e) => text_result(format!("error: {e}")),
        },
        _ => text_result(format!("unknown tool {name}")),
    }
}

/// Run a full workspace analysis pass for the curator's workspace and return a
/// summary. Awaited inline (NOT detached) so the agent's turn stays busy for the
/// pass's whole duration — subsequent user messages queue, and the UI's `analyzing`
/// flag (derived from the lead turn) accurately tracks it. Clicking 中止 flips a
/// cancel flag the pass checks at safe points (between repos, before the relation
/// pass) and returns cleanly — see the cooperative-cancellation note in curator.rs.
async fn reanalyze_tool(db: &Db, thread: i32) -> anyhow::Result<String> {
    let t = crate::store::repo::get_thread(db, thread)
        .await?
        .ok_or_else(|| anyhow::anyhow!("thread not found"))?;
    if t.kind != "curator" {
        anyhow::bail!("reanalyze is only available in the curator chat");
    }
    let ws_id = t.workspace_id;
    // Register the cancel token BEFORE the (multi-repo, disk-stat) checkout preflight,
    // so a Stop during it trips THIS call's token instead of being dropped. Everything
    // after registration runs through the helper so the token is always unregistered.
    let cancel = crate::curator::register_analysis_cancel(thread);
    let out = reanalyze_after_register(db, ws_id, &cancel).await;
    crate::curator::unregister_analysis_cancel(thread, &cancel);
    out
}

/// The post-registration body of `reanalyze_tool` (checkout preflight + the cancellable
/// forced pass + the summary), split out so `reanalyze_tool` always unregisters the
/// cancel token afterwards.
async fn reanalyze_after_register(
    db: &Db,
    ws_id: i32,
    cancel: &std::sync::atomic::AtomicBool,
) -> anyhow::Result<String> {
    // Every tracked repo's checkout gone → the pass filters them all out and would
    // analyze nothing, leaving the stale graph to read as a clean "complete". Tell
    // the human instead (matches the behavior of the removed analyze command).
    let repos = crate::store::repo::list_repos(db, ws_id).await?;
    if !repos.is_empty()
        && !repos
            .iter()
            .any(|r| std::path::Path::new(&r.local_git_path).exists())
    {
        return Ok("Could not re-analyze: every repository's checkout is missing on disk \
                   (moved or deleted). Restore the repos and try again."
            .to_string());
    }
    // Clicking 中止 interrupts the lead turn and calls `cancel_analysis(thread)`, which
    // trips the token; `reanalyze_workspace` checks it at safe points (between repos,
    // before the relation pass) and returns whether it ACTUALLY bailed — so a Stop that
    // lands after the pass already finished its relation write reports "complete", not
    // "cancelled". The pass runs directly under the gate lock (not via the coalescing
    // flags), so the cancel can't drop unrelated requests. Worker-chat "stop = stop".
    if crate::curator::reanalyze_workspace(db, ws_id, cancel).await {
        return Ok("Re-analysis cancelled.".to_string());
    }
    let g = crate::curator::graph(db, ws_id).await?;
    // Surface repos the pass left unclassified so the human sees them in the chat (the
    // map node renders them as plain "未分析"). Shared with the direct button command via
    // `unanalyzed_repo_names` (failed classification OR missing checkout).
    let unanalyzed = crate::curator::unanalyzed_repo_names(db, ws_id).await;
    let mut msg = format!(
        "Re-analysis complete: {} repos, {} dependency links. The repo map has been refreshed.",
        g.nodes.len(),
        g.edges.len()
    );
    if !unanalyzed.is_empty() {
        msg.push_str(&format!(
            " Note: {} repo(s) could not be analyzed and stayed unclassified (classifier \
             error or missing checkout): {}. Tell the human, who can re-run the analysis.",
            unanalyzed.len(),
            unanalyzed.join(", ")
        ));
    }
    Ok(msg)
}

/// Like `repo_map_json`, but every node carries its full `local_git_path` — the
/// curator agent must read each repo to find evidence, and the system-prompt
/// repo list is capped/truncated, so paths can't be sourced from there alone.
async fn curator_map_json(db: &Db, thread: i32) -> anyhow::Result<String> {
    let t = crate::store::repo::get_thread(db, thread)
        .await?
        .ok_or_else(|| anyhow::anyhow!("thread not found"))?;
    let g = crate::curator::graph(db, t.workspace_id).await?;
    let path_of: std::collections::HashMap<i32, String> =
        crate::store::repo::list_repos(db, t.workspace_id)
            .await?
            .into_iter()
            .map(|r| (r.id, r.local_git_path))
            .collect();
    let nodes: Vec<Value> = g
        .nodes
        .iter()
        .map(|n| {
            json!({
                "repo_id": n.repo_id,
                "repo_name": n.repo_name,
                "tier": n.tier,
                "category": n.category,
                "stack": n.stack,
                "summary": n.summary,
                "components": n.components,
                "path": path_of.get(&n.repo_id).cloned().unwrap_or_default(),
                // Per-repo analysis status, so the agent can tell the human which repos
                // an automatic (add/backfill/resume) pass failed to analyze — those
                // never flow through reanalyze's chat summary and the map node just
                // shows them as unclassified. "failed" carries an error; "" / "idle"
                // is normal.
                "analysis_state": n.analysis_state,
                "analysis_error": n.analysis_error,
            })
        })
        .collect();
    Ok(json!({ "nodes": nodes, "edges": g.edges }).to_string())
}

/// Apply one human calibration: validate ids, write a user-sourced relation (or
/// removal tombstone), then emit `repo-graph-updated` so the repo map refreshes.
async fn calibrate_edges_tool(db: &Db, thread: i32, args: &Value) -> Value {
    // i32::try_from (not a lossy `as i32`): a huge id like 4294967297 must NOT
    // wrap to a valid repo id and slip past the workspace membership check.
    let from = args.get("from").and_then(|v| v.as_i64()).and_then(|n| i32::try_from(n).ok());
    let to = args.get("to").and_then(|v| v.as_i64()).and_then(|n| i32::try_from(n).ok());
    let kind = args.get("kind").and_then(|v| v.as_str()).unwrap_or("");
    let via = args.get("via").and_then(|v| v.as_str()).unwrap_or("");
    let (Some(from), Some(to)) = (from, to) else {
        return text_result("from and to must be valid repo ids from get_repo_map".into());
    };
    if from == to {
        return text_result("from and to must be different repos".into());
    }
    // Validate kind against the allowed set: relations are keyed by (to, kind), so
    // a misspelling like "HTTP" would silently fail to match the visible edge.
    if !crate::profile::RELATION_KINDS.contains(&kind) {
        return text_result("kind must be one of: http, grpc, queue, infra, lib".into());
    }
    // Action is REQUIRED and must be add|remove. The store treats anything but
    // "remove" as an add, so a missing/misspelled action must be rejected here
    // rather than silently pinning the opposite of what the caller intended.
    let action = match args.get("action").and_then(|v| v.as_str()) {
        Some(a @ ("add" | "remove")) => a,
        _ => return text_result("action is required and must be \"add\" or \"remove\"".into()),
    };
    // Validate the ids belong to THIS curator's workspace, so a stale/hallucinated
    // id can't pin or remove relations on an unrelated workspace's repo.
    let Ok(Some(t)) = crate::store::repo::get_thread(db, thread).await else {
        return text_result("curator thread not found".into());
    };
    // Only the hidden curator thread may calibrate — reject a direct call to this
    // route with a normal feature thread id (it would bypass the chat boundary).
    if t.kind != "curator" {
        return text_result("calibrate_edges is only available in the curator chat".into());
    }
    let ws_ids: std::collections::HashSet<i32> = crate::store::repo::list_repos(db, t.workspace_id)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|r| r.id)
        .collect();
    if !ws_ids.contains(&from) || !ws_ids.contains(&to) {
        return text_result(
            "from/to must be repo ids in this workspace (use get_repo_map)".into(),
        );
    }
    match crate::store::repo::calibrate_repo_relation(db, from, to, kind, via, action).await {
        Ok(()) => {
            // Live-refresh the repo map for this curator thread's workspace.
            if let Some(app) = crate::APP_HANDLE.get() {
                use tauri::Emitter;
                let _ = app.emit("repo-graph-updated", t.workspace_id);
            }
            text_result(format!(
                "{action} {kind} edge {from}->{to} (pinned to your calibration)"
            ))
        }
        Err(e) => text_result(format!("error: {e}")),
    }
}

/// Pin ONE repo's tier/role classification (e.g. mark a frontend/local SDK that was
/// mislabeled as a backend service). Validates the repo belongs to THIS curator's
/// workspace and the tier is canonical, then writes via `curator::edit_profile`
/// (which pins ownership so the classification survives re-analysis) and emits
/// `repo-graph-updated`.
async fn set_classification_tool(db: &Db, thread: i32, args: &Value) -> Value {
    // i32::try_from (not lossy `as i32`): a huge id must NOT wrap to a valid repo id
    // and slip past the workspace membership check.
    let repo = args.get("repo").and_then(|v| v.as_i64()).and_then(|n| i32::try_from(n).ok());
    let Some(repo) = repo else {
        return text_result("repo must be a valid repo id from get_repo_map".into());
    };
    // Tier is REQUIRED and must canonicalize to frontend|backend: a misspelling would
    // otherwise clear the tier (or store a legacy value) and the node would read as
    // unclassified.
    let tier = args.get("tier").and_then(|v| v.as_str()).unwrap_or("");
    let Some(tier) = crate::profile::normalize_tier(tier) else {
        return text_result("tier is required and must be \"frontend\" or \"backend\"".into());
    };
    // Category (role within the tier) is optional free text; blank → leave it to a
    // later pass (don't pin an empty role).
    let category = args
        .get("category")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|c| !c.is_empty());
    let Ok(Some(t)) = crate::store::repo::get_thread(db, thread).await else {
        return text_result("curator thread not found".into());
    };
    // Only the hidden curator thread may classify — reject a direct call with a normal
    // feature thread id (it would bypass the chat boundary).
    if t.kind != "curator" {
        return text_result("set_classification is only available in the curator chat".into());
    }
    let in_ws = crate::store::repo::list_repos(db, t.workspace_id)
        .await
        .unwrap_or_default()
        .into_iter()
        .any(|r| r.id == repo);
    if !in_ws {
        return text_result("repo must be a repo id in this workspace (use get_repo_map)".into());
    }
    match crate::curator::edit_profile(db, repo, None, Some(tier.as_str()), category).await {
        Ok(_) => {
            if let Some(app) = crate::APP_HANDLE.get() {
                use tauri::Emitter;
                let _ = app.emit("repo-graph-updated", t.workspace_id);
            }
            let role = category.map(|c| format!("/{c}")).unwrap_or_default();
            text_result(format!(
                "repo {repo} classified as {tier}{role} (pinned — survives re-analysis)"
            ))
        }
        Err(e) => text_result(format!("error: {e}")),
    }
}

async fn call_planner(db: &Db, thread: i32, name: &str, args: &Value) -> Value {
    match name {
        "get_repo_map" => match repo_map_json(db, thread).await {
            Ok(v) => text_result(v),
            Err(e) => text_result(format!("error: {e}")),
        },
        "get_task" => match crate::store::repo::get_thread(db, thread).await {
            Ok(Some(t)) => text_result(json!({ "title": t.title, "type": t.kind }).to_string()),
            Ok(None) => text_result("error: thread not found".into()),
            Err(e) => text_result(format!("error: {e}")),
        },
        "get_test_cases" => match crate::store::repo::get_test_plan(db, thread).await {
            Ok(Some(p)) => text_result(p.content),
            Ok(None) => text_result("no test cases derived for this issue yet".into()),
            Err(e) => text_result(format!("error: {e}")),
        },
        "propose_directions" => {
            // `directions` must be a PRESENT, non-empty array. A missing / empty / malformed
            // payload is NOT a cancel: return an error so the lead retries (cancellation goes
            // through cancel_directions) rather than silently clearing the pending plan. Note
            // `Proposal.directions` is `#[serde(default)]`, so a missing/misspelled key would
            // otherwise deserialize to an empty list — hence the explicit presence check.
            let has_directions = args
                .get("directions")
                .and_then(|d| d.as_array())
                .map(|a| !a.is_empty())
                .unwrap_or(false);
            if !has_directions {
                return text_result(
                    "error: propose_directions requires a non-empty `directions` array; \
                     to withdraw pending tasks, call cancel_directions"
                        .into(),
                );
            }
            let proposal: crate::planner::Proposal = match serde_json::from_value(args.clone()) {
                Ok(p) => p,
                Err(e) => {
                    return text_result(format!("error: invalid propose_directions payload: {e}"))
                }
            };
            let n = proposal.directions.len();
            match crate::planner::save_proposal_value(db, thread, args).await {
                Ok(()) => {
                    // Anchor the proposal in the chat timeline at the moment it
                    // happened — the console renders it as an interactive card.
                    emit_proposal_row(db, thread, &proposal.rationale, n).await;
                    text_result(format!(
                        "proposed {n} task(s); the human will review and confirm in weft"
                    ))
                }
                Err(e) => text_result(format!("error: {e}")),
            }
        }
        "cancel_directions" => {
            let rationale = args
                .get("rationale")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            withdraw_and_emit(db, thread, &rationale).await
        }
        _ => text_result(format!("unknown tool: {name}")),
    }
}

/// Insert + emit a proposal-shaped timeline row (kind:"proposal", content
/// {rationale, count}). The console renders count>0 as the interactive review card
/// and count==0 as the settled "已撤回" line. Shared by propose + withdraw.
async fn emit_proposal_row(db: &Db, thread: i32, rationale: &str, count: usize) {
    let content = serde_json::json!({ "rationale": rationale, "count": count }).to_string();
    let turn = crate::store::repo::next_turn_id(db, thread).await.unwrap_or(1) - 1;
    if let Ok(m) = crate::store::repo::insert_lead_message(
        db,
        thread,
        None,
        turn.max(1),
        "system",
        "proposal",
        &content,
        "complete",
    )
    .await
    {
        if let Some(app) = crate::APP_HANDLE.get() {
            use tauri::Emitter;
            let _ = app.emit(
                crate::lead_chat::engine::EVENT,
                crate::lead_chat::engine::Push::Message {
                    thread_id: thread,
                    message: m,
                },
            );
        }
    }
}

/// Withdraw the pending proposal (the `cancel_directions` tool). Only records the count-0
/// ("已撤回") row when a pending proposal was ACTUALLY cleared — a no-op cancel (already
/// confirmed, a lane approved, or nothing proposed) must not leave a misleading withdrawn
/// row over live work.
async fn withdraw_and_emit(db: &Db, thread: i32, rationale: &str) -> Value {
    match crate::planner::withdraw_proposal(db, thread, rationale).await {
        Ok(true) => {
            emit_proposal_row(db, thread, rationale, 0).await;
            text_result("withdrew pending tasks".into())
        }
        Ok(false) => text_result(
            "nothing to withdraw: no pending proposal (it may be confirmed or already dispatched)"
                .into(),
        ),
        Err(e) => text_result(format!("error: {e}")),
    }
}

async fn repo_map_json(db: &Db, thread: i32) -> anyhow::Result<String> {
    let t = crate::store::repo::get_thread(db, thread)
        .await?
        .ok_or_else(|| anyhow::anyhow!("thread not found"))?;
    let g = crate::curator::graph(db, t.workspace_id).await?;
    Ok(serde_json::to_string(&g)?)
}

fn planner_specs() -> Value {
    let str_prop = || json!({ "type": "string" });
    json!([
        {
            "name": "get_task",
            "description": "Read this thread's Task: its title and type (feature|bugfix|refactor|spike).",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "get_repo_map",
            "description": "Read the workspace repo map: each repo's role/stack/summary/published+declared packages, plus the cross-repo dependency edges. Use it to decide which repos a task must touch and in what order.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "get_test_cases",
            "description": "Read the issue's current test-case document (markdown tree), if one exists. The human can edit it in weft, so read it back before shaping or revising the technical approach when you have not seen an update recently.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "propose_directions",
            "description": "Propose how to split the work into tasks. Each task targets EXACTLY ONE repo it will modify (by name, from the repo map) and MUST include a `reason` explaining why that repo must change. Reads are free — an agent may read any repo without declaring it, so never list reads. To modify N repos, propose N tasks. The human reviews each as a Needs-you card and approves before any worktree is created.",
            "inputSchema": { "type": "object", "properties": {
                "rationale": str_prop(),
                "directions": { "type": "array", "items": { "type": "object", "properties": {
                    "name": str_prop(),
                    "repo": str_prop(),
                    "reason": str_prop(),
                    "mandate": { "type": "string", "enum": ["plan+impl", "impl-only"],
                        "description": "Granularity of the role: plan+impl (default) — the worker plans its own task first, then builds; impl-only — the task is small/fully specified, the worker builds straight away. Do NOT write the task's implementation plan yourself; that is the worker's job." },
                    "hint": { "type": "string", "enum": ["normal", "deep"],
                        "description": "Optional routing hint only: normal prefers Codex for cheap batches; deep prefers Claude for deeper reasoning. This is not a tool choice." },
                    "base_branch": { "type": "string",
                        "description": "Branch in the target repo to branch the new work OFF. Leave empty to use the repo's default branch (main/master). Set it only when the repo merges into a non-default branch (develop/staging/a release branch)." }
                }, "required": ["name", "repo", "reason"] } }
            }, "required": ["directions"] }
        },
        {
            "name": "cancel_directions",
            "description": "Withdraw the pending proposed tasks. Use when the human says to hold off / cancel, or the write boundary is no longer settled — it clears the current proposal so nothing is dispatched and the review card collapses. Provide a short rationale. Do NOT call propose_directions with an empty directions list to cancel; use this.",
            "inputSchema": { "type": "object", "properties": {
                "rationale": str_prop()
            }, "required": ["rationale"] }
        }
    ])
}

fn tool_specs() -> Value {
    let str_prop = || json!({ "type": "string" });
    json!([
        {
            "name": "bus_post",
            "description": "Post a message to another participant's inbox in this thread. `to` is either \"lead\" (the thread lead; the lead has no numeric id) or the exact numeric id from a worker's messages' `from` field (not a display name or issue number). Prefer bus_post over broadcast when a specific participant must see the message, including when they may be idle.",
            "inputSchema": { "type": "object",
                "properties": { "to": str_prop(), "text": str_prop() },
                "required": ["to", "text"] }
        },
        {
            "name": "bus_broadcast",
            "description": "Send a message to every other participant currently active on the bus. Idle participants may miss it — use bus_post with \"lead\" or a worker's numeric id when a specific participant must receive the update.",
            "inputSchema": { "type": "object",
                "properties": { "text": str_prop() }, "required": ["text"] }
        },
        {
            "name": "bus_inbox",
            "description": "Read and clear your unread messages from other tasks. Call this whenever you are told there are new messages; do not assume silence means nothing happened.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "ask_human",
            "description": "Ask the human operator a question that only they can decide (a judgment call, a missing requirement, an approval). Surfaces in weft's Needs-you inbox; their answer returns via bus_inbox. Non-blocking — keep working and check your inbox.",
            "inputSchema": { "type": "object",
                "properties": { "text": str_prop() }, "required": ["text"] }
        },
        {
            "name": "thread_state_get",
            "description": "Read the shared thread state (a JSON object).",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "thread_state_set",
            "description": "Shallow-merge a patch object into the shared thread state.",
            "inputSchema": { "type": "object",
                "properties": { "patch": { "type": "object" } }, "required": ["patch"] }
        },
        {
            "name": "announce_interface_change",
            "description": "Broadcast a contract/interface change to the other tasks.",
            "inputSchema": { "type": "object",
                "properties": { "summary": str_prop() }, "required": ["summary"] }
        },
        {
            "name": "set_task_status",
            "description": "Move your task on the board as work really progresses: queued (not started), planning (working out this task's plan), working (actively building), review (done coding, awaiting the human's look), done (delivered/accepted). Reversible — set it back to working if the human asks for changes. Use this to keep the human's board honest instead of leaving it to guesswork.",
            "inputSchema": { "type": "object",
                "properties": { "status": str_prop() }, "required": ["status"] }
        },
        {
            "name": "register_pr",
            "description": "Tell weft you just opened a pull request for this task, so it tracks CI, review, and conflict state in the background and posts to Needs-you if something needs you — instead of that state only living in this conversation (which doesn't survive a restart). Call this right after `gh pr create` succeeds, with the URL it printed. Re-calling it for the same PR (e.g. after a restart) just refreshes context, it does not duplicate tracking. GitHub only for now — a GitLab merge request URL is recognized but currently REJECTED (not yet supported), so don't rely on this for GitLab repos.",
            "inputSchema": { "type": "object",
                "properties": {
                    "url": { "type": "string", "description": "The PR web URL, e.g. https://github.com/owner/repo/pull/123 — host, owner, repo, and number are all parsed from this." },
                    "title": str_prop()
                }, "required": ["url"] }
        }
    ])
}

/// Bind an ephemeral port and serve the router; returns the bound base URL.
pub async fn serve(
    bus: BusRegistry,
    db: Db,
    asks: AskRegistry,
) -> std::io::Result<(String, tokio::task::JoinHandle<()>)> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let base = format!("http://127.0.0.1:{}", addr.port());
    let app = router(bus, db, asks);
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Ok((base, handle))
}

#[cfg(test)]
mod tests {
    use super::{
        is_weft_internal_tool, register_pr_tool, serve, session_servers_for_kind, summarize,
        tool_specs, UNKNOWN_ENGINE,
    };
    use crate::ask::RiskLevel;
    use crate::store::Db;
    use serde_json::{json, Value};

    /// Issue #89: a Claude/opencode multi-line command truncates `summary` to
    /// its first line for display, but `action_key` must carry the FULL command
    /// — two commands sharing a first line are different actions.
    #[test]
    fn summarize_command_action_key_is_full_command_not_first_line() {
        let input = json!({"command": "npm test\nrm -rf /"});
        let (summary, detail, risk, action_key) = summarize("Bash", Some(&input));
        assert_eq!(summary, "Run: npm test"); // display: first line only
        assert_eq!(detail, "npm test\nrm -rf /"); // full command, untruncated
        assert!(action_key.contains("rm -rf /")); // match key carries the WHOLE command
        // issue #101: an unrecognized shell command is never waved through as
        // read-only.
        assert_eq!(risk, RiskLevel::Write);

        // A different multi-line command sharing the same first line must yield a
        // DIFFERENT action_key even though `summary` collides.
        let other = json!({"command": "npm test\necho safe"});
        let (summary2, _detail2, _risk2, action_key2) = summarize("Bash", Some(&other));
        assert_eq!(summary2, summary, "both display as \"Run: npm test\"");
        assert_ne!(action_key2, action_key);
    }

    /// Issue #89: an MCP/fallback ask (no `command`/`file_path` field) shows only
    /// the bare tool name as `summary`, but `action_key` must fold in the full
    /// args so two calls with different args don't collide.
    #[test]
    fn summarize_mcp_fallback_action_key_includes_full_args() {
        let a = json!({"url": "https://safe.example"});
        let b = json!({"url": "https://evil.example"});
        let (summary_a, _da, risk_a, key_a) = summarize("WebFetch", Some(&a));
        let (summary_b, _db, _risk_b, key_b) = summarize("WebFetch", Some(&b));
        assert_eq!(summary_a, "WebFetch"); // lossy tool-name-only display
        assert_eq!(summary_a, summary_b, "both display as just the tool name");
        assert_ne!(key_a, key_b, "different args must yield different action keys");
        // issue #101: WebFetch is network access regardless of the URL's args.
        assert_eq!(risk_a, RiskLevel::NetworkOrCredential);
    }

    /// A file-op action_key folds in the tool name: reading and writing the same
    /// path are different actions. issue #101: risk also follows the tool name
    /// (Read is read-only, Write is a write) for the SAME path.
    #[test]
    fn summarize_file_op_action_key_distinguishes_tool() {
        let input = json!({"file_path": "/tmp/x"});
        let (_s_read, _d_read, risk_read, key_read) = summarize("Read", Some(&input));
        let (_s_write, _d_write, risk_write, key_write) = summarize("Write", Some(&input));
        assert_ne!(key_read, key_write);
        assert_eq!(risk_read, RiskLevel::ReadOnly);
        assert_eq!(risk_write, RiskLevel::Write);
    }

    /// Round-2 finding: a naive `format!("{tool_name}:{content}")` join lets an
    /// ask from the `command` branch collide with an ask from the `file_path`
    /// branch when they share the SAME tool_name and the SAME content string —
    /// e.g. one MCP tool that sends `{"command": "X"}` on one call and
    /// `{"file_path": "X"}` on another. Without a branch/kind tag, both would
    /// produce the identical `action_key`, so an Always on one silently covers
    /// the other even though running a command and touching a file are
    /// different actions. Each branch must be distinguishable regardless of
    /// tool_name/content coincidence.
    #[test]
    fn summarize_cross_branch_same_tool_and_content_does_not_collide() {
        let cmd_input = json!({"command": "X"});
        let file_input = json!({"file_path": "X"});
        let (_sc, _dc, _rc, cmd_key) = summarize("SameTool", Some(&cmd_input));
        let (_sf, _df, _rf, file_key) = summarize("SameTool", Some(&file_input));
        assert_ne!(cmd_key, file_key);
    }

    #[test]
    fn session_servers_mirror_injection_policy() {
        // Lead family keys off the thread kind (worker lanes are handled in
        // session_injected — bus only — and a failed lookup fails closed there).
        assert_eq!(session_servers_for_kind("concierge"), &["weft_global"]);
        assert_eq!(
            session_servers_for_kind("curator"),
            &["weft_curator", "weft_bus"]
        );
        assert_eq!(
            session_servers_for_kind("issue"),
            &["weft_planner", "weft_bus"]
        );
        // An unknown but real kind maps to the common per-issue lead set.
        assert_eq!(
            session_servers_for_kind("something-new"),
            &["weft_planner", "weft_bus"]
        );
    }

    #[test]
    fn provenance_blocks_uninjected_servers() {
        // Concierge injects only weft_global — not the per-thread bus.
        assert!(!session_servers_for_kind("concierge").contains(&"weft_bus"));
        assert!(!session_servers_for_kind("concierge").contains(&"weft_planner"));
        // Curator has no planner.
        assert!(!session_servers_for_kind("curator").contains(&"weft_planner"));
        // A per-issue lead has no global / curator.
        assert!(!session_servers_for_kind("issue").contains(&"weft_global"));
        assert!(!session_servers_for_kind("issue").contains(&"weft_curator"));
    }

    #[test]
    fn weft_internal_known_tools_auto_allow() {
        // weft's own injected tools (unambiguous claude naming) → auto-allow.
        assert!(is_weft_internal_tool("mcp__weft_planner__get_test_cases"));
        assert!(is_weft_internal_tool("mcp__weft_planner__get_task"));
        assert!(is_weft_internal_tool("mcp__weft_planner__propose_directions"));
        assert!(is_weft_internal_tool("mcp__weft_bus__bus_inbox"));
        assert!(is_weft_internal_tool("mcp__weft_bus__set_task_status"));
        assert!(is_weft_internal_tool("mcp__weft_curator__get_repo_map"));
        assert!(is_weft_internal_tool("mcp__weft_global__list_workspaces"));
    }

    /// Issue #110: `register_pr` is a pure metadata/bookkeeping write (same
    /// rationale as `set_task_status` — governed by weft's own Needs-you
    /// surface downstream, no host-side effect), so it must be on the same
    /// auto-approved footing.
    #[test]
    fn register_pr_is_auto_approved_like_set_task_status() {
        assert!(is_weft_internal_tool("mcp__weft_bus__register_pr"));
    }

    /// The tool must actually be advertised to agents (`tools/list`), not just
    /// silently auto-approved if called blind — otherwise no agent would ever
    /// discover it exists.
    #[test]
    fn register_pr_is_advertised_in_tool_specs() {
        let specs = tool_specs();
        let names: Vec<&str> = specs
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t.get("name").and_then(|n| n.as_str()))
            .collect();
        assert!(names.contains(&"register_pr"), "tool_specs: {names:?}");
    }

    fn tool_text(result: &Value) -> &str {
        result["content"][0]["text"].as_str().unwrap_or("")
    }

    #[tokio::test]
    async fn register_pr_tool_rejects_an_unparseable_url() {
        let db = Db::connect("sqlite::memory:").await.unwrap();
        let result = register_pr_tool(&db, 1, "5", "not a valid pr url", "").await;
        assert!(tool_text(&result).contains("could not parse"), "got: {}", tool_text(&result));
    }

    /// End-to-end through the exact path an agent's `register_pr` call takes:
    /// URL parse → direction lookup (for `repo_id`) → DB upsert — confirming
    /// the row lands attributed to the calling direction/thread, and that the
    /// reply names the host's OWN vocabulary ("Pull request", not a neutral
    /// "PR"/"change" — issue #110's UI-terminology requirement).
    #[tokio::test]
    async fn register_pr_tool_tracks_a_valid_github_pr_url() {
        let db = Db::connect("sqlite::memory:").await.unwrap();
        let ws = crate::store::repo::create_workspace(&db, "ws").await.unwrap();
        let repo = crate::store::repo::add_repo_ref(&db, ws.id, "r", "/tmp/r", "main", "", true)
            .await
            .unwrap();
        let thread = crate::store::repo::create_thread(&db, ws.id, "t", "feature", "codex")
            .await
            .unwrap();
        let dir = crate::store::repo::create_direction(
            &db, thread.id, "task", "codex", repo.id, "why", "impl-only", "",
        )
        .await
        .unwrap();

        let result = register_pr_tool(
            &db,
            thread.id,
            &dir.id.to_string(),
            "https://github.com/acme/widgets/pull/9",
            "my title",
        )
        .await;
        assert!(tool_text(&result).contains("Pull request #9"), "got: {}", tool_text(&result));

        let tracked = crate::store::repo::find_pull_request(&db, "github", "acme", "widgets", 9)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(tracked.direction_id, dir.id);
        assert_eq!(tracked.thread_id, thread.id);
        assert_eq!(tracked.repo_id, repo.id);
        assert_eq!(tracked.title, "my title");
        assert_eq!(tracked.lifecycle, "open");
    }

    /// `dir` "lead" (non-numeric) must still register the row rather than
    /// erroring — see `register_pr_tool`'s doc: `direction_id`/`repo_id` fall
    /// back to the "0 = unset" convention `direction.repo_id` already uses.
    #[tokio::test]
    async fn register_pr_tool_from_the_lead_falls_back_to_unset_direction() {
        let db = Db::connect("sqlite::memory:").await.unwrap();
        let ws = crate::store::repo::create_workspace(&db, "ws").await.unwrap();
        let thread = crate::store::repo::create_thread(&db, ws.id, "t", "feature", "codex")
            .await
            .unwrap();

        let result = register_pr_tool(
            &db,
            thread.id,
            "lead",
            "https://github.com/acme/widgets/pull/1",
            "",
        )
        .await;
        assert!(tool_text(&result).contains("Pull request #1"), "got: {}", tool_text(&result));
        let tracked = crate::store::repo::find_pull_request(&db, "github", "acme", "widgets", 1)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(tracked.direction_id, 0);
        assert_eq!(tracked.repo_id, 0);
    }

    /// P1 (issue #110 adversarial review): a GitLab MR URL parses cleanly
    /// (`host::parse_pr_url` recognizes the shape) but `resolve_host(GitLab)`
    /// has no working backend — registering it anyway would create a row
    /// that fails every sweep FOREVER with no way to self-clear (readiness
    /// never resolves) and no manual dismiss (a NOTICE has no close button).
    /// Must be rejected at registration, honestly, with NOTHING written to
    /// the DB — not silently accepted and left to fail later.
    #[tokio::test]
    async fn register_pr_tool_rejects_an_unsupported_gitlab_host_without_creating_a_row() {
        let db = Db::connect("sqlite::memory:").await.unwrap();
        let ws = crate::store::repo::create_workspace(&db, "ws").await.unwrap();
        let thread = crate::store::repo::create_thread(&db, ws.id, "t", "feature", "codex")
            .await
            .unwrap();

        let result = register_pr_tool(
            &db,
            thread.id,
            "lead",
            "https://gitlab.com/my-group/my-project/-/merge_requests/12",
            "",
        )
        .await;
        let text = tool_text(&result).to_lowercase();
        assert!(text.contains("not registered"), "must clearly say it was not registered, got: {text}");
        assert!(text.contains("support"), "must clearly explain WHY (unsupported host), got: {text}");

        let tracked =
            crate::store::repo::find_pull_request(&db, "gitlab", "my-group", "my-project", 12)
                .await
                .unwrap();
        assert!(tracked.is_none(), "an unsupported host must never create a trackable (and un-clearable) row");
    }

    #[test]
    fn opencode_flat_names_surface() {
        // opencode's `<server>_<tool>` form is ambiguous (weft names contain `_`),
        // so it is NOT parsed as internal — those calls surface the card. A project
        // server `weft` + tool `bus_thread_state_set` must not read as weft_bus.
        assert!(!is_weft_internal_tool("weft_bus_bus_post"));
        assert!(!is_weft_internal_tool("weft_planner_get_task"));
        assert!(!is_weft_internal_tool("weft_bus_thread_state_set"));
    }

    #[test]
    fn permission_answering_tool_still_surfaces() {
        // answer_permission would let an agent erase a pending permission ask —
        // it must NOT be auto-approved even though weft injects weft_global.
        assert!(!is_weft_internal_tool("mcp__weft_global__answer_permission"));
    }

    #[test]
    fn unknown_tool_on_weft_server_surfaces() {
        // Provenance: sharing a weft server name is not enough — an unknown tool
        // (e.g. from a user/repo MCP server that reused the name) still surfaces.
        assert!(!is_weft_internal_tool("mcp__weft_planner__deploy"));
        assert!(!is_weft_internal_tool("mcp__weft_bus__rm_rf"));
        assert!(!is_weft_internal_tool("weft_planner_deploy"));
    }

    #[test]
    fn foreign_and_malformed_names_surface() {
        // A user's own `weft_*`-named server (different server name) must surface.
        assert!(!is_weft_internal_tool("mcp__weft_analytics__query"));
        assert!(!is_weft_internal_tool("mcp__weftly__do"));
        // Non-MCP tools (Bash, file ops) and third-party MCP surface as before.
        assert!(!is_weft_internal_tool("Bash"));
        assert!(!is_weft_internal_tool("mcp__github__create_pr"));
        // Malformed names never match.
        assert!(!is_weft_internal_tool("mcp__weft_bus"));
        assert!(!is_weft_internal_tool("weft_bus__x"));
    }

    /// Fast companion to `missing_tool_query_param_does_not_inherit_claudes_allowlist`
    /// below: the sentinel itself must resolve to no scope for both engines
    /// that actually have `SAFE_BUILTINS` rows.
    #[test]
    fn unknown_engine_sentinel_matches_no_safe_builtins_row() {
        assert_eq!(
            crate::bus::builtin_allow::safe_scope(UNKNOWN_ENGINE, "Read"),
            None
        );
        assert_eq!(
            crate::bus::builtin_allow::safe_scope(UNKNOWN_ENGINE, "NotebookRead"),
            None
        );
        assert_eq!(
            crate::bus::builtin_allow::safe_scope(UNKNOWN_ENGINE, "TodoWrite"),
            None
        );
        assert_eq!(
            crate::bus::builtin_allow::safe_scope(UNKNOWN_ENGINE, "update_plan"),
            None
        );
    }

    /// Structural companion to `unknown_engine_sentinel_matches_no_safe_
    /// builtins_row` above: that test enumerates 4 concrete tool names as of
    /// this writing, so it CANNOT catch a future `SAFE_BUILTINS` row keyed on
    /// `UNKNOWN_ENGINE` with some OTHER tool name — the enumeration would just
    /// stay green next to a newly-unsafe table. This iterates the actual table
    /// instead, so the invariant holds for every row, present or future, not
    /// just the ones a human thought to name here.
    #[test]
    fn unknown_engine_sentinel_matches_no_safe_builtins_row_structurally() {
        assert!(
            crate::bus::builtin_allow::SAFE_BUILTINS
                .iter()
                .all(|(engine, _, _)| *engine != UNKNOWN_ENGINE),
            "a SAFE_BUILTINS row keyed on UNKNOWN_ENGINE would let a request with a \
             missing or unrecognized ?tool= silently inherit that row's auto-approval \
             instead of surfacing the Needs-you card — see UNKNOWN_ENGINE's doc"
        );
    }

    /// PR #146 promoted the `?tool=` query param from a cosmetic card label
    /// into the LOOKUP KEY for `builtin_allow::safe_scope`'s auto-approval
    /// decision, but its default stayed `"claude"` — the most PERMISSIVE of
    /// the three engines' allowlists. A request with no `tool` param (or an
    /// unrecognized one) must surface the Needs-you card for a `Read`-shaped
    /// call exactly like an unknown engine, never silently inherit claude's
    /// `ReadOnlyPath` entry for `Read`. End-to-end through the real HTTP
    /// endpoint (not just `safe_scope` in isolation), so this also proves
    /// `handle_ask`'s query parsing — not just the constant — takes the fix.
    #[tokio::test]
    async fn missing_tool_query_param_does_not_inherit_claudes_allowlist() {
        use crate::ask::{Answer, AskRegistry};
        use crate::bus::BusRegistry;

        let dir = std::env::temp_dir().join(format!(
            "weft-ask-unknown-tool-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("main.rs");
        std::fs::write(&file, b"fn main() {}\n").unwrap();
        let root = std::fs::canonicalize(&dir).unwrap();

        let db = Db::connect("sqlite::memory:").await.unwrap();
        let ws = crate::store::repo::create_workspace(&db, "ws").await.unwrap();
        crate::store::repo::add_repo_ref(&db, ws.id, "r", &root.to_string_lossy(), "main", "", true)
            .await
            .unwrap();
        let thread = crate::store::repo::create_thread(&db, ws.id, "t", "feature", "codex")
            .await
            .unwrap();

        let asks = AskRegistry::new();
        let (base, _h) = serve(BusRegistry::new(), db, asks.clone()).await.unwrap();

        // Deliberately NO `?tool=` query parameter on this URL.
        let url = format!("{base}/ask/{}/{}", thread.id, crate::bus::LEAD);
        let client = reqwest::Client::new();
        let body = json!({
            "tool_name": "Read",
            "tool_input": { "file_path": file.to_string_lossy().to_string() }
        });

        let (resp, ()) = tokio::join!(
            async { client.post(url.as_str()).json(&body).send().await.unwrap() },
            crate::hook_test_support::answer_first_ask(&asks, Answer::Allow),
        );
        let out: Value = resp.json().await.unwrap();
        assert_eq!(
            out["hookSpecificOutput"]["permissionDecision"], "allow",
            "the human's own Allow must still reach the engine: {out}"
        );
        assert_eq!(
            out["hookSpecificOutput"]["permissionDecisionReason"], "Approved in weft",
            "a missing `tool` query param must surface the Needs-you card and \
             wait for a human decision, not silently auto-approve via claude's \
             Read allowlist entry (reason would read \"read-only builtin \
             (auto-approved)\" if it had): {out}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
