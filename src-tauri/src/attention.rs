//! Canonical workspace-scoped projection of concrete human actions.
//!
//! This module does not copy source state into an omnibus table. Permissions
//! come from the ephemeral AskRegistry, free-text questions from the durable
//! human_request table, plan/repo actions from timeline/plan rows, and PR Retry
//! from the tracked pull_request state machine.

use serde::Serialize;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QueryOrder};
use tauri::{AppHandle, Emitter, State};

use crate::ask::Ask;
use crate::bus::BusRegistry;
use crate::store::entities::human_request;
use crate::store::entities::{lead_message, pull_request, thread};
use crate::store::{repo, Db};

type R<T> = Result<T, String>;

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct RepoActionSummary {
    pub id: String,
    pub label: String,
    pub kind: String,
}

/// One actionable item. The tagged enum is the backend's exhaustive contract;
/// TypeScript mirrors it as one discriminated union.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AttentionItem {
    Permission {
        id: String,
        revision: String,
        created_at: String,
        ask: Ask,
    },
    Question {
        id: String,
        revision: String,
        created_at: String,
        request_id: i32,
        thread_id: i32,
        thread_title: String,
        direction_id: i32,
        direction_name: String,
        text: String,
    },
    PlanApproval {
        id: String,
        revision: String,
        created_at: String,
        message_id: i32,
        thread_id: i32,
        thread_title: String,
        title: String,
    },
    ScopeApproval {
        id: String,
        revision: String,
        created_at: Option<String>,
        thread_id: i32,
        thread_title: String,
    },
    RepoAction {
        id: String,
        revision: String,
        created_at: String,
        message_id: i32,
        thread_id: i32,
        thread_title: String,
        title: String,
        actions: Vec<RepoActionSummary>,
    },
    PrTrackingRetry {
        id: String,
        revision: String,
        created_at: String,
        pr_id: i32,
        thread_id: i32,
        thread_title: String,
        direction_id: i32,
        direction_name: String,
        repo_id: i32,
        number: i32,
        title: String,
        url: String,
        error: String,
        failure_episode: String,
    },
}

impl AttentionItem {
    fn id(&self) -> &str {
        match self {
            Self::Permission { id, .. }
            | Self::Question { id, .. }
            | Self::PlanApproval { id, .. }
            | Self::ScopeApproval { id, .. }
            | Self::RepoAction { id, .. }
            | Self::PrTrackingRetry { id, .. } => id,
        }
    }

    fn created_at(&self) -> Option<&str> {
        match self {
            Self::Permission { created_at, .. }
            | Self::Question { created_at, .. }
            | Self::PlanApproval { created_at, .. }
            | Self::RepoAction { created_at, .. }
            | Self::PrTrackingRetry { created_at, .. } => Some(created_at.as_str()),
            Self::ScopeApproval { created_at, .. } => created_at.as_deref(),
        }
    }

    fn rank(&self) -> u8 {
        match self {
            Self::Permission { .. } => 0,
            Self::Question { .. } => 1,
            Self::PlanApproval { .. } => 2,
            Self::ScopeApproval { .. } => 3,
            Self::RepoAction { .. } => 4,
            Self::PrTrackingRetry { .. } => 5,
        }
    }
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct AttentionSnapshot {
    pub workspace_id: i32,
    pub count: usize,
    pub items: Vec<AttentionItem>,
}

fn parse_card(message: &lead_message::Model) -> serde_json::Value {
    serde_json::from_str(&message.content).unwrap_or_else(|_| serde_json::json!({}))
}

fn card_is_open(message: &lead_message::Model) -> bool {
    let value = parse_card(message);
    let Some(resolved) = value.get("resolved") else {
        return true;
    };
    match resolved {
        serde_json::Value::Null => true,
        serde_json::Value::Bool(value) => !value,
        serde_json::Value::String(label) => label.trim().is_empty(),
        _ => false,
    }
}

fn card_title(message: &lead_message::Model) -> String {
    parse_card(message)
        .get("title")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn repo_actions(message: &lead_message::Model) -> Vec<RepoActionSummary> {
    let value = parse_card(message);
    let Some(actions) = value.get("actions").and_then(serde_json::Value::as_array) else {
        return Vec::new();
    };
    actions
        .iter()
        .filter_map(|action| {
            let kind = action.get("kind")?.as_str()?;
            if !matches!(kind, "add" | "new" | "clone") {
                return None;
            }
            Some(RepoActionSummary {
                id: action.get("id")?.as_str()?.to_string(),
                label: action.get("label")?.as_str()?.to_string(),
                kind: kind.to_string(),
            })
        })
        .collect()
}

fn direction_context(
    directions: &[crate::store::entities::direction::Model],
    direction_id: i32,
    scope: &str,
) -> (i32, String) {
    if let Some(direction) = directions.iter().find(|direction| direction.id == direction_id) {
        return (direction.id, direction.name.clone());
    }
    if scope == "lead" || scope.is_empty() {
        return (0, String::new());
    }
    (direction_id, scope.to_string())
}

fn sort_items(items: &mut [AttentionItem]) {
    items.sort_by(|left, right| {
        left.rank()
            .cmp(&right.rank())
            .then_with(|| left.created_at().cmp(&right.created_at()))
            .then_with(|| left.id().cmp(right.id()))
    });
}

/// The plan row's `created_at` is an opaque OCC revision, not a timestamp.
/// Proposal timeline rows are the durable wall-clock source for display. Old
/// plans can predate those rows, so absence stays explicit instead of inventing
/// an age from the thread or attempting to decode the revision.
async fn latest_proposal_created_at(db: &Db, thread_id: i32) -> anyhow::Result<Option<String>> {
    let message = lead_message::Entity::find()
        .filter(lead_message::Column::ThreadId.eq(thread_id))
        .filter(lead_message::Column::SessionId.is_null())
        .filter(lead_message::Column::Kind.eq("proposal"))
        .order_by_desc(lead_message::Column::Id)
        .one(&db.0)
        .await?;
    Ok(message
        .map(|message| message.created_at)
        .filter(|created_at| !created_at.trim().is_empty()))
}

async fn collect_snapshot(
    db: &Db,
    asks: &crate::ask::AskRegistry,
    workspace_id: i32,
) -> anyhow::Result<AttentionSnapshot> {
    collect_snapshot_from_asks(db, &asks.open(), workspace_id).await
}

async fn collect_snapshot_from_asks(
    db: &Db,
    open_asks: &[Ask],
    workspace_id: i32,
) -> anyhow::Result<AttentionSnapshot> {
    let threads: Vec<thread::Model> = repo::list_threads(db, workspace_id).await?;
    let mut items = Vec::new();
    let mut thread_map = std::collections::HashMap::new();
    let mut direction_map = std::collections::HashMap::new();

    for thread in &threads {
        thread_map.insert(thread.id, thread.clone());
        let directions = repo::list_directions(db, thread.id).await?;
        direction_map.insert(thread.id, directions.clone());

        // Curator permission/questions still belong to the workspace and must
        // remain answerable. Its hidden planning timeline is not a user issue,
        // so never project plan/repo cards from it.
        if thread.kind == "curator" {
            continue;
        }

        let actionable_card = repo::latest_actionable_attention_card(db, thread.id).await?;
        let proposed = repo::get_plan(db, thread.id)
            .await?
            .filter(|plan| plan.status == "proposed" && !plan.proposal.trim().is_empty());

        if let Some(plan) = proposed {
            let created_at = latest_proposal_created_at(db, thread.id).await?;
            items.push(AttentionItem::ScopeApproval {
                id: format!("scope:{}:{}", thread.id, plan.created_at),
                revision: plan.created_at.clone(),
                created_at,
                thread_id: thread.id,
                thread_title: thread.title.clone(),
            });
        } else if let Some(message) = actionable_card
            .as_ref()
            .filter(|message| message.kind == "plan_card" && card_is_open(message))
        {
            items.push(AttentionItem::PlanApproval {
                id: format!("plan:{}", message.id),
                revision: message.id.to_string(),
                created_at: message.created_at.clone(),
                message_id: message.id,
                thread_id: thread.id,
                thread_title: thread.title.clone(),
                title: card_title(message),
            });
        }

        if let Some(message) = actionable_card
            .as_ref()
            .filter(|message| message.kind == "action_card" && card_is_open(message))
        {
            let actions = repo_actions(message);
            if !actions.is_empty() {
                items.push(AttentionItem::RepoAction {
                    id: format!("repo_action:{}", message.id),
                    revision: message.id.to_string(),
                    created_at: message.created_at.clone(),
                    message_id: message.id,
                    thread_id: thread.id,
                    thread_title: thread.title.clone(),
                    title: card_title(message),
                    actions,
                });
            }
        }
    }

    for mut ask in open_asks.iter().cloned() {
        let Some(thread) = thread_map.get(&ask.thread) else {
            continue;
        };
        ask.thread_title = thread.title.clone();
        ask.workspace_id = Some(workspace_id);
        let direction_id = ask.dir.parse::<i32>().unwrap_or_default();
        let directions = direction_map.get(&ask.thread).map(Vec::as_slice).unwrap_or_default();
        let (_, direction_name) = direction_context(directions, direction_id, &ask.dir);
        ask.dir_name = direction_name;
        items.push(AttentionItem::Permission {
            id: format!(
                "permission:{workspace_id}:{}:{}:{}",
                ask.thread, ask.dir, ask.id
            ),
            revision: ask.id.to_string(),
            created_at: ask.ts.to_string(),
            ask,
        });
    }

    for request in repo::list_open_human_requests(db, workspace_id).await? {
        let Some(thread) = thread_map.get(&request.thread_id) else {
            continue;
        };
        let directions = direction_map
            .get(&request.thread_id)
            .map(Vec::as_slice)
            .unwrap_or_default();
        let (direction_id, direction_name) = direction_context(
            directions,
            request.direction_id,
            &request.direction_scope,
        );
        items.push(AttentionItem::Question {
            id: format!("question:{}", request.id),
            revision: request.revision.to_string(),
            created_at: request.created_at,
            request_id: request.id,
            thread_id: request.thread_id,
            thread_title: thread.title.clone(),
            direction_id,
            direction_name,
            text: request.question,
        });
    }

    for pr in repo::list_pull_requests_for_workspace(db, workspace_id).await? {
        if pr.lifecycle != "open"
            || pr.probe_fail_count < crate::host::monitor::MAX_CONSECUTIVE_PROBE_FAILURES
        {
            continue;
        }
        let Some(thread) = thread_map.get(&pr.thread_id) else {
            continue;
        };
        let directions = direction_map
            .get(&pr.thread_id)
            .map(Vec::as_slice)
            .unwrap_or_default();
        let (_, direction_name) = direction_context(directions, pr.direction_id, "");
        let failure_episode = pr.last_checked_at.clone();
        items.push(pr_retry_item(pr, thread, direction_name, failure_episode));
    }

    sort_items(&mut items);
    Ok(AttentionSnapshot {
        workspace_id,
        count: items.len(),
        items,
    })
}

async fn collect_snapshots(
    db: &Db,
    asks: &crate::ask::AskRegistry,
) -> anyhow::Result<Vec<AttentionSnapshot>> {
    let hidden_workspace = repo::get_setting(db, repo::K_CONCIERGE_WORKSPACE)
        .await?
        .and_then(|value| value.parse::<i32>().ok());
    let open_asks = asks.open();
    let mut workspaces = repo::list_workspaces(db).await?;
    workspaces.sort_by_key(|workspace| workspace.id);
    let mut snapshots = Vec::with_capacity(workspaces.len());
    for workspace in workspaces {
        if Some(workspace.id) == hidden_workspace {
            continue;
        }
        snapshots.push(collect_snapshot_from_asks(db, &open_asks, workspace.id).await?);
    }
    Ok(snapshots)
}

fn pr_retry_item(
    pr: pull_request::Model,
    thread: &thread::Model,
    direction_name: String,
    failure_episode: String,
) -> AttentionItem {
    AttentionItem::PrTrackingRetry {
        id: format!("pr_retry:{}:{failure_episode}", pr.id),
        revision: format!("{}:{failure_episode}", pr.probe_fail_count),
        created_at: failure_episode.clone(),
        pr_id: pr.id,
        thread_id: pr.thread_id,
        thread_title: thread.title.clone(),
        direction_id: pr.direction_id,
        direction_name,
        repo_id: pr.repo_id,
        number: pr.number,
        title: pr.title,
        url: pr.url,
        error: pr.last_error,
        failure_episode,
    }
}

#[tauri::command]
pub async fn attention_items(
    db: State<'_, Db>,
    asks: State<'_, crate::ask::AskRegistry>,
    workspace_id: i32,
) -> R<AttentionSnapshot> {
    collect_snapshot(&db, &asks, workspace_id)
        .await
        .map_err(|error| error.to_string())
}

/// One canonical global read for the polling frontend. Returning all visible
/// workspace snapshots together prevents an N+1 IPC loop, avoids rebuilding
/// the active workspace twice, and gives OS notifications stable identities
/// and routes for background-workspace actions.
#[tauri::command]
pub async fn attention_snapshots(
    db: State<'_, Db>,
    asks: State<'_, crate::ask::AskRegistry>,
) -> R<Vec<AttentionSnapshot>> {
    collect_snapshots(&db, &asks)
        .await
        .map_err(|error| error.to_string())
}

/// Linearize a durable answer with thread/repo/workspace deletion. The first
/// read only discovers the gate key; every authority and OCC check is repeated
/// while holding that thread's lifecycle gate before SQLite is mutated.
pub(crate) async fn answer_durable_human_request(
    db: &Db,
    bus: &BusRegistry,
    request_id: i32,
    expected_thread_id: Option<i32>,
    expected_workspace_id: Option<i32>,
    expected_revision: Option<i32>,
    text: &str,
) -> anyhow::Result<Option<human_request::Model>> {
    let Some(discovered) = repo::get_human_request(db, request_id).await? else {
        return Ok(None);
    };
    if expected_thread_id.is_some_and(|thread_id| discovered.thread_id != thread_id) {
        return Ok(None);
    }

    let gate = bus.thread_lifecycle_gate(discovered.thread_id);
    let _lifecycle = gate.lock().await;
    let Some(request) = repo::get_human_request(db, request_id).await? else {
        return Ok(None);
    };
    if request.status != repo::HUMAN_REQUEST_OPEN
        || request.thread_id != discovered.thread_id
        || expected_thread_id.is_some_and(|thread_id| request.thread_id != thread_id)
        || expected_workspace_id.is_some_and(|workspace_id| request.workspace_id != workspace_id)
        || expected_revision.is_some_and(|revision| request.revision != revision)
    {
        return Ok(None);
    }

    let Some(updated) = repo::answer_human_request(
        db,
        request.workspace_id,
        request.id,
        request.revision,
        text,
    )
    .await?
    else {
        return Ok(None);
    };
    let Ok(ask_id) = u64::try_from(updated.id) else {
        anyhow::bail!("invalid durable question id");
    };
    if !bus.answer_ask(updated.thread_id, ask_id, &updated.answer) {
        bus.deliver_durable_answer(
            updated.thread_id,
            ask_id,
            &updated.direction_scope,
            &updated.question,
            &updated.answer,
        );
    }
    Ok(Some(updated))
}

#[tauri::command]
pub async fn answer_human_request(
    app: AppHandle,
    db: State<'_, Db>,
    bus: State<'_, crate::bus::BusRegistry>,
    workspace_id: i32,
    request_id: i32,
    revision: i32,
    text: String,
) -> R<()> {
    let Some(request) = answer_durable_human_request(
        &db,
        &bus,
        request_id,
        None,
        Some(workspace_id),
        Some(revision),
        &text,
    )
    .await
    .map_err(|error| error.to_string())?
    else {
        return Err("stale_attention_item".to_string());
    };
    let _ = app.emit("needs-you://changed", request.thread_id);
    Ok(())
}

#[tauri::command]
pub async fn retry_pr_tracking(
    app: AppHandle,
    db: State<'_, Db>,
    merge_backoff: State<'_, crate::host::automerge::MergeBackoffState>,
    workspace_id: i32,
    pr_id: i32,
    failure_episode: String,
) -> R<()> {
    let thread_id = retry_pr_tracking_core(
        &db,
        &merge_backoff,
        workspace_id,
        pr_id,
        &failure_episode,
    )
    .await
    .map_err(|error| error.to_string())?;
    let _ = app.emit("needs-you://changed", thread_id);
    Ok(())
}

async fn retry_pr_tracking_core(
    db: &Db,
    merge_backoff: &crate::host::automerge::MergeBackoffState,
    workspace_id: i32,
    pr_id: i32,
    failure_episode: &str,
) -> anyhow::Result<i32> {
    let Some(pr) = repo::get_pull_request(db, pr_id).await?
    else {
        anyhow::bail!("stale_attention_item");
    };
    let Some(thread) = repo::get_thread(db, pr.thread_id).await?
    else {
        anyhow::bail!("stale_attention_item");
    };
    if thread.workspace_id != workspace_id
        || pr.lifecycle != "open"
        || pr.probe_fail_count < crate::host::monitor::MAX_CONSECUTIVE_PROBE_FAILURES
        || pr.last_checked_at != failure_episode
    {
        anyhow::bail!("stale_attention_item");
    }
    repo::refresh_pull_request_tracking(db, &pr).await?;
    merge_backoff.clear(pr.id);
    Ok(pr.thread_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sea_orm::{ActiveModelTrait, Set};

    async fn fixture() -> (Db, thread::Model, crate::store::entities::direction::Model) {
        let db = Db::connect("sqlite::memory:").await.unwrap();
        let workspace = repo::create_workspace(&db, "ws").await.unwrap();
        let repo_ref = repo::add_repo_ref(&db, workspace.id, "repo", "/tmp/repo", "main", "", true)
            .await
            .unwrap();
        let thread = repo::create_thread(&db, workspace.id, "Issue", "feature", "codex")
            .await
            .unwrap();
        let direction = repo::create_direction(
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
        (db, thread, direction)
    }

    #[tokio::test]
    async fn concurrent_questions_are_durable_scoped_and_occ_answered() {
        let (db, thread, direction) = fixture().await;
        let first = repo::create_human_request(
            &db,
            thread.workspace_id,
            thread.id,
            &direction.id.to_string(),
            direction.id,
            7,
            0,
            0,
            "Which API?",
        )
        .await
        .unwrap();
        let request = repo::create_human_request(
            &db,
            thread.workspace_id,
            thread.id,
            &direction.id.to_string(),
            direction.id,
            8,
            0,
            0,
            "REST or GraphQL?",
        )
        .await
        .unwrap();
        assert_eq!(
            repo::get_human_request(&db, first.id)
                .await
                .unwrap()
                .unwrap()
                .status,
            repo::HUMAN_REQUEST_OPEN
        );
        // A fresh in-memory registry models a process restart: the DB row is
        // still projected with the same stable id. Independent questions from
        // one direction remain separately answerable.
        let snapshot = collect_snapshot(&db, &crate::ask::AskRegistry::new(), thread.workspace_id)
            .await
            .unwrap();
        assert_eq!(snapshot.count, 2);
        let request_ids: std::collections::HashSet<i32> = snapshot
            .items
            .iter()
            .filter_map(|item| match item {
                AttentionItem::Question { request_id, .. } => Some(*request_id),
                _ => None,
            })
            .collect();
        assert_eq!(request_ids, std::collections::HashSet::from([first.id, request.id]));

        assert!(repo::answer_human_request(&db, thread.workspace_id, request.id, 99, "REST")
            .await
            .unwrap()
            .is_none());
        assert!(repo::answer_human_request(&db, thread.workspace_id, request.id, 1, "REST")
            .await
            .unwrap()
            .is_some());
        assert!(repo::answer_human_request(&db, thread.workspace_id, request.id, 1, "duplicate")
            .await
            .unwrap()
            .is_none());
        let open = repo::list_open_human_requests(&db, thread.workspace_id)
            .await
            .unwrap();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].id, first.id);
        assert!(repo::answer_human_request(&db, thread.workspace_id, first.id, 1, "GraphQL")
            .await
            .unwrap()
            .is_some());
        assert!(repo::list_open_human_requests(&db, thread.workspace_id)
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn cancelling_a_thread_withdraws_its_durable_questions() {
        let (db, thread, direction) = fixture().await;
        let request = repo::create_human_request(
            &db,
            thread.workspace_id,
            thread.id,
            &direction.id.to_string(),
            direction.id,
            1,
            0,
            0,
            "Continue?",
        )
        .await
        .unwrap();
        assert_eq!(
            repo::cancel_open_human_requests_for_thread(&db, thread.id)
                .await
                .unwrap()
                .len(),
            1
        );
        assert_eq!(repo::get_human_request(&db, request.id).await.unwrap().unwrap().status, "cancelled");
        let snapshot = collect_snapshot(&db, &crate::ask::AskRegistry::new(), thread.workspace_id)
            .await
            .unwrap();
        assert!(snapshot.items.is_empty());
    }

    #[tokio::test]
    async fn proposal_absorbs_plan_card_into_one_scope_item() {
        let (db, thread, _direction) = fixture().await;
        let plan_card = repo::insert_lead_message(
            &db,
            thread.id,
            None,
            1,
            "assistant",
            "plan_card",
            r#"{"title":"Plan","requirements":[]}"#,
            "complete",
        )
        .await
        .unwrap();
        let before = collect_snapshot(&db, &crate::ask::AskRegistry::new(), thread.workspace_id)
            .await
            .unwrap();
        assert!(matches!(before.items.as_slice(), [AttentionItem::PlanApproval { .. }]));

        let proposal_row = repo::insert_lead_message(
            &db,
            thread.id,
            None,
            1,
            "system",
            "proposal",
            r#"{"rationale":"x","count":1}"#,
            "complete",
        )
        .await
        .unwrap();
        let revision = "1700000000000000000-7";

        repo::upsert_plan(
            &db,
            thread.id,
            r#"{"rationale":"x","directions":[{"name":"Task"}]}"#,
            "proposed",
            revision,
        )
        .await
        .unwrap();
        let after = collect_snapshot(&db, &crate::ask::AskRegistry::new(), thread.workspace_id)
            .await
            .unwrap();
        let [AttentionItem::ScopeApproval {
            id,
            revision: projected_revision,
            created_at,
            ..
        }] = after.items.as_slice()
        else {
            panic!("the proposed plan should project as one scope approval");
        };
        assert_eq!(id, &format!("scope:{}:{revision}", thread.id));
        assert_eq!(projected_revision, revision);
        assert_eq!(created_at.as_deref(), Some(proposal_row.created_at.as_str()));
        let payload = serde_json::to_value(&after.items[0]).unwrap();
        assert_eq!(payload["revision"], revision);
        assert_eq!(payload["created_at"], proposal_row.created_at);
        assert!(!repo::attention_card_is_actionable(
            &db,
            thread.id,
            plan_card.id,
            "plan_card",
            false,
        )
        .await
        .unwrap());
        assert!(repo::attention_card_is_actionable(
            &db,
            thread.id,
            plan_card.id,
            "plan_card",
            true,
        )
        .await
        .unwrap());
    }

    #[tokio::test]
    async fn scope_without_a_proposal_timeline_keeps_revision_and_omits_display_time() {
        let (db, thread, _direction) = fixture().await;
        let legacy_revision = "1700000000";
        repo::upsert_plan(
            &db,
            thread.id,
            r#"{"rationale":"legacy","directions":[{"name":"Task"}]}"#,
            "proposed",
            legacy_revision,
        )
        .await
        .unwrap();

        let snapshot =
            collect_snapshot(&db, &crate::ask::AskRegistry::new(), thread.workspace_id)
                .await
                .unwrap();
        let [AttentionItem::ScopeApproval {
            revision,
            created_at,
            ..
        }] = snapshot.items.as_slice()
        else {
            panic!("the legacy proposed plan should remain actionable");
        };
        assert_eq!(revision, legacy_revision);
        assert_eq!(created_at, &None);
    }

    #[tokio::test]
    async fn historical_cards_never_resurface_as_actionable() {
        let (db, thread, _direction) = fixture().await;
        repo::insert_lead_message(
            &db,
            thread.id,
            None,
            1,
            "assistant",
            "action_card",
            r#"{"title":"Old","actions":[{"id":"a","label":"Add","kind":"add"}]}"#,
            "complete",
        )
        .await
        .unwrap();
        repo::insert_lead_message(
            &db,
            thread.id,
            None,
            2,
            "assistant",
            "text",
            r#"{"text":"newer"}"#,
            "complete",
        )
        .await
        .unwrap();
        let stale = collect_snapshot(&db, &crate::ask::AskRegistry::new(), thread.workspace_id)
            .await
            .unwrap();
        assert!(stale.items.is_empty());

        let latest = repo::insert_lead_message(
            &db,
            thread.id,
            None,
            3,
            "assistant",
            "action_card",
            r#"{"title":"Latest","actions":[{"id":"b","label":"Clone","kind":"clone"}]}"#,
            "complete",
        )
        .await
        .unwrap();
        repo::resolve_action_card(&db, latest.id, "done").await.unwrap();
        let settled = collect_snapshot(&db, &crate::ask::AskRegistry::new(), thread.workspace_id)
            .await
            .unwrap();
        assert!(settled.items.is_empty(), "an older unresolved card must not resurface");

        let plan = repo::insert_lead_message(
            &db,
            thread.id,
            None,
            4,
            "assistant",
            "plan_card",
            r#"{"title":"Plan"}"#,
            "complete",
        )
        .await
        .unwrap();
        assert!(repo::attention_card_is_actionable(
            &db,
            thread.id,
            plan.id,
            "plan_card",
            false,
        )
            .await
            .unwrap());
        repo::insert_lead_message(
            &db,
            thread.id,
            None,
            5,
            "user",
            "text",
            r#"{"text":"revise"}"#,
            "complete",
        )
        .await
        .unwrap();
        let revision = collect_snapshot(&db, &crate::ask::AskRegistry::new(), thread.workspace_id)
            .await
            .unwrap();
        assert!(revision.items.is_empty());
        assert!(!repo::attention_card_is_actionable(
            &db,
            thread.id,
            plan.id,
            "plan_card",
            false,
        )
            .await
            .unwrap());

        let settled_plan = repo::insert_lead_message(
            &db,
            thread.id,
            None,
            6,
            "assistant",
            "plan_card",
            r#"{"title":"Settled plan"}"#,
            "complete",
        )
        .await
        .unwrap();
        repo::resolve_action_card(&db, settled_plan.id, "Settled plan")
            .await
            .unwrap();
        assert!(!repo::attention_card_is_actionable(
            &db,
            thread.id,
            settled_plan.id,
            "plan_card",
            false,
        )
        .await
        .unwrap());
    }

    #[tokio::test]
    async fn curator_requests_remain_actionable_but_curator_cards_do_not() {
        let (db, thread, _direction) = fixture().await;
        let curator = repo::create_thread(
            &db,
            thread.workspace_id,
            "Curator",
            "curator",
            "codex",
        )
        .await
        .unwrap();
        repo::create_human_request(
            &db,
            thread.workspace_id,
            curator.id,
            "lead",
            0,
            1,
            0,
            0,
            "Which repository?",
        )
        .await
        .unwrap();
        repo::insert_lead_message(
            &db,
            curator.id,
            None,
            1,
            "assistant",
            "action_card",
            r#"{"title":"Hidden","actions":[{"id":"a","label":"Add","kind":"add"}]}"#,
            "complete",
        )
        .await
        .unwrap();
        let asks = crate::ask::AskRegistry::new();
        let (_ask_id, _rx) = asks.request(
            curator.id,
            "lead",
            "codex",
            "Inspect repository",
            "git status",
            crate::ask::RiskLevel::ReadOnly,
            "git status",
        );

        let snapshot = collect_snapshot(&db, &asks, thread.workspace_id)
            .await
            .unwrap();
        assert_eq!(snapshot.count, 2);
        assert!(snapshot.items.iter().any(|item| matches!(item, AttentionItem::Permission { .. })));
        assert!(snapshot.items.iter().any(|item| matches!(item, AttentionItem::Question { .. })));
        assert!(!snapshot.items.iter().any(|item| matches!(item, AttentionItem::RepoAction { .. })));
    }

    #[tokio::test]
    async fn global_snapshots_include_inactive_workspace_actions_once() {
        let (db, thread, direction) = fixture().await;
        let other_workspace = repo::create_workspace(&db, "background").await.unwrap();
        let other_thread = repo::create_thread(
            &db,
            other_workspace.id,
            "Background issue",
            "feature",
            "codex",
        )
        .await
        .unwrap();
        repo::create_human_request(
            &db,
            other_workspace.id,
            other_thread.id,
            "lead",
            0,
            1,
            0,
            0,
            "Ship now?",
        )
        .await
        .unwrap();
        repo::create_human_request(
            &db,
            thread.workspace_id,
            thread.id,
            &direction.id.to_string(),
            direction.id,
            1,
            0,
            0,
            "Use REST?",
        )
        .await
        .unwrap();

        let snapshots = collect_snapshots(&db, &crate::ask::AskRegistry::new())
            .await
            .unwrap();
        assert_eq!(snapshots.len(), 2);
        assert_eq!(snapshots.iter().map(|snapshot| snapshot.count).sum::<usize>(), 2);
        let background = snapshots
            .iter()
            .find(|snapshot| snapshot.workspace_id == other_workspace.id)
            .unwrap();
        assert!(matches!(background.items.as_slice(), [AttentionItem::Question { thread_id, .. }] if *thread_id == other_thread.id));
    }

    #[tokio::test]
    async fn repo_action_and_pr_retry_are_projected_without_notices() {
        let (db, thread, direction) = fixture().await;
        let action = repo::insert_lead_message(
            &db,
            thread.id,
            None,
            1,
            "assistant",
            "action_card",
            r#"{"title":"Add repo","actions":[{"id":"a","label":"Add","kind":"add"}]}"#,
            "complete",
        )
        .await
        .unwrap();
        let session = repo::create_session(
            &db,
            direction.id,
            direction.repo_id,
            "codex",
            "/tmp/attention-pr-source",
        )
        .await
        .unwrap();
        let pr = repo::register_pull_request(
            &db,
            thread.id,
            direction.id,
            direction.repo_id,
            Some(session.id),
            "github",
            "github.com",
            "acme",
            "repo",
            1,
            "https://github.com/acme/repo/pull/1",
            "PR",
        )
        .await
        .unwrap();
        let mut active: pull_request::ActiveModel = pr.into();
        active.probe_fail_count = Set(crate::host::monitor::MAX_CONSECUTIVE_PROBE_FAILURES);
        active.last_checked_at = Set("episode-1".to_string());
        active.last_error = Set("auth".to_string());
        active.update(&db.0).await.unwrap();

        let snapshot = collect_snapshot(&db, &crate::ask::AskRegistry::new(), thread.workspace_id)
            .await
            .unwrap();
        assert_eq!(snapshot.count, 2);
        assert!(snapshot.items.iter().any(|item| matches!(item, AttentionItem::RepoAction { .. })));
        assert!(snapshot.items.iter().any(|item| matches!(item, AttentionItem::PrTrackingRetry { .. })));

        repo::resolve_action_card(&db, action.id, "done").await.unwrap();
        let settled = collect_snapshot(&db, &crate::ask::AskRegistry::new(), thread.workspace_id)
            .await
            .unwrap();
        assert_eq!(settled.count, 1);
        assert!(matches!(settled.items[0], AttentionItem::PrTrackingRetry { .. }));
    }

    #[tokio::test]
    async fn pr_retry_is_occ_scoped_to_one_failure_episode() {
        let (db, thread, direction) = fixture().await;
        let session = repo::create_session(
            &db,
            direction.id,
            direction.repo_id,
            "codex",
            "/tmp/attention-retry-pr-source",
        )
        .await
        .unwrap();
        let pr = repo::register_pull_request(
            &db,
            thread.id,
            direction.id,
            direction.repo_id,
            Some(session.id),
            "github",
            "github.com",
            "acme",
            "repo",
            1,
            "https://github.com/acme/repo/pull/1",
            "PR",
        )
        .await
        .unwrap();
        let mut active: pull_request::ActiveModel = pr.clone().into();
        active.probe_fail_count = Set(crate::host::monitor::MAX_CONSECUTIVE_PROBE_FAILURES);
        active.last_checked_at = Set("episode-1".to_string());
        active.update(&db.0).await.unwrap();
        let backoff = crate::host::automerge::MergeBackoffState::default();

        assert!(retry_pr_tracking_core(&db, &backoff, thread.workspace_id, pr.id, "stale")
            .await
            .is_err());
        retry_pr_tracking_core(&db, &backoff, thread.workspace_id, pr.id, "episode-1")
            .await
            .unwrap();
        let reloaded = repo::get_pull_request(&db, pr.id).await.unwrap().unwrap();
        assert_eq!(reloaded.probe_fail_count, 0);
        assert!(retry_pr_tracking_core(&db, &backoff, thread.workspace_id, pr.id, "episode-1")
            .await
            .is_err());
    }

    #[tokio::test]
    async fn workspace_scope_does_not_leak_questions() {
        let (db, thread, direction) = fixture().await;
        let other = repo::create_workspace(&db, "other").await.unwrap();
        repo::create_human_request(
            &db,
            thread.workspace_id,
            thread.id,
            &direction.id.to_string(),
            direction.id,
            0,
            0,
            0,
            "secret scope",
        )
        .await
        .unwrap();

        let snapshot = collect_snapshot(&db, &crate::ask::AskRegistry::new(), other.id)
            .await
            .unwrap();
        assert_eq!(snapshot.count, 0);
    }

    #[test]
    fn all_action_kinds_have_one_deterministic_queue_order() {
        let permission_ask = crate::ask::Ask {
            id: 4,
            thread: 1,
            dir: "2".to_string(),
            tool: "codex".to_string(),
            summary: "Run tests".to_string(),
            detail: "cargo test".to_string(),
            detail_redacted: None,
            preview: None,
            risk: crate::ask::RiskLevel::Write,
            ts: 10,
            thread_title: "Issue".to_string(),
            dir_name: "Task".to_string(),
            workspace_id: Some(1),
            action_key: "cargo test".to_string(),
        };
        let mut items = vec![
            AttentionItem::PrTrackingRetry {
                id: "pr_retry:6:e".to_string(),
                revision: "3:e".to_string(),
                created_at: "6".to_string(),
                pr_id: 6,
                thread_id: 1,
                thread_title: "Issue".to_string(),
                direction_id: 2,
                direction_name: "Task".to_string(),
                repo_id: 1,
                number: 6,
                title: "PR".to_string(),
                url: "https://example.test/pr/6".to_string(),
                error: "offline".to_string(),
                failure_episode: "e".to_string(),
            },
            AttentionItem::RepoAction {
                id: "repo_action:5".to_string(),
                revision: "5".to_string(),
                created_at: "5".to_string(),
                message_id: 5,
                thread_id: 1,
                thread_title: "Issue".to_string(),
                title: "Add repo".to_string(),
                actions: vec![],
            },
            AttentionItem::ScopeApproval {
                id: "scope:1:4".to_string(),
                revision: "4".to_string(),
                created_at: Some("4".to_string()),
                thread_id: 1,
                thread_title: "Issue".to_string(),
            },
            AttentionItem::PlanApproval {
                id: "plan:3".to_string(),
                revision: "3".to_string(),
                created_at: "3".to_string(),
                message_id: 3,
                thread_id: 1,
                thread_title: "Issue".to_string(),
                title: "Plan".to_string(),
            },
            AttentionItem::Question {
                id: "question:2".to_string(),
                revision: "1".to_string(),
                created_at: "2".to_string(),
                request_id: 2,
                thread_id: 1,
                thread_title: "Issue".to_string(),
                direction_id: 2,
                direction_name: "Task".to_string(),
                text: "REST or GraphQL?".to_string(),
            },
            AttentionItem::Permission {
                id: "permission:1:1:2:4".to_string(),
                revision: "4".to_string(),
                created_at: "1".to_string(),
                ask: permission_ask,
            },
        ];

        sort_items(&mut items);
        let ranks: Vec<u8> = items.iter().map(AttentionItem::rank).collect();
        assert_eq!(ranks, vec![0, 1, 2, 3, 4, 5]);
        let ids: Vec<&str> = items.iter().map(AttentionItem::id).collect();
        assert_eq!(
            ids,
            vec![
                "permission:1:1:2:4",
                "question:2",
                "plan:3",
                "scope:1:4",
                "repo_action:5",
                "pr_retry:6:e",
            ]
        );
    }
}
