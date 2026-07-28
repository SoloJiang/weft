//! Desktop OS notifications via the community `user-notify` crate.
//!
//! Official `@tauri-apps/plugin-notification` cannot deliver a reliable click
//! callback on desktop (plugins-workspace#2150). `user-notify` talks to the
//! platform APIs directly and returns `NotificationResponse` with `user_info`,
//! which we bridge to the frontend as `notify://open` for deep-linking.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, Runtime};
use user_notify::{
    get_notification_manager, NotificationBuilder, NotificationManager, NotificationResponse,
    NotificationResponseAction,
};

/// Stable app id — matches `tauri.conf.json` `identifier`.
const APP_ID: &str = "com.weft.app";

/// Event the frontend listens for after the user clicks a notification.
pub const OPEN_EVENT: &str = "notify://open";

static MANAGER: OnceLock<Arc<dyn NotificationManager>> = OnceLock::new();
/// Last native permission settlement we observed (`granted` / `denied`).
static PERM_SETTLED: OnceLock<Mutex<Option<String>>> = OnceLock::new();
/// Click that arrived before the frontend listener was mounted.
static PENDING_OPEN: OnceLock<Mutex<Option<NotifyOpenPayload>>> = OnceLock::new();

fn manager() -> Result<&'static Arc<dyn NotificationManager>, String> {
    MANAGER
        .get()
        .ok_or_else(|| "os_notify not initialized".to_string())
}

fn perm_settled() -> &'static Mutex<Option<String>> {
    PERM_SETTLED.get_or_init(|| Mutex::new(None))
}

fn pending_open() -> &'static Mutex<Option<NotifyOpenPayload>> {
    PENDING_OPEN.get_or_init(|| Mutex::new(None))
}

/// Payload the frontend stores in `user_info` and gets back on click.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NotifyOpenPayload {
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub direction_id: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_id: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ask_id: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open_needs: Option<bool>,
}

/// Frontend → backend send request.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NotifySendRequest {
    pub title: String,
    pub body: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub thread_id: Option<i32>,
    #[serde(default)]
    pub direction_id: Option<i32>,
    #[serde(default)]
    pub repo_id: Option<i32>,
    #[serde(default)]
    pub session_id: Option<i32>,
    #[serde(default)]
    pub ask_id: Option<i32>,
    #[serde(default)]
    pub workspace_id: Option<i32>,
    #[serde(default)]
    pub open_needs: Option<bool>,
}

fn parse_bool(raw: &str) -> Option<bool> {
    match raw {
        "1" | "true" | "True" | "TRUE" => Some(true),
        "0" | "false" | "False" | "FALSE" => Some(false),
        _ => None,
    }
}

fn payload_from_user_info(info: &HashMap<String, String>) -> NotifyOpenPayload {
    NotifyOpenPayload {
        kind: info.get("kind").cloned().unwrap_or_default(),
        thread_id: info.get("threadId").and_then(|s| s.parse().ok()),
        direction_id: info.get("directionId").and_then(|s| s.parse().ok()),
        repo_id: info.get("repoId").and_then(|s| s.parse().ok()),
        session_id: info.get("sessionId").and_then(|s| s.parse().ok()),
        ask_id: info.get("askId").and_then(|s| s.parse().ok()),
        workspace_id: info.get("workspaceId").and_then(|s| s.parse().ok()),
        open_needs: info.get("openNeeds").and_then(|s| parse_bool(s)),
    }
}

fn user_info_from_req(req: &NotifySendRequest) -> HashMap<String, String> {
    let mut info = HashMap::new();
    if !req.kind.is_empty() {
        info.insert("kind".to_string(), req.kind.clone());
    }
    if let Some(v) = req.thread_id {
        info.insert("threadId".to_string(), v.to_string());
    }
    if let Some(v) = req.direction_id {
        info.insert("directionId".to_string(), v.to_string());
    }
    if let Some(v) = req.repo_id {
        info.insert("repoId".to_string(), v.to_string());
    }
    if let Some(v) = req.session_id {
        info.insert("sessionId".to_string(), v.to_string());
    }
    if let Some(v) = req.ask_id {
        info.insert("askId".to_string(), v.to_string());
    }
    if let Some(v) = req.workspace_id {
        info.insert("workspaceId".to_string(), v.to_string());
    }
    if let Some(v) = req.open_needs {
        info.insert(
            "openNeeds".to_string(),
            if v { "1".to_string() } else { "0".to_string() },
        );
    }
    info
}

fn focus_main_window<R: Runtime>(app: &AppHandle<R>) {
    if let Some(win) = app.get_webview_window("main") {
        let _ = win.unminimize();
        let _ = win.show();
        let _ = win.set_focus();
    }
}

fn remember_settled_permission(value: &str) {
    if let Ok(mut guard) = perm_settled().lock() {
        *guard = Some(value.to_string());
    }
}

fn handle_response<R: Runtime>(app: AppHandle<R>, response: NotificationResponse) {
    // Only deep-link on an actual click (Default) or a custom action button.
    // Dismiss must stay silent.
    let open = match &response.action {
        NotificationResponseAction::Default => true,
        NotificationResponseAction::Other(_) => true,
        NotificationResponseAction::Dismiss => false,
    };
    if !open {
        return;
    }
    focus_main_window(&app);
    let payload = payload_from_user_info(&response.user_info);
    // Retain as pending first so a cold-start click survives if no frontend
    // listener is mounted yet. Live handlers must ack/clear after handling.
    if let Ok(mut guard) = pending_open().lock() {
        *guard = Some(payload.clone());
    }
    if let Err(err) = app.emit(OPEN_EVENT, payload.clone()) {
        eprintln!("[weft] os_notify emit {OPEN_EVENT}: {err}");
    }
}

/// Register the platform notification manager once during Tauri setup.
/// Safe to call only from the main thread (macOS permission APIs require it).
pub fn init<R: Runtime>(app: &AppHandle<R>) -> Result<(), String> {
    if MANAGER.get().is_some() {
        return Ok(());
    }
    let mgr = get_notification_manager(APP_ID.to_string(), None);
    let app_handle = app.clone();
    mgr.register(
        Box::new(move |response| {
            handle_response(app_handle.clone(), response);
        }),
        Vec::new(),
    )
    .map_err(|e| format!("os_notify register: {e}"))?;

    if MANAGER.set(mgr).is_err() {
        // Another setup path won the race — fine, keep the first.
        return Ok(());
    }
    Ok(())
}

/// `"granted" | "denied" | "prompt"` — mirrors the frontend `NotifyPermission`.
#[tauri::command]
pub async fn os_notify_permission() -> Result<String, String> {
    let mgr = manager()?.clone();
    // Always re-query the OS so Settings recovery / revocation is visible without
    // restarting Weft. Cached settlement only disambiguates native `false`
    // (NotDetermined vs previously denied).
    match mgr.get_notification_permission_state().await {
        Ok(true) => {
            remember_settled_permission("granted");
            Ok("granted".to_string())
        }
        Ok(false) => {
            if let Ok(guard) = perm_settled().lock() {
                if guard.as_deref() == Some("denied") {
                    return Ok("denied".to_string());
                }
            }
            Ok("prompt".to_string())
        }
        Err(e) => Err(format!("os_notify permission: {e}")),
    }
}

#[tauri::command]
pub async fn os_notify_request_permission() -> Result<String, String> {
    let mgr = manager()?.clone();
    match mgr.first_time_ask_for_notification_permission().await {
        Ok(true) => {
            remember_settled_permission("granted");
            Ok("granted".to_string())
        }
        Ok(false) => {
            remember_settled_permission("denied");
            Ok("denied".to_string())
        }
        Err(e) => Err(format!("os_notify request permission: {e}")),
    }
}

#[tauri::command]
pub async fn os_notify_send(req: NotifySendRequest) -> Result<(), String> {
    let mgr = manager()?.clone();
    let info = user_info_from_req(&req);
    let mut builder = NotificationBuilder::new()
        .title(&req.title)
        .body(&req.body);
    if !info.is_empty() {
        builder = builder.set_user_info(info);
    }
    // Group by kind so the OS can collapse related pings.
    if !req.kind.is_empty() {
        builder = builder.set_thread_id(&format!("weft.{}", req.kind));
    }
    mgr.send_notification(builder)
        .await
        .map_err(|e| format!("os_notify send: {e}"))?;
    Ok(())
}

/// Drain a click that arrived before the frontend listener was ready.
#[tauri::command]
pub fn os_notify_take_pending_open() -> Result<Option<NotifyOpenPayload>, String> {
    let mut guard = pending_open()
        .lock()
        .map_err(|e| format!("os_notify pending lock poisoned: {e}"))?;
    Ok(guard.take())
}

/// Clear a retained pending open after the frontend has handled a live event.
#[tauri::command]
pub fn os_notify_ack_open() -> Result<(), String> {
    let mut guard = pending_open()
        .lock()
        .map_err(|e| format!("os_notify pending lock poisoned: {e}"))?;
    *guard = None;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_info_roundtrip_preserves_route_fields() {
        let req = NotifySendRequest {
            title: "t".into(),
            body: "b".into(),
            kind: "needs".into(),
            thread_id: Some(7),
            direction_id: Some(12),
            repo_id: Some(3),
            session_id: Some(44),
            ask_id: Some(99),
            workspace_id: Some(3),
            open_needs: Some(true),
        };
        let info = user_info_from_req(&req);
        let payload = payload_from_user_info(&info);
        assert_eq!(payload.kind, "needs");
        assert_eq!(payload.thread_id, Some(7));
        assert_eq!(payload.direction_id, Some(12));
        assert_eq!(payload.repo_id, Some(3));
        assert_eq!(payload.session_id, Some(44));
        assert_eq!(payload.ask_id, Some(99));
        assert_eq!(payload.workspace_id, Some(3));
        assert_eq!(payload.open_needs, Some(true));
    }

    #[test]
    fn empty_request_yields_empty_info() {
        let req = NotifySendRequest {
            title: "t".into(),
            body: "b".into(),
            kind: String::new(),
            thread_id: None,
            direction_id: None,
            repo_id: None,
            session_id: None,
            ask_id: None,
            workspace_id: None,
            open_needs: None,
        };
        assert!(user_info_from_req(&req).is_empty());
    }
}
