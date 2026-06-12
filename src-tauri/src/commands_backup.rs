//! Tauri commands for the Backup settings panel. Thin wrappers around
//! `backup::config` / `BackupService` / `recovery_key`.

use serde::Serialize;
use std::path::PathBuf;
use tauri::State;

use crate::backup::{BackupService, config, recovery_key};

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupStatusDto {
    pub enabled: bool,
    pub remote_url: String,
    pub auto_backup_enabled: bool,
    pub backup_on_exit: bool,
    pub interval_seconds: i64,
    pub last_backup_at: Option<String>,
    pub last_backup_commit_sha: Option<String>,
    pub last_backup_bytes: Option<i64>,
    pub last_error: Option<String>,
}

#[tauri::command]
pub async fn backup_get_status(svc: State<'_, BackupService>) -> Result<BackupStatusDto, String> {
    let cfg = config::load(svc.db()).await.map_err(|e| e.to_string())?;
    Ok(BackupStatusDto {
        enabled: cfg.enabled,
        remote_url: cfg.remote_url,
        auto_backup_enabled: cfg.auto_backup_enabled,
        backup_on_exit: cfg.backup_on_exit,
        interval_seconds: cfg.interval_seconds,
        last_backup_at: cfg.last_backup_at,
        last_backup_commit_sha: cfg.last_backup_commit_sha,
        last_backup_bytes: cfg.last_backup_bytes,
        last_error: cfg.last_error,
    })
}

#[tauri::command]
pub async fn backup_save_prefs(
    svc: State<'_, BackupService>,
    enabled: bool,
    remote_url: String,
    auto_backup_enabled: bool,
    backup_on_exit: bool,
) -> Result<(), String> {
    config::save_prefs(
        svc.db(),
        config::UpdatePrefs {
            enabled,
            remote_url,
            auto_backup_enabled,
            backup_on_exit,
        },
    )
    .await
    .map(|_| ())
    .map_err(|e| e.to_string())
}

/// `git ls-remote` against the URL. Runs on a blocking thread because it
/// shells out and can stall on network/SSH negotiation.
#[tauri::command]
pub async fn backup_test_remote(remote_url: String) -> Result<(), String> {
    tokio::task::spawn_blocking(move || crate::backup::git_remote::ls_remote(&remote_url))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn backup_run_now(svc: State<'_, BackupService>) -> Result<BackupStatusDto, String> {
    svc.run_now().await.map_err(|e| e.to_string())?;
    backup_get_status(svc).await
}

#[tauri::command]
pub async fn backup_export_recovery_key(target_path: String) -> Result<(), String> {
    let p = PathBuf::from(target_path);
    tokio::task::spawn_blocking(move || recovery_key::export_to(&p))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn backup_restore(
    svc: State<'_, BackupService>,
    remote_url: String,
    recovery_key_path: String,
) -> Result<(), String> {
    let path = PathBuf::from(recovery_key_path);
    svc.restore_from(&remote_url, &path)
        .await
        .map_err(|e| e.to_string())
}
