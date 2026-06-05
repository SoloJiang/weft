# Thread Bus v1c (coordinator wake) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close the coordination loop: when a bus message is posted to a direction, automatically wake that direction's running agent to read its inbox (rate-limited), so coordination is near-realtime instead of poll-on-next-turn.

**Architecture:** `BusRegistry` emits a `Wake { thread, dir }` on every `post`/`broadcast` through a channel set at startup. A `coordinator` async task receives wakes and asks `PtyState` to inject a one-line "check your inbox" prompt into that direction's live session, rate-limited per direction. We rely on the agent TUIs queueing mid-turn input (so the wake runs after the current turn) rather than fragile idle detection.

**Tech Stack:** Rust/Tauri, tokio mpsc, the v1a/v1b bus. No new deps.

---

## Reference
- Spec: `docs/superpowers/specs/2026-06-05-thread-bus-coordination-design.md` (§ coordinator wake; the honest poll-vs-push constraint).
- v1a/v1b (committed): `BusRegistry { post, broadcast, inbox, log, join, ... }`; `Msg`; the bus runs at startup; `BusRegistry`/`BusBase`/`PtyState`/`Db` are Tauri-managed. `pty::spawn(app, tool, inject_args, cwd, resume_id, session_id, db)` builds `Active { child, writer, master, alive }` stored in `PtyState.sessions: Mutex<HashMap<i32, Active>>`. `open_session_impl` has `direction_id`; `resume_impl` has `s.direction_id`.
- Lint: `#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::panic))]` — new code must not unwrap/expect/panic. Mutex locks use `.lock().unwrap_or_else(|e| e.into_inner())`.

## Scope
**In (v1c):** wake channel on the registry; `Active` learns its `direction_id`; `PtyState::wake_direction`; coordinator task + startup wiring; live e2e (A posts → B's session receives the wake prompt).
**Out (v1d):** passive `.thread/` + PLAN.md layer; precise idle detection; focus-aware suppression.

## File structure
```
src-tauri/src/bus/state.rs    # MODIFY: optional wake sender; emit Wake on post/broadcast (+ test)
src-tauri/src/bus/mod.rs      # MODIFY: pub use Wake
src-tauri/src/coordinator.rs  # CREATE: the wake-consuming task
src-tauri/src/pty.rs          # MODIFY: Active.direction_id; PtyState::wake_direction; spawn carries direction_id
src-tauri/src/lib.rs          # MODIFY: mod coordinator; create channel; set sender; spawn task
```

## Shared types
- `Wake { thread: i32, dir: String }` — `dir` is a direction id as a string (the bus identity). The human ("you") never joins as a member, so wakes only ever target real numeric direction ids.
- Wake prompt: `"You have new messages on the thread bus. Call the bus_inbox tool to read them.\r"`.
- Rate limit: at most one wake per direction per 8 seconds.

---

## Task 1: BusRegistry emits Wake on post/broadcast

**Files:** Modify `src-tauri/src/bus/state.rs`, `src-tauri/src/bus/mod.rs`.

- [ ] **Step 1: Add the `Wake` type + an optional sender to the registry**

In `src-tauri/src/bus/state.rs`, add near the top (after the imports):
```rust
use std::sync::mpsc::Sender;

/// Emitted when a direction should be woken to read its inbox.
#[derive(Clone, Debug)]
pub struct Wake {
    pub thread: i32,
    pub dir: String,
}
```
Add a sender field to `BusRegistry` (it currently is `{ inner: Arc<Mutex<HashMap<i32, ThreadBus>>> }`):
```rust
#[derive(Default, Clone)]
pub struct BusRegistry {
    inner: Arc<Mutex<HashMap<i32, ThreadBus>>>,
    wake: Arc<Mutex<Option<Sender<Wake>>>>,
}
```
Add a setter + a private emit helper (inside `impl BusRegistry`):
```rust
    /// Install the channel the coordinator listens on (called once at startup).
    pub fn set_wake_sender(&self, tx: Sender<Wake>) {
        *self.wake.lock().unwrap_or_else(|e| e.into_inner()) = Some(tx);
    }

    fn emit_wake(&self, thread: i32, dir: &str) {
        if let Some(tx) = self.wake.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            let _ = tx.send(Wake { thread, dir: dir.to_string() });
        }
    }
```

- [ ] **Step 2: Emit on post + broadcast**

In `post`, after the message is pushed to the inbox, add `self.emit_wake(thread, to);` — but note `post` currently holds the `inner` lock via `g`; emit_wake locks `wake` (a different mutex), so call it AFTER the `g` guard is dropped. Restructure `post` so the inner-lock block ends, then emit:
```rust
    pub fn post(&self, thread: i32, from: &str, to: &str, text: &str, kind: &str) {
        let m = Msg {
            from: from.to_string(),
            to: to.to_string(),
            text: text.to_string(),
            ts: now(),
            kind: kind.to_string(),
        };
        {
            let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            let bus = g.entry(thread).or_default();
            bus.log.push(m.clone());
            bus.inboxes.entry(to.to_string()).or_default().push(m);
        }
        self.emit_wake(thread, to);
    }
```
In `broadcast`, collect the `targets` (already computed), and after the inner-lock block, emit a wake for each:
```rust
    pub fn broadcast(&self, thread: i32, from: &str, text: &str, kind: &str) {
        let m = Msg {
            from: from.to_string(),
            to: "*".to_string(),
            text: text.to_string(),
            ts: now(),
            kind: kind.to_string(),
        };
        let targets: Vec<String> = {
            let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            let bus = g.entry(thread).or_default();
            let targets: Vec<String> = bus
                .members
                .iter()
                .filter(|d| d.as_str() != from)
                .cloned()
                .collect();
            bus.log.push(m.clone());
            for d in &targets {
                bus.inboxes.entry(d.clone()).or_default().push(m.clone());
            }
            targets
        };
        for d in targets {
            self.emit_wake(thread, &d);
        }
    }
```
(Replace the existing `post`/`broadcast` bodies with these. Behavior is identical to v1a plus the wake emit; the v1a `bus::state` tests still pass because they don't set a sender — `emit_wake` is a no-op when none is set.)

- [ ] **Step 3: Re-export `Wake`**

In `src-tauri/src/bus/mod.rs`, change `pub use state::{BusRegistry, Msg};` to:
```rust
pub use state::{BusRegistry, Msg, Wake};
```

- [ ] **Step 4: Add a test that a wake is emitted on post**

Append to the `#[cfg(test)] mod tests` in `state.rs`:
```rust
    #[test]
    fn post_emits_wake() {
        let (tx, rx) = std::sync::mpsc::channel();
        let r = BusRegistry::new();
        r.set_wake_sender(tx);
        r.join(1, "10");
        r.post(1, "20", "10", "hi", "message");
        let w = rx.recv_timeout(std::time::Duration::from_secs(1)).unwrap();
        assert_eq!(w.thread, 1);
        assert_eq!(w.dir, "10");
    }
```

- [ ] **Step 5: Verify + commit**

Run: `cd /Users/solojiang/workspace/weft/src-tauri && cargo test bus::state 2>&1 | tail -12 && cargo clippy --lib 2>&1 | tail -3`
Expected: all `bus::state` tests pass incl. `post_emits_wake`; clippy clean (no new unwrap in non-test code).
```bash
cd /Users/solojiang/workspace/weft
git add src-tauri/src/bus/state.rs src-tauri/src/bus/mod.rs
git commit -m "feat(bus): emit Wake on post/broadcast via an optional channel"
```
End every commit in this plan with: `Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>`

---

## Task 2: PtyState knows each session's direction + can wake it

**Files:** Modify `src-tauri/src/pty.rs`.

- [ ] **Step 1: Add `direction_id` to `Active` and a `wake_direction` method**

In `src-tauri/src/pty.rs`, add a field to `Active`:
```rust
struct Active {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    writer: Box<dyn Write + Send>,
    master: Box<dyn portable_pty::MasterPty + Send>,
    alive: Arc<AtomicBool>,
    direction_id: i32,
}
```
Add a method to `PtyState` (after the struct, in an `impl PtyState` block — create one if absent):
```rust
impl PtyState {
    /// Write `data` to the live session of `direction_id`, if any. Returns true
    /// if a session was found and written to.
    pub fn wake_direction(&self, direction_id: i32, data: &str) -> bool {
        let mut g = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        for a in g.values_mut() {
            if a.direction_id == direction_id {
                let _ = a.writer.write_all(data.as_bytes());
                let _ = a.writer.flush();
                return true;
            }
        }
        false
    }
}
```

- [ ] **Step 2: Thread `direction_id` into `spawn` and set it on `Active`**

Change `spawn`'s signature to take `direction_id: i32` (add it right after `tool`):
```rust
fn spawn(
    app: &AppHandle,
    tool: &str,
    direction_id: i32,
    inject_args: &[String],
    cwd: &PathBuf,
    resume_id: Option<&str>,
    session_id: i32,
    db: Db,
) -> Result<Active> {
```
At the end of `spawn`, set it on the returned `Active`:
```rust
    Ok(Active {
        child,
        writer,
        master: pair.master,
        alive,
        direction_id,
    })
```
Update both call sites:
- In `open_session_impl`: `spawn(&app, &dir.tool, direction_id, &inj.args, &cwd, None, sess.id, db.clone())`.
- In `resume_impl`: `spawn(&app, &s.tool, s.direction_id, &inj.args, &cwd, Some(&native), session_id, db.clone())`.

- [ ] **Step 3: Verify it compiles + no regressions**

Run: `cd /Users/solojiang/workspace/weft/src-tauri && cargo build 2>&1 | tail -6 && cargo test --lib 2>&1 | tail -4 && cargo clippy --lib 2>&1 | tail -3`
Expected: `Finished`; lib tests pass; clippy clean.

- [ ] **Step 4: Commit**
```bash
cd /Users/solojiang/workspace/weft
git add src-tauri/src/pty.rs
git commit -m "feat(pty): track each session's direction_id; PtyState::wake_direction"
```

---

## Task 3: The coordinator task

**Files:** Create `src-tauri/src/coordinator.rs`; modify `src-tauri/src/lib.rs`.

- [ ] **Step 1: Write `src-tauri/src/coordinator.rs`**

```rust
//! Consumes bus Wake events and nudges the target direction's live session to
//! read its inbox. Rate-limited per direction. Relies on the agent TUIs queueing
//! mid-turn input (the wake runs after the current turn) rather than fragile idle
//! detection — this is the honest "push" half of bus + coordinator = near-realtime.

use crate::bus::Wake;
use crate::pty::PtyState;
use std::collections::HashMap;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Manager};

const WAKE_PROMPT: &str =
    "You have new messages on the thread bus. Call the bus_inbox tool to read them.\r";
const RATE_LIMIT: Duration = Duration::from_secs(8);

/// Run the coordinator loop on a dedicated OS thread (the mpsc Receiver is
/// blocking). `app` provides access to the managed `PtyState`.
pub fn run(app: AppHandle, rx: Receiver<Wake>) {
    std::thread::spawn(move || {
        let mut last: HashMap<i32, Instant> = HashMap::new();
        while let Ok(w) = rx.recv() {
            // The bus identity is a direction id as a string; ignore non-numeric
            // targets (e.g. a human "you" never registers as a member anyway).
            let Ok(dir) = w.dir.parse::<i32>() else {
                continue;
            };
            let now = Instant::now();
            if let Some(t) = last.get(&dir) {
                if now.duration_since(*t) < RATE_LIMIT {
                    continue; // rate-limited: don't spam the agent
                }
            }
            let Some(state) = app.try_state::<PtyState>() else {
                continue;
            };
            if state.wake_direction(dir, WAKE_PROMPT) {
                last.insert(dir, now);
            }
        }
    });
}
```

- [ ] **Step 2: Wire it at startup in `src-tauri/src/lib.rs`**

Add `mod coordinator;` alongside the other `mod` lines.

In `run()`, after the bus is created and BEFORE building the Tauri app, create the channel and set the sender:
```rust
    // Wire the coordinator: bus wakes -> nudge the target direction's session.
    let (wake_tx, wake_rx) = std::sync::mpsc::channel::<bus::Wake>();
    bus.set_wake_sender(wake_tx);
```
The coordinator needs an `AppHandle`, which only exists after the app is built — so start it in Tauri's `setup` hook. Change the builder chain to add a `.setup(...)` that moves `wake_rx` in and starts the coordinator:
```rust
    builder
        .manage(db)
        .manage(pty::PtyState::default())
        .manage(bus)
        .manage(BusBase(bus_base))
        .setup(move |app| {
            coordinator::run(app.handle().clone(), wake_rx);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            // ... unchanged list ...
        ])
        .run(tauri::generate_context!())
        .unwrap_or_else(|e| fatal("running tauri application", e));
```
(Keep the existing `.manage(...)`, `invoke_handler`, and the `#[cfg(debug_assertions)]` mcp_bridge block; only add the `.setup(...)`. `wake_rx` is `Send` and moved once into the closure.)

- [ ] **Step 3: Verify it compiles + clippy**

Run: `cd /Users/solojiang/workspace/weft/src-tauri && cargo build 2>&1 | tail -6 && cargo clippy --lib 2>&1 | tail -3`
Expected: `Finished`; clippy clean.

- [ ] **Step 4: Commit**
```bash
cd /Users/solojiang/workspace/weft
git add src-tauri/src/coordinator.rs src-tauri/src/lib.rs
git commit -m "feat(coordinator): wake a direction's session when it gets a bus message"
```

---

## Task 4: Live verification

**Files:** none (verification).

- [ ] **Step 1: Launch the app (isolated home), connect the bridge, seed a thread with 2 directions (claude + codex), open the claude direction's session and drive it past its trust gate so it is RUNNING.**

(Same harness as v1b. Capture the `[weft] thread bus on <base>` URL.)

- [ ] **Step 2: Trigger a wake and confirm the session received it**

From Bash, `curl` a `bus_post` to the claude direction's id from a fake direction: `POST <base>/bus/<thread>/<otherDir>/mcp` `bus_post {to:"<claudeDir>", text:"need the API shape"}` (initialize the fake dir first so it's a member, then post). The coordinator should write the wake prompt into the claude session's PTY.

Confirm by reading the claude session's xterm (`webview_execute_js` on `.xterm-rows`): the input line should now contain the wake prompt "You have new messages on the thread bus..." (queued or submitted). Alternatively, instruct claude to actually call `bus_inbox` and confirm via `curl` that the inbox is drained by the agent.

- [ ] **Step 3: Confirm rate-limiting**

Post two messages within 8s; confirm the wake prompt is injected at most once in that window (read the xterm — only one wake line within the rate-limit window).

- [ ] **Step 4: Record + commit**

Add a "✅ v1c 实测结论" line to `docs/superpowers/specs/2026-06-05-thread-bus-coordination-design.md`, then:
```bash
cd /Users/solojiang/workspace/weft
git add docs/superpowers/specs/2026-06-05-thread-bus-coordination-design.md
git commit -m "docs(thread-bus): record v1c coordinator-wake live verification"
```

---

## Self-review checklist
- **Spec coverage:** bus emits wakes (T1); session is reachable by direction + injectable (T2); coordinator consumes wakes + injects, rate-limited, relying on TUI queueing (T3); live e2e + rate-limit check (T4). Passive `.thread/` layer + precise idle detection are explicitly **v1d** (out of scope, documented in the spec's honest-constraint section).
- **Placeholder scan:** none — real code/commands throughout.
- **Type consistency:** `Wake{thread:i32, dir:String}`, `BusRegistry::set_wake_sender(Sender<Wake>)`, `Active{...,direction_id:i32}`, `PtyState::wake_direction(direction_id:i32, data:&str)->bool`, `spawn(app, tool, direction_id, inject_args, cwd, resume_id, session_id, db)`, `coordinator::run(AppHandle, Receiver<Wake>)` — consistent across tasks.

## Notes for the executor
- Locks use `.lock().unwrap_or_else(|e| e.into_inner())` (the no-panic rule). The coordinator's mpsc `Receiver` is blocking, so it runs on its own OS thread (Step T3 uses `std::thread::spawn`), not a tokio task.
- Don't add idle detection here. Rate-limiting + TUI input-queueing is the v1c contract; precise idle/focus suppression is v1d.
- The human ("you") posting also emits wakes for real member directions, so human messages auto-deliver to agents too — desirable.
- After T4, the natural next step (v1d) is the passive `.thread/` + PLAN.md layer and/or focus-aware wake suppression.
