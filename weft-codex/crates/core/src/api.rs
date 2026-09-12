//! HTTP API for the weft-codex kanban UI (Stage 3 consumes this). Served by
//! the same axum listener as the MCP bus; the daemon merges both routers.
//!
//! Classified failures are [`crate::api_error::ApiError`] (and [`ArtifactError`]
//! for documents). `fail()` walks the anyhow chain for one of those; anything
//! else is 500. It does not sniff message text.

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{
        sse::{Event, Sse},
        IntoResponse, Response,
    },
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio_stream::StreamExt;

use crate::api_error::{classified, ApiError};
use crate::orchestrator::Orchestrator;
use crate::store::{ArtifactError, ArtifactRow, ArtifactStatus, NewArtifact, Store};
use crate::{curator, events, repo_intake};

#[derive(Clone)]
pub struct ApiState {
    pub store: Store,
    pub orch: Orchestrator,
}

pub fn router(state: ApiState) -> Router {
    Router::new()
        .route(
            "/api/workspaces",
            post(create_workspace).get(list_workspaces),
        )
        .route("/api/ui-state", get(get_ui_state).post(set_ui_state))
        .route("/api/workspaces/{id}/repos", post(add_repo).get(list_repos))
        .route("/api/workspaces/{id}/repos/import", post(import_repos))
        .route("/api/issues", post(create_issue).get(kanban))
        .route("/api/issues/{id}/spawn-lead", post(spawn_lead))
        .route("/api/issues/{id}/message", post(message_lead))
        .route("/api/issues/{id}/bus", get(bus_log))
        .route("/api/threads/resolve", post(resolve_thread))
        .route("/api/issues/{id}/lead-thread", post(promote_lead_thread))
        .route("/api/directions/{id}/spawn", post(spawn_direction))
        .route("/api/directions/{id}/message", post(message_direction))
        .route("/api/directions/{id}/complete", post(complete_direction))
        .route(
            "/api/directions/{id}/attention/clear",
            post(clear_attention),
        )
        .route("/api/repos/{id}/analyze", post(analyze_repo))
        .route("/api/repos/{id}/profile", get(repo_profile))
        .route("/api/workspaces/{id}/analyze", post(analyze_workspace))
        .route(
            "/api/workspaces/{id}/analyze-relations",
            post(analyze_relations),
        )
        .route("/api/workspaces/{id}/repo-map", get(repo_map))
        .route(
            "/api/issues/{id}/artifacts",
            post(create_artifact).get(list_artifacts),
        )
        .route(
            "/api/issues/{id}/artifact-staleness",
            get(artifact_staleness),
        )
        .route("/api/artifacts/{id}", get(get_artifact).post(update_artifact))
        .route("/api/artifacts/{id}/status", post(set_artifact_status))
        .route("/api/events", get(sse_events))
        .with_state(state)
}

/// Live UI event feed (Stage 3's kanban subscribes here). Lagged frames are
/// skipped — events are advisory, never load-bearing.
async fn sse_events() -> Response {
    let Some(rx) = events::subscribe() else {
        return reject(ApiError::bad_request(
            "events_unavailable",
            "events channel not installed",
        ));
    };
    let stream = tokio_stream::wrappers::BroadcastStream::new(rx).filter_map(|item| {
        let (event, value) = item.ok()?;
        Some(Ok::<_, std::convert::Infallible>(
            Event::default().event(event).data(value.to_string()),
        ))
    });
    Sse::new(stream).into_response()
}

fn http_status(code: u16) -> StatusCode {
    match code {
        400 => StatusCode::BAD_REQUEST,
        404 => StatusCode::NOT_FOUND,
        409 => StatusCode::CONFLICT,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

fn reject(error: ApiError) -> Response {
    (http_status(error.status()), Json(error.body())).into_response()
}

fn fail(e: anyhow::Error) -> Response {
    if let Some(api) = classified(&e) {
        return reject(api.clone());
    }
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({ "code": "internal", "error": format!("{e:#}") })),
    )
        .into_response()
}

fn ok(value: Value) -> Response {
    Json(value).into_response()
}

/// Map an artifact failure to a response the UI can branch on.
///
/// Unlike [`fail`], nothing here inspects a message: the reason is a type, and
/// it travels as a stable `code` plus the fields that make it actionable. A
/// revision conflict carries both revisions so the client can reload or merge
/// without a second round trip.
fn artifact_fail(error: ArtifactError) -> Response {
    let (status, body) = match error {
        ArtifactError::NotFound { id } => (
            StatusCode::NOT_FOUND,
            json!({ "code": "not_found", "artifactId": id }),
        ),
        ArtifactError::RevisionConflict {
            id,
            expected,
            actual,
        } => (
            StatusCode::CONFLICT,
            json!({
                "code": "revision_conflict",
                "artifactId": id,
                "expectedRevision": expected,
                "actualRevision": actual
            }),
        ),
        ArtifactError::ContentTooLarge { limit, actual } => (
            StatusCode::PAYLOAD_TOO_LARGE,
            json!({ "code": "content_too_large", "limit": limit, "actual": actual }),
        ),
        ArtifactError::UnsupportedValue { field, ref value } => (
            StatusCode::BAD_REQUEST,
            json!({ "code": "unsupported_value", "field": field, "value": value }),
        ),
        ArtifactError::IllegalTransition { from, to } => (
            StatusCode::CONFLICT,
            json!({
                "code": "illegal_transition",
                "from": from.as_str(),
                "to": to.as_str()
            }),
        ),
        ArtifactError::Database(ref inner) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({ "code": "store_failure", "detail": format!("{inner:#}") }),
        ),
    };
    let mut payload = body;
    if let Some(object) = payload.as_object_mut() {
        object.insert("error".into(), Value::String(error.to_string()));
    }
    (status, Json(payload)).into_response()
}

fn artifact_event(name: &str, row: &ArtifactRow) {
    events::emit(
        name,
        json!({
            "id": row.id,
            "issueId": row.issue_id,
            "kind": row.kind,
            "revision": row.revision,
            "status": row.status
        }),
    );
}

/// The SSE name for a status move, so a client can react to "this went stale"
/// without diffing two snapshots.
fn status_event(status: ArtifactStatus) -> &'static str {
    match status {
        ArtifactStatus::Stale => "artifact.stale",
        ArtifactStatus::Superseded => "artifact.superseded",
        _ => "artifact.updated",
    }
}

#[derive(Deserialize)]
struct CreateArtifact {
    kind: String,
    #[serde(default)]
    title: String,
    #[serde(default = "default_format")]
    format: String,
    #[serde(default)]
    content: String,
    #[serde(default = "default_source")]
    source: String,
    #[serde(default)]
    source_thread_id: String,
}

fn default_format() -> String {
    "markdown".to_string()
}

fn default_source() -> String {
    "agent".to_string()
}

#[derive(Deserialize)]
struct UpdateArtifact {
    /// Required, and deliberately not defaulted: a client that forgets it must
    /// get a deserialisation error rather than silently overwrite whatever the
    /// current revision happens to be.
    expected_revision: i64,
    content: String,
    #[serde(default)]
    title: String,
    #[serde(default = "default_source")]
    source: String,
    #[serde(default)]
    source_thread_id: String,
}

#[derive(Deserialize)]
struct SetArtifactStatus {
    expected_revision: i64,
    status: String,
    #[serde(default)]
    stale_reason: String,
}

#[derive(Deserialize)]
struct CreateWorkspace {
    name: String,
    slug: String,
}

async fn list_artifacts(State(state): State<ApiState>, Path(issue_id): Path<i64>) -> Response {
    match state.store.list_artifacts(issue_id).await {
        Ok(rows) => ok(json!(rows)),
        Err(error) => artifact_fail(error),
    }
}

/// Tasks whose planning document has moved on since they were created.
///
/// Served as its own answer rather than left for each client to join: the
/// comparison is a product rule, and three clients re-deriving it would
/// eventually disagree about what "stale" means.
async fn artifact_staleness(State(state): State<ApiState>, Path(issue_id): Path<i64>) -> Response {
    match state.store.stale_artifact_basis(issue_id).await {
        Ok(rows) => ok(json!(rows
            .into_iter()
            .map(|(direction_id, used, current)| json!({
                "directionId": direction_id,
                "usedRevision": used,
                "currentRevision": current
            }))
            .collect::<Vec<Value>>())),
        Err(error) => fail(error),
    }
}

async fn get_artifact(State(state): State<ApiState>, Path(id): Path<i64>) -> Response {
    match state.store.get_artifact(id).await {
        Ok(Some(row)) => ok(json!(row)),
        Ok(None) => artifact_fail(ArtifactError::NotFound { id }),
        Err(error) => artifact_fail(error),
    }
}

async fn create_artifact(
    State(state): State<ApiState>,
    Path(issue_id): Path<i64>,
    Json(body): Json<CreateArtifact>,
) -> Response {
    let created = state
        .store
        .create_artifact(NewArtifact {
            issue_id,
            kind: &body.kind,
            title: &body.title,
            format: &body.format,
            content: &body.content,
            source: &body.source,
            source_thread_id: &body.source_thread_id,
        })
        .await;
    match created {
        Ok(row) => {
            artifact_event("artifact.created", &row);
            ok(json!(row))
        }
        Err(error) => artifact_fail(error),
    }
}

async fn update_artifact(
    State(state): State<ApiState>,
    Path(id): Path<i64>,
    Json(body): Json<UpdateArtifact>,
) -> Response {
    let updated = state
        .store
        .update_artifact_content(
            id,
            body.expected_revision,
            &body.content,
            &body.title,
            &body.source,
            &body.source_thread_id,
        )
        .await;
    match updated {
        Ok(row) => {
            artifact_event("artifact.updated", &row);
            ok(json!(row))
        }
        Err(error) => artifact_fail(error),
    }
}

/// Lifecycle moves, including supersede — `{"status": "superseded"}` is the
/// supersede operation. Keeping one endpoint means there is exactly one place
/// where the transition rules apply.
async fn set_artifact_status(
    State(state): State<ApiState>,
    Path(id): Path<i64>,
    Json(body): Json<SetArtifactStatus>,
) -> Response {
    let Some(status) = ArtifactStatus::parse(&body.status) else {
        return artifact_fail(ArtifactError::UnsupportedValue {
            field: "status",
            value: body.status,
        });
    };
    let updated = state
        .store
        .set_artifact_status(id, body.expected_revision, status, &body.stale_reason)
        .await;
    match updated {
        Ok(row) => {
            artifact_event(status_event(status), &row);
            ok(json!(row))
        }
        Err(error) => artifact_fail(error),
    }
}

#[derive(Deserialize)]
struct PromoteLeadThread {
    thread_id: String,
}

/// Point the issue's lead at a different one of its own threads.
///
/// The human forked the lead in Codex and kept working there; this makes that
/// fork the one the bus talks to. Delivery follows immediately because
/// `thread_for` reads the canonical column this moves. A turn already running
/// on the previous thread finishes where it is — messages are durable and the
/// agent can always `bus_read`, so nothing is lost, it just lands one turn late.
async fn promote_lead_thread(
    State(state): State<ApiState>,
    Path(issue_id): Path<i64>,
    Json(body): Json<PromoteLeadThread>,
) -> Response {
    let thread_id = body.thread_id.trim();
    if thread_id.is_empty() {
        return reject(ApiError::bad_request("empty_thread_id", "empty thread_id"));
    }
    match state.store.promote_lead_thread(issue_id, thread_id).await {
        Ok(()) => {
            events::emit(
                "thread.binding.updated",
                json!({ "issueId": issue_id, "threadId": thread_id, "primary": true }),
            );
            ok(json!({ "leadThreadId": thread_id }))
        }
        Err(e) => fail(e),
    }
}

async fn create_workspace(
    State(state): State<ApiState>,
    Json(body): Json<CreateWorkspace>,
) -> Response {
    match state.store.create_workspace(&body.name, &body.slug).await {
        Ok(id) => {
            events::emit("workspace.updated", json!({ "id": id }));
            ok(json!({ "id": id }))
        }
        Err(e) => fail(e),
    }
}

async fn list_workspaces(State(state): State<ApiState>) -> Response {
    match state.store.list_workspaces().await {
        Ok(rows) => ok(json!(rows)),
        Err(e) => fail(e),
    }
}

/// Every field is optional and absent means "leave it alone".
///
/// The UI writes these from different places at different times — the
/// workspace on selection, the pane width on drag end — so a POST that carried
/// only the field it knows about must not clear the other one.
/// Distinguishes an absent key from an explicit `null`.
///
/// `Option<Option<T>>` alone does not: serde lets the outer `Option` swallow a
/// JSON `null`, so both cases arrive as `None` and an explicit clear becomes a
/// no-op. Going through the inner `Option` and wrapping the result keeps them
/// apart — absent falls to `default` (`None`), `null` becomes `Some(None)`.
fn explicit_option<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

/// Absent leaves the stored value alone; `null` clears it.
#[derive(Deserialize)]
struct SetUiState {
    #[serde(rename = "lastWorkspaceId", default, deserialize_with = "explicit_option")]
    last_workspace_id: Option<Option<i64>>,
    #[serde(rename = "detailPaneWidth", default, deserialize_with = "explicit_option")]
    detail_pane_width: Option<Option<i64>>,
}

async fn get_ui_state(State(state): State<ApiState>) -> Response {
    let last_workspace_id = match state.store.last_workspace_id().await {
        Ok(value) => value,
        Err(e) => return fail(e),
    };
    let detail_pane_width = match state.store.detail_pane_width().await {
        Ok(value) => value,
        Err(e) => return fail(e),
    };
    ok(json!({
        "lastWorkspaceId": last_workspace_id,
        "detailPaneWidth": detail_pane_width,
    }))
}

async fn set_ui_state(State(state): State<ApiState>, Json(body): Json<SetUiState>) -> Response {
    if let Some(width) = body.detail_pane_width {
        if let Err(e) = state.store.set_detail_pane_width(width).await {
            return fail(e);
        }
    }
    if let Some(workspace_id) = body.last_workspace_id {
        if let Err(e) = state.store.set_last_workspace_id(workspace_id).await {
            return fail(e);
        }
    }
    get_ui_state(State(state)).await
}

#[derive(Deserialize)]
struct AddRepo {
    name: Option<String>,
    path: String,
}

fn queue_intake_analysis(store: Store, workspace_id: i64, repo_ids: Vec<i64>) {
    if repo_ids.is_empty() {
        return;
    }
    tokio::spawn(async move {
        if let Err(error) = curator::analyze_imported_repos(&store, workspace_id, repo_ids).await {
            events::emit(
                "repo.relations",
                json!({ "workspaceId": workspace_id, "error": format!("{error:#}") }),
            );
        }
    });
}

async fn repo_needs_analysis(store: &Store, repo_id: i64) -> anyhow::Result<bool> {
    let profile = store.get_profile(repo_id).await?;
    Ok(match profile {
        None => true,
        Some(profile) => profile.run_state == "idle" || profile.run_state == "failed",
    })
}

async fn add_repo(
    State(state): State<ApiState>,
    Path(workspace_id): Path<i64>,
    Json(body): Json<AddRepo>,
) -> Response {
    let inspected = match repo_intake::inspect_local_repo(&body.path, body.name.as_deref()).await {
        Ok(repo) => repo,
        Err(error) => return fail(error),
    };
    let (repo, added) = match state
        .store
        .register_repo(
            workspace_id,
            &inspected.name,
            &inspected.path,
            &inspected.base_ref,
            &inspected.remote_url,
            inspected.base_ref_is_default,
        )
        .await
    {
        Ok(result) => result,
        Err(error) => return fail(error),
    };
    if added {
        events::emit(
            "repo.added",
            json!({ "id": repo.id, "workspaceId": workspace_id }),
        );
    }
    let analysis_queued = match repo_needs_analysis(&state.store, repo.id).await {
        Ok(needs_analysis) => needs_analysis,
        Err(error) => return fail(error),
    };
    if analysis_queued {
        queue_intake_analysis(state.store.clone(), workspace_id, vec![repo.id]);
    }
    ok(json!({
        "id": repo.id,
        "repo": repo,
        "added": added,
        "analysisQueued": analysis_queued,
    }))
}

#[derive(Deserialize)]
struct ImportRepos {
    paths: Vec<String>,
}

/// Batch local-repository intake. Each row is independent so one malformed
/// path does not discard repositories already validated in the same action.
async fn import_repos(
    State(state): State<ApiState>,
    Path(workspace_id): Path<i64>,
    Json(body): Json<ImportRepos>,
) -> Response {
    if body.paths.is_empty() {
        return reject(ApiError::bad_request(
            "import_empty",
            "at least one path is required",
        ));
    }
    if body.paths.len() > 64 {
        return reject(ApiError::bad_request(
            "import_too_many",
            "at most 64 paths are allowed",
        ));
    }

    let mut results = Vec::with_capacity(body.paths.len());
    let mut analysis_ids = std::collections::BTreeSet::new();
    let mut added_count = 0usize;
    let mut existing_count = 0usize;
    let mut failed_count = 0usize;
    for requested_path in body.paths {
        let inspected = match repo_intake::inspect_local_repo(&requested_path, None).await {
            Ok(repo) => repo,
            Err(error) => {
                failed_count += 1;
                results.push(json!({
                    "requested_path": requested_path,
                    "status": "error",
                    "error": format!("{error:#}"),
                }));
                continue;
            }
        };
        let registration = state
            .store
            .register_repo(
                workspace_id,
                &inspected.name,
                &inspected.path,
                &inspected.base_ref,
                &inspected.remote_url,
                inspected.base_ref_is_default,
            )
            .await;
        let (repo, added) = match registration {
            Ok(result) => result,
            Err(error) => {
                failed_count += 1;
                results.push(json!({
                    "requested_path": requested_path,
                    "status": "error",
                    "error": format!("{error:#}"),
                }));
                continue;
            }
        };
        if added {
            added_count += 1;
            events::emit(
                "repo.added",
                json!({ "id": repo.id, "workspaceId": workspace_id }),
            );
        } else {
            existing_count += 1;
        }
        match repo_needs_analysis(&state.store, repo.id).await {
            Ok(true) => {
                analysis_ids.insert(repo.id);
            }
            Ok(false) => {}
            Err(error) => {
                failed_count += 1;
                results.push(json!({
                    "requested_path": requested_path,
                    "status": "error",
                    "error": format!("{error:#}"),
                }));
                continue;
            }
        }
        results.push(json!({
            "requested_path": requested_path,
            "status": if added { "added" } else { "existing" },
            "repo": repo,
        }));
    }

    let analysis_ids: Vec<i64> = analysis_ids.into_iter().collect();
    let analysis_queued = !analysis_ids.is_empty();
    queue_intake_analysis(state.store.clone(), workspace_id, analysis_ids);
    ok(json!({
        "results": results,
        "added": added_count,
        "existing": existing_count,
        "failed": failed_count,
        "analysisQueued": analysis_queued,
    }))
}

async fn list_repos(State(state): State<ApiState>, Path(workspace_id): Path<i64>) -> Response {
    match state.store.list_repos(workspace_id).await {
        Ok(rows) => ok(json!(rows)),
        Err(e) => fail(e),
    }
}

#[derive(Deserialize)]
struct CreateIssue {
    workspace_id: i64,
    title: String,
    slug: String,
    kind: String,
}

async fn create_issue(State(state): State<ApiState>, Json(body): Json<CreateIssue>) -> Response {
    let id = match state
        .store
        .create_issue_with_kind(body.workspace_id, &body.title, &body.slug, &body.kind)
        .await
    {
        Ok(id) => id,
        Err(e) => return fail(e),
    };
    events::emit("issue.updated", json!({ "id": id }));

    // Start the lead here rather than leaving it to the caller. Doing it client
    // side meant every new issue existed without a lead for as long as the
    // round trip took, and an issue created through the API or MCP never got
    // one at all — three different situations that looked identical on the
    // board. Now "no lead" means exactly one thing: starting it failed.
    //
    // A failed start does not fail the issue: the issue is real and the lead is
    // retryable, so the failure is recorded as attention (spawn_lead flags it)
    // and surfaced, not thrown away.
    let lead = state.orch.spawn_lead(id).await.ok();
    ok(json!({ "id": id, "codexThreadId": lead }))
}

#[derive(Deserialize)]
struct KanbanQuery {
    workspace_id: i64,
}

/// The kanban board payload: every issue with its directions.
async fn kanban(State(state): State<ApiState>, Query(q): Query<KanbanQuery>) -> Response {
    match state
        .store
        .list_issues_with_directions(q.workspace_id)
        .await
    {
        Ok(pairs) => {
            let mut board = Vec::with_capacity(pairs.len());
            for (issue, directions) in pairs {
                let threads = match state.store.list_thread_bindings(issue.id).await {
                    Ok(rows) => rows,
                    Err(error) => return fail(error),
                };
                // Summaries only: the board refreshes on every SSE nudge, and
                // artifact bodies can be a megabyte each.
                let artifacts = match state.store.list_artifact_summaries(issue.id).await {
                    Ok(rows) => rows,
                    Err(error) => return artifact_fail(error),
                };
                board.push(json!({
                    "issue": issue,
                    "directions": directions,
                    "threads": threads,
                    "artifacts": artifacts
                }));
            }
            ok(json!(board))
        }
        Err(e) => fail(e),
    }
}

#[derive(Deserialize)]
struct ResolveThread {
    thread_id: String,
}

fn valid_thread_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'-'))
}

/// Map the active native Codex thread to a Weft location. Unknown ordinary
/// Codex chats are a normal null result; explicit native forks are persisted
/// only when their ancestry reaches a known Lead/Task primary.
async fn resolve_thread(
    State(state): State<ApiState>,
    Json(body): Json<ResolveThread>,
) -> Response {
    if !valid_thread_id(&body.thread_id) {
        return reject(ApiError::bad_request("invalid_thread_id", "invalid thread id"));
    }
    let binding = match state.orch.resolve_thread_binding(&body.thread_id).await {
        Ok(binding) => binding,
        Err(error) => return fail(error),
    };
    let Some(binding) = binding else {
        return ok(json!({ "binding": null, "workspaceId": null }));
    };
    let issue = match state.store.get_issue(binding.issue_id).await {
        Ok(Some(issue)) => issue,
        Ok(None) => return reject(ApiError::not_found("issue", binding.issue_id)),
        Err(error) => return fail(error),
    };
    ok(json!({ "binding": binding, "workspaceId": issue.workspace_id }))
}

async fn spawn_lead(State(state): State<ApiState>, Path(issue_id): Path<i64>) -> Response {
    match state.orch.spawn_lead(issue_id).await {
        Ok(thread_id) => ok(json!({ "codexThreadId": thread_id })),
        Err(e) => fail(e),
    }
}

async fn spawn_direction(State(state): State<ApiState>, Path(direction_id): Path<i64>) -> Response {
    match state.orch.dispatch_direction(direction_id).await {
        Ok(thread_id) => ok(json!({ "codexThreadId": thread_id })),
        Err(e) => fail(e),
    }
}

#[derive(Deserialize)]
struct Message {
    text: String,
}

async fn message_lead(
    State(state): State<ApiState>,
    Path(issue_id): Path<i64>,
    Json(body): Json<Message>,
) -> Response {
    if body.text.trim().is_empty() {
        return reject(ApiError::bad_request("empty_message", "empty message text"));
    }
    match state.orch.human_message_lead(issue_id, &body.text).await {
        Ok(()) => ok(json!({ "queued": true })),
        Err(e) => fail(e),
    }
}

async fn message_direction(
    State(state): State<ApiState>,
    Path(direction_id): Path<i64>,
    Json(body): Json<Message>,
) -> Response {
    if body.text.trim().is_empty() {
        return reject(ApiError::bad_request("empty_message", "empty message text"));
    }
    match state
        .orch
        .human_message_direction(direction_id, &body.text)
        .await
    {
        Ok(()) => ok(json!({ "queued": true })),
        Err(e) => fail(e),
    }
}

async fn complete_direction(
    State(state): State<ApiState>,
    Path(direction_id): Path<i64>,
) -> Response {
    match state.orch.complete_direction(direction_id).await {
        Ok(()) => ok(json!({ "ok": true })),
        Err(e) => fail(e),
    }
}

async fn clear_attention(State(state): State<ApiState>, Path(direction_id): Path<i64>) -> Response {
    match state.orch.clear_direction_attention(direction_id).await {
        Ok(()) => ok(json!({ "ok": true })),
        Err(e) => fail(e),
    }
}

/// The issue's durable bus log (audit feed for the kanban UI).
async fn bus_log(State(state): State<ApiState>, Path(issue_id): Path<i64>) -> Response {
    match state.store.bus_log(issue_id).await {
        Ok(rows) => ok(json!(rows)),
        Err(e) => fail(e),
    }
}

/// Kick a per-repo curator analysis (background task; watch `repo.profile`
/// events / poll the profile endpoint for the result).
async fn analyze_repo(State(state): State<ApiState>, Path(repo_id): Path<i64>) -> Response {
    match state.store.get_repo(repo_id).await {
        Ok(Some(_)) => {}
        Ok(None) => return reject(ApiError::not_found("repo", repo_id)),
        Err(e) => return fail(e),
    }
    match state.store.get_profile(repo_id).await {
        Ok(Some(p)) if p.run_state == "running" => {
            return reject(ApiError::conflict(
                "already_running",
                format!("repo {repo_id} already has a running analysis"),
            ));
        }
        Ok(_) => {}
        Err(e) => return fail(e),
    }
    let store = state.store.clone();
    tokio::spawn(async move {
        if let Err(e) = curator::analyze_repo(&store, repo_id).await {
            events::emit(
                "repo.profile",
                json!({ "repoId": repo_id, "runState": "failed", "error": format!("{e:#}") }),
            );
        }
    });
    (StatusCode::ACCEPTED, Json(json!({ "queued": true }))).into_response()
}

async fn repo_profile(State(state): State<ApiState>, Path(repo_id): Path<i64>) -> Response {
    match state.store.get_profile(repo_id).await {
        Ok(Some(p)) => ok(json!(p)),
        Ok(None) => reject(ApiError::not_found("profile", repo_id)),
        Err(e) => fail(e),
    }
}

/// Kick the full workspace pass: profile every repo, then cross-repo
/// relations + layers + the repo-map document.
async fn analyze_workspace(
    State(state): State<ApiState>,
    Path(workspace_id): Path<i64>,
) -> Response {
    match state.store.list_repos(workspace_id).await {
        Ok(repos) if repos.is_empty() => {
            return reject(ApiError::bad_request(
                "workspace_empty",
                format!("workspace {workspace_id} has no repos"),
            ));
        }
        Ok(_) => {}
        Err(e) => return fail(e),
    }
    let store = state.store.clone();
    tokio::spawn(async move {
        if let Err(e) = curator::analyze_workspace(&store, workspace_id).await {
            events::emit(
                "repo.relations",
                json!({ "workspaceId": workspace_id, "error": format!("{e:#}") }),
            );
        }
    });
    (StatusCode::ACCEPTED, Json(json!({ "queued": true }))).into_response()
}

/// Re-run ONLY the cross-repo pass (profiles must already be done).
async fn analyze_relations(
    State(state): State<ApiState>,
    Path(workspace_id): Path<i64>,
) -> Response {
    let store = state.store.clone();
    tokio::spawn(async move {
        if let Err(e) = curator::analyze_relations(&store, workspace_id).await {
            events::emit(
                "repo.relations",
                json!({ "workspaceId": workspace_id, "error": format!("{e:#}") }),
            );
        }
    });
    (StatusCode::ACCEPTED, Json(json!({ "queued": true }))).into_response()
}

/// The repo-map payload: every repo with its profile, all relation edges,
/// and the synthesized markdown document.
async fn repo_map(State(state): State<ApiState>, Path(workspace_id): Path<i64>) -> Response {
    let repos = match state.store.list_repos(workspace_id).await {
        Ok(r) => r,
        Err(e) => return fail(e),
    };
    let mut entries = Vec::with_capacity(repos.len());
    for repo in repos {
        let profile = match state.store.get_profile(repo.id).await {
            Ok(p) => p,
            Err(e) => return fail(e),
        };
        entries.push(json!({ "repo": repo, "profile": profile }));
    }
    let relations = match state.store.list_relations(workspace_id).await {
        Ok(r) => r,
        Err(e) => return fail(e),
    };
    let doc = match state.store.get_workspace_repo_map(workspace_id).await {
        Ok(d) => d,
        Err(e) => return fail(e),
    };
    ok(json!({ "repos": entries, "relations": relations, "repoMap": doc }))
}

#[cfg(test)]
mod fail_tests {
    use super::*;

    #[test]
    fn a_message_that_says_unknown_is_not_a_404() {
        let response = fail(anyhow::anyhow!("unknown internal invariant broken"));
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn a_classified_unknown_issue_is_404() {
        let response = fail(ApiError::not_found("issue", 3).into());
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn a_classified_conflict_is_409() {
        let response = fail(ApiError::conflict("cannot_complete", "cannot complete task 1 from status \"working\"; expected review").into());
        assert_eq!(response.status(), StatusCode::CONFLICT);
    }

    #[test]
    fn a_classified_bad_request_is_400() {
        let response = fail(ApiError::bad_request("empty_message", "empty message text").into());
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
}
