//! Background scheduler.
//!
//! - `spawn` loops forever, sleeping `next_delay` between iterations.
//! - `run_on_exit` is a single shot the app calls from its close-requested
//!   handler, capped at 10s so we never block the user closing the window.
//!
//! Failures don't kill the loop; they get recorded into `backup_config`
//! and the next tick still fires.

use anyhow::Result;
use std::time::Duration;
use tokio::time::{Instant, sleep_until};

use crate::backup::{BackupService, config};

/// Spawn the long-lived scheduler task. The handle is intentionally discarded
/// — the task lives for the lifetime of the tokio runtime, which dies with
/// the Tauri app process, so there's nothing to join.
pub fn spawn(svc: BackupService) {
    tokio::spawn(async move {
        loop {
            let next_in = match next_delay(&svc).await {
                Ok(d) => d,
                Err(e) => {
                    eprintln!("[weft][backup] scheduler tick read config failed: {e:#}");
                    Duration::from_secs(60)
                }
            };
            sleep_until(Instant::now() + next_in).await;
            let _ = svc.run_now().await;
        }
    });
}

/// Pick when to fire next.
/// - backup disabled / no auto / no url → 60s and re-check (so toggling auto
///   on in the UI takes effect within a minute).
/// - no prior backup → fire immediately (10ms — give the runtime room).
/// - last + interval already in the past → fire immediately.
/// - otherwise → sleep until `last + interval`.
async fn next_delay(svc: &BackupService) -> Result<Duration> {
    let cfg = config::load(svc.db()).await?;
    if !cfg.enabled || !cfg.auto_backup_enabled || cfg.remote_url.is_empty() {
        return Ok(Duration::from_secs(60));
    }
    let interval = Duration::from_secs(cfg.interval_seconds.max(1) as u64);
    let last = cfg
        .last_backup_at
        .as_ref()
        .and_then(|s| s.parse::<u64>().ok());
    let Some(last) = last else {
        return Ok(Duration::from_millis(10));
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let target = last.saturating_add(interval.as_secs());
    if now >= target {
        Ok(Duration::from_millis(10))
    } else {
        Ok(Duration::from_secs(target - now))
    }
}

/// One-shot fire from the app's close handler. Bounded at 10s so a slow
/// remote can't keep the user from quitting. Timeout still records a failure
/// so the UI shows something useful on next start.
pub async fn run_on_exit(svc: &BackupService) {
    let cfg = match config::load(svc.db()).await {
        Ok(c) => c,
        Err(_) => return,
    };
    if !cfg.enabled || !cfg.backup_on_exit || cfg.remote_url.is_empty() {
        return;
    }
    let fut = svc.run_now();
    match tokio::time::timeout(Duration::from_secs(10), fut).await {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => eprintln!("[weft][backup] on-exit backup failed: {e:#}"),
        Err(_) => {
            let _ = config::record_failure(svc.db(), "timeout on exit").await;
        }
    }
}
