//! One global engine-routing policy shared by every Weft-owned agent surface.
//!
//! The resolver is deliberately pure at its decision boundary.  The small async
//! wrapper below only reads the global settings, installed commands, and the
//! structured quota hub before handing plain values to [`resolve`].  No agent
//! payload can select a tool: the only planner hint accepted by the wrapper is
//! `normal` or `deep`, and the tool pool is closed here to Codex and Claude.

use crate::engine_quota::{QuotaSnapshot, QuotaStatus};
use crate::store::{repo, Db};
use serde::{Deserialize, Serialize};
use serde_json::json;
#[cfg(unix)]
use std::path::Path;

pub const K_AUTOMATIC_ROUTING_ENABLED: &str = "automatic_engine_routing";
pub const K_QUOTA_FAILOVER_ENABLED: &str = "quota_failover_on_exceeded";

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum EngineId {
    Claude,
    Codex,
    Opencode,
}

impl EngineId {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Opencode => "opencode",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "claude" => Some(Self::Claude),
            "codex" => Some(Self::Codex),
            "opencode" => Some(Self::Opencode),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RoutingHint {
    #[default]
    Normal,
    Deep,
}

impl RoutingHint {
    pub fn parse(raw: Option<&str>) -> Self {
        match raw.map(str::trim).map(str::to_ascii_lowercase).as_deref() {
            Some("deep") => Self::Deep,
            _ => Self::Normal,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Deep => "deep",
        }
    }

    fn preferred(self) -> EngineId {
        match self {
            Self::Normal => EngineId::Codex,
            Self::Deep => EngineId::Claude,
        }
    }

    fn alternate(self) -> EngineId {
        match self {
            Self::Normal => EngineId::Claude,
            Self::Deep => EngineId::Codex,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RoutingSource {
    Manual,
    Automatic,
    Legacy,
    Blocked,
}

impl RoutingSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Automatic => "automatic",
            Self::Legacy => "legacy",
            Self::Blocked => "blocked",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RouteReason {
    AutomaticDisabled,
    ManualPin,
    NormalPreference,
    DeepPreference,
    PreferredWarning,
    PreferredUnavailable,
    QuotaUnknown,
    LegacyFallback,
    NoAutomaticCandidate,
    AutomaticCandidateUnavailable,
    BothAutomaticCandidatesExceeded,
    InvalidManualTool,
    ManualToolUnavailable,
}

impl RouteReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AutomaticDisabled => "automatic_disabled",
            Self::ManualPin => "manual_pin",
            Self::NormalPreference => "normal_preference",
            Self::DeepPreference => "deep_preference",
            Self::PreferredWarning => "preferred_warning",
            Self::PreferredUnavailable => "preferred_unavailable",
            Self::QuotaUnknown => "quota_unknown",
            Self::LegacyFallback => "legacy_fallback",
            Self::NoAutomaticCandidate => "no_automatic_candidate",
            Self::AutomaticCandidateUnavailable => "automatic_candidate_unavailable",
            Self::BothAutomaticCandidatesExceeded => "both_automatic_candidates_exceeded",
            Self::InvalidManualTool => "invalid_manual_tool",
            Self::ManualToolUnavailable => "manual_tool_unavailable",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RouteCandidate {
    pub tool: EngineId,
    pub installed: bool,
    pub quota: Option<QuotaStatus>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RouteRequest {
    pub automatic_enabled: bool,
    pub manual_tool: Option<EngineId>,
    pub legacy_tool: EngineId,
    pub hint: RoutingHint,
    pub candidates: Vec<RouteCandidate>,
}

/// The one decision returned to callers.  A blocked result has no tool by
/// construction, so callers cannot accidentally launch a known-exhausted or
/// otherwise ineligible engine.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RouteDecision {
    pub tool: Option<EngineId>,
    pub source: RoutingSource,
    pub reason: RouteReason,
    pub hint: RoutingHint,
    pub quota: Option<QuotaStatus>,
    pub blocked: bool,
}

impl RouteDecision {
    pub fn selected(&self) -> Option<EngineId> {
        if self.blocked {
            None
        } else {
            self.tool
        }
    }

    pub fn reason_code(&self) -> &'static str {
        self.reason.as_str()
    }
}

fn candidate<'a>(candidates: &'a [RouteCandidate], tool: EngineId) -> Option<&'a RouteCandidate> {
    candidates.iter().find(|candidate| candidate.tool == tool)
}

fn quota_rank(status: Option<QuotaStatus>) -> u8 {
    match status {
        Some(QuotaStatus::Ok) => 0,
        None => 1,
        Some(QuotaStatus::Warning) => 2,
        Some(QuotaStatus::Exceeded) => 3,
    }
}

fn selected(
    tool: EngineId,
    source: RoutingSource,
    reason: RouteReason,
    hint: RoutingHint,
    quota: Option<QuotaStatus>,
) -> RouteDecision {
    RouteDecision {
        tool: Some(tool),
        source,
        reason,
        hint,
        quota,
        blocked: false,
    }
}

fn blocked(reason: RouteReason, hint: RoutingHint) -> RouteDecision {
    RouteDecision {
        tool: None,
        source: RoutingSource::Blocked,
        reason,
        hint,
        quota: None,
        blocked: true,
    }
}

/// Resolve a new participant or an unstarted dispatch.
pub fn resolve(request: &RouteRequest) -> RouteDecision {
    if let Some(manual) = request.manual_tool {
        let Some(candidate) = candidate(&request.candidates, manual) else {
            return blocked(RouteReason::ManualToolUnavailable, request.hint);
        };
        // An explicit user pin is never rewritten by an automatic policy. If
        // its command is currently missing, let the normal spawn error explain
        // that fact rather than silently choosing another engine.
        return selected(
            manual,
            RoutingSource::Manual,
            RouteReason::ManualPin,
            request.hint,
            candidate.quota,
        );
    }

    let legacy = candidate(&request.candidates, request.legacy_tool);
    if !request.automatic_enabled {
        let quota = legacy.and_then(|candidate| candidate.quota);
        return selected(
            request.legacy_tool,
            RoutingSource::Legacy,
            RouteReason::AutomaticDisabled,
            request.hint,
            quota,
        );
    }

    let preferred = request.hint.preferred();
    let alternate = request.hint.alternate();
    let preferred_candidate = candidate(&request.candidates, preferred);
    let alternate_candidate = candidate(&request.candidates, alternate);
    let both_exceeded = [preferred_candidate, alternate_candidate]
        .into_iter()
        .all(|candidate| {
            candidate.is_some_and(|candidate| {
                candidate.installed && candidate.quota == Some(QuotaStatus::Exceeded)
            })
        });

    let mut eligible: Vec<&RouteCandidate> = [preferred_candidate, alternate_candidate]
        .into_iter()
        .flatten()
        .filter(|candidate| candidate.installed && candidate.quota != Some(QuotaStatus::Exceeded))
        .collect();
    eligible.sort_by_key(|candidate| {
        let preference = if candidate.tool == preferred { 0 } else { 1 };
        (quota_rank(candidate.quota), preference)
    });

    if let Some(choice) = eligible.first().copied() {
        let reason = if choice.tool == preferred {
            match choice.quota {
                Some(QuotaStatus::Warning) => RouteReason::PreferredWarning,
                None => RouteReason::QuotaUnknown,
                Some(QuotaStatus::Ok) => match request.hint {
                    RoutingHint::Normal => RouteReason::NormalPreference,
                    RoutingHint::Deep => RouteReason::DeepPreference,
                },
                Some(QuotaStatus::Exceeded) => RouteReason::PreferredUnavailable,
            }
        } else {
            match preferred_candidate {
                Some(candidate) if candidate.quota == Some(QuotaStatus::Warning) => {
                    RouteReason::PreferredWarning
                }
                _ => RouteReason::PreferredUnavailable,
            }
        };
        return selected(
            choice.tool,
            RoutingSource::Automatic,
            reason,
            request.hint,
            choice.quota,
        );
    }

    if both_exceeded {
        return blocked(RouteReason::BothAutomaticCandidatesExceeded, request.hint);
    }

    // OpenCode remains a manual/legacy engine, never an automatic fallback.
    // A configured Codex/Claude legacy identity can still preserve the old
    // default behavior when the automatic pool has no usable reading.
    if request.legacy_tool != EngineId::Opencode {
        if let Some(legacy) = legacy {
            if legacy.installed && legacy.quota != Some(QuotaStatus::Exceeded) {
                return selected(
                    request.legacy_tool,
                    RoutingSource::Legacy,
                    RouteReason::LegacyFallback,
                    request.hint,
                    legacy.quota,
                );
            }
        }
    }

    if preferred_candidate.is_some_and(|candidate| candidate.quota == Some(QuotaStatus::Exceeded))
        || alternate_candidate
            .is_some_and(|candidate| candidate.quota == Some(QuotaStatus::Exceeded))
    {
        return blocked(RouteReason::AutomaticCandidateUnavailable, request.hint);
    }
    blocked(RouteReason::NoAutomaticCandidate, request.hint)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FailoverSkipReason {
    Disabled,
    ManualPin,
    NotStructuredExceeded,
    NoFallback,
    CooldownActive,
    PolicyUnavailable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FailoverBlockedReason {
    FallbackUnavailable,
    BothAutomaticCandidatesExceeded,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FailoverDecision {
    SwitchTo {
        tool: EngineId,
        quota: Option<QuotaStatus>,
    },
    Skip(FailoverSkipReason),
    Blocked(FailoverBlockedReason),
}

/// Resolve the only automatic mid-task transition.  The caller invokes this
/// exclusively at the existing failed-turn boundary; this function itself never
/// knows about or interrupts a healthy engine.
pub fn resolve_failover(
    current: EngineId,
    enabled: bool,
    manual_pin: bool,
    structured_exceeded: bool,
    fallback: Option<&RouteCandidate>,
    cooldown_ok: bool,
) -> FailoverDecision {
    if !enabled {
        return FailoverDecision::Skip(FailoverSkipReason::Disabled);
    }
    if manual_pin {
        return FailoverDecision::Skip(FailoverSkipReason::ManualPin);
    }
    if !structured_exceeded {
        return FailoverDecision::Skip(FailoverSkipReason::NotStructuredExceeded);
    }
    // A structured turn-end quota code is authoritative for THIS failed turn.
    // Account/rate-limit snapshots can arrive later (or never), so requiring a
    // current hub reading here would suppress the only supported L3 handoff.
    let expected = match current {
        EngineId::Claude => EngineId::Codex,
        EngineId::Codex => EngineId::Claude,
        EngineId::Opencode => return FailoverDecision::Skip(FailoverSkipReason::NoFallback),
    };
    let Some(fallback) = fallback else {
        return FailoverDecision::Skip(FailoverSkipReason::NoFallback);
    };
    if fallback.tool != expected || !fallback.installed {
        return FailoverDecision::Blocked(FailoverBlockedReason::FallbackUnavailable);
    }
    if fallback.quota == Some(QuotaStatus::Exceeded) {
        return FailoverDecision::Blocked(FailoverBlockedReason::BothAutomaticCandidatesExceeded);
    }
    if !cooldown_ok {
        return FailoverDecision::Skip(FailoverSkipReason::CooldownActive);
    }
    FailoverDecision::SwitchTo {
        tool: expected,
        quota: fallback.quota,
    }
}

fn is_enabled(raw: Option<&str>) -> bool {
    matches!(raw.map(str::trim), Some("1" | "true" | "on" | "yes"))
}

fn snapshots_by_tool() -> Vec<QuotaSnapshot> {
    crate::engine_quota::all()
}

#[cfg(unix)]
fn is_executable_regular_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;

    std::fs::metadata(path)
        .map(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(unix)]
fn command_is_ready_on_path(command: &str, search_path: &str) -> bool {
    let path = Path::new(command);
    if path.is_absolute() {
        return is_executable_regular_file(path);
    }
    std::env::split_paths(search_path)
        .map(|directory| directory.join(command))
        .any(|path| is_executable_regular_file(&path))
}

/// Match the eventual bare `Command::new(command)` spawn closely enough for
/// automatic routing. Windows keeps the existing PATHEXT-aware detector; Unix
/// additionally rejects regular files that the process cannot execute.
fn command_is_ready(command: &str) -> bool {
    if !crate::detect::resolves_on_path(command) {
        return false;
    }

    #[cfg(windows)]
    {
        true
    }

    #[cfg(unix)]
    {
        command_is_ready_on_path(command, &crate::detect::tool_path())
    }
}

fn candidate_for(tool: EngineId, snapshots: &[QuotaSnapshot]) -> RouteCandidate {
    let command = crate::tool_command::command_for(tool.as_str());
    // A route must only select a command that the eventual bare
    // `Command::new(command)` spawn can reach. This excludes the Codex app
    // bundle fallback on Unix, rejects non-executable files, and uses
    // PATHEXT-aware lookup for Windows shims.
    let installed = command_is_ready(&command);
    let quota = snapshots
        .iter()
        .find(|snapshot| snapshot.tool == tool.as_str())
        .map(|snapshot| snapshot.status);
    RouteCandidate {
        tool,
        installed,
        quota,
    }
}

fn candidate_list() -> Vec<RouteCandidate> {
    let snapshots = snapshots_by_tool();
    [EngineId::Codex, EngineId::Claude, EngineId::Opencode]
        .into_iter()
        .map(|tool| candidate_for(tool, &snapshots))
        .collect()
}

/// Read the one global setting and resolve a request against the live local
/// installation/quota state. Invalid manual identities are blocked rather than
/// silently transformed into a different engine.
pub async fn try_resolve_for_db(
    db: &Db,
    manual_tool: Option<&str>,
    legacy_tool: &str,
    hint: RoutingHint,
) -> anyhow::Result<RouteDecision> {
    let legacy = EngineId::parse(legacy_tool).unwrap_or(EngineId::Codex);
    if let Some(manual_tool) = manual_tool {
        let Some(manual) = EngineId::parse(manual_tool) else {
            return Ok(blocked(RouteReason::InvalidManualTool, hint));
        };
        return Ok(resolve(&RouteRequest {
            automatic_enabled: false,
            manual_tool: Some(manual),
            legacy_tool: legacy,
            hint,
            candidates: candidate_list(),
        }));
    }

    let automatic_enabled = is_enabled(
        repo::get_setting(db, K_AUTOMATIC_ROUTING_ENABLED)
            .await?
            .as_deref(),
    );
    Ok(resolve(&RouteRequest {
        automatic_enabled,
        manual_tool: None,
        legacy_tool: legacy,
        hint,
        candidates: candidate_list(),
    }))
}

/// Compatibility boundary for existing callers that consume a decision
/// directly. Policy read failures become an explicit blocked route, never a
/// false/disabled policy that can launch the persisted legacy tool. Callers
/// that need the underlying database error should use [`try_resolve_for_db`].
pub async fn resolve_for_db(
    db: &Db,
    manual_tool: Option<&str>,
    legacy_tool: &str,
    hint: RoutingHint,
) -> RouteDecision {
    match try_resolve_for_db(db, manual_tool, legacy_tool, hint).await {
        Ok(decision) => decision,
        Err(err) => {
            eprintln!("[weft][routing] policy read failed; route blocked: {err}");
            blocked(RouteReason::AutomaticCandidateUnavailable, hint)
        }
    }
}

pub async fn try_quota_failover_for_db(
    db: &Db,
    current: &str,
    manual_pin: bool,
    structured_exceeded: bool,
    cooldown_ok: bool,
) -> anyhow::Result<FailoverDecision> {
    let enabled = is_enabled(
        repo::get_setting(db, K_QUOTA_FAILOVER_ENABLED)
            .await?
            .as_deref(),
    );
    let Some(current) = EngineId::parse(current) else {
        return Ok(FailoverDecision::Skip(FailoverSkipReason::NoFallback));
    };
    let candidates = candidate_list();
    let fallback_tool = match current {
        EngineId::Claude => Some(EngineId::Codex),
        EngineId::Codex => Some(EngineId::Claude),
        EngineId::Opencode => None,
    };
    let fallback = fallback_tool.and_then(|tool| candidate(&candidates, tool));
    Ok(resolve_failover(
        current,
        enabled,
        manual_pin,
        structured_exceeded,
        fallback,
        cooldown_ok,
    ))
}

/// Compatibility boundary for the quota callback. An unreadable opt-in
/// setting safely skips handoff and leaves a distinct reason for callers to
/// log/audit; it never treats the policy as disabled and proceeds implicitly.
pub async fn quota_failover_for_db(
    db: &Db,
    current: &str,
    manual_pin: bool,
    structured_exceeded: bool,
    cooldown_ok: bool,
) -> FailoverDecision {
    match try_quota_failover_for_db(
        db,
        current,
        manual_pin,
        structured_exceeded,
        cooldown_ok,
    )
    .await
    {
        Ok(decision) => decision,
        Err(err) => {
            eprintln!("[weft][quota] policy read failed; failover skipped: {err}");
            FailoverDecision::Skip(FailoverSkipReason::PolicyUnavailable)
        }
    }
}

fn quota_code(quota: Option<QuotaStatus>) -> Option<&'static str> {
    match quota {
        Some(QuotaStatus::Ok) => Some("ok"),
        Some(QuotaStatus::Warning) => Some("warning"),
        Some(QuotaStatus::Exceeded) => Some("exceeded"),
        None => None,
    }
}

/// Persist an automatic/manual route decision as a normal timeline marker.  It
/// is intentionally best-effort: a marker failure must not turn a successful
/// engine start into a failed start, while the selected engine itself remains
/// the durable thread/direction/session value.
pub async fn record_decision(
    db: &Db,
    thread_id: i32,
    session_id: Option<i32>,
    direction_id: Option<i32>,
    operation: &str,
    decision: &RouteDecision,
) {
    if decision.source == RoutingSource::Legacy && decision.reason == RouteReason::AutomaticDisabled
    {
        return;
    }
    let kind = if decision.blocked {
        "engine_route_blocked"
    } else {
        "engine_route"
    };
    let content = json!({
        "tool": decision.tool.map(EngineId::as_str),
        "source": decision.source.as_str(),
        "reason": decision.reason.as_str(),
        "hint": decision.hint.as_str(),
        "quota_status": quota_code(decision.quota),
        "direction_id": direction_id,
        "operation": operation,
    })
    .to_string();
    let turn_id = match repo::next_turn_id(db, thread_id).await {
        Ok(turn) => turn,
        Err(err) => {
            eprintln!("[weft] engine-route marker turn lookup failed: {err}");
            return;
        }
    };
    match repo::insert_lead_message(
        db, thread_id, session_id, turn_id, "system", kind, &content, "complete",
    )
    .await
    {
        Ok(message) => {
            if let Some(app) = crate::APP_HANDLE.get() {
                use tauri::Emitter;
                let _ = app.emit(
                    crate::lead_chat::engine::EVENT,
                    crate::lead_chat::engine::Push::Message { thread_id, message },
                );
            }
        }
        Err(err) => eprintln!("[weft] engine-route marker insert failed: {err}"),
    }
}

pub async fn record_failover_blocked(
    db: &Db,
    thread_id: i32,
    session_id: Option<i32>,
    current: &str,
    fallback: Option<&str>,
    reason: &str,
) {
    let content = json!({
        "tool": current,
        "fallback": fallback,
        "source": "quota_failover",
        "reason": reason,
        "quota_status": "exceeded",
    })
    .to_string();
    let turn_id = match repo::next_turn_id(db, thread_id).await {
        Ok(turn) => turn,
        Err(err) => {
            eprintln!("[weft] quota-route marker turn lookup failed: {err}");
            return;
        }
    };
    match repo::insert_lead_message(
        db,
        thread_id,
        session_id,
        turn_id,
        "system",
        "engine_route_blocked",
        &content,
        "complete",
    )
    .await
    {
        Ok(message) => {
            if let Some(app) = crate::APP_HANDLE.get() {
                use tauri::Emitter;
                let _ = app.emit(
                    crate::lead_chat::engine::EVENT,
                    crate::lead_chat::engine::Push::Message { thread_id, message },
                );
            }
        }
        Err(err) => eprintln!("[weft] quota-route marker insert failed: {err}"),
    }
}

fn default_initial_hint(thread: &crate::store::entities::thread::Model) -> RoutingHint {
    if thread.kind == "curator" {
        return RoutingHint::Deep;
    }
    RoutingHint::Normal
}

/// `(was_blocked, hint)` for an initial lead decision marker. Direction-scoped
/// planner markers are deliberately excluded: they describe workers, not the
/// issue lead that this recovery path starts.
fn initial_lead_route_marker(
    message: &crate::store::entities::lead_message::Model,
) -> Option<(bool, RoutingHint)> {
    if message.session_id.is_some()
        || !matches!(message.kind.as_str(), "engine_route" | "engine_route_blocked")
    {
        return None;
    }
    let value = serde_json::from_str::<serde_json::Value>(&message.content).ok()?;
    let source = value.get("source").and_then(|source| source.as_str());
    let operation = value
        .get("operation")
        .and_then(|operation| operation.as_str());
    let direction_id = value.get("direction_id");
    if source == Some("quota_failover")
        || matches!(direction_id, Some(direction_id) if !direction_id.is_null())
        || !matches!(
            operation,
            Some("new_thread") | Some("new_issue") | Some("curator_start") | Some("concierge_start")
        )
    {
        return None;
    }
    let hint = value
        .get("hint")
        .and_then(|hint| hint.as_str())
        .map(|hint| RoutingHint::parse(Some(hint)))
        .unwrap_or_default();
    Some((message.kind == "engine_route_blocked", hint))
}

fn is_later_lead_route_or_switch(message: &crate::store::entities::lead_message::Model) -> bool {
    if message.session_id.is_some() {
        return false;
    }
    if message.kind == "engine_switch" {
        return true;
    }
    if message.kind != "engine_route" {
        return false;
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&message.content) else {
        return false;
    };
    if matches!(value.get("direction_id"), Some(direction_id) if !direction_id.is_null()) {
        return false;
    }
    matches!(
        value.get("operation").and_then(|operation| operation.as_str()),
        Some("lead_start" | "new_thread" | "new_issue" | "curator_start" | "concierge_start")
    )
}

/// Reconcile the initial route of an unstarted, unpinned lead. A blocked marker
/// carries its original hint; if that best-effort marker was never written, the
/// durable unpinned/unstarted thread state is enough to force a fresh decision
/// instead of launching the fallback tool that automatic policy rejected.
/// Existing healthy/running leads and migrated legacy/manual threads never move.
pub async fn prepare_initial_lead(
    db: &Db,
    thread: &crate::store::entities::thread::Model,
) -> anyhow::Result<crate::store::entities::thread::Model> {
    // A manual switch persists its pin before lead_engine re-enters this path.
    // Never reinterpret an older blocked automatic marker as permission to
    // overwrite that explicit recovery choice.
    if thread.engine_pinned {
        return Ok(thread.clone());
    }
    // A status row means this participant has already been started at least
    // once. Initial routing is not a license to rebalance it on a later
    // resume, even if the quota/installation snapshot has changed.
    if repo::lead_status(db, thread.id)
        .await
        .ok()
        .flatten()
        .is_some()
    {
        return Ok(thread.clone());
    }
    let messages = repo::list_lead_messages(db, thread.id)
        .await
        .unwrap_or_default();
    let initial = messages
        .iter()
        .enumerate()
        .rev()
        .find_map(|(index, message)| initial_lead_route_marker(message).map(|route| (index, route)));
    let hint = match initial {
        Some((_index, (false, _))) => return Ok(thread.clone()),
        Some((blocked_index, (true, hint))) => {
            // Once a blocked initial route has been resolved, a later LEAD
            // route/switch marker is the durable witness that this participant
            // made its one pre-start choice. Worker route markers must not
            // suppress a separately blocked lead recovery.
            if messages
                .iter()
                .skip(blocked_index + 1)
                .any(is_later_lead_route_or_switch)
            {
                return Ok(thread.clone());
            }
            hint
        }
        // `record_decision` is intentionally best-effort audit output. A crash
        // or write failure after thread creation must not turn a blocked route
        // into permission to run its persisted fallback tool.
        None => default_initial_hint(thread),
    };
    let decision = try_resolve_for_db(db, None, &thread.lead_tool, hint).await?;
    let Some(tool) = decision.selected() else {
        record_decision(db, thread.id, None, None, "lead_start", &decision).await;
        anyhow::bail!("engine_route_blocked:{}", decision.reason_code());
    };
    if !repo::refresh_unpinned_thread_route(db, thread.id, tool.as_str()).await? {
        return repo::get_thread(db, thread.id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("thread {} disappeared while refreshing its route", thread.id));
    }
    record_decision(db, thread.id, None, None, "lead_start", &decision).await;
    repo::get_thread(db, thread.id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("thread {} disappeared while recording its route", thread.id))
}

/// Copy the direction-level route explanation into the worker's own timeline,
/// once a session id exists.  This keeps both the lead/board and worker session
/// surfaces honest without creating a second routing policy.
pub async fn mirror_direction_route(db: &Db, thread_id: i32, direction_id: i32, session_id: i32) {
    let messages = repo::list_lead_messages(db, thread_id)
        .await
        .unwrap_or_default();
    let Some(source) = messages.iter().rev().find(|message| {
        if message.kind != "engine_route" || message.session_id.is_some() {
            return false;
        }
        serde_json::from_str::<serde_json::Value>(&message.content)
            .ok()
            .and_then(|value| value.get("direction_id").and_then(|id| id.as_i64()))
            .is_some_and(|id| id == i64::from(direction_id))
    }) else {
        return;
    };
    // A session can be created before native startup, then be reopened after
    // its unpinned direction is re-resolved. Reuse the old mirror only when it
    // is byte-for-byte the current authoritative direction marker; otherwise
    // append the newer marker so the timeline retains both route decisions.
    if messages.iter().rev().any(|message| {
        message.session_id == Some(session_id)
            && message.kind == "engine_route"
            && message.content == source.content
    }) {
        return;
    }
    let source_content = source.content.clone();
    let turn_id = match repo::next_turn_id(db, thread_id).await {
        Ok(turn) => turn,
        Err(_) => return,
    };
    let _ = repo::insert_lead_message(
        db,
        thread_id,
        Some(session_id),
        turn_id,
        "system",
        "engine_route",
        &source_content,
        "complete",
    )
    .await;
}

/// Manual pin detection is persisted alongside the engine identity. Timeline
/// rows are an audit trail, not a source of truth: history can be pruned and a
/// legacy row has no marker from which to infer whether a human chose it. A
/// lookup failure propagates so the fail-over caller can fail closed rather
/// than treating an unreadable pin as permission to switch.
pub async fn has_manual_pin(
    db: &Db,
    thread_id: i32,
    session_id: Option<i32>,
) -> anyhow::Result<bool> {
    match session_id {
        Some(session_id) => {
            let session = repo::get_session(db, session_id)
                .await?
                .ok_or_else(|| anyhow::anyhow!("session {session_id} not found"))?;
            let direction = repo::get_direction(db, session.direction_id)
                .await?
                .ok_or_else(|| anyhow::anyhow!("direction {} not found", session.direction_id))?;
            Ok(session.engine_pinned || direction.engine_pinned)
        }
        None => {
            let thread = repo::get_thread(db, thread_id)
                .await?
                .ok_or_else(|| anyhow::anyhow!("thread {thread_id} not found"))?;
            Ok(thread.engine_pinned)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sea_orm::ConnectionTrait;

    fn candidate(tool: EngineId, installed: bool, quota: Option<QuotaStatus>) -> RouteCandidate {
        RouteCandidate {
            tool,
            installed,
            quota,
        }
    }

    fn request(
        automatic_enabled: bool,
        manual_tool: Option<EngineId>,
        legacy_tool: EngineId,
        hint: RoutingHint,
        candidates: Vec<RouteCandidate>,
    ) -> RouteRequest {
        RouteRequest {
            automatic_enabled,
            manual_tool,
            legacy_tool,
            hint,
            candidates,
        }
    }

    #[test]
    fn routing_disabled_preserves_legacy_tool() {
        let out = resolve(&request(
            false,
            None,
            EngineId::Opencode,
            RoutingHint::Normal,
            vec![candidate(
                EngineId::Codex,
                true,
                Some(QuotaStatus::Exceeded),
            )],
        ));
        assert_eq!(out.selected(), Some(EngineId::Opencode));
        assert_eq!(out.reason, RouteReason::AutomaticDisabled);
    }

    #[test]
    fn normal_prefers_codex_and_deep_prefers_claude() {
        let candidates = vec![
            candidate(EngineId::Codex, true, Some(QuotaStatus::Ok)),
            candidate(EngineId::Claude, true, Some(QuotaStatus::Ok)),
        ];
        assert_eq!(
            resolve(&request(
                true,
                None,
                EngineId::Opencode,
                RoutingHint::Normal,
                candidates.clone()
            ))
            .selected(),
            Some(EngineId::Codex)
        );
        assert_eq!(
            resolve(&request(
                true,
                None,
                EngineId::Opencode,
                RoutingHint::Deep,
                candidates
            ))
            .selected(),
            Some(EngineId::Claude)
        );
    }

    #[test]
    fn manual_pin_wins_even_when_quota_is_exceeded() {
        let out = resolve(&request(
            true,
            Some(EngineId::Claude),
            EngineId::Codex,
            RoutingHint::Normal,
            vec![
                candidate(EngineId::Codex, true, Some(QuotaStatus::Ok)),
                candidate(EngineId::Claude, true, Some(QuotaStatus::Exceeded)),
            ],
        ));
        assert_eq!(out.selected(), Some(EngineId::Claude));
        assert_eq!(out.source, RoutingSource::Manual);
    }

    #[cfg(unix)]
    #[test]
    fn automatic_candidates_require_executable_regular_files() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let executable = dir.path().join("agent");
        std::fs::write(&executable, b"#!/bin/sh\n").unwrap();
        let mut permissions = std::fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o644);
        std::fs::set_permissions(&executable, permissions).unwrap();

        let search_path = dir.path().to_string_lossy().into_owned();
        assert!(!command_is_ready_on_path(
            executable.to_str().unwrap(),
            &search_path
        ));
        assert!(!command_is_ready_on_path("agent", &search_path));

        let mut permissions = std::fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&executable, permissions).unwrap();
        assert!(command_is_ready_on_path(
            executable.to_str().unwrap(),
            &search_path
        ));
        assert!(command_is_ready_on_path("agent", &search_path));
    }

    #[tokio::test]
    async fn routing_policy_read_errors_fail_closed() {
        let db = Db::connect("sqlite::memory:").await.unwrap();
        db.0
            .execute_unprepared("DROP TABLE app_setting")
            .await
            .unwrap();

        let checked = try_resolve_for_db(&db, None, "codex", RoutingHint::Normal).await;
        assert!(checked.is_err());

        let manual = try_resolve_for_db(&db, Some("opencode"), "codex", RoutingHint::Normal)
            .await
            .unwrap();
        assert_eq!(manual.selected(), Some(EngineId::Opencode));

        let blocked = resolve_for_db(&db, None, "codex", RoutingHint::Normal).await;
        assert!(blocked.blocked);
        assert_eq!(blocked.reason, RouteReason::AutomaticCandidateUnavailable);

        let checked_failover =
            try_quota_failover_for_db(&db, "codex", false, true, true).await;
        assert!(checked_failover.is_err());
        assert_eq!(
            quota_failover_for_db(&db, "codex", false, true, true).await,
            FailoverDecision::Skip(FailoverSkipReason::PolicyUnavailable)
        );
    }

    #[tokio::test]
    async fn manual_pin_bypasses_blocked_initial_route_recovery() {
        let db = Db::connect("sqlite::memory:").await.unwrap();
        let workspace = repo::create_workspace(&db, "ws").await.unwrap();
        // An invalid legacy identity makes a missing manual-pin guard observable:
        // recovery would normalize it to Codex even with automatic routing off.
        let thread = repo::create_thread(&db, workspace.id, "issue", "feature", "not-a-tool")
            .await
            .unwrap();
        repo::set_thread_engine_pinned(&db, thread.id, true)
            .await
            .unwrap();
        let content = serde_json::json!({
            "source": "blocked",
            "operation": "new_issue",
            "hint": "normal"
        })
        .to_string();
        repo::insert_lead_message(
            &db,
            thread.id,
            None,
            1,
            "system",
            "engine_route_blocked",
            &content,
            "complete",
        )
        .await
        .unwrap();

        let pinned = repo::get_thread(&db, thread.id).await.unwrap().unwrap();
        let recovered = prepare_initial_lead(&db, &pinned).await.unwrap();
        assert_eq!(recovered.lead_tool, "not-a-tool");
    }

    #[tokio::test]
    async fn manual_pin_lookup_errors_for_a_missing_participant() {
        let db = Db::connect("sqlite::memory:").await.unwrap();
        assert!(has_manual_pin(&db, 999, None).await.is_err());
        assert!(has_manual_pin(&db, 1, Some(999)).await.is_err());
    }

    #[tokio::test]
    async fn missing_initial_marker_rechecks_an_unpinned_unstarted_lead() {
        let db = Db::connect("sqlite::memory:").await.unwrap();
        let workspace = repo::create_workspace(&db, "ws").await.unwrap();
        repo::set_setting(&db, K_AUTOMATIC_ROUTING_ENABLED, "true")
            .await
            .unwrap();
        let thread = repo::create_thread(&db, workspace.id, "issue", "feature", "not-a-tool")
            .await
            .unwrap();

        let outcome = prepare_initial_lead(&db, &thread).await;
        match outcome {
            Ok(resolved) => assert_ne!(resolved.lead_tool, "not-a-tool"),
            Err(err) => assert!(err.to_string().starts_with("engine_route_blocked:")),
        }
    }

    #[tokio::test]
    async fn worker_route_marker_does_not_suppress_blocked_lead_recovery() {
        let db = Db::connect("sqlite::memory:").await.unwrap();
        let workspace = repo::create_workspace(&db, "ws").await.unwrap();
        repo::set_setting(&db, K_AUTOMATIC_ROUTING_ENABLED, "true")
            .await
            .unwrap();
        let thread = repo::create_thread(&db, workspace.id, "issue", "feature", "not-a-tool")
            .await
            .unwrap();
        let blocked = serde_json::json!({
            "source": "blocked",
            "operation": "new_issue",
            "hint": "normal",
            "direction_id": null,
        })
        .to_string();
        repo::insert_lead_message(
            &db,
            thread.id,
            None,
            1,
            "system",
            "engine_route_blocked",
            &blocked,
            "complete",
        )
        .await
        .unwrap();
        let worker_route = serde_json::json!({
            "source": "manual",
            "operation": "planner_confirm",
            "hint": "normal",
            "direction_id": 42,
        })
        .to_string();
        repo::insert_lead_message(
            &db,
            thread.id,
            None,
            2,
            "system",
            "engine_route",
            &worker_route,
            "complete",
        )
        .await
        .unwrap();

        let outcome = prepare_initial_lead(&db, &thread).await;
        match outcome {
            Ok(resolved) => assert_ne!(resolved.lead_tool, "not-a-tool"),
            Err(err) => assert!(err.to_string().starts_with("engine_route_blocked:")),
        }
    }

    #[tokio::test]
    async fn curator_analysis_route_does_not_suppress_blocked_lead_recovery() {
        let db = Db::connect("sqlite::memory:").await.unwrap();
        let workspace = repo::create_workspace(&db, "ws").await.unwrap();
        repo::set_setting(&db, K_AUTOMATIC_ROUTING_ENABLED, "true")
            .await
            .unwrap();
        let thread = repo::create_thread(&db, workspace.id, "curator", "curator", "not-a-tool")
            .await
            .unwrap();
        let blocked = serde_json::json!({
            "source": "blocked",
            "operation": "curator_start",
            "hint": "deep",
            "direction_id": null,
        })
        .to_string();
        repo::insert_lead_message(
            &db,
            thread.id,
            None,
            1,
            "system",
            "engine_route_blocked",
            &blocked,
            "complete",
        )
        .await
        .unwrap();
        let analysis_route = serde_json::json!({
            "source": "automatic",
            "operation": "curator_analysis",
            "hint": "deep",
            "direction_id": null,
        })
        .to_string();
        repo::insert_lead_message(
            &db,
            thread.id,
            None,
            2,
            "system",
            "engine_route",
            &analysis_route,
            "complete",
        )
        .await
        .unwrap();

        let outcome = prepare_initial_lead(&db, &thread).await;
        match outcome {
            Ok(resolved) => assert_ne!(resolved.lead_tool, "not-a-tool"),
            Err(err) => assert!(err.to_string().starts_with("engine_route_blocked:")),
        }
    }

    #[tokio::test]
    async fn mirror_direction_route_appends_a_newer_authoritative_marker() {
        let db = Db::connect("sqlite::memory:").await.unwrap();
        let workspace = repo::create_workspace(&db, "ws").await.unwrap();
        let thread = repo::create_thread(&db, workspace.id, "issue", "feature", "codex")
            .await
            .unwrap();
        let old_route = serde_json::json!({
            "tool": "codex",
            "source": "automatic",
            "operation": "planner_confirm",
            "direction_id": 7,
        })
        .to_string();
        repo::insert_lead_message(
            &db,
            thread.id,
            None,
            1,
            "system",
            "engine_route",
            &old_route,
            "complete",
        )
        .await
        .unwrap();

        mirror_direction_route(&db, thread.id, 7, 19).await;

        let new_route = serde_json::json!({
            "tool": "claude",
            "source": "automatic",
            "operation": "planner_confirm",
            "direction_id": 7,
        })
        .to_string();
        repo::insert_lead_message(
            &db,
            thread.id,
            None,
            2,
            "system",
            "engine_route",
            &new_route,
            "complete",
        )
        .await
        .unwrap();

        mirror_direction_route(&db, thread.id, 7, 19).await;
        mirror_direction_route(&db, thread.id, 7, 19).await;

        let mirrors: Vec<_> = repo::list_lead_messages(&db, thread.id)
            .await
            .unwrap()
            .into_iter()
            .filter(|message| {
                message.session_id == Some(19) && message.kind == "engine_route"
            })
            .collect();
        assert_eq!(mirrors.len(), 2);
        assert_eq!(mirrors[0].content, old_route);
        assert_eq!(mirrors[1].content, new_route);
    }

    #[test]
    fn warning_is_soft_and_does_not_force_a_running_switch() {
        let out = resolve(&request(
            true,
            None,
            EngineId::Opencode,
            RoutingHint::Normal,
            vec![
                candidate(EngineId::Codex, true, Some(QuotaStatus::Warning)),
                candidate(EngineId::Claude, true, Some(QuotaStatus::Ok)),
            ],
        ));
        assert_eq!(out.selected(), Some(EngineId::Claude));
        assert_eq!(out.reason, RouteReason::PreferredWarning);
    }

    #[test]
    fn exceeded_candidate_is_excluded() {
        let out = resolve(&request(
            true,
            None,
            EngineId::Opencode,
            RoutingHint::Normal,
            vec![
                candidate(EngineId::Codex, true, Some(QuotaStatus::Exceeded)),
                candidate(EngineId::Claude, true, Some(QuotaStatus::Ok)),
            ],
        ));
        assert_eq!(out.selected(), Some(EngineId::Claude));
        assert_eq!(out.reason, RouteReason::PreferredUnavailable);
    }

    #[test]
    fn both_exhausted_are_blocked_without_opencode_auto_selection() {
        let out = resolve(&request(
            true,
            None,
            EngineId::Opencode,
            RoutingHint::Normal,
            vec![
                candidate(EngineId::Codex, true, Some(QuotaStatus::Exceeded)),
                candidate(EngineId::Claude, true, Some(QuotaStatus::Exceeded)),
                candidate(EngineId::Opencode, true, Some(QuotaStatus::Ok)),
            ],
        ));
        assert!(out.blocked);
        assert_eq!(out.reason, RouteReason::BothAutomaticCandidatesExceeded);
    }

    #[test]
    fn explicit_opencode_override_wins_when_automatic_pool_is_blocked() {
        let out = resolve(&request(
            true,
            Some(EngineId::Opencode),
            EngineId::Codex,
            RoutingHint::Normal,
            vec![
                candidate(EngineId::Codex, true, Some(QuotaStatus::Exceeded)),
                candidate(EngineId::Claude, true, Some(QuotaStatus::Exceeded)),
                candidate(EngineId::Opencode, true, Some(QuotaStatus::Ok)),
            ],
        ));
        assert_eq!(out.selected(), Some(EngineId::Opencode));
        assert_eq!(out.source, RoutingSource::Manual);
        assert_eq!(out.reason, RouteReason::ManualPin);
        assert!(!out.blocked);
    }

    #[test]
    fn opencode_is_not_an_automatic_fallback_when_the_pool_is_unavailable() {
        let out = resolve(&request(
            true,
            None,
            EngineId::Opencode,
            RoutingHint::Normal,
            vec![
                candidate(EngineId::Codex, false, None),
                candidate(EngineId::Claude, false, None),
                candidate(EngineId::Opencode, true, Some(QuotaStatus::Ok)),
            ],
        ));
        assert!(out.blocked);
        assert_eq!(out.reason, RouteReason::NoAutomaticCandidate);
    }

    #[test]
    fn quota_failover_is_opt_in_and_respects_manual_pin() {
        let fallback = candidate(EngineId::Claude, true, Some(QuotaStatus::Ok));
        assert_eq!(
            resolve_failover(
                EngineId::Codex,
                false,
                false,
                true,
                Some(&fallback),
                true,
            ),
            FailoverDecision::Skip(FailoverSkipReason::Disabled)
        );
        assert_eq!(
            resolve_failover(
                EngineId::Codex,
                true,
                true,
                true,
                Some(&fallback),
                true,
            ),
            FailoverDecision::Skip(FailoverSkipReason::ManualPin)
        );
    }

    #[test]
    fn quota_failover_blocks_when_both_candidates_are_exhausted() {
        let fallback = candidate(EngineId::Claude, true, Some(QuotaStatus::Exceeded));
        assert_eq!(
            resolve_failover(
                EngineId::Codex,
                true,
                false,
                true,
                Some(&fallback),
                true,
            ),
            FailoverDecision::Blocked(FailoverBlockedReason::BothAutomaticCandidatesExceeded)
        );
    }

    #[test]
    fn quota_failover_requires_a_structured_exceeded_signal() {
        let fallback = candidate(EngineId::Claude, true, Some(QuotaStatus::Ok));
        assert_eq!(
            resolve_failover(
                EngineId::Codex,
                true,
                false,
                false,
                Some(&fallback),
                true,
            ),
            FailoverDecision::Skip(FailoverSkipReason::NotStructuredExceeded)
        );
    }

    #[test]
    fn quota_failover_accepts_a_structured_turn_code_without_a_snapshot() {
        let fallback = candidate(EngineId::Claude, true, None);
        assert_eq!(
            resolve_failover(
                EngineId::Codex,
                true,
                false,
                true,
                Some(&fallback),
                true,
            ),
            FailoverDecision::SwitchTo {
                tool: EngineId::Claude,
                quota: None,
            }
        );
    }
}
