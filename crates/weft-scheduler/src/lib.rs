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

/// A direction accepted onto the automatic-dispatch queue after a human or
/// lead gate. Adapters persist [`EnqueueJob::status`] and start the worker;
/// this crate does not touch a store or a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnqueueJob {
    pub direction_id: i64,
    pub status: &'static str,
}

/// After the adapter's human/lead gate, these direction ids become
/// automatic-dispatch jobs. Order is preserved. Duplicates and non-positive
/// ids are dropped.
pub fn enqueue(direction_ids: impl IntoIterator<Item = i64>) -> Vec<EnqueueJob> {
    let mut seen = HashSet::new();
    let mut jobs = Vec::new();
    for direction_id in direction_ids {
        if direction_id <= 0 {
            continue;
        }
        if !seen.insert(direction_id) {
            continue;
        }
        jobs.push(EnqueueJob {
            direction_id,
            status: initial_status(),
        });
    }
    jobs
}

/// Persist `initial_status` only for lanes that have not started yet.
/// Already-working / review / done rows stay put on an idempotent re-confirm.
pub fn enqueue_promotes_status(stored: &str) -> bool {
    if stored == "planning" {
        return true;
    }
    normalize_status(stored) == STATUS_QUEUED
}

/// Persist [`WORKER_START_FAILED`] when enqueue spawn never started or any
/// attempt failed. Adapters clear that code only after a successful start.
pub fn enqueue_dispatch_failed(attempted: bool, failed: bool) -> bool {
    failed || !attempted
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

/// Inflight / coalescing key: thread (or issue) plus the wake's party dir.
pub fn inflight_key(issue_id: impl std::fmt::Display, dir: &str) -> String {
    format!("{issue_id}/{dir}")
}

impl PartyRoute {
    /// Bus party string for this route (`you` / `lead` / direction id).
    pub fn party_id(self) -> String {
        match self {
            Self::Human => HUMAN_PARTY.to_string(),
            Self::Lead => LEAD_PARTY.to_string(),
            Self::Worker(id) => id.to_string(),
        }
    }
}

/// Adapter-observed facts. The scheduler decides; the adapter does not
/// re-derive skip/attach policy at the call site.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WakeFacts {
    /// Persisted single-writer release (`STATUS_STOPPED` / human terminal).
    pub taken_over: bool,
    /// Adapter still owns a live child for this party.
    pub live: bool,
    /// Session engine is already in the adapter's map (may be a dead child).
    pub resident: bool,
    /// Worker direction is `done`. Lead ignores this.
    pub worker_done: bool,
    /// Wake's numeric dir belongs to another thread. Lead / human ignore this.
    pub foreign_thread: bool,
    /// Direction or session row is missing. Lead / human ignore this.
    pub missing_target: bool,
}

/// Why [`route`] asked the adapter not to deliver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WakeSkip {
    TakenOver,
    AlreadyDone,
    ForeignThread,
    MissingTarget,
}

/// What the adapter should do after `classify_party`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WakeAction {
    NotifyHuman,
    Deliver { attach: bool },
    Skip(WakeSkip),
}

/// Wake → route. Adapters then speak through [`SessionPort`].
///
/// Taken-over + not live → skip (never spawn a competing headless process).
/// A live resident process means the adapter re-owns the slot, so deliver.
/// Done workers are not attached; a still-resident engine may be nudged.
pub fn route(party: PartyRoute, facts: WakeFacts) -> WakeAction {
    match party {
        PartyRoute::Human => WakeAction::NotifyHuman,
        PartyRoute::Lead => route_lead(facts),
        PartyRoute::Worker(_) => route_worker(facts),
    }
}

fn route_lead(facts: WakeFacts) -> WakeAction {
    if facts.taken_over && !facts.live {
        return WakeAction::Skip(WakeSkip::TakenOver);
    }
    WakeAction::Deliver {
        attach: !facts.resident,
    }
}

fn route_worker(facts: WakeFacts) -> WakeAction {
    if facts.missing_target {
        return WakeAction::Skip(WakeSkip::MissingTarget);
    }
    if facts.foreign_thread {
        return WakeAction::Skip(WakeSkip::ForeignThread);
    }
    if facts.taken_over && !facts.live {
        return WakeAction::Skip(WakeSkip::TakenOver);
    }
    if facts.worker_done && !facts.resident {
        return WakeAction::Skip(WakeSkip::AlreadyDone);
    }
    WakeAction::Deliver {
        attach: !facts.resident,
    }
}

/// Abstract party the scheduler asks an adapter to nudge.
pub fn party_ref(issue_id: i64, party: PartyRoute) -> PartyRef {
    PartyRef {
        issue_id,
        party: party.party_id(),
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

    #[test]
    fn inflight_key_joins_issue_and_dir() {
        assert_eq!(inflight_key(7, "lead"), "7/lead");
        assert_eq!(inflight_key(3, "10"), "3/10");
    }

    #[test]
    fn party_ref_uses_bus_identity() {
        assert_eq!(party_ref(9, PartyRoute::Human).party, HUMAN_PARTY);
        assert_eq!(party_ref(9, PartyRoute::Lead).party, LEAD_PARTY);
        assert_eq!(party_ref(9, PartyRoute::Worker(4)).party, "4");
        assert_eq!(party_ref(9, PartyRoute::Lead).issue_id, 9);
    }

    #[test]
    fn route_human_is_notify_regardless_of_facts() {
        let facts = WakeFacts {
            taken_over: true,
            live: true,
            resident: true,
            worker_done: true,
            foreign_thread: true,
            missing_target: true,
        };
        assert_eq!(route(PartyRoute::Human, facts), WakeAction::NotifyHuman);
        assert_eq!(
            route(PartyRoute::Human, WakeFacts::default()),
            WakeAction::NotifyHuman
        );
    }

    #[test]
    fn route_lead_skips_taken_over_when_not_live() {
        assert_eq!(
            route(
                PartyRoute::Lead,
                WakeFacts {
                    taken_over: true,
                    live: false,
                    resident: false,
                    ..WakeFacts::default()
                }
            ),
            WakeAction::Skip(WakeSkip::TakenOver)
        );
    }

    #[test]
    fn route_lead_delivers_when_adapter_reowns() {
        assert_eq!(
            route(
                PartyRoute::Lead,
                WakeFacts {
                    taken_over: true,
                    live: true,
                    resident: true,
                    ..WakeFacts::default()
                }
            ),
            WakeAction::Deliver { attach: false }
        );
    }

    #[test]
    fn route_lead_attaches_when_not_resident() {
        assert_eq!(
            route(PartyRoute::Lead, WakeFacts::default()),
            WakeAction::Deliver { attach: true }
        );
    }

    #[test]
    fn route_worker_skips_store_invariants() {
        assert_eq!(
            route(
                PartyRoute::Worker(2),
                WakeFacts {
                    missing_target: true,
                    ..WakeFacts::default()
                }
            ),
            WakeAction::Skip(WakeSkip::MissingTarget)
        );
        assert_eq!(
            route(
                PartyRoute::Worker(2),
                WakeFacts {
                    foreign_thread: true,
                    ..WakeFacts::default()
                }
            ),
            WakeAction::Skip(WakeSkip::ForeignThread)
        );
    }

    #[test]
    fn route_worker_skips_taken_over_and_done_attach() {
        assert_eq!(
            route(
                PartyRoute::Worker(2),
                WakeFacts {
                    taken_over: true,
                    live: false,
                    resident: false,
                    ..WakeFacts::default()
                }
            ),
            WakeAction::Skip(WakeSkip::TakenOver)
        );
        assert_eq!(
            route(
                PartyRoute::Worker(2),
                WakeFacts {
                    worker_done: true,
                    resident: false,
                    live: false,
                    ..WakeFacts::default()
                }
            ),
            WakeAction::Skip(WakeSkip::AlreadyDone)
        );
    }

    #[test]
    fn route_worker_nudges_resident_done_without_attach() {
        assert_eq!(
            route(
                PartyRoute::Worker(2),
                WakeFacts {
                    worker_done: true,
                    resident: true,
                    live: false,
                    ..WakeFacts::default()
                }
            ),
            WakeAction::Deliver { attach: false }
        );
    }

    #[test]
    fn route_worker_attaches_idle_open_direction() {
        assert_eq!(
            route(PartyRoute::Worker(2), WakeFacts::default()),
            WakeAction::Deliver { attach: true }
        );
    }

    #[test]
    fn enqueue_dedupes_and_skips_non_positive() {
        assert!(enqueue(std::iter::empty()).is_empty());
        let jobs = enqueue([1, 1, 0, -3, 2]);
        assert_eq!(
            jobs,
            vec![
                EnqueueJob {
                    direction_id: 1,
                    status: STATUS_WORKING,
                },
                EnqueueJob {
                    direction_id: 2,
                    status: STATUS_WORKING,
                },
            ]
        );
    }

    #[test]
    fn enqueue_promotes_only_unstarted_lanes() {
        assert!(enqueue_promotes_status("queued"));
        assert!(enqueue_promotes_status("planning"));
        assert!(enqueue_promotes_status("unknown"));
        assert!(!enqueue_promotes_status("working"));
        assert!(!enqueue_promotes_status("review"));
        assert!(!enqueue_promotes_status("done"));
    }

    #[test]
    fn enqueue_dispatch_failed_when_unattempted_or_errored() {
        assert!(!enqueue_dispatch_failed(true, false));
        assert!(enqueue_dispatch_failed(true, true));
        assert!(enqueue_dispatch_failed(false, false));
        assert!(enqueue_dispatch_failed(false, true));
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
