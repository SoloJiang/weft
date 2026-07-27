//! Cross-engine usage-limit awareness (issue #97).
//!
//! Distinct from [`crate::process_quota`]: that module tracks Weft's OWN process
//! headroom (RLIMIT_NPROC) — this one tracks each coding-agent CLI's ACCOUNT-side
//! API quota (claude's usage limit, codex's rate limit). Different axis entirely;
//! don't conflate the two "quota" words.
//!
//! Both engines this issue covers already push STRUCTURED, PROACTIVE quota
//! signals on the wire — Weft previously just dropped them on the floor:
//! - claude (`-p --output-format stream-json`) emits a `rate_limit_event` line
//!   whenever its rate-limit status changes: `{"type":"rate_limit_event",
//!   "rate_limit_info":{"status":"allowed"|"allowed_warning"|"rejected",
//!   "resetsAt":<unix secs>,"rateLimitType":"five_hour"|"seven_day"|...,
//!   "utilization":<0..1 fraction>}, "uuid", "session_id"}`. Verified against the
//!   installed claude-code 2.1.201 binary's own bundled schema/strings — Weft's
//!   parser had a test asserting this exact shape was silently ignored
//!   (`parses_result_and_garbage`, proto.rs).
//! - codex app-server (v2 protocol, the transport `codex_app_server.rs` already
//!   speaks) exposes account-scoped rate limits via a REQUEST/NOTIFICATION pair
//!   (verified against openai/codex's `codex-rs/app-server-protocol` source):
//!   `account/rateLimits/read` (no params) → `{rateLimits: RateLimitSnapshot}`,
//!   and an unsolicited `account/rateLimits/updated` notification with the same
//!   shape whenever the backend has fresh info (typically right after a turn).
//!   `RateLimitSnapshot.primary`/`.secondary` are rolling windows
//!   (`usedPercent` 0-100 int, `resetsAt` unix seconds, `windowDurationMins`);
//!   `rateLimitReachedType` is set (to any variant) only once the account has
//!   actually hit a limit — that field, not a raw percentage, is codex's
//!   authoritative "exceeded" signal.
//!
//! This module is transport-agnostic: it's just a small tool-keyed hub the two
//! transports (`codex_app_server.rs`, the claude branch of
//! `adapters::AgentAdapter::quota_signal` consumed from `engine.rs::spawn_reader`)
//! report into, plus the pure `status_for` threshold shared by both mappings so
//! "what counts as a warning" lives in exactly one place.

use serde::Serialize;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// Usage climbing past this percent (of either engine's rolling window) counts
/// as "approaching the limit" even before the engine itself calls it out
/// (codex's `usedPercent` has no built-in warning tier — only claude's
/// `allowed_warning` status does). Not exposed as a knob: matches the
/// process-quota governor's own `WARNING_PERCENT` (`process_quota.rs`) so the
/// two unrelated "quota" concepts at least agree on what "close" means.
pub const WARN_THRESHOLD_PERCENT: u32 = 80;

/// One engine's usage-limit standing, coarsened to the three states any UI or
/// fail-over decision actually needs. `Ord` (derived via the field order below)
/// is NOT implemented — severity comparisons go through [`more_severe`] so the
/// "which is worse" rule stays in one named place instead of leaning on enum
/// declaration order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum QuotaStatus {
    Ok,
    Warning,
    Exceeded,
}

/// `a` if it is at least as severe as `b`, else `b` — the single place that
/// knows Exceeded > Warning > Ok, so combining two windows' statuses (codex's
/// primary/secondary) never has to re-derive the ordering ad hoc.
pub fn more_severe(a: QuotaStatus, b: QuotaStatus) -> QuotaStatus {
    fn rank(s: QuotaStatus) -> u8 {
        match s {
            QuotaStatus::Ok => 0,
            QuotaStatus::Warning => 1,
            QuotaStatus::Exceeded => 2,
        }
    }
    if rank(a) >= rank(b) {
        a
    } else {
        b
    }
}

/// Derive a status from a used-percent reading plus an explicit "the engine
/// itself says this is actually exhausted" flag (codex's `rateLimitReachedType`
/// being set, claude's `status == "rejected"`). A percentage remains advisory,
/// including 100%, until the provider explicitly rejects the engine.
pub fn status_for(used_percent: Option<u32>, reached: bool) -> QuotaStatus {
    if reached {
        return QuotaStatus::Exceeded;
    }
    match used_percent {
        Some(p) if p >= WARN_THRESHOLD_PERCENT => QuotaStatus::Warning,
        _ => QuotaStatus::Ok,
    }
}

/// One tool's most recently observed quota signal. Account-scoped, not
/// thread-scoped — the same claude/codex login is shared by every thread and
/// worker using that tool, so the hub keys on `tool` alone (last write wins).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QuotaSnapshot {
    /// "claude" | "codex" — matches `thread.lead_tool` / `session.tool`.
    pub tool: String,
    pub status: QuotaStatus,
    /// 0-100, when the source carried a usable reading.
    pub used_percent: Option<u32>,
    /// Unix seconds the window resets, when the source reported one.
    pub resets_at: Option<i64>,
    /// Raw window/type label from the source (codex: "primary"/"secondary";
    /// claude: `rateLimitType` e.g. "five_hour"/"seven_day") — display-only,
    /// not matched on.
    pub window_label: Option<String>,
    /// Unix seconds Weft captured this reading (NOT the source's own clock).
    pub observed_at: i64,
}

fn hub() -> &'static Mutex<HashMap<String, QuotaSnapshot>> {
    static HUB: OnceLock<Mutex<HashMap<String, QuotaSnapshot>>> = OnceLock::new();
    HUB.get_or_init(|| Mutex::new(HashMap::new()))
}

/// A provider can omit a reset time on a sparse rate-limit event. That signal is
/// still useful briefly, but must not keep new work blocked forever when no
/// later account update arrives.
const UNKNOWN_RESET_SNAPSHOT_TTL_SECS: i64 = 15 * 60;

fn snapshot_is_expired(snapshot: &QuotaSnapshot, now: i64) -> bool {
    if snapshot.resets_at.is_some_and(|resets_at| resets_at <= now) {
        return true;
    }
    if snapshot.resets_at.is_some() {
        return false;
    }
    now.saturating_sub(snapshot.observed_at) > UNKNOWN_RESET_SNAPSHOT_TTL_SECS
}

fn prune_expired_snapshots(snapshots: &mut HashMap<String, QuotaSnapshot>, now: i64) {
    snapshots.retain(|_, snapshot| !snapshot_is_expired(snapshot, now));
}

/// Record (replace) the latest snapshot for `snapshot.tool`. Called from the
/// codex app-server transport and from claude's per-line quota-signal check —
/// never from a hot chat-render path, so a plain std Mutex is fine.
pub fn report(snapshot: QuotaSnapshot) {
    hub()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(snapshot.tool.clone(), snapshot);
}

/// Record a quota snapshot only when its reporting process is the command that
/// new automatic routes would launch for this tool. Existing sessions can stay
/// pinned to an older command after an alias change; their account state must
/// not repopulate the global routing snapshot for the newly configured binary.
pub fn report_for_command(snapshot: QuotaSnapshot, command: &str) {
    if crate::tool_command::command_for(&snapshot.tool) != command {
        return;
    }
    report(snapshot);
}

/// Drop the account snapshot for one tool after its effective command changes.
/// A quota reading is account/command scoped in practice, while the in-memory
/// hub is intentionally keyed by the stable tool identity for normal routing.
/// Clearing only the affected identity prevents an old command's exhaustion
/// state from blocking the newly configured command.
pub fn clear(tool: &str) {
    hub()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(tool);
}

/// The current snapshot for one tool, if any has been observed this run.
pub fn current(tool: &str) -> Option<QuotaSnapshot> {
    let mut snapshots = hub().lock().unwrap_or_else(|e| e.into_inner());
    prune_expired_snapshots(&mut snapshots, now_unix());
    snapshots.get(tool).cloned()
}

/// Every observed snapshot, sorted by tool name — the Resources dashboard's
/// read side.
pub fn all() -> Vec<QuotaSnapshot> {
    let mut snapshots = hub().lock().unwrap_or_else(|e| e.into_inner());
    prune_expired_snapshots(&mut snapshots, now_unix());
    let mut v: Vec<QuotaSnapshot> = snapshots.values().cloned().collect();
    v.sort_by(|a, b| a.tool.cmp(&b.tool));
    v
}

/// Current unix time in seconds. Small indirection so tests can't accidentally
/// depend on wall-clock skew across a slow CI box — callers needing a fixed
/// clock construct `QuotaSnapshot` by hand instead of going through the
/// `*_snapshot` parsers.
pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
/// Test-only reset so cases don't leak tool snapshots into each other — the
/// hub is a process-global singleton (`cargo test` runs cases in one process).
pub fn clear_for_test() {
    hub().lock().unwrap_or_else(|e| e.into_inner()).clear();
}

#[cfg(test)]
/// Serialize every test that mutates the process-global quota hub. The
/// `tool_command::override_test_lock` is an alias to this same synchronous
/// mutex because changing overrides also clears the hub. It is intentionally a
/// sync mutex, including for async tests that serialize their setup sequence.
pub(crate) fn hub_test_lock() -> &'static Mutex<()> {
    static TEST_HUB_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    TEST_HUB_LOCK.get_or_init(|| Mutex::new(()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_for_thresholds() {
        assert_eq!(status_for(None, false), QuotaStatus::Ok);
        assert_eq!(status_for(Some(0), false), QuotaStatus::Ok);
        assert_eq!(status_for(Some(79), false), QuotaStatus::Ok);
        assert_eq!(status_for(Some(80), false), QuotaStatus::Warning);
        assert_eq!(status_for(Some(99), false), QuotaStatus::Warning);
        assert_eq!(status_for(Some(100), false), QuotaStatus::Warning);
        // The explicit "reached" flag wins even over a low/absent percent —
        // codex can report `rateLimitReachedType` without a fresh usedPercent
        // in a sparse rolling update.
        assert_eq!(status_for(Some(10), true), QuotaStatus::Exceeded);
        assert_eq!(status_for(None, true), QuotaStatus::Exceeded);
    }

    #[test]
    fn more_severe_orders_exceeded_over_warning_over_ok() {
        assert_eq!(more_severe(QuotaStatus::Ok, QuotaStatus::Warning), QuotaStatus::Warning);
        assert_eq!(more_severe(QuotaStatus::Warning, QuotaStatus::Ok), QuotaStatus::Warning);
        assert_eq!(
            more_severe(QuotaStatus::Warning, QuotaStatus::Exceeded),
            QuotaStatus::Exceeded
        );
        assert_eq!(more_severe(QuotaStatus::Ok, QuotaStatus::Ok), QuotaStatus::Ok);
    }

    #[test]
    fn hub_report_current_all_round_trip() {
        let _test_hub_lock = hub_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        clear_for_test();
        let now = now_unix();
        assert!(current("claude").is_none());
        report(QuotaSnapshot {
            tool: "claude".into(),
            status: QuotaStatus::Warning,
            used_percent: Some(85),
            resets_at: Some(now + 3_600),
            window_label: Some("five_hour".into()),
            observed_at: now,
        });
        report(QuotaSnapshot {
            tool: "codex".into(),
            status: QuotaStatus::Exceeded,
            used_percent: Some(100),
            resets_at: None,
            window_label: Some("primary".into()),
            observed_at: now,
        });
        assert_eq!(current("claude").unwrap().status, QuotaStatus::Warning);
        assert_eq!(current("codex").unwrap().status, QuotaStatus::Exceeded);
        assert!(current("opencode").is_none());
        let all_snapshots = all();
        assert_eq!(all_snapshots.len(), 2);
        // Sorted by tool name.
        assert_eq!(all_snapshots[0].tool, "claude");
        assert_eq!(all_snapshots[1].tool, "codex");
        // Last-write-wins replace, not accumulate.
        report(QuotaSnapshot {
            tool: "claude".into(),
            status: QuotaStatus::Ok,
            used_percent: Some(5),
            resets_at: None,
            window_label: None,
            observed_at: now,
        });
        assert_eq!(current("claude").unwrap().status, QuotaStatus::Ok);
        assert_eq!(all().len(), 2);
    }

    #[test]
    fn expired_or_stale_snapshots_stop_affecting_routing() {
        let _test_hub_lock = hub_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        clear_for_test();
        let now = now_unix();
        report(QuotaSnapshot {
            tool: "claude".into(),
            status: QuotaStatus::Exceeded,
            used_percent: Some(100),
            resets_at: Some(now - 1),
            window_label: Some("five_hour".into()),
            observed_at: now - 60,
        });
        assert!(current("claude").is_none());

        report(QuotaSnapshot {
            tool: "codex".into(),
            status: QuotaStatus::Exceeded,
            used_percent: Some(100),
            resets_at: None,
            window_label: None,
            observed_at: now - UNKNOWN_RESET_SNAPSHOT_TTL_SECS - 1,
        });
        assert!(all().is_empty());
    }

    #[test]
    fn clear_invalidates_only_the_changed_tool_snapshot() {
        let _test_hub_lock = hub_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        clear_for_test();
        let now = now_unix();
        report(QuotaSnapshot {
            tool: "claude".into(),
            status: QuotaStatus::Exceeded,
            used_percent: Some(100),
            resets_at: Some(now + 3_600),
            window_label: Some("five_hour".into()),
            observed_at: now,
        });
        report(QuotaSnapshot {
            tool: "codex".into(),
            status: QuotaStatus::Warning,
            used_percent: Some(85),
            resets_at: Some(now + 3_600),
            window_label: Some("primary".into()),
            observed_at: now,
        });

        clear("claude");

        assert!(current("claude").is_none());
        assert_eq!(
            current("codex").map(|snapshot| snapshot.status),
            Some(QuotaStatus::Warning)
        );
        clear_for_test();
    }

    #[test]
    fn pinned_old_command_cannot_repopulate_the_current_routing_snapshot() {
        let _test_hub_lock = hub_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        clear_for_test();
        crate::tool_command::set_overrides(std::collections::HashMap::from([(
            "claude".to_string(),
            "old-claude".to_string(),
        )]));
        let snapshot = QuotaSnapshot {
            tool: "claude".into(),
            status: QuotaStatus::Exceeded,
            used_percent: Some(100),
            resets_at: Some(now_unix() + 3_600),
            window_label: Some("five_hour".into()),
            observed_at: now_unix(),
        };
        report_for_command(snapshot.clone(), "old-claude");
        assert_eq!(current("claude").map(|reading| reading.status), Some(QuotaStatus::Exceeded));

        crate::tool_command::set_overrides(std::collections::HashMap::from([(
            "claude".to_string(),
            "new-claude".to_string(),
        )]));
        assert!(current("claude").is_none(), "the override change clears the old account state");

        // A legacy session pinned to old-claude can keep reporting, but its
        // exhausted account must not block new work routed to new-claude.
        report_for_command(snapshot.clone(), "old-claude");
        assert!(current("claude").is_none());

        report_for_command(snapshot, "new-claude");
        assert_eq!(current("claude").map(|reading| reading.status), Some(QuotaStatus::Exceeded));

        crate::tool_command::set_overrides(std::collections::HashMap::new());
        clear_for_test();
    }

    #[test]
    fn now_unix_is_a_plausible_recent_timestamp() {
        // Sanity bound, not a real clock test: catches a broken/zeroed clock
        // without hardcoding a moving "current" value.
        assert!(now_unix() > 1_700_000_000);
    }
}
