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
use std::collections::{HashMap, HashSet};
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
/// each one's undocumented/unverified empty-response path behaves. Thread and
/// direction come from the URL path, and a worker's exact session comes from
/// Weft's injected query parameter — never from the body.
async fn handle_ask(
    Path((thread, dir)): Path<(i32, String)>,
    Query(q): Query<HashMap<String, String>>,
    State(asks): State<AskRegistry>,
    State(bus): State<BusRegistry>,
    State(db): State<Db>,
    Json(req): Json<Value>,
) -> Response {
    let tool = q.get("tool").map(|s| s.as_str()).unwrap_or(UNKNOWN_ENGINE);
    let source_session_id = q.get("session_id").map(String::as_str);
    let tool_name = req
        .get("tool_name")
        .and_then(|v| v.as_str())
        .unwrap_or("tool");

    let (id, rx) = {
        // Ask admission shares the exact same per-thread lifecycle gate and
        // durable identity proof as bus tool calls. Keep the gate only through
        // the auto verdict or atomic AskRegistry registration; the one-hour
        // human wait below must never block deletion or an answer path.
        let lifecycle_gate = bus.thread_lifecycle_gate(thread);
        let _lifecycle = lifecycle_gate.lock().await;
        let identity = match admit_bus_identity(&db, thread, &dir, source_session_id).await {
            Ok(identity) => identity,
            Err(error) => {
                return hook_decision(
                    "deny",
                    &format!("Invalid or deleted weft session — denied: {error}"),
                );
            }
        };

        // weft's OWN injected MCP tools are never permission-gated: the human
        // governs them through weft's surfaces (Needs-you, the board, the
        // direction-confirm flow), so a per-call prompt to read the task or post
        // to the bus is pure interruption. Short-circuit before summarizing — but
        // ONLY when weft actually injected this server for THIS exact session. A
        // repo/user MCP server that reused a weft server name must still surface.
        if is_weft_internal_tool(tool_name) {
            if let Some((server, _)) = split_internal_tool(tool_name) {
                if session_injected(&db, thread, &dir, identity, server).await {
                    return hook_decision("allow", "weft-internal tool (auto-approved)");
                }
            }
        }

        let (summary, detail, risk, action_key) = summarize(tool_name, req.get("tool_input"));

        // A read-only BUILTIN of the engine itself (claude's Read/Grep/Glob, …)
        // is waved through, so a turn that reads twenty files doesn't cost
        // twenty human clicks. Identity and deletion markers have already been
        // proven under the lifecycle gate above, before this branch can allow.
        match builtin_allow::safe_scope(tool, tool_name) {
            Some(builtin_allow::SafeScope::NoTarget) => {
                return hook_decision("allow", "read-only builtin (auto-approved)");
            }
            Some(builtin_allow::SafeScope::ReadOnlyPath) => {
                if risk == crate::ask::RiskLevel::ReadOnly {
                    let roots = session_roots(&db, thread, identity).await;
                    if builtin_allow::paths_contained(req.get("tool_input"), &roots) {
                        return hook_decision("allow", "read-only builtin (auto-approved)");
                    }
                }
            }
            None => {}
        }

        // A standing rule (full access / always-allow / issue #103's read-only
        // batch-or-issue grant) decides without surfacing. Matches on the exact
        // action key, but only after the exact live identity proof above.
        if asks.auto_decision(thread, &dir, risk, &action_key) == Some(Decision::Allow) {
            return hook_decision("allow", "Auto-approved by a weft rule");
        }

        asks.request(thread, &dir, tool, &summary, &detail, risk, &action_key)
    };

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
            hook_decision(
                "deny",
                "No answer in time — denied by default (weft ask bridge)",
            )
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
    ("weft_bus", "bus_ack"),
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
    split_internal_tool(tool_name).is_some_and(|pair| AUTO_APPROVED_INTERNAL_TOOLS.contains(&pair))
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

/// Whether weft injected `server` for the already-admitted exact identity. A
/// worker lane injects only the bus; the lead family keys off the thread kind
/// and fails closed if that row can no longer be resolved.
async fn session_injected(
    db: &Db,
    thread: i32,
    dir: &str,
    identity: AdmittedBusIdentity,
    server: &str,
) -> bool {
    match identity {
        AdmittedBusIdentity::Worker { direction_id, .. } => {
            server == "weft_bus" && dir == direction_id.to_string()
        }
        AdmittedBusIdentity::Lead => {
            if dir != crate::bus::LEAD {
                return false;
            }
            match crate::store::repo::get_thread(db, thread).await {
                Ok(Some(thread)) => session_servers_for_kind(&thread.kind).contains(&server),
                _ => false,
            }
        }
    }
}

/// The directories a session is entitled to READ without asking — the "working
/// directory and additional directories" that claude's own permission model
/// scopes `Read`/`Grep`/`Glob` to, expressed in weft's terms.
///
/// Derived from weft's OWN database and the already-admitted URL identity,
/// never from the hook payload. A payload field (`cwd`) or an injected route
/// file would both be things a repo could plant — see the planted
/// `.weft-codex-ask-url` defense in `codex::ensure_codex_hook_in`.
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
/// FAILS CLOSED at every step: a DB error yields an empty (or narrowed) list,
/// and `builtin_allow::paths_contained` then refuses every absolute path.
async fn session_roots(
    db: &Db,
    thread: i32,
    identity: AdmittedBusIdentity,
) -> Vec<std::path::PathBuf> {
    let mut roots: Vec<std::path::PathBuf> = Vec::new();
    if let AdmittedBusIdentity::Worker { direction_id, .. } = identity {
        if let Ok(worktrees) = crate::store::repo::list_worktrees(db, Some(direction_id)).await {
            roots.extend(
                worktrees
                    .into_iter()
                    .map(|w| std::path::PathBuf::from(w.path)),
            );
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AdmittedBusIdentity {
    Lead,
    Worker {
        direction_id: i32,
        session_id: i32,
        repo_id: i32,
    },
}

impl AdmittedBusIdentity {
    fn direction_id(self) -> i32 {
        match self {
            Self::Lead => 0,
            Self::Worker { direction_id, .. } => direction_id,
        }
    }

    fn session_id(self) -> Option<i32> {
        match self {
            Self::Lead => None,
            Self::Worker { session_id, .. } => Some(session_id),
        }
    }

    fn repo_id(self) -> i32 {
        match self {
            Self::Lead => 0,
            Self::Worker { repo_id, .. } => repo_id,
        }
    }
}

/// Resolve the exact identity baked into one injected bus URL and prove its
/// complete durable parent chain is still live. Callers must hold this
/// thread's [`BusRegistry::thread_lifecycle_gate`] from before this validation
/// through the complete admitted bus-tool or Ask-Bridge registration.
///
/// A direction may have one session per repository. Consequently the session
/// repo is intentionally allowed to differ from the direction's primary repo,
/// but both repos must still exist in the thread's workspace and both deletion
/// markers must be clear. This is what makes a deleted secondary session inert
/// even while its direction and primary repo survive.
async fn admit_bus_identity(
    db: &Db,
    thread_id: i32,
    direction_scope: &str,
    source_session_id: Option<&str>,
) -> anyhow::Result<AdmittedBusIdentity> {
    let thread = crate::store::repo::ensure_thread_workspace_accepts_writes(db, thread_id).await?;
    if direction_scope == crate::bus::LEAD {
        if source_session_id.is_some() {
            anyhow::bail!("lead bus identity cannot carry a worker session");
        }
        return Ok(AdmittedBusIdentity::Lead);
    }

    let direction_id = direction_scope
        .parse::<i32>()
        .map_err(|_| anyhow::anyhow!("invalid direction scope '{direction_scope}'"))?;
    let raw_session_id = source_session_id.ok_or_else(|| {
        anyhow::anyhow!("worker bus identity is missing its exact source session")
    })?;
    let session_id = raw_session_id
        .parse::<i32>()
        .map_err(|_| anyhow::anyhow!("invalid session id '{raw_session_id}'"))?;

    let direction = crate::store::repo::get_direction(db, direction_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("direction {direction_id} not found"))?;
    if direction.thread_id != thread_id {
        anyhow::bail!("direction {direction_id} does not belong to thread {thread_id}");
    }
    let session = crate::store::repo::get_session(db, session_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("session {session_id} not found"))?;
    if session.direction_id != direction_id {
        anyhow::bail!("session {session_id} does not belong to direction {direction_id}");
    }

    let direction_repo =
        crate::store::repo::ensure_repo_workspace_accepts_writes(db, direction.repo_id).await?;
    if direction_repo.workspace_id != thread.workspace_id {
        anyhow::bail!(
            "direction {direction_id} repo {} does not belong to thread {thread_id}'s workspace",
            direction_repo.id
        );
    }
    let session_repo =
        crate::store::repo::ensure_repo_workspace_accepts_writes(db, session.repo_id).await?;
    if session_repo.workspace_id != thread.workspace_id {
        anyhow::bail!(
            "session {session_id} repo {} does not belong to thread {thread_id}'s workspace",
            session_repo.id
        );
    }

    Ok(AdmittedBusIdentity::Worker {
        direction_id,
        session_id,
        repo_id: session.repo_id,
    })
}

async fn initialize_bus_session(
    db: &Db,
    bus: &BusRegistry,
    thread_id: i32,
    direction_scope: &str,
    source_session_id: Option<&str>,
) -> Value {
    let lifecycle_gate = bus.thread_lifecycle_gate(thread_id);
    let _lifecycle = lifecycle_gate.lock().await;
    if let Err(error) = admit_bus_identity(db, thread_id, direction_scope, source_session_id).await
    {
        return text_result(format!("error: {error}"));
    }
    bus.join(thread_id, direction_scope);
    json!({
        "protocolVersion": "2024-11-05",
        "capabilities": { "tools": { "listChanged": false } },
        "serverInfo": { "name": "weft_bus", "version": "1.0.0" }
    })
}

// `thread`/`dir` and the optional exact worker `session_id` come from Weft's
// injected URL, so tool arguments cannot spoof identity. The session is still
// checked against the path's direction/thread before any bus read or effect.
// This does NOT defend against a local process forging the URL itself (no auth
// — local-first tradeoff).
async fn handle(
    Path((thread, dir)): Path<(i32, String)>,
    Query(query): Query<HashMap<String, String>>,
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
    let source_session_id = query.get("session_id").map(String::as_str);

    let result: Value = match method {
        "initialize" => initialize_bus_session(&db, &reg, thread, &dir, source_session_id).await,
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
            dispatch_bus_tool_call(&db, &reg, thread, &dir, source_session_id, name, &args).await
        }
        _ => json!({}),
    };

    sse(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
}

/// Single admission/serialization point for every bus tool. The same
/// per-thread lifecycle guard spans parent-chain validation, membership, all
/// in-memory reads/effects, and durable writes. Deletion installs its marker
/// before waiting for this guard, so a stale engine waiting behind a delete
/// always observes the fence before it can touch the surviving bus.
async fn dispatch_bus_tool_call(
    db: &Db,
    bus: &BusRegistry,
    thread_id: i32,
    direction_scope: &str,
    source_session_id: Option<&str>,
    name: &str,
    args: &Value,
) -> Value {
    let lifecycle_gate = bus.thread_lifecycle_gate(thread_id);
    let _lifecycle = lifecycle_gate.lock().await;
    let identity = match admit_bus_identity(db, thread_id, direction_scope, source_session_id).await
    {
        Ok(identity) => identity,
        Err(error) => return text_result(format!("error: {error}")),
    };

    bus.join(thread_id, direction_scope);
    match name {
        "set_task_status" => {
            let status = args.get("status").and_then(|v| v.as_str()).unwrap_or("");
            set_task_status_tool(db, identity, status).await
        }
        "register_pr" => {
            let url = args.get("url").and_then(|v| v.as_str()).unwrap_or("");
            let title = args.get("title").and_then(|v| v.as_str()).unwrap_or("");
            register_pr_tool(db, thread_id, identity, url, title).await
        }
        "ask_human" => {
            let text = args.get("text").and_then(|v| v.as_str()).unwrap_or("");
            ask_human_tool(db, bus, thread_id, direction_scope, identity, text).await
        }
        _ => call_tool(db, bus, thread_id, direction_scope, name, args).await,
    }
}

async fn ask_human_tool(
    db: &Db,
    bus: &BusRegistry,
    thread_id: i32,
    direction_scope: &str,
    identity: AdmittedBusIdentity,
    text: &str,
) -> Value {
    let thread = match crate::store::repo::get_thread(db, thread_id).await {
        Ok(Some(thread)) => thread,
        Ok(None) => return text_result(format!("error: thread {thread_id} not found")),
        Err(err) => return text_result(format!("error: {err}")),
    };
    let direction_id = identity.direction_id();
    let (source_session_id, source) = match crate::store::repo::human_request_source(
        db,
        thread_id,
        direction_scope,
        identity.session_id(),
    )
    .await
    {
        Ok(source) => source,
        Err(error) => return text_result(format!("error: {error}")),
    };
    let (turn_id, source_message_id) = source
        .map(|message| (message.turn_id, message.id))
        .unwrap_or((0, 0));
    match crate::store::repo::create_human_request(
        db,
        thread.workspace_id,
        thread_id,
        direction_scope,
        direction_id,
        turn_id,
        source_message_id,
        source_session_id,
        text,
    )
    .await
    {
        Ok(request) => {
            let Ok(id) = u64::try_from(request.id) else {
                return text_result("error: invalid durable question id".to_string());
            };
            bus.ask_human_with_id(thread_id, direction_scope, &request.question, id);
            let still_open = match crate::store::repo::get_human_request(db, request.id).await {
                Ok(Some(current)) => current.status == crate::store::repo::HUMAN_REQUEST_OPEN,
                Ok(None) => false,
                Err(error) => {
                    bus.cancel_open_asks_by_id(thread_id, id);
                    return text_result(format!(
                        "error: could not verify durable question: {error}"
                    ));
                }
            };
            if !still_open {
                bus.cancel_open_asks_by_id(thread_id, id);
                return text_result(
                    "error: source turn was rewound before the question opened".to_string(),
                );
            }
            text_result(format!(
                "asked the human (ask #{}); their answer will arrive in your bus_inbox — keep working and check it",
                request.id
            ))
        }
        Err(err) => text_result(format!("error: {err}")),
    }
}

/// Bus tool: the agent sets its own task's lifecycle status. `dir` is the
/// direction id from the URL path, so the agent can't move another task.
async fn set_task_status_tool(db: &Db, identity: AdmittedBusIdentity, status: &str) -> Value {
    let allowed = ["queued", "planning", "working", "review", "done"];
    if !allowed.contains(&status) {
        return text_result(format!(
            "invalid status '{status}'; use one of: queued, planning, working, review, done"
        ));
    }
    match identity {
        AdmittedBusIdentity::Worker { direction_id, .. } => {
            match crate::store::repo::set_direction_status(db, direction_id, status).await {
                Ok(()) => text_result(format!("status set to {status}")),
                Err(e) => text_result(format!("error: {e}")),
            }
        }
        AdmittedBusIdentity::Lead => text_result("this session has no task to update".into()),
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
async fn register_pr_tool(
    db: &Db,
    thread: i32,
    identity: AdmittedBusIdentity,
    url: &str,
    title: &str,
) -> Value {
    let Some(parts) = crate::host::parse_pr_url(url) else {
        return text_result(format!(
            "could not parse a PR/MR number and repo from '{url}' — expected a GitHub pull request URL (…/pull/N) or a GitLab merge request URL (…/-/merge_requests/N)"
        ));
    };
    // Reject an unimplemented host BEFORE creating a row, not after: a row
    // for a host with no working `PrHost` backend would only create an
    // unserviceable tracking row. Fail registration honestly instead.
    if crate::host::resolve_host(parts.host_kind).is_err() {
        return text_result(format!(
            "{} tracking isn't supported yet — weft's PR/MR automation currently only implements GitHub. This {} was NOT registered; please track it yourself for now.",
            parts.host_kind.native_noun(),
            parts.host_kind.native_noun()
        ));
    }
    match crate::store::repo::register_pull_request(
        db,
        thread,
        identity.direction_id(),
        identity.repo_id(),
        identity.session_id(),
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

async fn call_tool(
    db: &Db,
    reg: &BusRegistry,
    thread: i32,
    me: &str,
    name: &str,
    args: &Value,
) -> Value {
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
            let mut msgs = reg.inbox(thread, me);
            let pending = match crate::store::repo::list_pending_human_answers_for_scope(
                db, thread, me,
            )
            .await
            {
                Ok(pending) => pending,
                Err(error) => {
                    reg.restore_inbox(thread, me, msgs);
                    return text_result(format!("error: could not read durable inbox: {error}"));
                }
            };
            let mut seen: HashSet<u64> = msgs
                .iter()
                .filter_map(|message| message.request_id)
                .collect();
            for request in pending {
                let Ok(request_id) = u64::try_from(request.id) else {
                    reg.restore_inbox(thread, me, msgs);
                    return text_result("error: invalid durable question id".into());
                };
                if !seen.insert(request_id) {
                    continue;
                }
                msgs.push(crate::bus::Msg {
                    from: crate::bus::HUMAN.to_string(),
                    to: me.to_string(),
                    text: request.answer,
                    ts: 0,
                    kind: "message".to_string(),
                    request_id: Some(request_id),
                });
            }
            text_result(serde_json::to_string(&msgs).unwrap_or_else(|_| "[]".into()))
        }
        "bus_ack" => {
            let mut request_ids: Vec<i32> = args
                .get("request_ids")
                .and_then(serde_json::Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(serde_json::Value::as_i64)
                .filter_map(|request_id| i32::try_from(request_id).ok())
                .collect();
            if let Some(request_id) = args
                .get("request_id")
                .and_then(serde_json::Value::as_i64)
                .and_then(|request_id| i32::try_from(request_id).ok())
            {
                request_ids.push(request_id);
            }
            request_ids.sort_unstable();
            request_ids.dedup();
            if request_ids.is_empty() {
                return text_result("error: request_ids required".into());
            }
            match crate::store::repo::mark_human_answers_delivered(db, thread, me, &request_ids)
                .await
            {
                Ok(acknowledged) => {
                    let durable_ids: Vec<u64> = request_ids
                        .iter()
                        .filter_map(|request_id| u64::try_from(*request_id).ok())
                        .collect();
                    reg.discard_durable_answers_for_scope(thread, me, &durable_ids);
                    text_result(format!("acknowledged {acknowledged} durable answer(s)"))
                }
                Err(error) => text_result(format!("error: could not persist inbox ack: {error}")),
            }
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
        return Ok(
            "Could not re-analyze: every repository's checkout is missing on disk \
                   (moved or deleted). Restore the repos and try again."
                .to_string(),
        );
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
    let from = args
        .get("from")
        .and_then(|v| v.as_i64())
        .and_then(|n| i32::try_from(n).ok());
    let to = args
        .get("to")
        .and_then(|v| v.as_i64())
        .and_then(|n| i32::try_from(n).ok());
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
        return text_result("from/to must be repo ids in this workspace (use get_repo_map)".into());
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
    let repo = args
        .get("repo")
        .and_then(|v| v.as_i64())
        .and_then(|n| i32::try_from(n).ok());
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
    let turn = crate::store::repo::next_turn_id(db, thread)
        .await
        .unwrap_or(1)
        - 1;
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
                        "description": "Branch in the target repo to branch the new work OFF. Leave empty to use the repo's default branch (main/master). Set it only when the repo merges into a non-default branch (develop/staging/a release branch)." },
                    "depends_on": { "type": "string",
                        "description": "Optional: the exact `name` of ANOTHER task in THIS SAME call's `directions` list that must be merged before this one is allowed to merge — use this for a cross-repo change set where this task consumes something the other task produces (e.g. a submodule SHA bump, a version bump). Leave empty when this task has no upstream. The name MUST resolve to exactly one task proposed in this SAME call: a typo, an ambiguous duplicate name, a name from another proposal, or a task that gets denied all BLOCK this task's merge (it is never released on a bad reference) until a later propose_directions call supplies a name that resolves cleanly." }
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
            "description": "Read your unread messages from other tasks. Durable human answers include request_id and are replayed until acknowledged. After you have incorporated those answers, call bus_ack with their request_ids. Call this whenever you are told there are new messages; do not assume silence means nothing happened.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "bus_ack",
            "description": "Acknowledge durable human-answer messages only after you received and incorporated them. Idempotent; request_ids are the integers returned by bus_inbox.",
            "inputSchema": { "type": "object",
                "properties": {
                    "request_ids": { "type": "array", "items": { "type": "integer" } }
                }, "required": ["request_ids"] }
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
    restore_durable_human_requests(&db, &bus)
        .await
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::Other, error.to_string()))?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let base = format!("http://127.0.0.1:{}", addr.port());
    let app = router(bus, db, asks);
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Ok((base, handle))
}

/// Rebuild the process-local half of the durable question state before the
/// MCP listener accepts traffic. Open rows become answerable asks; answered
/// rows become inbox messages and stay durable until an explicit `bus_ack`.
async fn restore_durable_human_requests(db: &Db, bus: &BusRegistry) -> anyhow::Result<()> {
    for request in crate::store::repo::list_pending_human_requests(db).await? {
        let id = u64::try_from(request.id)
            .map_err(|_| anyhow::anyhow!("invalid durable question id {}", request.id))?;
        if request.status == crate::store::repo::HUMAN_REQUEST_OPEN {
            bus.restore_human_request(
                request.thread_id,
                &request.direction_scope,
                &request.question,
                id,
            );
        } else if request.status == crate::store::repo::HUMAN_REQUEST_ANSWERED {
            bus.restore_durable_answer(
                request.thread_id,
                id,
                &request.direction_scope,
                &request.answer,
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        is_weft_internal_tool, planner_specs, register_pr_tool, serve, session_servers_for_kind,
        summarize, tool_specs, AdmittedBusIdentity, UNKNOWN_ENGINE,
    };
    use crate::ask::RiskLevel;
    use crate::store::Db;
    use serde_json::{json, Value};

    async fn bus_tool_text(
        base: &str,
        thread: i32,
        direction: &str,
        name: &str,
        arguments: Value,
    ) -> String {
        bus_tool_text_for_session(base, thread, direction, None, name, arguments).await
    }

    async fn bus_tool_text_for_session(
        base: &str,
        thread: i32,
        direction: &str,
        session_id: Option<i32>,
        name: &str,
        arguments: Value,
    ) -> String {
        let mut url = format!("{base}/bus/{thread}/{direction}/mcp");
        if let Some(session_id) = session_id {
            url.push_str(&format!("?session_id={session_id}"));
        }
        let response = reqwest::Client::new()
            .post(url)
            .json(&json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": { "name": name, "arguments": arguments },
            }))
            .send()
            .await
            .unwrap();
        assert!(response.status().is_success());
        let body = response.text().await.unwrap();
        let data = body
            .lines()
            .find_map(|line| line.strip_prefix("data: "))
            .expect("SSE data line");
        let envelope: Value = serde_json::from_str(data).unwrap();
        envelope["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string()
    }

    fn ask_url_for_session(
        base: &str,
        thread: i32,
        direction: &str,
        session_id: Option<i32>,
        tool: &str,
    ) -> String {
        let mut url = format!("{base}/ask/{thread}/{direction}?tool={tool}");
        if let Some(session_id) = session_id {
            url.push_str(&format!("&session_id={session_id}"));
        }
        url
    }

    async fn ask_decision_for_session(
        base: &str,
        thread: i32,
        direction: &str,
        session_id: Option<i32>,
        tool: &str,
        tool_name: &str,
        tool_input: Value,
    ) -> Value {
        reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .post(ask_url_for_session(
                base,
                thread,
                direction,
                session_id,
                tool,
            ))
            .json(&json!({
                "tool_name": tool_name,
                "tool_input": tool_input,
            }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap()
    }

    fn assert_ask_denied(response: &Value) {
        assert_eq!(
            response["hookSpecificOutput"]["permissionDecision"], "deny",
            "invalid or deleting identities must fail closed: {response}"
        );
    }

    struct BusIdentityFixture {
        db: Db,
        workspace_id: i32,
        primary_repo_id: i32,
        secondary_repo_id: i32,
        thread_id: i32,
        direction_id: i32,
        primary_session_id: i32,
        secondary_session_id: i32,
    }

    async fn bus_identity_fixture() -> BusIdentityFixture {
        let db = Db::connect("sqlite::memory:").await.unwrap();
        let workspace = crate::store::repo::create_workspace(&db, "bus identity")
            .await
            .unwrap();
        let primary = crate::store::repo::add_repo_ref(
            &db,
            workspace.id,
            "primary",
            "/tmp/bus-identity-primary",
            "main",
            "",
            true,
        )
        .await
        .unwrap();
        let secondary = crate::store::repo::add_repo_ref(
            &db,
            workspace.id,
            "secondary",
            "/tmp/bus-identity-secondary",
            "main",
            "",
            true,
        )
        .await
        .unwrap();
        let thread = crate::store::repo::create_thread(
            &db,
            workspace.id,
            "bus identity issue",
            "feature",
            "codex",
        )
        .await
        .unwrap();
        let direction = crate::store::repo::create_direction(
            &db,
            thread.id,
            "implementation",
            "codex",
            primary.id,
            "why",
            "impl-only",
            "",
        )
        .await
        .unwrap();
        let primary_session = crate::store::repo::create_session(
            &db,
            direction.id,
            primary.id,
            "codex",
            "/tmp/bus-identity-primary-wt",
        )
        .await
        .unwrap();
        let secondary_session = crate::store::repo::create_session(
            &db,
            direction.id,
            secondary.id,
            "codex",
            "/tmp/bus-identity-secondary-wt",
        )
        .await
        .unwrap();
        BusIdentityFixture {
            db,
            workspace_id: workspace.id,
            primary_repo_id: primary.id,
            secondary_repo_id: secondary.id,
            thread_id: thread.id,
            direction_id: direction.id,
            primary_session_id: primary_session.id,
            secondary_session_id: secondary_session.id,
        }
    }

    #[tokio::test]
    async fn ask_bridge_rejects_missing_wrong_or_forged_exact_session_without_side_effects() {
        let fixture = bus_identity_fixture().await;
        let other_thread = crate::store::repo::create_thread(
            &fixture.db,
            fixture.workspace_id,
            "other ask issue",
            "feature",
            "codex",
        )
        .await
        .unwrap();
        let other_direction = crate::store::repo::create_direction(
            &fixture.db,
            fixture.thread_id,
            "other ask direction",
            "codex",
            fixture.primary_repo_id,
            "why",
            "impl-only",
            "",
        )
        .await
        .unwrap();
        let other_session = crate::store::repo::create_session(
            &fixture.db,
            other_direction.id,
            fixture.primary_repo_id,
            "codex",
            "/tmp/bus-identity-other-ask-wt",
        )
        .await
        .unwrap();
        let bus = crate::bus::BusRegistry::new();
        let asks = crate::ask::AskRegistry::new();
        let (notify_tx, mut notify_rx) = tokio::sync::mpsc::unbounded_channel();
        assert!(asks.set_notifier(notify_tx).is_empty());
        let (persist_tx, mut persist_rx) = tokio::sync::mpsc::unbounded_channel();
        asks.set_persist_notifier(persist_tx);
        let (base, _handle) = serve(bus, fixture.db.clone(), asks.clone()).await.unwrap();
        let scope = fixture.direction_id.to_string();

        let missing = ask_decision_for_session(
            &base,
            fixture.thread_id,
            &scope,
            None,
            "claude",
            "TodoWrite",
            json!({ "todos": [] }),
        )
        .await;
        assert_ask_denied(&missing);

        let wrong = ask_decision_for_session(
            &base,
            fixture.thread_id,
            &scope,
            Some(i32::MAX),
            "claude",
            "TodoWrite",
            json!({ "todos": [] }),
        )
        .await;
        assert_ask_denied(&wrong);

        let wrong_direction = ask_decision_for_session(
            &base,
            fixture.thread_id,
            &scope,
            Some(other_session.id),
            "claude",
            "TodoWrite",
            json!({ "todos": [] }),
        )
        .await;
        assert_ask_denied(&wrong_direction);

        let wrong_thread = ask_decision_for_session(
            &base,
            other_thread.id,
            &scope,
            Some(fixture.primary_session_id),
            "claude",
            "TodoWrite",
            json!({ "todos": [] }),
        )
        .await;
        assert_ask_denied(&wrong_thread);

        let forged_lead = ask_decision_for_session(
            &base,
            fixture.thread_id,
            crate::bus::LEAD,
            Some(fixture.primary_session_id),
            "claude",
            "TodoWrite",
            json!({ "todos": [] }),
        )
        .await;
        assert_ask_denied(&forged_lead);

        assert!(asks.open().is_empty());
        assert!(asks.snapshot_grants().is_empty());
        assert_eq!(asks.read_only_grants(), crate::ask::ReadOnlyGrants::default());
        assert!(notify_rx.try_recv().is_err());
        assert!(persist_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn ask_bridge_preserves_valid_lead_worker_internal_builtin_and_standing_allows() {
        use crate::ask::{FullGrant, GrantSnapshot};

        let fixture = bus_identity_fixture().await;
        let bus = crate::bus::BusRegistry::new();
        let asks = crate::ask::AskRegistry::new();
        let scope = fixture.direction_id.to_string();
        let (base, _handle) = serve(bus, fixture.db, asks.clone()).await.unwrap();

        let lead = ask_decision_for_session(
            &base,
            fixture.thread_id,
            crate::bus::LEAD,
            None,
            "claude",
            "TodoWrite",
            json!({ "todos": [] }),
        )
        .await;
        assert_eq!(lead["hookSpecificOutput"]["permissionDecision"], "allow");
        assert_eq!(
            lead["hookSpecificOutput"]["permissionDecisionReason"],
            "read-only builtin (auto-approved)"
        );

        let internal = ask_decision_for_session(
            &base,
            fixture.thread_id,
            &scope,
            Some(fixture.primary_session_id),
            "claude",
            "mcp__weft_bus__bus_inbox",
            json!({}),
        )
        .await;
        assert_eq!(internal["hookSpecificOutput"]["permissionDecision"], "allow");
        assert_eq!(
            internal["hookSpecificOutput"]["permissionDecisionReason"],
            "weft-internal tool (auto-approved)"
        );

        asks.seed_grants(GrantSnapshot {
            full: vec![FullGrant {
                thread: fixture.thread_id,
                dir: scope.clone(),
            }],
            always: vec![],
        });
        let standing = ask_decision_for_session(
            &base,
            fixture.thread_id,
            &scope,
            Some(fixture.secondary_session_id),
            "claude",
            "Bash",
            json!({ "command": "touch admitted" }),
        )
        .await;
        assert_eq!(standing["hookSpecificOutput"]["permissionDecision"], "allow");
        assert_eq!(
            standing["hookSpecificOutput"]["permissionDecisionReason"],
            "Auto-approved by a weft rule"
        );
        assert!(asks.open().is_empty());
    }

    #[tokio::test]
    async fn ask_bridge_checks_every_delete_marker_before_internal_builtin_or_standing_allow() {
        use crate::ask::{FullGrant, GrantSnapshot};

        let fixture = bus_identity_fixture().await;
        let bus = crate::bus::BusRegistry::new();
        let asks = crate::ask::AskRegistry::new();
        let scope = fixture.direction_id.to_string();
        let seeded = GrantSnapshot {
            full: vec![FullGrant {
                thread: fixture.thread_id,
                dir: scope.clone(),
            }],
            always: vec![],
        };
        asks.seed_grants(seeded.clone());
        let (notify_tx, mut notify_rx) = tokio::sync::mpsc::unbounded_channel();
        assert!(asks.set_notifier(notify_tx).is_empty());
        let (persist_tx, mut persist_rx) = tokio::sync::mpsc::unbounded_channel();
        asks.set_persist_notifier(persist_tx);
        let (base, _handle) = serve(bus, fixture.db.clone(), asks.clone()).await.unwrap();

        crate::store::repo::mark_thread_deleting(&fixture.db, fixture.thread_id)
            .await
            .unwrap();
        let internal = ask_decision_for_session(
            &base,
            fixture.thread_id,
            &scope,
            Some(fixture.secondary_session_id),
            "claude",
            "mcp__weft_bus__bus_inbox",
            json!({}),
        )
        .await;
        assert_ask_denied(&internal);
        crate::store::repo::clear_thread_deleting(&fixture.db, fixture.thread_id)
            .await
            .unwrap();

        crate::store::repo::mark_workspace_deleting(&fixture.db, fixture.workspace_id)
            .await
            .unwrap();
        let standing = ask_decision_for_session(
            &base,
            fixture.thread_id,
            &scope,
            Some(fixture.secondary_session_id),
            "claude",
            "Bash",
            json!({ "command": "touch marker" }),
        )
        .await;
        assert_ask_denied(&standing);
        crate::store::repo::clear_workspace_deleting(&fixture.db, fixture.workspace_id)
            .await
            .unwrap();

        crate::store::repo::mark_repo_deleting(&fixture.db, fixture.primary_repo_id)
            .await
            .unwrap();
        let primary_repo = ask_decision_for_session(
            &base,
            fixture.thread_id,
            &scope,
            Some(fixture.secondary_session_id),
            "claude",
            "TodoWrite",
            json!({ "todos": [] }),
        )
        .await;
        assert_ask_denied(&primary_repo);
        crate::store::repo::clear_repo_deleting(&fixture.db, fixture.primary_repo_id)
            .await
            .unwrap();

        crate::store::repo::mark_repo_deleting(&fixture.db, fixture.secondary_repo_id)
            .await
            .unwrap();
        let session_repo = ask_decision_for_session(
            &base,
            fixture.thread_id,
            &scope,
            Some(fixture.secondary_session_id),
            "claude",
            "TodoWrite",
            json!({ "todos": [] }),
        )
        .await;
        assert_ask_denied(&session_repo);

        assert!(asks.open().is_empty());
        assert_eq!(asks.snapshot_grants(), seeded);
        assert!(notify_rx.try_recv().is_err());
        assert!(persist_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn ask_registration_uses_lifecycle_gate_but_releases_it_before_waiting() {
        let fixture = bus_identity_fixture().await;
        let bus = crate::bus::BusRegistry::new();
        let asks = crate::ask::AskRegistry::new();
        let (base, _handle) = serve(bus.clone(), fixture.db, asks.clone()).await.unwrap();
        let held = bus
            .thread_lifecycle_gate(fixture.thread_id)
            .lock_owned()
            .await;
        let url = ask_url_for_session(
            &base,
            fixture.thread_id,
            &fixture.direction_id.to_string(),
            Some(fixture.secondary_session_id),
            "claude",
        );
        let request = tokio::spawn(async move {
            reqwest::Client::builder()
                .no_proxy()
                .build()
                .unwrap()
                .post(url)
                .json(&json!({
                    "tool_name": "Bash",
                    "tool_input": { "command": "touch gated" },
                }))
                .send()
                .await
                .unwrap()
        });

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(
            asks.open().is_empty(),
            "permission registration must wait behind the thread lifecycle gate"
        );
        drop(held);

        let ask_id = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if let Some(ask) = asks.open().first() {
                    break ask.id;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("permission ask never registered after lifecycle release");
        let waiting_guard = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            bus.thread_lifecycle_gate(fixture.thread_id).lock_owned(),
        )
        .await
        .expect("the one-hour human wait must not retain the lifecycle gate");
        drop(waiting_guard);

        asks.cancel(ask_id);
        let response = request.await.unwrap();
        assert!(response.status().is_success());
        assert_ask_denied(&response.json().await.unwrap());
    }

    #[tokio::test]
    async fn bus_dispatch_admits_live_lead_primary_and_secondary_workers() {
        let fixture = bus_identity_fixture().await;
        let bus = crate::bus::BusRegistry::new();
        let (base, _handle) = serve(
            bus.clone(),
            fixture.db.clone(),
            crate::ask::AskRegistry::new(),
        )
        .await
        .unwrap();
        let scope = fixture.direction_id.to_string();

        let lead = bus_tool_text(
            &base,
            fixture.thread_id,
            crate::bus::LEAD,
            "bus_post",
            json!({ "to": scope, "text": "from lead" }),
        )
        .await;
        assert!(lead.contains("posted to"), "got: {lead}");

        let primary = bus_tool_text_for_session(
            &base,
            fixture.thread_id,
            &scope,
            Some(fixture.primary_session_id),
            "bus_post",
            json!({ "to": "lead", "text": "from primary" }),
        )
        .await;
        assert!(primary.contains("posted to lead"), "got: {primary}");

        let secondary = bus_tool_text_for_session(
            &base,
            fixture.thread_id,
            &scope,
            Some(fixture.secondary_session_id),
            "bus_post",
            json!({ "to": "lead", "text": "from secondary" }),
        )
        .await;
        assert!(secondary.contains("posted to lead"), "got: {secondary}");
        assert_eq!(bus.log(fixture.thread_id).len(), 3);
    }

    #[tokio::test]
    async fn bus_dispatch_rejects_missing_or_wrong_exact_session_identity() {
        let fixture = bus_identity_fixture().await;
        let other_direction = crate::store::repo::create_direction(
            &fixture.db,
            fixture.thread_id,
            "other task",
            "codex",
            fixture.primary_repo_id,
            "why",
            "impl-only",
            "",
        )
        .await
        .unwrap();
        let other_session = crate::store::repo::create_session(
            &fixture.db,
            other_direction.id,
            fixture.primary_repo_id,
            "codex",
            "/tmp/bus-identity-other-wt",
        )
        .await
        .unwrap();
        let other_thread = crate::store::repo::create_thread(
            &fixture.db,
            fixture.workspace_id,
            "other issue",
            "feature",
            "codex",
        )
        .await
        .unwrap();
        let bus = crate::bus::BusRegistry::new();
        let (base, _handle) = serve(
            bus.clone(),
            fixture.db.clone(),
            crate::ask::AskRegistry::new(),
        )
        .await
        .unwrap();
        let scope = fixture.direction_id.to_string();

        let missing = bus_tool_text(
            &base,
            fixture.thread_id,
            &scope,
            "bus_post",
            json!({ "to": "lead", "text": "missing" }),
        )
        .await;
        assert!(
            missing.contains("missing its exact source session"),
            "got: {missing}"
        );

        let wrong_session = bus_tool_text_for_session(
            &base,
            fixture.thread_id,
            &scope,
            Some(other_session.id),
            "bus_post",
            json!({ "to": "lead", "text": "wrong session" }),
        )
        .await;
        assert!(
            wrong_session.contains("does not belong to direction"),
            "got: {wrong_session}"
        );

        let wrong_thread = bus_tool_text_for_session(
            &base,
            other_thread.id,
            &scope,
            Some(fixture.primary_session_id),
            "bus_post",
            json!({ "to": "lead", "text": "wrong thread" }),
        )
        .await;
        assert!(
            wrong_thread.contains("does not belong to thread"),
            "got: {wrong_thread}"
        );

        let forged_lead = bus_tool_text_for_session(
            &base,
            fixture.thread_id,
            crate::bus::LEAD,
            Some(fixture.primary_session_id),
            "bus_post",
            json!({ "to": &scope, "text": "forged lead" }),
        )
        .await;
        assert!(
            forged_lead.contains("lead bus identity cannot carry a worker session"),
            "got: {forged_lead}"
        );
        assert!(bus.log(fixture.thread_id).is_empty());
        assert!(bus.log(other_thread.id).is_empty());
    }

    #[tokio::test]
    async fn deleted_secondary_session_cannot_post_or_wake_surviving_thread() {
        let fixture = bus_identity_fixture().await;
        let bus = crate::bus::BusRegistry::new();
        let (wake_tx, wake_rx) = std::sync::mpsc::channel();
        bus.set_wake_sender(wake_tx);
        let (base, _handle) = serve(
            bus.clone(),
            fixture.db.clone(),
            crate::ask::AskRegistry::new(),
        )
        .await
        .unwrap();

        crate::store::repo::mark_repo_deleting(&fixture.db, fixture.secondary_repo_id)
            .await
            .unwrap();
        crate::store::repo::delete_repo_cascade_with_human_cancellations(
            &fixture.db,
            fixture.secondary_repo_id,
        )
        .await
        .unwrap();
        assert!(
            crate::store::repo::get_direction(&fixture.db, fixture.direction_id)
                .await
                .unwrap()
                .is_some(),
            "deleting a secondary repo must leave the primary direction alive"
        );

        let rejected = bus_tool_text_for_session(
            &base,
            fixture.thread_id,
            &fixture.direction_id.to_string(),
            Some(fixture.secondary_session_id),
            "bus_post",
            json!({ "to": "lead", "text": "late secondary post" }),
        )
        .await;
        assert!(
            rejected.contains("session") && rejected.contains("not found"),
            "got: {rejected}"
        );
        assert!(bus.log(fixture.thread_id).is_empty());
        assert!(bus.inbox(fixture.thread_id, crate::bus::LEAD).is_empty());
        assert!(
            wake_rx.try_recv().is_err(),
            "rejected post must not wake the lead"
        );
    }

    #[tokio::test]
    async fn bus_dispatch_rejects_thread_workspace_and_both_repo_markers() {
        let fixture = bus_identity_fixture().await;
        let bus = crate::bus::BusRegistry::new();
        let (wake_tx, wake_rx) = std::sync::mpsc::channel();
        bus.set_wake_sender(wake_tx);
        let (base, _handle) = serve(
            bus.clone(),
            fixture.db.clone(),
            crate::ask::AskRegistry::new(),
        )
        .await
        .unwrap();
        let scope = fixture.direction_id.to_string();

        crate::store::repo::mark_thread_deleting(&fixture.db, fixture.thread_id)
            .await
            .unwrap();
        let thread_rejected = bus_tool_text(
            &base,
            fixture.thread_id,
            crate::bus::LEAD,
            "bus_post",
            json!({ "to": &scope, "text": "thread marker" }),
        )
        .await;
        assert!(thread_rejected.contains("thread") && thread_rejected.contains("being deleted"));
        crate::store::repo::clear_thread_deleting(&fixture.db, fixture.thread_id)
            .await
            .unwrap();

        crate::store::repo::mark_workspace_deleting(&fixture.db, fixture.workspace_id)
            .await
            .unwrap();
        let workspace_rejected = bus_tool_text_for_session(
            &base,
            fixture.thread_id,
            &scope,
            Some(fixture.primary_session_id),
            "bus_post",
            json!({ "to": "lead", "text": "workspace marker" }),
        )
        .await;
        assert!(
            workspace_rejected.contains("workspace")
                && workspace_rejected.contains("being deleted")
        );
        crate::store::repo::clear_workspace_deleting(&fixture.db, fixture.workspace_id)
            .await
            .unwrap();

        crate::store::repo::mark_repo_deleting(&fixture.db, fixture.primary_repo_id)
            .await
            .unwrap();
        let direction_repo_rejected = bus_tool_text_for_session(
            &base,
            fixture.thread_id,
            &scope,
            Some(fixture.secondary_session_id),
            "bus_post",
            json!({ "to": "lead", "text": "primary repo marker" }),
        )
        .await;
        assert!(
            direction_repo_rejected.contains(&format!("repo {}", fixture.primary_repo_id))
                && direction_repo_rejected.contains("being deleted")
        );
        crate::store::repo::clear_repo_deleting(&fixture.db, fixture.primary_repo_id)
            .await
            .unwrap();

        crate::store::repo::mark_repo_deleting(&fixture.db, fixture.secondary_repo_id)
            .await
            .unwrap();
        let session_repo_rejected = bus_tool_text_for_session(
            &base,
            fixture.thread_id,
            &scope,
            Some(fixture.secondary_session_id),
            "bus_post",
            json!({ "to": "lead", "text": "secondary repo marker" }),
        )
        .await;
        assert!(
            session_repo_rejected.contains(&format!("repo {}", fixture.secondary_repo_id))
                && session_repo_rejected.contains("being deleted")
        );
        crate::store::repo::clear_repo_deleting(&fixture.db, fixture.secondary_repo_id)
            .await
            .unwrap();

        assert!(bus.log(fixture.thread_id).is_empty());
        assert!(bus.inbox(fixture.thread_id, crate::bus::LEAD).is_empty());
        assert!(
            wake_rx.try_recv().is_err(),
            "rejected calls must not emit wakes"
        );
    }

    #[tokio::test]
    async fn durable_answer_replays_until_explicit_bus_ack() {
        let db = Db::connect("sqlite::memory:").await.unwrap();
        let workspace = crate::store::repo::create_workspace(&db, "ws")
            .await
            .unwrap();
        let repo_ref = crate::store::repo::add_repo_ref(
            &db,
            workspace.id,
            "repo",
            "/tmp/repo",
            "main",
            "",
            true,
        )
        .await
        .unwrap();
        let thread =
            crate::store::repo::create_thread(&db, workspace.id, "Issue", "feature", "codex")
                .await
                .unwrap();
        let direction = crate::store::repo::create_direction(
            &db,
            thread.id,
            "Task",
            "codex",
            repo_ref.id,
            "why",
            "impl-only",
            "",
        )
        .await
        .unwrap();
        let session = crate::store::repo::create_session(
            &db,
            direction.id,
            repo_ref.id,
            "codex",
            "/tmp/durable-answer",
        )
        .await
        .unwrap();
        let scope = direction.id.to_string();
        let request = crate::store::repo::create_human_request(
            &db,
            workspace.id,
            thread.id,
            &scope,
            direction.id,
            1,
            0,
            0,
            "REST or GraphQL?",
        )
        .await
        .unwrap();
        crate::store::repo::answer_human_request(
            &db,
            workspace.id,
            request.id,
            request.revision,
            "REST",
        )
        .await
        .unwrap()
        .unwrap();

        // A fresh registry is the process-restart boundary under review.
        let bus = crate::bus::BusRegistry::new();
        let (base, _handle) = serve(bus, db.clone(), crate::ask::AskRegistry::new())
            .await
            .unwrap();

        let first: Value = serde_json::from_str(
            &bus_tool_text_for_session(
                &base,
                thread.id,
                &scope,
                Some(session.id),
                "bus_inbox",
                json!({}),
            )
            .await,
        )
        .unwrap();
        assert_eq!(first.as_array().unwrap().len(), 1);
        assert_eq!(first[0]["request_id"], request.id);
        assert_eq!(first[0]["text"], "REST");
        assert_eq!(
            crate::store::repo::get_human_request(&db, request.id)
                .await
                .unwrap()
                .unwrap()
                .status,
            crate::store::repo::HUMAN_REQUEST_ANSWERED
        );

        // No ack: even though the in-memory inbox was drained, the DB outbox
        // replays the same stable request id instead of losing the answer.
        let replay: Value = serde_json::from_str(
            &bus_tool_text_for_session(
                &base,
                thread.id,
                &scope,
                Some(session.id),
                "bus_inbox",
                json!({}),
            )
            .await,
        )
        .unwrap();
        assert_eq!(replay[0]["request_id"], request.id);

        let wrong_route = bus_tool_text(
            &base,
            thread.id,
            "lead",
            "bus_ack",
            json!({ "request_ids": [request.id] }),
        )
        .await;
        assert!(wrong_route.contains("acknowledged 0"));
        assert_eq!(
            crate::store::repo::get_human_request(&db, request.id)
                .await
                .unwrap()
                .unwrap()
                .status,
            crate::store::repo::HUMAN_REQUEST_ANSWERED
        );

        let ack = bus_tool_text_for_session(
            &base,
            thread.id,
            &scope,
            Some(session.id),
            "bus_ack",
            json!({ "request_ids": [request.id] }),
        )
        .await;
        assert!(ack.contains("acknowledged 1"));
        assert_eq!(
            crate::store::repo::get_human_request(&db, request.id)
                .await
                .unwrap()
                .unwrap()
                .status,
            crate::store::repo::HUMAN_REQUEST_RESOLVED
        );
        let empty: Value = serde_json::from_str(
            &bus_tool_text_for_session(
                &base,
                thread.id,
                &scope,
                Some(session.id),
                "bus_inbox",
                json!({}),
            )
            .await,
        )
        .unwrap();
        assert!(empty.as_array().unwrap().is_empty());

        // Ack is idempotent and route-scoped.
        let duplicate = bus_tool_text_for_session(
            &base,
            thread.id,
            &scope,
            Some(session.id),
            "bus_ack",
            json!({ "request_ids": [request.id] }),
        )
        .await;
        assert!(duplicate.contains("acknowledged 0"));
    }

    #[tokio::test]
    async fn ask_human_persists_the_exact_completed_source_turn() {
        let db = Db::connect("sqlite::memory:").await.unwrap();
        let workspace = crate::store::repo::create_workspace(&db, "ws")
            .await
            .unwrap();
        let repo_ref = crate::store::repo::add_repo_ref(
            &db,
            workspace.id,
            "repo",
            "/tmp/repo",
            "main",
            "",
            true,
        )
        .await
        .unwrap();
        let thread =
            crate::store::repo::create_thread(&db, workspace.id, "Issue", "feature", "codex")
                .await
                .unwrap();
        let direction = crate::store::repo::create_direction(
            &db,
            thread.id,
            "Task",
            "codex",
            repo_ref.id,
            "why",
            "impl-only",
            "",
        )
        .await
        .unwrap();
        let session =
            crate::store::repo::create_session(&db, direction.id, repo_ref.id, "codex", "/tmp/cwd")
                .await
                .unwrap();
        let source = crate::store::repo::insert_lead_message(
            &db,
            thread.id,
            Some(session.id),
            7,
            "user",
            "text",
            r#"{"text":"current turn"}"#,
            "complete",
        )
        .await
        .unwrap();
        crate::store::repo::insert_lead_message(
            &db,
            thread.id,
            Some(session.id),
            8,
            "user",
            "text",
            r#"{"text":"queued revision"}"#,
            "queued",
        )
        .await
        .unwrap();

        // A newer session in the same direction must not steal the source
        // identity of the older engine that owns this injected URL.
        let newer_session = crate::store::repo::create_session(
            &db,
            direction.id,
            repo_ref.id,
            "codex",
            "/tmp/newer-cwd",
        )
        .await
        .unwrap();
        crate::store::repo::insert_lead_message(
            &db,
            thread.id,
            Some(newer_session.id),
            9,
            "user",
            "text",
            r#"{"text":"newer session turn"}"#,
            "complete",
        )
        .await
        .unwrap();

        let scope = direction.id.to_string();
        let (base, _handle) = serve(
            crate::bus::BusRegistry::new(),
            db.clone(),
            crate::ask::AskRegistry::new(),
        )
        .await
        .unwrap();
        let result = bus_tool_text_for_session(
            &base,
            thread.id,
            &scope,
            Some(session.id),
            "ask_human",
            json!({ "text": "Which API?" }),
        )
        .await;
        assert!(result.contains("asked the human"));

        let requests = crate::store::repo::list_open_human_requests(&db, workspace.id)
            .await
            .unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].turn_id, source.turn_id);
        assert_eq!(requests[0].source_message_id, source.id);
        assert_eq!(requests[0].source_session_id, session.id);

        let missing_identity = bus_tool_text(
            &base,
            thread.id,
            &scope,
            "ask_human",
            json!({ "text": "Should fail closed" }),
        )
        .await;
        assert!(missing_identity.contains("missing its exact source session"));
        assert_eq!(
            crate::store::repo::list_open_human_requests(&db, workspace.id)
                .await
                .unwrap()
                .len(),
            1
        );
    }

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

    /// THE FIX (Codex review, PR #159 bus/server.rs:1201): this SPEC TEXT is the lead's only
    /// source of truth for what `depends_on` actually does — it must never drift from the
    /// real, fail-closed behavior `record_upstream_edges` implements (see planner.rs's
    /// `record_upstream_edges_blocks_an_ambiguous_duplicate_name_instead_of_guessing` and
    /// `confirm_blocks_a_consumer_whose_named_upstream_was_denied`: an unresolvable name
    /// BLOCKS the task's merge, it is never silently ignored). An earlier version of this text
    /// claimed the OPPOSITE ("a typo ... is silently ignored"), which could lead the lead to
    /// believe a bad reference is harmless when it actually strands the task's readiness until
    /// a fresh `propose_directions` call fixes the reference.
    #[test]
    fn depends_on_spec_text_describes_the_real_fail_closed_behavior() {
        let specs = planner_specs();
        let propose = specs
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == "propose_directions")
            .expect("propose_directions tool spec exists");
        let desc = propose["inputSchema"]["properties"]["directions"]["items"]["properties"]
            ["depends_on"]["description"]
            .as_str()
            .expect("depends_on has a description");
        assert!(
            desc.contains("BLOCK"),
            "must tell the lead an unresolvable name blocks the task, not just describe the \
             happy path: {desc}"
        );
        assert!(
            !desc.to_lowercase().contains("ignored"),
            "must never claim (even in passing) that a bad reference is ignored — the real \
             behavior is fail-closed, not a silent no-op: {desc}"
        );
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
        assert_ne!(
            key_a, key_b,
            "different args must yield different action keys"
        );
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
        assert!(is_weft_internal_tool(
            "mcp__weft_planner__propose_directions"
        ));
        assert!(is_weft_internal_tool("mcp__weft_bus__bus_inbox"));
        assert!(is_weft_internal_tool("mcp__weft_bus__bus_ack"));
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
        assert!(names.contains(&"bus_ack"), "tool_specs: {names:?}");
    }

    fn tool_text(result: &Value) -> &str {
        result["content"][0]["text"].as_str().unwrap_or("")
    }

    #[tokio::test]
    async fn register_pr_tool_rejects_an_unparseable_url() {
        let db = Db::connect("sqlite::memory:").await.unwrap();
        let result =
            register_pr_tool(&db, 1, AdmittedBusIdentity::Lead, "not a valid pr url", "").await;
        assert!(
            tool_text(&result).contains("could not parse"),
            "got: {}",
            tool_text(&result)
        );
    }

    /// End-to-end through the exact path an agent's `register_pr` call takes:
    /// URL parse → direction lookup (for `repo_id`) → DB upsert — confirming
    /// the row lands attributed to the calling direction/thread, and that the
    /// reply names the host's OWN vocabulary ("Pull request", not a neutral
    /// "PR"/"change" — issue #110's UI-terminology requirement).
    #[tokio::test]
    async fn register_pr_tool_tracks_a_valid_github_pr_url() {
        let db = Db::connect("sqlite::memory:").await.unwrap();
        let ws = crate::store::repo::create_workspace(&db, "ws")
            .await
            .unwrap();
        let repo = crate::store::repo::add_repo_ref(&db, ws.id, "r", "/tmp/r", "main", "", true)
            .await
            .unwrap();
        let thread = crate::store::repo::create_thread(&db, ws.id, "t", "feature", "codex")
            .await
            .unwrap();
        let dir = crate::store::repo::create_direction(
            &db,
            thread.id,
            "task",
            "codex",
            repo.id,
            "why",
            "impl-only",
            "",
        )
        .await
        .unwrap();
        let session =
            crate::store::repo::create_session(&db, dir.id, repo.id, "codex", "/tmp/register-pr")
                .await
                .unwrap();
        let (base, _handle) = serve(
            crate::bus::BusRegistry::new(),
            db.clone(),
            crate::ask::AskRegistry::new(),
        )
        .await
        .unwrap();
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            bus_tool_text_for_session(
                &base,
                thread.id,
                &dir.id.to_string(),
                Some(session.id),
                "register_pr",
                json!({
                    "url": "https://github.com/acme/widgets/pull/9",
                    "title": "my title",
                }),
            ),
        )
        .await
        .expect("register_pr dispatch must not deadlock on the lifecycle gate");
        assert!(result.contains("Pull request #9"), "got: {result}");

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
        let ws = crate::store::repo::create_workspace(&db, "ws")
            .await
            .unwrap();
        let thread = crate::store::repo::create_thread(&db, ws.id, "t", "feature", "codex")
            .await
            .unwrap();
        let result = register_pr_tool(
            &db,
            thread.id,
            AdmittedBusIdentity::Lead,
            "https://github.com/acme/widgets/pull/1",
            "",
        )
        .await;
        assert!(
            tool_text(&result).contains("Pull request #1"),
            "got: {}",
            tool_text(&result)
        );
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
        let ws = crate::store::repo::create_workspace(&db, "ws")
            .await
            .unwrap();
        let thread = crate::store::repo::create_thread(&db, ws.id, "t", "feature", "codex")
            .await
            .unwrap();
        let result = register_pr_tool(
            &db,
            thread.id,
            AdmittedBusIdentity::Lead,
            "https://gitlab.com/my-group/my-project/-/merge_requests/12",
            "",
        )
        .await;
        let text = tool_text(&result).to_lowercase();
        assert!(
            text.contains("not registered"),
            "must clearly say it was not registered, got: {text}"
        );
        assert!(
            text.contains("support"),
            "must clearly explain WHY (unsupported host), got: {text}"
        );

        let tracked =
            crate::store::repo::find_pull_request(&db, "gitlab", "my-group", "my-project", 12)
                .await
                .unwrap();
        assert!(
            tracked.is_none(),
            "an unsupported host must never create a trackable (and un-clearable) row"
        );
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
        assert!(!is_weft_internal_tool(
            "mcp__weft_global__answer_permission"
        ));
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
        let ws = crate::store::repo::create_workspace(&db, "ws")
            .await
            .unwrap();
        crate::store::repo::add_repo_ref(
            &db,
            ws.id,
            "r",
            &root.to_string_lossy(),
            "main",
            "",
            true,
        )
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
