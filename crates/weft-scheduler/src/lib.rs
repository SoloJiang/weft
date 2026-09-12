//! Shared Weft dispatch state machine.
//!
//! Zero Tauri, zero axum, zero `weft-app-server`. Adapters own how a session
//! is spoken to: Weft `lead_chat` maps both delivery modes to
//! `TurnState::request_bus_read`; the Codex adapter maps [`DeliveryMode::MergeActive`]
//! to `turn/steer` and [`DeliveryMode::StartIdle`] to `turn/start`.
#![cfg_attr(
    not(test),
    deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)
)]

use std::collections::HashSet;

pub use weft_brief::LEAD_PARTY;

/// Human bus party (Weft Needs You / `you`). Codex does not implement HumanPort.
pub const HUMAN_PARTY: &str = "you";

pub const STATUS_QUEUED: &str = "queued";
pub const STATUS_WORKING: &str = "working";
pub const STATUS_REVIEW: &str = "review";
pub const STATUS_DONE: &str = "done";

/// Stable attention codes. UI maps these; adapters must not interpolate raw errors.
pub const WORKER_START_FAILED: &str = "worker-start-failed";
pub const LEAD_START_FAILED: &str = "start-failed";
pub const LEAD_RESUME_FAILED: &str = "resume-failed";
pub const LEAD_TURN_ERROR: &str = "turn-error";
pub const THREAD_RESUME_FAILED: &str = "thread-resume-failed";

/// Kanban status for a freshly spawned direction.
pub fn initial_status() -> &'static str {
    STATUS_WORKING
}

/// Status a direction lands in when its turn completes.
pub fn status_after_turn_end() -> &'static str {
    STATUS_REVIEW
}

/// Fold the retired `planning` column into working; unknown values stay queued.
pub fn normalize_status(status: &str) -> &'static str {
    if status == "planning" {
        return STATUS_WORKING;
    }
    if status == STATUS_QUEUED {
        return STATUS_QUEUED;
    }
    if status == STATUS_WORKING {
        return STATUS_WORKING;
    }
    if status == STATUS_REVIEW {
        return STATUS_REVIEW;
    }
    if status == STATUS_DONE {
        return STATUS_DONE;
    }
    STATUS_QUEUED
}

/// The only human status transition on the Codex path is review → done.
pub fn can_complete(status: &str) -> bool {
    normalize_status(status) == STATUS_REVIEW
}

/// How the scheduler tells a SessionPort to speak to the model.
///
/// The crate never names `turn/steer` — that string stays in the Codex adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryMode {
    MergeActive,
    StartIdle,
}

pub fn delivery_mode(has_active_turn: bool) -> DeliveryMode {
    if has_active_turn {
        DeliveryMode::MergeActive
    } else {
        DeliveryMode::StartIdle
    }
}

/// Where a wake's party string routes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PartyRoute {
    Human,
    Lead,
    Worker(i64),
}

/// Pure classifier over a wake's `dir` / party string.
pub fn classify_party(dir: &str) -> Option<PartyRoute> {
    if dir == HUMAN_PARTY {
        Some(PartyRoute::Human)
    } else if dir == LEAD_PARTY {
        Some(PartyRoute::Lead)
    } else {
        dir.parse::<i64>().ok().map(PartyRoute::Worker)
    }
}

/// Per-key serialization. At most one delivery runs per key; a wake that lands
/// while one is in flight is remembered and re-delivered the instant it ends.
#[derive(Default)]
pub struct Inflight {
    running: HashSet<String>,
    dirty: HashSet<String>,
}

impl Inflight {
    /// Register a wake. Returns true if the caller should start a delivery loop.
    pub fn begin(&mut self, key: &str) -> bool {
        if self.running.contains(key) {
            self.dirty.insert(key.to_string());
            false
        } else {
            self.running.insert(key.to_string());
            true
        }
    }

    /// After one delivery completes. Returns true if a wake coalesced during it.
    pub fn next(&mut self, key: &str) -> bool {
        if self.dirty.remove(key) {
            true
        } else {
            self.running.remove(key);
            false
        }
    }
}

/// Abstract party the scheduler asks an adapter to nudge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartyRef {
    pub issue_id: i64,
    pub party: String,
}

#[derive(Debug)]
pub struct SchedulerError {
    pub message: String,
}

impl SchedulerError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for SchedulerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for SchedulerError {}

/// Session adapter. Weft: `lead_chat`. Codex: `weft_app_server` inject.
pub trait SessionPort: Send + Sync {
    fn deliver(
        &self,
        party: &PartyRef,
        text: &str,
        mode: DeliveryMode,
    ) -> impl std::future::Future<Output = Result<bool, SchedulerError>> + Send;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_mapping() {
        assert_eq!(initial_status(), "working");
        assert_eq!(status_after_turn_end(), "review");
        assert_eq!(normalize_status("planning"), "working");
        assert_eq!(normalize_status("unknown"), "queued");
        assert!(can_complete("review"));
        assert!(!can_complete("working"));
        assert!(!can_complete("planning"));
    }

    #[test]
    fn classify_routes_each_bus_identity() {
        assert_eq!(classify_party("you"), Some(PartyRoute::Human));
        assert_eq!(classify_party("lead"), Some(PartyRoute::Lead));
        assert_eq!(classify_party("10"), Some(PartyRoute::Worker(10)));
        assert_eq!(classify_party(""), None);
        assert_eq!(classify_party("leader"), None);
        assert_eq!(classify_party("worker-3"), None);
    }

    #[test]
    fn inflight_serializes_and_coalesces() {
        let mut f = Inflight::default();
        let k = "7/lead";
        assert!(f.begin(k));
        assert!(!f.begin(k));
        assert!(!f.begin(k));
        assert!(f.next(k));
        assert!(!f.next(k));
        assert!(f.begin(k));
    }

    #[test]
    fn delivery_mode_is_idle_or_merge_not_protocol() {
        assert_eq!(delivery_mode(true), DeliveryMode::MergeActive);
        assert_eq!(delivery_mode(false), DeliveryMode::StartIdle);
    }

    struct FakePort;

    impl SessionPort for FakePort {
        fn deliver(
            &self,
            _party: &PartyRef,
            _text: &str,
            mode: DeliveryMode,
        ) -> impl std::future::Future<Output = Result<bool, SchedulerError>> + Send {
            async move {
                match mode {
                    DeliveryMode::MergeActive | DeliveryMode::StartIdle => Ok(true),
                }
            }
        }
    }

    #[test]
    fn session_port_is_implementable_without_app_server() {
        let port = FakePort;
        let party = PartyRef {
            issue_id: 1,
            party: LEAD_PARTY.to_string(),
        };
        let future = port.deliver(&party, "hi", DeliveryMode::StartIdle);
        let result = futures_lite_block_on(future).expect("ok");
        assert!(result);
    }

    fn futures_lite_block_on<F: std::future::Future>(fut: F) -> F::Output {
        use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
        fn dummy(_: *const ()) {}
        fn clone(_: *const ()) -> RawWaker {
            RawWaker::new(std::ptr::null(), &VTABLE)
        }
        static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, dummy, dummy, dummy);
        let waker = unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VTABLE)) };
        let mut cx = Context::from_waker(&waker);
        let mut fut = std::pin::pin!(fut);
        match fut.as_mut().poll(&mut cx) {
            Poll::Ready(v) => v,
            Poll::Pending => panic!("fake port must be ready"),
        }
    }
}
