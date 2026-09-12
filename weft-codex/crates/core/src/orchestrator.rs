//! Orchestrator: lead/direction Codex-thread lifecycle and bus delivery.
//!
//! Every lead/direction conversation IS a Codex thread (migration spec §5);
//! this module creates them, feeds them, and folds app-server notifications
//! back into kanban state. Injection semantics are spike-verified
//! (docs/spike-app-server): `turn/start` while a turn is active is silently
//! dropped by the server (returns success, never runs), so mid-turn delivery
//! MUST go through `turn/steer` with the tracked active turn id.
//!
//! Concurrency: `inject_lock` serializes every thread mutation weftd makes
//! (spawn's first turn, bus delivery, human messages). Without it, a
//! delivery racing a `start_turn` → `set_active_turn` window would see "no
//! active turn" and fire a second `turn/start` — the silent-drop case above.
//!
//! Human takeover: a human can open any orchestrated thread in Desktop and
//! drive it. The watcher flags such threads via `turn/started` whose id
//! isn't ours (`foreign` map); while flagged, injection stands down and the
//! bus inbox stays parked. When the human's turn ends, the flag clears and
//! the backlog flushes through the normal delivery path. NOTE (verified
//! 2026-08-08, docs/spike-app-server/probe_takeover.py): turn lifecycle
//! notifications do NOT cross app-server processes, so a Desktop-driven
//! turn never reaches our watcher — the foreign flag only fires for
//! same-process starts. The REAL silent-drop guard is therefore start
//! confirmation: after every `turn/start`, inject waits for OUR watcher to
//! route the matching `turn/started` (same process → always arrives);
//! absence within [`TURN_CONFIRM_TIMEOUT`] means the server silently
//! dropped the start (busy with a foreign turn), the phantom active-turn
//! record is cleared, and the message parks for a later wake.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use serde_json::json;
use tokio::sync::{mpsc::UnboundedReceiver, Mutex};
use weft_app_server::client as codex;
use weft_app_server::client::{Client, ThreadInfo, ThreadMsg};
use weft_app_server::proto::ChatEvent;

use crate::api_error::ApiError;
use crate::bus::{BusRegistry, Msg};
use crate::store::{Store, ThreadBindingRow};
use crate::{brief, events, runtime, worktree};

/// Stable attention reason stored when automatic worker dispatch fails. The UI
/// translates this code instead of exposing backend error text as product copy.
pub const WORKER_START_FAILED: &str = "worker-start-failed";

/// Kanban status for a freshly spawned direction.
///
/// Planning and implementation share the in-progress column; mandate still
/// chooses the brief, not the board column.
pub fn initial_status() -> &'static str {
    "working"
}

/// The status a direction lands in when its turn completes (success or not —
/// an errored turn also sets `attention`; both then wait on the next pass).
pub fn status_after_turn_end() -> &'static str {
    "review"
}

fn non_empty(s: String) -> Option<String> {
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// How long inject waits for the watcher to confirm a `turn/start` via its
/// routed `turn/started` before concluding the server silently dropped it
/// (busy with a turn from another app-server, e.g. human takeover).
pub const TURN_CONFIRM_TIMEOUT: Duration = Duration::from_millis(3000);

/// Poll interval for [`await_turn_confirmation`].
const TURN_CONFIRM_POLL: Duration = Duration::from_millis(100);

/// Wait until the watcher records `turn/started` for `(thread, turn)` —
/// proof the turn REALLY started — or `timeout` expires. Free fn (not a
/// method) so tests drive it without a Client.
async fn await_turn_confirmation(
    confirmed: &StdMutex<HashMap<String, String>>,
    thread_id: &str,
    turn_id: &str,
    timeout: Duration,
) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        {
            let g = match confirmed.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            if g.get(thread_id).map(String::as_str) == Some(turn_id) {
                return true;
            }
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(TURN_CONFIRM_POLL).await;
    }
}

/// What a spawned watcher folds notifications into.
#[derive(Clone, Copy)]
enum WatchTarget {
    Direction(i64),
    Lead(i64),
}

/// Shared orchestration state. Cheap to clone (Arc fields / pool inside).
#[derive(Clone)]
pub struct Orchestrator {
    store: Store,
    bus: BusRegistry,
    /// Public base URL of weftd's MCP server, e.g. `http://127.0.0.1:47810`.
    bus_base: String,
    /// weft-codex home; direction worktrees live under `<home>/worktrees/`.
    home: PathBuf,
    inject_lock: Arc<Mutex<()>>,
    /// Threads a HUMAN is currently driving in Desktop (thread_id → their
    /// turn id). Detected via `turn/started` whose id isn't our tracked
    /// active turn; while set, injection into that thread stands down
    /// (turn/start would be silently dropped) and the bus inbox stays
    /// parked until their turn ends. std Mutex like the bus — never held
    /// across an await.
    foreign: Arc<StdMutex<HashMap<String, String>>>,
    /// thread_id → last turn id the watcher confirmed via `turn/started`.
    /// inject cross-checks every `turn/start` against this to detect the
    /// server's silent drop (Ok response, turn never runs). Bounded by
    /// live-thread count; entries are overwritten per turn, never removed.
    confirmed_turns: Arc<StdMutex<HashMap<String, String>>>,
}

impl Orchestrator {
    pub fn new(store: Store, bus: BusRegistry, bus_base: String, home: PathBuf) -> Self {
        Self {
            store,
            bus,
            bus_base,
            home,
            inject_lock: Arc::new(Mutex::new(())),
            foreign: Arc::new(StdMutex::new(HashMap::new())),
            confirmed_turns: Arc::new(StdMutex::new(HashMap::new())),
        }
    }

    /// Watcher hook: a `turn/started` routed for this thread. Recorded
    /// unconditionally — the foreign-vs-ours classification happens after,
    /// and inject's confirmation check needs the id even when the
    /// notification beat our own `set_active_turn` bookkeeping.
    fn note_turn_started(&self, thread_id: &str, turn_id: &str) {
        let mut g = match self.confirmed_turns.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        g.insert(thread_id.to_string(), turn_id.to_string());
    }

    fn set_foreign_turn(&self, thread_id: &str, turn_id: &str) {
        let mut g = match self.foreign.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        g.insert(thread_id.to_string(), turn_id.to_string());
    }

    /// Returns true when a foreign turn WAS tracked — i.e. the caller just
    /// witnessed the human's turn ending and should flush what parked.
    fn clear_foreign_turn(&self, thread_id: &str) -> bool {
        let mut g = match self.foreign.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        g.remove(thread_id).is_some()
    }

    fn foreign_turn(&self, thread_id: &str) -> Option<String> {
        let g = match self.foreign.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        g.get(thread_id).cloned()
    }

    fn bus_url(&self, issue_id: i64, party: &str) -> String {
        format!("{}/bus/{issue_id}/{party}/mcp", self.bus_base)
    }

    /// Resolve a bus party to its Codex thread: `lead` → the issue's lead
    /// thread, a numeric party → that direction's thread. A direction id
    /// from ANOTHER issue resolves to None — a party can only ever reach
    /// participants of its own issue.
    pub async fn thread_for(&self, issue_id: i64, party: &str) -> anyhow::Result<Option<String>> {
        if party == brief::LEAD_PARTY {
            let issue = self.store.get_issue(issue_id).await?;
            return Ok(issue.and_then(|i| non_empty(i.lead_codex_thread_id)));
        }
        let dir_id = match party.parse::<i64>() {
            Ok(id) => id,
            Err(_) => return Ok(None),
        };
        let direction = self.store.get_direction(dir_id).await?;
        match direction {
            Some(d) if d.issue_id == issue_id => Ok(non_empty(d.codex_thread_id)),
            _ => Ok(None),
        }
    }

    /// Resolve a native Codex thread back to its logical Lead/Task owner.
    /// Known primary and fork rows are a pure store lookup. An unknown active
    /// thread is inspected through metadata-only `thread/read`; only an
    /// explicit `forkedFromId` chain reaching a known binding is adopted.
    /// This prevents unrelated native Codex chats from being pulled into a
    /// Weft issue merely because they share a cwd or source label.
    pub async fn resolve_thread_binding(
        &self,
        thread_id: &str,
    ) -> anyhow::Result<Option<ThreadBindingRow>> {
        if let Some(binding) = self.store.get_thread_binding(thread_id).await? {
            return Ok(Some(binding));
        }
        if !runtime::agents_allowed() {
            return Ok(None);
        }

        let client = codex::client().await?;
        adopt_fork_chain(&self.store, thread_id, move |id| {
            let client = client.clone();
            async move { client.read_thread(&id).await }
        })
        .await
    }

    // ── spawning ──────────────────────────────────────────────────────────

    /// Start a worker and turn any startup failure into an actionable task
    /// marker. Normal creation uses this path automatically; the UI calls the
    /// same method only when retrying a failed dispatch.
    pub async fn dispatch_direction(&self, direction_id: i64) -> anyhow::Result<String> {
        match self.spawn_direction(direction_id).await {
            Ok(thread_id) => Ok(thread_id),
            Err(error) => {
                if let Err(marker_error) = self
                    .store
                    .set_direction_attention(direction_id, Some(WORKER_START_FAILED))
                    .await
                {
                    eprintln!(
                        "[weftd] mark task {direction_id} dispatch failure failed: {marker_error:#}"
                    );
                }
                events::emit(
                    "direction.updated",
                    json!({
                        "id": direction_id,
                        "attention": true,
                        "reason": WORKER_START_FAILED
                    }),
                );
                Err(error)
            }
        }
    }

    /// Materialize the direction's worktree, create its Codex thread (cwd =
    /// worktree, workspace-write sandbox, per-thread bus MCP), subscribe a
    /// watcher, and kick off the brief turn. The thread id is persisted only
    /// after the initial turn starts, so a failed first turn remains retryable.
    /// Returns the new Codex thread id.
    pub async fn spawn_direction(&self, direction_id: i64) -> anyhow::Result<String> {
        // The automatic queue and a stale/manual retry can race on the same
        // task. Serialize before reading the thread id so only one caller can
        // materialize and start it; later callers reuse the durable result.
        let gate = self.inject_lock.lock().await;
        let direction = self
            .store
            .get_direction(direction_id)
            .await?
            .ok_or(ApiError::not_found("direction", direction_id))?;
        if !direction.codex_thread_id.is_empty() {
            return Ok(direction.codex_thread_id);
        }
        if !runtime::agents_allowed() {
            anyhow::bail!("daemon is not live; refusing to spawn agents");
        }
        let issue = self
            .store
            .get_issue(direction.issue_id)
            .await?
            .ok_or(ApiError::not_found("issue", direction.issue_id))?;
        let repo = self
            .store
            .get_repo(direction.repo_id)
            .await?
            .ok_or(ApiError::not_found("repo", direction.repo_id))?;

        let branch = worktree::branch_name(&issue.slug, &direction.slug);
        let wt_path = worktree::worktree_path(&self.home, &issue.slug, &direction.slug);
        let info = worktree::ensure_worktree(
            std::path::Path::new(&repo.path),
            &wt_path,
            &branch,
            &direction.base_branch,
        )
        .await?;
        self.store
            .record_worktree(direction.id, repo.id, &info.path.to_string_lossy(), &info.branch)
            .await?;
        self.store
            .set_direction_branch(direction.id, &info.branch)
            .await?;

        let party = brief::direction_party(direction.id);
        let url = self.bus_url(issue.id, &party);
        let client = codex::client().await?;
        let result = client
            .request(
                "thread/start",
                codex::thread_start_params_configured(
                    &info.path.to_string_lossy(),
                    "never",
                    "workspace-write",
                    Some(&url),
                    false,
                ),
            )
            .await?;
        let thread_id = codex::thread_id_of(&result)
            .ok_or_else(|| anyhow::anyhow!("thread/start: no thread.id"))?;

        let rx = client.subscribe(&thread_id).await;
        tokio::spawn(watch(
            self.clone(),
            client.clone(),
            WatchTarget::Direction(direction.id),
            thread_id.clone(),
            rx,
        ));

        // Resolve the basis title lazily: the id/revision are already on the
        // task, so a missing artifact row degrades to "no basis" rather than
        // failing the spawn.
        let basis_row = if direction.source_artifact_id != 0 {
            self.store
                .get_artifact(direction.source_artifact_id)
                .await
                .ok()
                .flatten()
        } else {
            None
        };
        let basis = basis_row.as_ref().map(|row| brief::ArtifactBasis {
            id: row.id,
            revision: direction.source_artifact_revision,
            title: &row.title,
        });
        let text = brief::direction_brief(
            &issue.title,
            &direction.name,
            &direction.spec,
            &direction.mandate,
            &direction.reason,
            &repo.name,
            &party,
            &url,
            basis.as_ref(),
        );
        let turn_id = match client.start_turn(&thread_id, &text).await {
            Ok(turn_id) => turn_id,
            Err(error) => {
                client.unsubscribe(&thread_id).await;
                return Err(error);
            }
        };
        client.set_active_turn(&thread_id, &turn_id).await;
        let status = initial_status();
        self.store
            .activate_direction_thread(direction.id, &thread_id, status)
            .await?;
        events::emit(
            "direction.updated",
            json!({
                "id": direction.id,
                "status": status,
                "codexThreadId": thread_id,
                "attention": false
            }),
        );

        // Flush anything queued for this party before it existed.
        drop(gate);
        self.deliver(issue.id, &party).await;
        Ok(thread_id)
    }

    /// Create the issue's lead thread: read-only sandbox over the first repo
    /// (the lead coordinates; it does not write code). Returns the new
    /// Codex thread id.
    /// Reasons are stable codes, not error prose: the UI renders them through
    /// i18n, and a raw error chain would leak internals into the interface.
    pub const LEAD_START_FAILED: &'static str = "start-failed";
    pub const LEAD_RESUME_FAILED: &'static str = "resume-failed";
    pub const LEAD_TURN_ERROR: &'static str = "turn-error";

    /// Record that the lead needs a human, and say so. Persisting matters more
    /// than the event: the resume failures happen at daemon boot, before any UI
    /// has connected to hear about them.
    pub async fn flag_lead(&self, issue_id: i64, reason: &str) {
        if let Err(error) = self.store.set_lead_attention(issue_id, Some(reason)).await {
            eprintln!("[weftd] flag lead {issue_id} failed: {error:#}");
            return;
        }
        events::emit(
            "lead.attention",
            json!({ "issueId": issue_id, "reason": reason }),
        );
    }

    /// Starting or recovering the lead is what clears the flag — a retry that
    /// succeeds must not leave the issue looking broken.
    async fn clear_lead_attention(&self, issue_id: i64) {
        if let Err(error) = self.store.set_lead_attention(issue_id, None).await {
            eprintln!("[weftd] clear lead attention {issue_id} failed: {error:#}");
        }
    }

    pub async fn spawn_lead(&self, issue_id: i64) -> anyhow::Result<String> {
        // Checked out here so it never reaches the flag: a daemon that is not
        // live yet is a system state, not this issue failing. Flagging it would
        // mark every issue created during startup as broken.
        if !runtime::agents_allowed() {
            anyhow::bail!("daemon is not live; refusing to spawn agents");
        }
        match self.spawn_lead_inner(issue_id).await {
            Ok(thread_id) => {
                self.clear_lead_attention(issue_id).await;
                Ok(thread_id)
            }
            Err(error) => {
                eprintln!("[weftd] spawn lead {issue_id} failed: {error:#}");
                self.flag_lead(issue_id, Self::LEAD_START_FAILED).await;
                Err(error)
            }
        }
    }

    async fn spawn_lead_inner(&self, issue_id: i64) -> anyhow::Result<String> {
        // Repeated create responses and concurrent retries are idempotent.
        let gate = self.inject_lock.lock().await;
        let issue = self
            .store
            .get_issue(issue_id)
            .await?
            .ok_or(ApiError::not_found("issue", issue_id))?;
        if !issue.lead_codex_thread_id.is_empty() {
            return Ok(issue.lead_codex_thread_id);
        }
        let directions = self.store.list_directions(issue_id).await?;
        let repos = self.store.list_repos(issue.workspace_id).await?;
        let cwd = match repos.first() {
            Some(r) => r.path.clone(),
            None => self.home.to_string_lossy().to_string(),
        };

        let url = self.bus_url(issue.id, brief::LEAD_PARTY);
        let client = codex::client().await?;
        let result = client
            .request(
                "thread/start",
                codex::thread_start_params_configured(&cwd, "never", "read-only", Some(&url), false),
            )
            .await?;
        let thread_id = codex::thread_id_of(&result)
            .ok_or_else(|| anyhow::anyhow!("thread/start: no thread.id"))?;

        let rx = client.subscribe(&thread_id).await;
        tokio::spawn(watch(
            self.clone(),
            client.clone(),
            WatchTarget::Lead(issue.id),
            thread_id.clone(),
            rx,
        ));

        let pairs: Vec<(i64, String)> = directions.iter().map(|d| (d.id, d.name.clone())).collect();
        let repo_specs: Vec<(i64, String, String)> = repos
            .iter()
            .map(|repo| (repo.id, repo.name.clone(), repo.base_ref.clone()))
            .collect();
        let text = brief::lead_brief(&issue.title, &issue.kind, &pairs, &repo_specs, &url);
        let turn_id = match client.start_turn(&thread_id, &text).await {
            Ok(turn_id) => turn_id,
            Err(error) => {
                client.unsubscribe(&thread_id).await;
                return Err(error);
            }
        };
        client.set_active_turn(&thread_id, &turn_id).await;
        self.store.set_lead_thread(issue.id, &thread_id).await?;

        events::emit(
            "issue.updated",
            json!({ "id": issue.id, "leadCodexThreadId": thread_id }),
        );

        drop(gate);
        self.deliver(issue.id, brief::LEAD_PARTY).await;
        Ok(thread_id)
    }

    // ── injection ───────────────────────────────────────────────────────────

    /// Steer-or-start: deliver `text` into `thread_id`. Returns false when
    /// both paths errored or refused (the caller requeues/parks). Refuses
    /// up front while a foreign turn is flagged: `turn/start` against a
    /// human-driven busy thread returns Ok but never runs (the spike-
    /// documented silent drop), so attempting it would fabricate a phantom
    /// active turn. The TurnEnd watcher flushes what parked meanwhile.
    async fn inject(&self, client: &Client, thread_id: &str, text: &str) -> bool {
        if self.foreign_turn(thread_id).is_some() {
            return false;
        }
        if let Some(turn_id) = client.active_turn(thread_id).await {
            if client.steer_turn(thread_id, &turn_id, text).await.is_ok() {
                return true;
            }
        }
        match client.start_turn(thread_id, text).await {
            Ok(new_turn) => {
                client.set_active_turn(thread_id, &new_turn).await;
                // The server answers Ok even when it silently dropped the
                // start (busy with a foreign turn from another app-server
                // process — probe-verified). Only our own watcher's routed
                // turn/started proves the turn is really running.
                if await_turn_confirmation(
                    &self.confirmed_turns,
                    thread_id,
                    &new_turn,
                    TURN_CONFIRM_TIMEOUT,
                )
                .await
                {
                    return true;
                }
                // Dropped: clear the phantom so the next delivery doesn't
                // steer into a turn that never existed.
                client.clear_active_turn(thread_id).await;
                false
            }
            Err(_) => false,
        }
    }

    /// Deliver everything queued for `(issue_id, party)` into its thread.
    /// No thread yet (party not spawned) → the inbox stays put for
    /// `bus_read` and for the flush at spawn time.
    pub async fn deliver(&self, issue_id: i64, party: &str) {
        let thread_id = match self.thread_for(issue_id, party).await {
            Ok(Some(t)) => t,
            _ => return,
        };
        if self.foreign_turn(&thread_id).is_some() {
            // A human is mid-turn on this thread in Desktop — leave the
            // inbox intact (draining here would only requeue) and say so;
            // the TurnEnd watcher flushes the backlog when they finish.
            events::emit(
                "bus.parked",
                json!({ "issueId": issue_id, "party": party }),
            );
            return;
        }
        let _gate = self.inject_lock.lock().await;
        let msgs = self.bus.drain(issue_id, party);
        if msgs.is_empty() {
            return;
        }
        let text = join_envelopes(&msgs);
        let client = match codex::client().await {
            Ok(c) => c,
            Err(_) => {
                self.bus.requeue_front(issue_id, party, msgs);
                return;
            }
        };
        if self.inject(&client, &thread_id, &text).await {
            let ids: Vec<i64> = msgs.iter().map(|msg| msg.id).collect();
            if self.store.mark_bus_delivered(&ids).await.is_err() {
                // Injection already happened, so recovery may duplicate the
                // envelope; retaining it is safer than silently losing the
                // only durable copy before settlement.
                self.bus.requeue_front(issue_id, party, msgs);
                events::emit(
                    "bus.undelivered",
                    json!({ "issueId": issue_id, "party": party, "reason": "settlement-failed" }),
                );
                return;
            }
            if let Ok(dir_id) = party.parse::<i64>() {
                let _ = self.store.set_direction_status(dir_id, "working").await;
                events::emit(
                    "direction.updated",
                    json!({ "id": dir_id, "status": "working" }),
                );
            }
        } else {
            self.bus.requeue_front(issue_id, party, msgs);
            events::emit(
                "bus.undelivered",
                json!({ "issueId": issue_id, "party": party }),
            );
        }
    }

    /// Bus wake loop: every `bus_post` lands here for live delivery. A
    /// `Lagged` wake is harmless — the inbox persists and the next wake
    /// re-drains it.
    pub async fn run_bus_delivery(self) {
        let mut wakes = self.bus.subscribe_wake();
        loop {
            match wakes.recv().await {
                Ok((issue, party)) => self.deliver(issue, &party).await,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    }

    /// Rehydrate every durable, unsettled bus row before the listener starts.
    /// No wake is emitted here; [`flush_pending_bus`](Self::flush_pending_bus)
    /// runs after the loop and HTTP listener are ready.
    pub async fn restore_pending_bus(&self) -> anyhow::Result<usize> {
        let rows = self.store.pending_bus_messages().await?;
        let count = rows.len();
        for row in rows {
            self.bus.restore(
                row.issue_id,
                &row.to_party,
                Msg {
                    id: row.id,
                    from: row.from_party,
                    text: row.text,
                    kind: row.kind,
                    ts: row.ts,
                },
            );
        }
        Ok(count)
    }

    /// Attempt every restored inbox once. Failures remain queued and durable;
    /// a later post/wake or explicit `bus_read` retries them.
    pub async fn flush_pending_bus(&self) {
        for (issue_id, party) in self.bus.pending_parties() {
            self.deliver(issue_id, &party).await;
        }
    }

    /// Boot re-attach: subscribe watchers for every thread that outlived a
    /// daemon restart (threads persist in ~/.codex; watchers do not).
    /// Without this, post-restart TurnEnds never reach the kanban.
    /// Returns the number of re-attached threads.
    pub async fn reattach_all(&self) -> anyhow::Result<usize> {
        if !runtime::agents_allowed() {
            return Ok(0);
        }
        let client = codex::client().await?;
        let mut count = 0usize;
        for d in self.store.list_live_directions().await? {
            if client.is_subscribed(&d.codex_thread_id).await {
                continue;
            }
            if let Err(error) = client.resume_thread(&d.codex_thread_id).await {
                let _ = self
                    .store
                    .set_direction_attention(d.id, Some("thread-resume-failed"))
                    .await;
                events::emit(
                    "direction.updated",
                    json!({ "id": d.id, "attention": true, "reason": "thread-resume-failed" }),
                );
                eprintln!("[weftd] resume worker thread {} failed: {error:#}", d.id);
                continue;
            }
            let rx = client.subscribe(&d.codex_thread_id).await;
            tokio::spawn(watch(
                self.clone(),
                client.clone(),
                WatchTarget::Direction(d.id),
                d.codex_thread_id.clone(),
                rx,
            ));
            count += 1;
        }
        for issue in self.store.list_live_leads().await? {
            if client.is_subscribed(&issue.lead_codex_thread_id).await {
                continue;
            }
            if let Err(error) = client.resume_thread(&issue.lead_codex_thread_id).await {
                eprintln!("[weftd] resume lead thread {} failed: {error:#}", issue.id);
                self.flag_lead(issue.id, Self::LEAD_RESUME_FAILED).await;
                continue;
            }
            self.clear_lead_attention(issue.id).await;
            let rx = client.subscribe(&issue.lead_codex_thread_id).await;
            tokio::spawn(watch(
                self.clone(),
                client.clone(),
                WatchTarget::Lead(issue.id),
                issue.lead_codex_thread_id.clone(),
                rx,
            ));
            count += 1;
        }
        Ok(count)
    }

    // ── human input (the kanban UI's talk buttons) ────────────────────────

    /// Inject a human message into a direction's thread. Audit-logged as
    /// from `human`; on delivery failure the message is parked on the bus
    /// (one wake-driven retry, then `bus_read` fallback), never lost.
    pub async fn human_message_direction(
        &self,
        direction_id: i64,
        text: &str,
    ) -> anyhow::Result<()> {
        let direction = self
            .store
            .get_direction(direction_id)
            .await?
            .ok_or(ApiError::not_found("direction", direction_id))?;
        if direction.codex_thread_id.is_empty() {
            return Err(ApiError::conflict(
                "no_thread",
                format!("task {direction_id} has no Codex thread yet"),
            )
            .into());
        }
        let party = brief::direction_party(direction.id);
        let message_id = self
            .store
            .bus_append(direction.issue_id, "human", &party, text)
            .await?;
        self.human_message(direction.issue_id, &party, message_id, text)
            .await
    }

    /// Inject a human message into the issue's lead thread.
    pub async fn human_message_lead(&self, issue_id: i64, text: &str) -> anyhow::Result<()> {
        let issue = self
            .store
            .get_issue(issue_id)
            .await?
            .ok_or(ApiError::not_found("issue", issue_id))?;
        if issue.lead_codex_thread_id.is_empty() {
            return Err(ApiError::conflict("no_thread", "lead has no Codex thread yet").into());
        }
        let message_id = self
            .store
            .bus_append(issue.id, "human", brief::LEAD_PARTY, text)
            .await?;
        self.human_message(issue.id, brief::LEAD_PARTY, message_id, text)
            .await
    }

    async fn human_message(
        &self,
        issue_id: i64,
        party: &str,
        message_id: i64,
        text: &str,
    ) -> anyhow::Result<()> {
        if self.thread_for(issue_id, party).await?.is_none() {
            return Err(ApiError::conflict(
                "no_thread",
                format!("{party} has no Codex thread yet"),
            )
            .into());
        }
        let msg = Msg {
            id: message_id,
            from: "human".to_string(),
            text: text.to_string(),
            kind: "message".to_string(),
            ts: crate::store::now_unix(),
        };
        self.bus.post(issue_id, party, msg);
        // Also attempt synchronously so this API stays responsive. The wake
        // loop racing this call can drain the inbox only once.
        self.deliver(issue_id, party).await;
        Ok(())
    }

    /// Accept a worker result. Runtime states are event-derived, so the only
    /// human status transition is review -> done. A repeated acceptance is
    /// idempotent; a concurrent resume wins over a stale acceptance click.
    pub async fn complete_direction(&self, direction_id: i64) -> anyhow::Result<()> {
        let direction = self
            .store
            .get_direction(direction_id)
            .await?
            .ok_or(ApiError::not_found("direction", direction_id))?;
        if direction.status == "done" {
            return Ok(());
        }
        if direction.status != "review" {
            return Err(ApiError::conflict(
                "cannot_complete",
                format!(
                    "cannot complete task {direction_id} from status {:?}; expected review",
                    direction.status
                ),
            )
            .into());
        }
        if !self
            .store
            .complete_direction_if_review(direction_id)
            .await?
        {
            let current = self
                .store
                .get_direction(direction_id)
                .await?
                .ok_or(ApiError::not_found("direction", direction_id))?;
            if current.status == "done" {
                return Ok(());
            }
            return Err(ApiError::conflict(
                "cannot_complete",
                format!(
                    "cannot complete task {direction_id} from status {:?}; expected review",
                    current.status
                ),
            )
            .into());
        }
        events::emit(
            "direction.updated",
            json!({ "id": direction_id, "status": "done", "attention": false }),
        );
        Ok(())
    }

    pub async fn clear_direction_attention(&self, direction_id: i64) -> anyhow::Result<()> {
        self.store.set_direction_attention(direction_id, None).await?;
        events::emit(
            "direction.updated",
            json!({ "id": direction_id, "attention": false }),
        );
        Ok(())
    }

    // ── watcher ─────────────────────────────────────────────────────────────

    async fn on_turn_end(&self, target: WatchTarget, is_error: bool) {
        match target {
            WatchTarget::Direction(id) => {
                let status = status_after_turn_end();
                let _ = self.store.set_direction_status(id, status).await;
                if is_error {
                    // Keep an earlier, more specific reason (e.g. quota).
                    let flagged = self
                        .store
                        .get_direction(id)
                        .await
                        .ok()
                        .flatten()
                        .map(|d| d.attention != 0)
                        .unwrap_or(false);
                    if !flagged {
                        let _ = self
                            .store
                            .set_direction_attention(id, Some("turn failed"))
                            .await;
                    }
                }
                events::emit(
                    "direction.updated",
                    json!({ "id": id, "status": status, "turnError": is_error }),
                );
            }
            WatchTarget::Lead(issue_id) => {
                if is_error {
                    self.flag_lead(issue_id, Self::LEAD_TURN_ERROR).await;
                }
            }
        }
    }

    /// A turn ended on `thread_id`: if it was a FOREIGN turn (human drove),
    /// flush whatever parked on the bus while injection stood down. No flag
    /// → the turn was ours (or irrelevant) and normal delivery scheduling
    /// already applies. With an empty inbox the flush delivery is a no-op
    /// that never touches the app-server client.
    async fn end_foreign_turn(&self, target: WatchTarget, thread_id: &str) {
        if !self.clear_foreign_turn(thread_id) {
            return;
        }
        match target {
            WatchTarget::Direction(id) => {
                if let Ok(Some(d)) = self.store.get_direction(id).await {
                    let party = brief::direction_party(d.id);
                    self.deliver(d.issue_id, &party).await;
                }
            }
            WatchTarget::Lead(issue_id) => {
                self.deliver(issue_id, brief::LEAD_PARTY).await;
            }
        }
    }
}

/// Envelope-join a drained inbox into one turn input.
fn join_envelopes(msgs: &[Msg]) -> String {
    msgs.iter()
        .map(|m| brief::bus_envelope(&m.from, &m.text))
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Per-thread notification fold: turn completion → kanban state; anomalous
/// quota → attention flag. Blocking approval/elicitation requests are always
/// declined inside the app-server protocol client and never reach this layer.
async fn watch(
    orch: Orchestrator,
    client: Client,
    target: WatchTarget,
    thread_id: String,
    mut rx: UnboundedReceiver<ThreadMsg>,
) {
    while let Some(msg) = rx.recv().await {
        match msg {
            ThreadMsg::Event(ChatEvent::TurnEnd { is_error, .. }) => {
                client.clear_active_turn(&thread_id).await;
                orch.on_turn_end(target, is_error).await;
                orch.end_foreign_turn(target, &thread_id).await;
            }
            ThreadMsg::TurnStarted { turn_id } => {
                // Record unconditionally first: inject's silent-drop guard
                // needs the id even when this notification beat our own
                // set_active_turn bookkeeping.
                orch.note_turn_started(&thread_id, &turn_id);
                // A started turn whose id isn't our tracked active turn
                // means someone ELSE is driving (human takeover in
                // Desktop). Benign race: turn/started can beat our own
                // set_active_turn by a hair and briefly misflag OUR turn
                // as foreign — TurnEnd clears the flag and flushes, so
                // the cost is latency, never loss.
                let ours = client.active_turn(&thread_id).await;
                if ours.as_deref() != Some(turn_id.as_str()) {
                    orch.set_foreign_turn(&thread_id, &turn_id);
                    events::emit(
                        "thread.human-active",
                        json!({ "threadId": thread_id }),
                    );
                }
            }
            ThreadMsg::QuotaExceeded => {
                if let WatchTarget::Direction(id) = target {
                    let _ = orch
                        .store
                        .set_direction_attention(id, Some("quota exceeded"))
                        .await;
                    events::emit(
                        "direction.updated",
                        json!({ "id": id, "attention": true, "reason": "quota exceeded" }),
                    );
                }
            }
            _ => {}
        }
    }
}

/// Longest `forkedFromId` chain we will follow before giving up. A native fork
/// chain this deep is not something Codex produces; treating it as corrupt
/// metadata is safer than walking indefinitely.
const MAX_FORK_DEPTH: usize = 32;

/// Walk an unknown thread's `forkedFromId` chain until it reaches a thread we
/// already know, then bind the whole chain to that owner.
///
/// Split out of [`Orchestrator::resolve_thread_binding`] and parameterised over
/// the metadata read so the fail-closed guarantees — bounded depth, cycle
/// detection, id mismatch — are reachable from tests. Inside the orchestrator
/// the read goes to a live app-server, which `runtime::agents_allowed()` makes
/// unreachable off the daemon.
///
/// Fails closed: a cycle, an over-long chain, or metadata whose id disagrees
/// with the thread we asked for is an error, never a partially-adopted chain.
/// An unreadable thread is reported as unbound rather than guessed at.
async fn adopt_fork_chain<F, Fut>(
    store: &Store,
    thread_id: &str,
    read_thread: F,
) -> anyhow::Result<Option<ThreadBindingRow>>
where
    F: Fn(String) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<ThreadInfo>>,
{
    let mut cursor = thread_id.to_string();
    let mut pending = Vec::new();
    let mut seen = HashSet::new();
    let ancestor = loop {
        if !seen.insert(cursor.clone()) {
            anyhow::bail!("invalid Codex fork cycle at {cursor}");
        }
        if let Some(binding) = store.get_thread_binding(&cursor).await? {
            break Some(binding);
        }
        if pending.len() >= MAX_FORK_DEPTH {
            anyhow::bail!("invalid Codex fork chain exceeds {MAX_FORK_DEPTH} threads");
        }
        let info = match read_thread(cursor.clone()).await {
            Ok(info) => info,
            Err(_) => break None,
        };
        if info.id != cursor {
            anyhow::bail!("invalid Codex thread metadata for {cursor}");
        }
        let Some(parent_id) = info.forked_from_id.clone() else {
            break None;
        };
        pending.push(info);
        cursor = parent_id;
    };

    let Some(mut binding) = ancestor else {
        return Ok(None);
    };
    for info in pending.iter().rev() {
        let title = info.name.as_deref().unwrap_or(info.preview.as_str());
        binding = store
            .bind_thread_fork(&info.id, &binding.thread_id, title)
            .await?;
    }
    if !pending.is_empty() {
        events::emit(
            "thread.binding.updated",
            json!({
                "threadId": thread_id,
                "issueId": binding.issue_id,
                "directionId": binding.direction_id
            }),
        );
    }
    Ok(Some(binding))
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn fixture() -> (Orchestrator, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tmp");
        let store = Store::open(&dir.path().join("t.db")).await.expect("open");
        let orch = Orchestrator::new(
            store,
            BusRegistry::new(),
            "http://127.0.0.1:1".to_string(),
            dir.path().join("home"),
        );
        (orch, dir)
    }

    #[test]
    fn status_mapping() {
        assert_eq!(initial_status(), "working");
        assert_eq!(status_after_turn_end(), "review");
    }

    #[tokio::test]
    async fn thread_for_resolves_parties_and_blocks_cross_issue() {
        let (orch, _dir) = fixture().await;
        let ws = orch.store.create_workspace("W", "w").await.expect("ws");
        let repo = orch
            .store
            .add_repo(ws, "api", "/tmp/api", "main")
            .await
            .expect("repo");
        let i1 = orch.store.create_issue(ws, "one", "one").await.expect("i1");
        let i2 = orch.store.create_issue(ws, "two", "two").await.expect("i2");
        let d = orch
            .store
            .add_direction(i1, "backend", "backend", repo, "impl-only", "main", "", "")
            .await
            .expect("d");
        orch.store
            .set_direction_thread(d, "t-dir")
            .await
            .expect("thread");
        orch.store.set_lead_thread(i1, "t-lead").await.expect("lead");

        assert_eq!(
            orch.thread_for(i1, "lead").await.expect("resolve"),
            Some("t-lead".to_string())
        );
        let party = d.to_string();
        assert_eq!(
            orch.thread_for(i1, &party).await.expect("resolve"),
            Some("t-dir".to_string())
        );
        // Cross-issue reference resolves to None.
        assert_eq!(orch.thread_for(i2, &party).await.expect("resolve"), None);
        // Unknown / non-numeric parties resolve to None.
        assert_eq!(orch.thread_for(i1, "nobody").await.expect("resolve"), None);
        assert_eq!(orch.thread_for(i1, "999").await.expect("resolve"), None);
    }

    #[tokio::test]
    async fn thread_for_empty_means_not_spawned() {
        let (orch, _dir) = fixture().await;
        let ws = orch.store.create_workspace("W", "w").await.expect("ws");
        let issue = orch.store.create_issue(ws, "one", "one").await.expect("i");
        assert_eq!(orch.thread_for(issue, "lead").await.expect("resolve"), None);
    }

    #[tokio::test]
    async fn resolve_known_thread_binding_does_not_require_app_server() {
        let (orch, _dir) = fixture().await;
        let ws = orch.store.create_workspace("W", "w").await.expect("ws");
        let issue = orch.store.create_issue(ws, "one", "one").await.expect("i");
        orch.store
            .set_lead_thread(issue, "lead-primary")
            .await
            .expect("lead");

        let binding = orch
            .resolve_thread_binding("lead-primary")
            .await
            .expect("resolve")
            .expect("binding");
        assert_eq!(binding.issue_id, issue);
        assert_eq!(binding.direction_id, None);
        assert_eq!(binding.is_primary, 1);
    }

    fn thread_info(id: &str, parent: Option<&str>, name: &str) -> ThreadInfo {
        ThreadInfo {
            id: id.to_string(),
            forked_from_id: parent.map(str::to_string),
            name: Some(name.to_string()),
            preview: String::new(),
        }
    }

    /// Stand-in for `Client::read_thread`. Off the live daemon
    /// `runtime::agents_allowed()` is always false, so the real walk is
    /// unreachable — every guarantee below is tested through this instead.
    fn fake_reader(
        threads: Vec<ThreadInfo>,
    ) -> impl Fn(String) -> std::future::Ready<anyhow::Result<ThreadInfo>> {
        let map: HashMap<String, ThreadInfo> = threads
            .into_iter()
            .map(|info| (info.id.clone(), info))
            .collect();
        move |id: String| {
            std::future::ready(
                map.get(&id)
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("unreadable thread {id}")),
            )
        }
    }

    async fn issue_with_lead(orch: &Orchestrator, thread: &str) -> i64 {
        let ws = orch.store.create_workspace("W", "w").await.expect("ws");
        let issue = orch.store.create_issue(ws, "one", "one").await.expect("i");
        orch.store.set_lead_thread(issue, thread).await.expect("lead");
        issue
    }

    #[tokio::test]
    async fn a_nested_fork_chain_is_adopted_by_its_root_owner() {
        let (orch, _dir) = fixture().await;
        let issue = issue_with_lead(&orch, "lead-primary").await;
        // grandchild -> child -> lead-primary
        let reader = fake_reader(vec![
            thread_info("grandchild", Some("child"), "Grandchild"),
            thread_info("child", Some("lead-primary"), "Child"),
        ]);

        let binding = adopt_fork_chain(&orch.store, "grandchild", reader)
            .await
            .expect("walk")
            .expect("binding");

        assert_eq!(binding.issue_id, issue);
        assert_eq!(binding.thread_id, "grandchild");
        assert_eq!(binding.is_primary, 0, "a fork must not become primary");
        assert_eq!(binding.parent_thread_id, "child");
        assert_eq!(binding.root_thread_id, "lead-primary");

        // The intermediate hop is written back too, not just the thread asked for.
        let middle = orch
            .store
            .get_thread_binding("child")
            .await
            .expect("get")
            .expect("child bound");
        assert_eq!(middle.issue_id, issue);
        assert_eq!(middle.is_primary, 0);

        // The primary is untouched by adoption.
        let lead = orch
            .store
            .get_thread_binding("lead-primary")
            .await
            .expect("get")
            .expect("lead bound");
        assert_eq!(lead.is_primary, 1);
    }

    #[tokio::test]
    async fn a_fork_cycle_fails_closed_without_binding_anything() {
        let (orch, _dir) = fixture().await;
        issue_with_lead(&orch, "lead-primary").await;
        // a -> b -> a, never reaching a known thread.
        let reader = fake_reader(vec![
            thread_info("a", Some("b"), "A"),
            thread_info("b", Some("a"), "B"),
        ]);

        let error = adopt_fork_chain(&orch.store, "a", reader)
            .await
            .expect_err("a cycle must fail, not resolve");
        assert!(error.to_string().contains("cycle"), "got: {error}");

        for id in ["a", "b"] {
            assert!(
                orch.store.get_thread_binding(id).await.expect("get").is_none(),
                "{id} must not be bound by a failed walk"
            );
        }
    }

    #[tokio::test]
    async fn an_over_long_fork_chain_fails_closed() {
        let (orch, _dir) = fixture().await;
        issue_with_lead(&orch, "lead-primary").await;
        // Longer than MAX_FORK_DEPTH and never reaching the known lead.
        let depth = MAX_FORK_DEPTH + 8;
        let chain: Vec<ThreadInfo> = (0..depth)
            .map(|i| thread_info(&format!("t{i}"), Some(&format!("t{}", i + 1)), "T"))
            .collect();

        let error = adopt_fork_chain(&orch.store, "t0", fake_reader(chain))
            .await
            .expect_err("an unbounded chain must fail");
        assert!(error.to_string().contains("exceeds"), "got: {error}");
        assert!(orch
            .store
            .get_thread_binding("t0")
            .await
            .expect("get")
            .is_none());
    }

    #[tokio::test]
    async fn metadata_for_the_wrong_thread_fails_closed() {
        let (orch, _dir) = fixture().await;
        issue_with_lead(&orch, "lead-primary").await;
        // Asked about "child", the host answered about a different thread. Trusting
        // that reply would bind the wrong conversation to the issue.
        let error = adopt_fork_chain(&orch.store, "child", |_| {
            std::future::ready(Ok(thread_info(
                "someone-else",
                Some("lead-primary"),
                "Impostor",
            )))
        })
        .await
        .expect_err("mismatched metadata must fail");

        assert!(error.to_string().contains("metadata"), "got: {error}");
        assert!(orch
            .store
            .get_thread_binding("child")
            .await
            .expect("get")
            .is_none());
    }

    #[tokio::test]
    async fn an_unrelated_or_unreadable_thread_stays_unbound() {
        let (orch, _dir) = fixture().await;
        issue_with_lead(&orch, "lead-primary").await;

        // A plain Codex chat: readable, but not a fork of anything.
        let plain = adopt_fork_chain(
            &orch.store,
            "plain",
            fake_reader(vec![thread_info("plain", None, "Plain")]),
        )
        .await
        .expect("walk");
        assert!(plain.is_none(), "a non-fork must not be adopted");

        // Unreadable metadata is reported as unbound rather than guessed at.
        let unreadable = adopt_fork_chain(&orch.store, "ghost", fake_reader(vec![]))
            .await
            .expect("an unreadable thread is not an error");
        assert!(unreadable.is_none());
        assert!(orch
            .store
            .get_thread_binding("plain")
            .await
            .expect("get")
            .is_none());
    }

    #[tokio::test]
    async fn restore_pending_bus_rehydrates_durable_rows() {
        let (orch, _dir) = fixture().await;
        orch.store
            .bus_append(7, "3", "lead", "finished")
            .await
            .expect("append");
        assert_eq!(orch.restore_pending_bus().await.expect("restore"), 1);
        assert_eq!(orch.bus.pending_parties(), vec![(7, "lead".to_string())]);
        let restored = orch.bus.drain(7, "lead");
        assert_eq!(restored.len(), 1);
        assert_eq!(restored[0].from, "3");
        assert_eq!(restored[0].text, "finished");
    }

    #[test]
    fn join_envelopes_wraps_each_sender() {
        let msgs = vec![
            Msg {
                id: 1,
                from: "lead".to_string(),
                text: "hi".to_string(),
                kind: "message".to_string(),
                ts: "0".to_string(),
            },
            Msg {
                id: 2,
                from: "3".to_string(),
                text: "report".to_string(),
                kind: "message".to_string(),
                ts: "1".to_string(),
            },
        ];
        let text = join_envelopes(&msgs);
        assert!(text.contains("[bus message from lead]\nhi"));
        assert!(text.contains("[bus message from 3]\nreport"));
        assert!(text.contains("\n\n"));
    }

    #[tokio::test]
    async fn complete_direction_requires_review_and_is_idempotent() {
        let (orch, _dir) = fixture().await;
        let ws = orch.store.create_workspace("W", "w").await.expect("ws");
        let repo = orch
            .store
            .add_repo(ws, "api", "/tmp/api", "main")
            .await
            .expect("repo");
        let issue = orch.store.create_issue(ws, "one", "one").await.expect("i");
        let d = orch
            .store
            .add_direction(issue, "backend", "backend", repo, "impl-only", "main", "", "")
            .await
            .expect("d");
        assert!(orch.complete_direction(d).await.is_err());
        orch.store
            .set_direction_status(d, "review")
            .await
            .expect("review");
        orch.store
            .set_direction_attention(d, Some("turn failed"))
            .await
            .expect("attention");
        orch.complete_direction(d).await.expect("done");
        orch.complete_direction(d).await.expect("idempotent");
        let row = orch.store.get_direction(d).await.expect("get").expect("some");
        assert_eq!(row.status, "done");
        assert_eq!(row.attention, 0);
        assert!(row.attention_reason.is_empty());
    }

    /// Seed a direction WITH a Codex thread id, so delivery paths resolve.
    async fn seeded_direction(orch: &Orchestrator) -> (i64, i64) {
        let ws = orch.store.create_workspace("W", "w").await.expect("ws");
        let repo = orch
            .store
            .add_repo(ws, "api", "/tmp/api", "main")
            .await
            .expect("repo");
        let issue = orch.store.create_issue(ws, "one", "one").await.expect("i");
        let d = orch
            .store
            .add_direction(issue, "backend", "backend", repo, "impl-only", "main", "", "")
            .await
            .expect("d");
        orch.store
            .set_direction_thread(d, "t-dir")
            .await
            .expect("thread");
        (issue, d)
    }

    #[tokio::test]
    async fn dispatch_reuses_an_existing_worker_thread() {
        let (orch, _dir) = fixture().await;
        let (_issue, direction) = seeded_direction(&orch).await;
        let thread = orch
            .dispatch_direction(direction)
            .await
            .expect("idempotent dispatch");
        assert_eq!(thread, "t-dir");
    }

    fn parked_msg() -> Msg {
        Msg {
            id: 1,
            from: "lead".to_string(),
            text: "held".to_string(),
            kind: "message".to_string(),
            ts: "0".to_string(),
        }
    }

    #[tokio::test]
    async fn foreign_turn_flag_roundtrip() {
        let (orch, _dir) = fixture().await;
        assert!(orch.foreign_turn("t1").is_none());
        assert!(!orch.clear_foreign_turn("t1"));
        orch.set_foreign_turn("t1", "turn_x");
        assert_eq!(orch.foreign_turn("t1").as_deref(), Some("turn_x"));
        assert!(orch.clear_foreign_turn("t1"));
        assert!(orch.foreign_turn("t1").is_none());
    }

    #[tokio::test]
    async fn deliver_parks_while_human_drives() {
        let (orch, _dir) = fixture().await;
        let (issue, d) = seeded_direction(&orch).await;
        let party = brief::direction_party(d);
        orch.bus.post(issue, &party, parked_msg());
        orch.set_foreign_turn("t-dir", "turn-human");
        orch.deliver(issue, &party).await;
        // Parked delivery must NOT have drained the inbox — the message
        // survives for the TurnEnd flush.
        let held = orch.bus.drain(issue, &party);
        assert_eq!(held.len(), 1);
        assert_eq!(held[0].text, "held");
    }

    #[tokio::test]
    async fn end_foreign_turn_flushes_only_when_flagged() {
        let (orch, _dir) = fixture().await;
        let (issue, d) = seeded_direction(&orch).await;
        // No flag → early return; an empty inbox would have made a flush
        // harmless anyway, but nothing should even attempt delivery.
        orch.end_foreign_turn(WatchTarget::Direction(d), "t-dir").await;
        assert!(orch.foreign_turn("t-dir").is_none());
        // Flagged + empty inbox → flag clears; the flush delivery drains
        // nothing and returns before ever touching the app-server client.
        orch.set_foreign_turn("t-dir", "turn-human");
        orch.end_foreign_turn(WatchTarget::Direction(d), "t-dir").await;
        assert!(orch.foreign_turn("t-dir").is_none());
        assert!(orch.bus.drain(issue, &brief::direction_party(d)).is_empty());
    }

    #[tokio::test]
    async fn turn_confirmation_waits_for_watcher_record() {
        // No record within the window → silent drop detected.
        let empty = StdMutex::new(HashMap::new());
        assert!(
            !await_turn_confirmation(&empty, "t", "turn-1", Duration::from_millis(250)).await
        );
        // A record landing mid-wait (watcher routed turn/started) confirms.
        let shared = Arc::new(StdMutex::new(HashMap::new()));
        let writer = shared.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let mut g = writer.lock().expect("lock");
            g.insert("t".to_string(), "turn-2".to_string());
        });
        assert!(await_turn_confirmation(&shared, "t", "turn-2", Duration::from_secs(2)).await);
        // A different turn id on the same thread is NOT a confirmation.
        assert!(
            !await_turn_confirmation(&shared, "t", "turn-3", Duration::from_millis(150)).await
        );
    }

    #[tokio::test]
    async fn note_turn_started_feeds_confirmation() {
        let (orch, _dir) = fixture().await;
        assert!(
            !await_turn_confirmation(&orch.confirmed_turns, "t", "turn-9", Duration::from_millis(120)).await
        );
        orch.note_turn_started("t", "turn-9");
        assert!(
            await_turn_confirmation(&orch.confirmed_turns, "t", "turn-9", Duration::from_millis(120)).await
        );
    }
}
