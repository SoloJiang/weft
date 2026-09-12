//! MCP-over-HTTP for the thread bus. Stateless: each POST yields one SSE
//! `event: message` carrying the JSON-RPC response. Identity is derived from
//! the URL path, never agent input (see bus.rs).

use crate::bus::{BusRegistry, Msg};
use crate::events;
use crate::orchestrator::WORKER_START_FAILED;
use crate::store::{ArtifactError, ArtifactStatus, NewArtifact, Store};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde_json::{json, Value};
use tokio::sync::mpsc::UnboundedSender;

/// Shared server state.
#[derive(Clone)]
pub struct McpState {
    pub bus: BusRegistry,
    pub store: Store,
    pub task_dispatch: UnboundedSender<i64>,
}

pub fn router(state: McpState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/bus/{issue}/{party}/mcp", post(mcp_post))
        .with_state(state)
}

async fn healthz() -> &'static str {
    "ok"
}

fn now_unix() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{secs}")
}

fn tool_list(party: &str) -> Value {
    let mut tools = vec![
        json!({
            "name": "bus_post",
            "description": "Post a message to another participant's inbox in this issue. `to` is \"lead\" or a task's numeric id. Your own identity comes from the connection URL — you cannot impersonate anyone.",
            // Codex core treats annotation-less MCP tools as
            // destructive/open-world and demands an approval that
            // approvalPolicy=never auto-rejects ("user rejected MCP tool
            // call"). Declaring the hints below makes the call run
            // unconditionally (verified against codex-rs
            // requires_mcp_tool_approval, 2026-08-08).
            "annotations": {
                "readOnlyHint": false,
                "destructiveHint": false,
                "openWorldHint": false
            },
            "inputSchema": {
                "type": "object",
                "properties": {
                    "to": { "type": "string", "description": "\"lead\" or a task id" },
                    "text": { "type": "string" }
                },
                "required": ["to", "text"]
            }
        }),
        json!({
            "name": "bus_read",
            "description": "Drain your own pending inbox (oldest first). Read messages are durably settled and remain in the issue activity log.",
            "annotations": {
                "readOnlyHint": true,
                "destructiveHint": false,
                "openWorldHint": false
            },
            "inputSchema": { "type": "object", "properties": {} }
        }),
        json!({
            "name": "artifact_list",
            "description": "List this issue's artifacts (test cases, requirements, plans) with their kind, status and current revision. Content is omitted — read one with `artifact_read`. Artifacts belong to the issue, not to your thread: every fork of the lead sees the same documents.",
            "annotations": {
                "readOnlyHint": true,
                "destructiveHint": false,
                "openWorldHint": false,
                "idempotentHint": true
            },
            "inputSchema": { "type": "object", "additionalProperties": false, "properties": {} }
        }),
        json!({
            "name": "artifact_read",
            "description": "Read one artifact in full, including its current revision. Always read before writing: `artifact_write` requires the revision you actually edited, and a stale one is refused.",
            "annotations": {
                "readOnlyHint": true,
                "destructiveHint": false,
                "openWorldHint": false,
                "idempotentHint": true
            },
            "inputSchema": {
                "type": "object",
                "additionalProperties": false,
                "properties": { "id": { "type": "integer", "minimum": 1 } },
                "required": ["id"]
            }
        }),
    ];
    if party == "lead" {
        tools.push(json!({
            "name": "task_create",
            "description": "Create and automatically dispatch one worker task for the current issue. Lead only. Use this after decomposing the issue; the human does not create or approve worker tasks manually.",
            "annotations": {
                "readOnlyHint": false,
                "destructiveHint": false,
                "openWorldHint": false,
                "idempotentHint": false
            },
            "inputSchema": {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "name": { "type": "string", "maxLength": 120, "description": "Short user-facing task name" },
                    "repo_id": { "type": "integer", "minimum": 1, "description": "Repository id from the lead brief" },
                    "spec": { "type": "string", "maxLength": 20000, "description": "Complete implementation brief for the worker" },
                    "reason": { "type": "string", "maxLength": 2000, "description": "Why this repository must change" },
                    "mandate": { "type": "string", "enum": ["plan+impl", "impl-only"], "default": "plan+impl" },
                    "base_branch": { "type": "string", "maxLength": 255, "description": "Defaults to the repository base branch" }
                },
                "required": ["name", "repo_id", "spec"]
            }
        }));
        tools.push(json!({
            "name": "repo_list",
            "description": "List the repositories currently attached to this issue's workspace. Lead only. Refresh this after the human adds repositories.",
            "annotations": {
                "readOnlyHint": true,
                "destructiveHint": false,
                "openWorldHint": false,
                "idempotentHint": true
            },
            "inputSchema": {
                "type": "object",
                "additionalProperties": false,
                "properties": {}
            }
        }));
        tools.push(json!({
            "name": "artifact_write",
            "description": "Create or revise an issue artifact. Lead only. Omit `id` to create; supply `id` together with the `expected_revision` you just read to revise. A stale revision is refused with the revision that won, so read again and re-apply rather than retrying blindly. Publish structured documents here instead of embedding them in chat — the human edits this, and plans reference it by revision.",
            "annotations": {
                "readOnlyHint": false,
                "destructiveHint": false,
                "openWorldHint": false,
                "idempotentHint": false
            },
            "inputSchema": {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "id": { "type": "integer", "minimum": 1, "description": "Omit to create a new artifact" },
                    "expected_revision": { "type": "integer", "minimum": 1, "description": "Required when `id` is given" },
                    "kind": { "type": "string", "enum": ["test_cases", "requirements", "plan", "change_set_summary"], "description": "Required when creating" },
                    "title": { "type": "string", "maxLength": 200 },
                    "format": { "type": "string", "enum": ["markdown_tree", "markdown", "json"], "default": "markdown" },
                    "content": { "type": "string" }
                },
                "required": ["content"]
            }
        }));
        tools.push(json!({
            "name": "artifact_status",
            "description": "Move an artifact to ready, stale or superseded. Lead only. `superseded` is final — a replaced artifact never comes back. Mark an artifact stale when the work it describes has moved on, with a reason the human can act on.",
            "annotations": {
                "readOnlyHint": false,
                "destructiveHint": false,
                "openWorldHint": false,
                "idempotentHint": false
            },
            "inputSchema": {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "id": { "type": "integer", "minimum": 1 },
                    "expected_revision": { "type": "integer", "minimum": 1 },
                    "status": { "type": "string", "enum": ["ready", "stale", "superseded"] },
                    "stale_reason": { "type": "string", "maxLength": 2000, "description": "Kept only for `stale`" }
                },
                "required": ["id", "expected_revision", "status"]
            }
        }));
    }
    json!({ "tools": tools })
}

fn text_result(text: String) -> Value {
    json!({ "content": [{ "type": "text", "text": text }] })
}

/// Report an artifact failure to an agent as parseable JSON rather than prose.
///
/// The agent has to *act* on the difference — a revision conflict means "read
/// again and re-apply", an illegal transition means "stop" — so the reason
/// travels as a `code` plus the same fields the HTTP layer exposes.
fn artifact_error_result(error: ArtifactError) -> Value {
    let payload = match &error {
        ArtifactError::NotFound { id } => json!({ "code": "not_found", "artifactId": id }),
        ArtifactError::RevisionConflict {
            id,
            expected,
            actual,
        } => json!({
            "code": "revision_conflict",
            "artifactId": id,
            "expectedRevision": expected,
            "actualRevision": actual,
            "hint": "read the artifact again and re-apply your change on top of the current revision"
        }),
        ArtifactError::ContentTooLarge { limit, actual } => {
            json!({ "code": "content_too_large", "limit": limit, "actual": actual })
        }
        ArtifactError::UnsupportedValue { field, value } => {
            json!({ "code": "unsupported_value", "field": field, "value": value })
        }
        ArtifactError::IllegalTransition { from, to } => json!({
            "code": "illegal_transition",
            "from": from.as_str(),
            "to": to.as_str()
        }),
        ArtifactError::Database(inner) => {
            json!({ "code": "store_failure", "detail": format!("{inner:#}") })
        }
    };
    let mut payload = payload;
    if let Some(object) = payload.as_object_mut() {
        object.insert("error".into(), Value::String(error.to_string()));
    }
    text_result(payload.to_string())
}

/// Artifact writes are lead-only by default. A worker that wants a change asks
/// for it over the bus, so the request is visible and attributable instead of
/// two threads racing on the same document.
fn lead_only(party: &str, tool: &str) -> Option<Value> {
    if party == "lead" {
        return None;
    }
    Some(text_result(
        json!({
            "code": "lead_only",
            "error": format!("{tool} is available only to the lead"),
            "hint": "post to the lead with bus_post describing the change you need"
        })
        .to_string(),
    ))
}

/// Handle one JSON-RPC payload. Returns None for notifications (202 path).
/// Kept transport-free so tests don't need HTTP.
pub async fn handle_rpc(state: &McpState, issue: i64, party: &str, body: &Value) -> Option<Value> {
    let method = body.get("method").and_then(Value::as_str).unwrap_or("");
    let id = body.get("id").cloned();
    match (method, id) {
        ("initialize", Some(id)) => Some(json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "protocolVersion": "2024-11-05",
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "weft-codex bus", "version": env!("CARGO_PKG_VERSION") }
            }
        })),
        ("tools/list", Some(id)) => Some(json!({
            "jsonrpc": "2.0", "id": id, "result": tool_list(party)
        })),
        ("tools/call", Some(id)) => {
            let params = body.get("params").cloned().unwrap_or(Value::Null);
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            let args = params.get("arguments").cloned().unwrap_or(Value::Null);
            let result = call_tool(state, issue, party, name, &args).await;
            Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
        }
        // Notifications (initialized, cancelled, …) need no response.
        (_, None) => None,
        (_, Some(id)) => Some(json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": -32601, "message": format!("unknown method: {method}") }
        })),
    }
}

async fn call_tool(state: &McpState, issue: i64, party: &str, name: &str, args: &Value) -> Value {
    match name {
        "bus_post" => {
            let to = args.get("to").and_then(Value::as_str).unwrap_or("");
            let text = args.get("text").and_then(Value::as_str).unwrap_or("");
            if to.is_empty() || text.is_empty() {
                return text_result("error: `to` and `text` are required".into());
            }
            if to == party {
                return text_result("error: cannot post to yourself".into());
            }
            let id = match state.store.bus_append(issue, party, to, text).await {
                Ok(id) => id,
                Err(error) => {
                    return text_result(format!("error: durable write failed: {error:#}"));
                }
            };
            let msg = Msg {
                id,
                from: party.to_string(),
                text: text.to_string(),
                kind: "message".to_string(),
                ts: now_unix(),
            };
            state.bus.post(issue, to, msg);
            text_result(format!("posted to {to}"))
        }
        "bus_read" => {
            let msgs = state.bus.drain(issue, party);
            let ids: Vec<i64> = msgs.iter().map(|msg| msg.id).collect();
            if let Err(error) = state.store.mark_bus_delivered(&ids).await {
                state.bus.requeue_front(issue, party, msgs);
                return text_result(format!("error: durable read settlement failed: {error:#}"));
            }
            text_result(serde_json::to_string(&msgs).unwrap_or_else(|_| "[]".into()))
        }
        "task_create" => create_task(state, issue, party, args).await,
        "repo_list" => list_repos(state, issue, party).await,
        "artifact_list" => match state.store.list_artifacts(issue).await {
            // Summaries only: an agent that needs a body calls artifact_read,
            // so listing never floods the context with full documents.
            Ok(rows) => {
                let summary: Vec<Value> = rows
                    .iter()
                    .map(|row| {
                        json!({
                            "id": row.id,
                            "kind": row.kind,
                            "title": row.title,
                            "format": row.format,
                            "status": row.status,
                            "revision": row.revision,
                            "staleReason": row.stale_reason,
                            "updatedAt": row.updated_at
                        })
                    })
                    .collect();
                text_result(json!(summary).to_string())
            }
            Err(error) => artifact_error_result(error),
        },
        "artifact_read" => {
            let Some(id) = args.get("id").and_then(Value::as_i64) else {
                return text_result("error: `id` is required".into());
            };
            match state.store.get_artifact(id).await {
                Ok(Some(row)) if row.issue_id == issue => text_result(json!(row).to_string()),
                // Cross-issue reads are refused rather than reported as a
                // lookup failure: an artifact of another issue is not yours.
                Ok(Some(_)) | Ok(None) => artifact_error_result(ArtifactError::NotFound { id }),
                Err(error) => artifact_error_result(error),
            }
        }
        "artifact_write" => {
            if let Some(refusal) = lead_only(party, "artifact_write") {
                return refusal;
            }
            write_artifact(state, issue, args).await
        }
        "artifact_status" => {
            if let Some(refusal) = lead_only(party, "artifact_status") {
                return refusal;
            }
            let id = args.get("id").and_then(Value::as_i64).unwrap_or(0);
            let expected = args
                .get("expected_revision")
                .and_then(Value::as_i64)
                .unwrap_or(0);
            let status = args.get("status").and_then(Value::as_str).unwrap_or("");
            let Some(status) = ArtifactStatus::parse(status) else {
                return artifact_error_result(ArtifactError::UnsupportedValue {
                    field: "status",
                    value: status.to_string(),
                });
            };
            let reason = args
                .get("stale_reason")
                .and_then(Value::as_str)
                .unwrap_or("");
            match owned_artifact(state, issue, id).await {
                Err(error) => artifact_error_result(error),
                Ok(()) => match state
                    .store
                    .set_artifact_status(id, expected, status, reason)
                    .await
                {
                    Ok(row) => {
                        events::emit(
                            match status {
                                ArtifactStatus::Stale => "artifact.stale",
                                ArtifactStatus::Superseded => "artifact.superseded",
                                _ => "artifact.updated",
                            },
                            json!({ "id": row.id, "issueId": row.issue_id, "kind": row.kind,
                                    "revision": row.revision, "status": row.status }),
                        );
                        text_result(json!(row).to_string())
                    }
                    Err(error) => artifact_error_result(error),
                },
            }
        }
        other => text_result(format!("error: unknown tool: {other}")),
    }
}

/// Confirm an artifact exists *and* belongs to this issue before mutating it.
/// The MCP connection is scoped to one issue, so a thread must never be able to
/// reach across into another issue's documents by guessing an id.
async fn owned_artifact(state: &McpState, issue: i64, id: i64) -> Result<(), ArtifactError> {
    match state.store.get_artifact(id).await? {
        Some(row) if row.issue_id == issue => Ok(()),
        _ => Err(ArtifactError::NotFound { id }),
    }
}

async fn write_artifact(state: &McpState, issue: i64, args: &Value) -> Value {
    let content = args.get("content").and_then(Value::as_str).unwrap_or("");
    let title = args.get("title").and_then(Value::as_str).unwrap_or("");

    let Some(id) = args.get("id").and_then(Value::as_i64) else {
        // No id means "create". `kind` is mandatory here and nowhere else.
        let kind = args.get("kind").and_then(Value::as_str).unwrap_or("");
        let format = args
            .get("format")
            .and_then(Value::as_str)
            .unwrap_or("markdown");
        return match state
            .store
            .create_artifact(NewArtifact {
                issue_id: issue,
                kind,
                title,
                format,
                content,
                source: "agent",
                source_thread_id: "",
            })
            .await
        {
            Ok(row) => {
                events::emit(
                    "artifact.created",
                    json!({ "id": row.id, "issueId": row.issue_id, "kind": row.kind,
                            "revision": row.revision, "status": row.status }),
                );
                text_result(json!(row).to_string())
            }
            Err(error) => artifact_error_result(error),
        };
    };

    let Some(expected) = args.get("expected_revision").and_then(Value::as_i64) else {
        return artifact_error_result(ArtifactError::UnsupportedValue {
            field: "expected_revision",
            value: "missing".to_string(),
        });
    };
    if let Err(error) = owned_artifact(state, issue, id).await {
        return artifact_error_result(error);
    }
    match state
        .store
        .update_artifact_content(id, expected, content, title, "agent", "")
        .await
    {
        Ok(row) => {
            events::emit(
                "artifact.updated",
                json!({ "id": row.id, "issueId": row.issue_id, "kind": row.kind,
                        "revision": row.revision, "status": row.status }),
            );
            text_result(json!(row).to_string())
        }
        Err(error) => artifact_error_result(error),
    }
}

async fn list_repos(state: &McpState, issue_id: i64, party: &str) -> Value {
    if party != "lead" {
        return text_result("error: repo_list is available only to the lead".into());
    }
    let issue = match state.store.get_issue(issue_id).await {
        Ok(Some(issue)) => issue,
        Ok(None) => return text_result(format!("error: unknown issue {issue_id}")),
        Err(error) => return text_result(format!("error: issue lookup failed: {error:#}")),
    };
    let repos = match state.store.list_repos(issue.workspace_id).await {
        Ok(repos) => repos,
        Err(error) => return text_result(format!("error: repository lookup failed: {error:#}")),
    };
    let result: Vec<Value> = repos
        .into_iter()
        .map(|repo| {
            json!({
                "repo_id": repo.id,
                "name": repo.name,
                "base_branch": repo.base_ref
            })
        })
        .collect();
    text_result(Value::Array(result).to_string())
}

fn task_slug(name: &str) -> String {
    let mut slug = String::new();
    let mut length = 0;
    let mut separator_pending = false;
    for character in name.chars().flat_map(char::to_lowercase) {
        let is_cjk = ('\u{4e00}'..='\u{9fff}').contains(&character);
        if character.is_ascii_alphanumeric() || is_cjk {
            if separator_pending && !slug.is_empty() {
                if length + 1 >= 48 {
                    break;
                }
                slug.push('-');
                length += 1;
            }
            separator_pending = false;
            if length >= 48 {
                break;
            }
            slug.push(character);
            length += 1;
        } else if !slug.is_empty() {
            separator_pending = true;
        }
    }
    if slug.is_empty() {
        "task".to_string()
    } else {
        slug
    }
}

async fn create_task(state: &McpState, issue_id: i64, party: &str, args: &Value) -> Value {
    if party != "lead" {
        return text_result("error: task_create is available only to the lead".into());
    }
    let name = args
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    let spec = args
        .get("spec")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    let Some(repo_id) = args.get("repo_id").and_then(Value::as_i64) else {
        return text_result("error: `repo_id` must be a positive integer".into());
    };
    if name.is_empty() || spec.is_empty() {
        return text_result("error: non-empty `name` and `spec` are required".into());
    }
    if name.chars().count() > 120 || spec.chars().count() > 20_000 {
        return text_result("error: task name or spec exceeds the allowed length".into());
    }
    if repo_id <= 0 {
        return text_result("error: `repo_id` must be a positive integer".into());
    }
    let mandate = args
        .get("mandate")
        .and_then(Value::as_str)
        .unwrap_or("plan+impl");
    if mandate != "plan+impl" && mandate != "impl-only" {
        return text_result("error: `mandate` must be plan+impl or impl-only".into());
    }
    let reason = args
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    if reason.chars().count() > 2_000 {
        return text_result("error: task reason exceeds the allowed length".into());
    }

    let issue = match state.store.get_issue(issue_id).await {
        Ok(Some(issue)) => issue,
        Ok(None) => return text_result(format!("error: unknown issue {issue_id}")),
        Err(error) => return text_result(format!("error: issue lookup failed: {error:#}")),
    };
    let repo = match state.store.get_repo(repo_id).await {
        Ok(Some(repo)) => repo,
        Ok(None) => return text_result(format!("error: unknown repository {repo_id}")),
        Err(error) => return text_result(format!("error: repository lookup failed: {error:#}")),
    };
    if repo.workspace_id != issue.workspace_id {
        return text_result(format!(
            "error: repository {repo_id} is not in issue {issue_id}'s workspace"
        ));
    }
    let requested_base = args
        .get("base_branch")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    let base_branch = if requested_base.is_empty() {
        repo.base_ref.as_str()
    } else {
        requested_base
    };
    let slug = task_slug(name);
    let task_id = match state
        .store
        .add_direction(
            issue_id,
            name,
            &slug,
            repo_id,
            mandate,
            base_branch,
            reason,
            spec,
        )
        .await
    {
        Ok(id) => id,
        Err(error) => return text_result(format!("error: task creation failed: {error:#}")),
    };
    events::emit(
        "direction.updated",
        json!({ "id": task_id, "issueId": issue_id, "status": "queued" }),
    );
    if state.task_dispatch.send(task_id).is_err() {
        let _ = state
            .store
            .set_direction_attention(task_id, Some(WORKER_START_FAILED))
            .await;
        events::emit(
            "direction.updated",
            json!({
                "id": task_id,
                "issueId": issue_id,
                "status": "queued",
                "attention": true,
                "reason": WORKER_START_FAILED
            }),
        );
        return text_result(format!(
            "error: task {task_id} was created, but automatic worker dispatch is unavailable"
        ));
    }
    text_result(
        json!({
            "task_id": task_id,
            "name": name,
            "repo_id": repo_id,
            "repo": repo.name,
            "status": "queued",
            "dispatch": "automatic"
        })
        .to_string(),
    )
}

async fn mcp_post(
    State(state): State<McpState>,
    Path((issue, party)): Path<(i64, String)>,
    Json(body): Json<Value>,
) -> Response {
    match handle_rpc(&state, issue, &party, &body).await {
        Some(resp) => {
            let frame = format!("event: message\ndata: {resp}\n\n");
            (
                StatusCode::OK,
                [("content-type", "text/event-stream")],
                frame,
            )
                .into_response()
        }
        None => StatusCode::ACCEPTED.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Returns the state plus the TempDir GUARD — the guard must outlive the
    /// test, otherwise the directory (and the SQLite file with it) is deleted
    /// while the pool is still using it.
    async fn fixture() -> (
        McpState,
        tempfile::TempDir,
        tokio::sync::mpsc::UnboundedReceiver<i64>,
    ) {
        let dir = tempfile::tempdir().expect("tmp");
        let store = Store::open(&dir.path().join("t.db")).await.expect("open");
        let (task_dispatch, task_dispatch_rx) = tokio::sync::mpsc::unbounded_channel();
        let state = McpState {
            bus: BusRegistry::new(),
            store,
            task_dispatch,
        };
        (state, dir, task_dispatch_rx)
    }

    #[tokio::test]
    async fn initialize_and_tool_flow() {
        let (st, _dir, _task_dispatch_rx) = fixture().await;
        let init = handle_rpc(
            &st,
            1,
            "lead",
            &json!({
                "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}
            }),
        )
        .await
        .expect("init resp");
        assert_eq!(init["result"]["serverInfo"]["name"], "weft-codex bus");

        let list = handle_rpc(
            &st,
            1,
            "lead",
            &json!({
                "jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}
            }),
        )
        .await
        .expect("list resp");
        // Assert by name, not position: the list grows, and an index-based
        // assertion only reports that something moved, not what broke.
        let tool = |name: &str| -> Value {
            list["result"]["tools"]
                .as_array()
                .and_then(|tools| {
                    tools
                        .iter()
                        .find(|entry| entry["name"] == name)
                        .cloned()
                })
                .unwrap_or_else(|| panic!("lead is missing the {name} tool"))
        };

        // Codex auto-rejects annotation-less MCP tools under
        // approvalPolicy=never, so every tool must declare its hints.
        for name in [
            "bus_post",
            "bus_read",
            "task_create",
            "repo_list",
            "artifact_list",
            "artifact_read",
            "artifact_write",
            "artifact_status",
        ] {
            let entry = tool(name);
            let annotations = &entry["annotations"];
            assert!(
                annotations.is_object(),
                "{name} has no annotations; Codex would auto-reject it"
            );
            assert_eq!(annotations["destructiveHint"], false, "{name}");
            assert_eq!(annotations["openWorldHint"], false, "{name}");
        }
        for name in ["bus_read", "repo_list", "artifact_list", "artifact_read"] {
            assert_eq!(tool(name)["annotations"]["readOnlyHint"], true, "{name}");
        }
        for name in [
            "bus_post",
            "task_create",
            "artifact_write",
            "artifact_status",
        ] {
            assert_eq!(tool(name)["annotations"]["readOnlyHint"], false, "{name}");
        }

        let worker_list = handle_rpc(
            &st,
            1,
            "3",
            &json!({
                "jsonrpc": "2.0", "id": 20, "method": "tools/list", "params": {}
            }),
        )
        .await
        .expect("worker list resp");
        // A worker sees the read tools but none of the writers.
        let worker_tools: Vec<String> = worker_list["result"]["tools"]
            .as_array()
            .map(|tools| {
                tools
                    .iter()
                    .filter_map(|entry| entry["name"].as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        assert!(worker_tools.contains(&"artifact_list".to_string()));
        assert!(worker_tools.contains(&"artifact_read".to_string()));
        for denied in ["artifact_write", "artifact_status", "task_create", "repo_list"] {
            assert!(
                !worker_tools.contains(&denied.to_string()),
                "a worker must not be offered {denied}"
            );
        }

        // lead posts to direction 3; identity comes from the URL (party).
        let post = handle_rpc(
            &st,
            1,
            "lead",
            &json!({
                "jsonrpc": "2.0", "id": 3, "method": "tools/call",
                "params": { "name": "bus_post", "arguments": { "to": "3", "text": "hello" } }
            }),
        )
        .await
        .expect("post resp");
        let posted_text = post["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or("")
            .to_string();
        assert!(
            posted_text.contains("posted to 3"),
            "unexpected bus_post response: {posted_text}; full: {post}"
        );

        // Direction 3 reads its inbox; the durable audit log has the row too.
        let read = handle_rpc(
            &st,
            1,
            "3",
            &json!({
                "jsonrpc": "2.0", "id": 4, "method": "tools/call",
                "params": { "name": "bus_read", "arguments": {} }
            }),
        )
        .await
        .expect("read resp");
        let text = read["result"]["content"][0]["text"].as_str().unwrap_or("");
        assert!(text.contains("hello"));
        assert!(text.contains("\"from\":\"lead\""));
        let durable = st.store.bus_inbox(1, "3").await.expect("inbox");
        assert_eq!(durable.len(), 1);
        assert!(!durable[0].delivered_at.is_empty());
        assert!(st
            .store
            .pending_bus_messages()
            .await
            .expect("pending")
            .is_empty());
    }

    #[tokio::test]
    async fn task_create_is_lead_only_and_scoped_to_the_issue_workspace() {
        let (st, _dir, mut task_dispatch_rx) = fixture().await;
        let workspace = st
            .store
            .create_workspace("Product", "product")
            .await
            .expect("workspace");
        let repo = st
            .store
            .add_repo(workspace, "api", "/tmp/api", "trunk")
            .await
            .expect("repo");
        let issue = st
            .store
            .create_issue(workspace, "Fix login", "fix-login")
            .await
            .expect("issue");

        let listed = handle_rpc(
            &st,
            issue,
            "lead",
            &json!({
                "jsonrpc": "2.0", "id": 0, "method": "tools/call",
                "params": { "name": "repo_list", "arguments": {} }
            }),
        )
        .await
        .expect("repo list response");
        let listed_text = listed["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or("");
        assert!(listed_text.contains("\"repo_id\":1"));
        assert!(listed_text.contains("\"name\":\"api\""));
        assert!(listed_text.contains("\"base_branch\":\"trunk\""));

        let list_denied = handle_rpc(
            &st,
            issue,
            "7",
            &json!({
                "jsonrpc": "2.0", "id": 10, "method": "tools/call",
                "params": { "name": "repo_list", "arguments": {} }
            }),
        )
        .await
        .expect("denied repo list response");
        assert!(list_denied["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or("")
            .contains("only to the lead"));

        let denied = handle_rpc(
            &st,
            issue,
            "7",
            &json!({
                "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                "params": { "name": "task_create", "arguments": {
                    "name": "Backend fix", "repo_id": repo, "spec": "Repair session expiry"
                } }
            }),
        )
        .await
        .expect("denied response");
        assert!(denied["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or("")
            .contains("only to the lead"));

        let created = handle_rpc(
            &st,
            issue,
            "lead",
            &json!({
                "jsonrpc": "2.0", "id": 2, "method": "tools/call",
                "params": { "name": "task_create", "arguments": {
                    "name": "Backend fix",
                    "repo_id": repo,
                    "spec": "Repair session expiry",
                    "reason": "The session store is owned here"
                } }
            }),
        )
        .await
        .expect("create response");
        assert!(created["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or("")
            .contains("\"task_id\":1"));
        let tasks = st.store.list_directions(issue).await.expect("tasks");
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].name, "Backend fix");
        assert_eq!(tasks[0].spec, "Repair session expiry");
        assert_eq!(tasks[0].base_branch, "trunk");
        assert_eq!(tasks[0].status, "queued");
        assert_eq!(
            task_dispatch_rx.try_recv().expect("automatic dispatch"),
            tasks[0].id
        );

        let other_workspace = st
            .store
            .create_workspace("Other", "other")
            .await
            .expect("other workspace");
        let other_repo = st
            .store
            .add_repo(other_workspace, "web", "/tmp/web", "main")
            .await
            .expect("other repo");
        let rejected = handle_rpc(
            &st,
            issue,
            "lead",
            &json!({
                "jsonrpc": "2.0", "id": 3, "method": "tools/call",
                "params": { "name": "task_create", "arguments": {
                    "name": "Wrong workspace", "repo_id": other_repo, "spec": "Do not create"
                } }
            }),
        )
        .await
        .expect("scope response");
        assert!(rejected["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or("")
            .contains("not in issue"));
        assert_eq!(
            st.store.list_directions(issue).await.expect("tasks").len(),
            1
        );
    }

    #[test]
    fn task_slug_preserves_cjk_and_has_no_trailing_separator() {
        assert_eq!(task_slug("修复 Login 回调"), "修复-login-回调");
        assert!(!task_slug(&format!("{} end", "x".repeat(60))).ends_with('-'));
        assert!(task_slug("***").len() > 0);
    }

    #[tokio::test]
    async fn task_create_marks_attention_when_automatic_dispatch_is_unavailable() {
        let (st, _dir, task_dispatch_rx) = fixture().await;
        drop(task_dispatch_rx);
        let workspace = st
            .store
            .create_workspace("Product", "product")
            .await
            .expect("workspace");
        let repo = st
            .store
            .add_repo(workspace, "api", "/tmp/api", "main")
            .await
            .expect("repo");
        let issue = st
            .store
            .create_issue(workspace, "Fix login", "fix-login")
            .await
            .expect("issue");

        let response = handle_rpc(
            &st,
            issue,
            "lead",
            &json!({
                "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                "params": { "name": "task_create", "arguments": {
                    "name": "Backend fix", "repo_id": repo, "spec": "Repair session expiry"
                } }
            }),
        )
        .await
        .expect("create response");
        assert!(response["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or("")
            .contains("automatic worker dispatch is unavailable"));
        let task = st
            .store
            .list_directions(issue)
            .await
            .expect("tasks")
            .pop()
            .expect("task");
        assert_eq!(task.attention, 1);
        assert_eq!(task.attention_reason, WORKER_START_FAILED);
    }

    #[tokio::test]
    async fn notifications_get_no_response() {
        let (st, _dir, _task_dispatch_rx) = fixture().await;
        let resp = handle_rpc(
            &st,
            1,
            "lead",
            &json!({
                "jsonrpc": "2.0", "method": "notifications/initialized"
            }),
        )
        .await;
        assert!(resp.is_none());
    }

    /// The test-cases skill tells an agent which tools to call by name. If a
    /// tool is renamed and the skill is not, the agent follows instructions
    /// that quietly no-op — so the document is checked against the real list.
    #[test]
    fn the_test_cases_skill_only_names_tools_that_exist() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../skills/weft-derive-test-cases/SKILL.md");
        let skill = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));

        let offered: Vec<String> = tool_list("lead")["tools"]
            .as_array()
            .expect("tools")
            .iter()
            .filter_map(|entry| entry["name"].as_str().map(str::to_string))
            .collect();

        // Every `artifact_*` token the skill mentions must be a real tool.
        let mut mentioned: Vec<String> = Vec::new();
        for raw in skill.split(|c: char| !c.is_alphanumeric() && c != '_') {
            if raw.starts_with("artifact_") && !mentioned.iter().any(|seen| seen == raw) {
                mentioned.push(raw.to_string());
            }
        }
        assert!(
            !mentioned.is_empty(),
            "the skill no longer references any artifact tool"
        );
        for name in &mentioned {
            assert!(
                offered.contains(name),
                "the skill tells the agent to call `{name}`, which is not offered"
            );
        }

        // And the workflow it describes has to be reachable: read, write, status.
        for required in ["artifact_read", "artifact_write", "artifact_status"] {
            assert!(
                mentioned.iter().any(|name| name == required),
                "the skill stopped mentioning {required}"
            );
        }

        // The sentinel is gone for good — publishing goes through MCP now.
        assert!(
            !skill.contains("<weft:test_cases>"),
            "the skill still emits the legacy sentinel instead of writing an artifact"
        );
    }

    /// Tool results are text, so the structured payload rides inside it.
    /// Parsing here mirrors exactly what an agent has to do.
    fn tool_json(response: &Value) -> Value {
        let text = response["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or("");
        serde_json::from_str(text).unwrap_or_else(|_| json!({ "raw": text }))
    }

    async fn call(state: &McpState, issue: i64, party: &str, name: &str, args: Value) -> Value {
        handle_rpc(
            state,
            issue,
            party,
            &json!({
                "jsonrpc": "2.0", "id": 99, "method": "tools/call",
                "params": { "name": name, "arguments": args }
            }),
        )
        .await
        .expect("tool response")
    }

    #[tokio::test]
    async fn artifact_write_round_trips_and_refuses_a_stale_revision() {
        let (st, _dir, _rx) = fixture().await;
        let ws = st.store.create_workspace("W", "w").await.expect("ws");
        let issue = st.store.create_issue(ws, "one", "one").await.expect("i");

        let created = tool_json(
            &call(
                &st,
                issue,
                "lead",
                "artifact_write",
                json!({ "kind": "test_cases", "title": "Checkout", "content": "- case one" }),
            )
            .await,
        );
        assert_eq!(created["revision"], 1);
        assert_eq!(created["status"], "draft");
        let id = created["id"].as_i64().expect("id");

        let read = tool_json(&call(&st, issue, "lead", "artifact_read", json!({ "id": id })).await);
        assert_eq!(read["content"], "- case one");

        let updated = tool_json(
            &call(
                &st,
                issue,
                "lead",
                "artifact_write",
                json!({ "id": id, "expected_revision": 1, "content": "- case two" }),
            )
            .await,
        );
        assert_eq!(updated["revision"], 2);

        // A fork that still holds revision 1 must be told what won, not just
        // that it failed — it has to re-read and re-apply.
        let conflict = tool_json(
            &call(
                &st,
                issue,
                "lead",
                "artifact_write",
                json!({ "id": id, "expected_revision": 1, "content": "from a stale fork" }),
            )
            .await,
        );
        assert_eq!(conflict["code"], "revision_conflict");
        assert_eq!(conflict["expectedRevision"], 1);
        assert_eq!(conflict["actualRevision"], 2);
        assert!(conflict["hint"].is_string());

        let listed = tool_json(&call(&st, issue, "lead", "artifact_list", json!({})).await);
        assert_eq!(listed.as_array().map(Vec::len), Some(1));
        assert!(
            listed[0].get("content").is_none(),
            "listing must stay a summary"
        );
    }

    #[tokio::test]
    async fn a_worker_may_read_artifacts_but_never_write_them() {
        let (st, _dir, _rx) = fixture().await;
        let ws = st.store.create_workspace("W", "w").await.expect("ws");
        let issue = st.store.create_issue(ws, "one", "one").await.expect("i");
        let created = tool_json(
            &call(
                &st,
                issue,
                "lead",
                "artifact_write",
                json!({ "kind": "plan", "content": "step one" }),
            )
            .await,
        );
        let id = created["id"].as_i64().expect("id");

        let read = tool_json(&call(&st, issue, "7", "artifact_read", json!({ "id": id })).await);
        assert_eq!(read["content"], "step one", "workers may read");

        for (tool, args) in [
            (
                "artifact_write",
                json!({ "id": id, "expected_revision": 1, "content": "worker edit" }),
            ),
            (
                "artifact_status",
                json!({ "id": id, "expected_revision": 1, "status": "ready" }),
            ),
        ] {
            let refused = tool_json(&call(&st, issue, "7", tool, args).await);
            assert_eq!(refused["code"], "lead_only", "{tool}");
            // The refusal has to say what to do instead, or the worker will
            // simply retry the same call.
            assert!(
                refused["hint"].as_str().unwrap_or("").contains("bus_post"),
                "{tool} refusal must point at the bus"
            );
        }

        let current = st.store.get_artifact(id).await.expect("get").expect("row");
        assert_eq!(current.content, "step one");
        assert_eq!(current.revision, 1);
    }

    #[tokio::test]
    async fn an_artifact_from_another_issue_is_not_reachable() {
        let (st, _dir, _rx) = fixture().await;
        let ws = st.store.create_workspace("W", "w").await.expect("ws");
        let mine = st.store.create_issue(ws, "one", "one").await.expect("i1");
        let theirs = st.store.create_issue(ws, "two", "two").await.expect("i2");
        let created = tool_json(
            &call(
                &st,
                theirs,
                "lead",
                "artifact_write",
                json!({ "kind": "plan", "content": "not yours" }),
            )
            .await,
        );
        let id = created["id"].as_i64().expect("id");

        // The connection is scoped to one issue; guessing an id must not cross
        // that boundary, and the refusal must not confirm the row exists.
        for tool in ["artifact_read"] {
            let refused = tool_json(&call(&st, mine, "lead", tool, json!({ "id": id })).await);
            assert_eq!(refused["code"], "not_found", "{tool}");
        }
        let refused = tool_json(
            &call(
                &st,
                mine,
                "lead",
                "artifact_write",
                json!({ "id": id, "expected_revision": 1, "content": "hijack" }),
            )
            .await,
        );
        assert_eq!(refused["code"], "not_found");

        let untouched = st.store.get_artifact(id).await.expect("get").expect("row");
        assert_eq!(untouched.content, "not yours");
    }

    #[tokio::test]
    async fn superseding_is_final_over_mcp() {
        let (st, _dir, _rx) = fixture().await;
        let ws = st.store.create_workspace("W", "w").await.expect("ws");
        let issue = st.store.create_issue(ws, "one", "one").await.expect("i");
        let created = tool_json(
            &call(
                &st,
                issue,
                "lead",
                "artifact_write",
                json!({ "kind": "requirements", "content": "v1" }),
            )
            .await,
        );
        let id = created["id"].as_i64().expect("id");

        let superseded = tool_json(
            &call(
                &st,
                issue,
                "lead",
                "artifact_status",
                json!({ "id": id, "expected_revision": 1, "status": "superseded" }),
            )
            .await,
        );
        assert_eq!(superseded["status"], "superseded");

        let revived = tool_json(
            &call(
                &st,
                issue,
                "lead",
                "artifact_write",
                json!({ "id": id, "expected_revision": 2, "content": "resurrected" }),
            )
            .await,
        );
        assert_eq!(revived["code"], "illegal_transition");
    }
}
