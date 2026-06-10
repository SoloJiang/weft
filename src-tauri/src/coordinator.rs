//! Consumes bus Wake events and nudges the target direction's live session to
//! read its inbox. Rate-limited per direction. A busy engine queues the nudge
//! for after the current turn rather than fragile idle detection — this is the
//! honest "push" half of bus + coordinator = near-realtime.

use crate::bus::{Wake, HUMAN};
use std::collections::HashMap;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager};

const WAKE_PROMPT: &str =
    "You have new messages on the thread bus. Call the bus_inbox tool to read them.";
const RATE_LIMIT: Duration = Duration::from_secs(8);

/// Run the coordinator loop on a dedicated OS thread (the mpsc Receiver is
/// blocking).
pub fn run(app: AppHandle, rx: Receiver<Wake>) {
    std::thread::spawn(move || {
        let mut last: HashMap<i32, Instant> = HashMap::new();
        while let Ok(w) = rx.recv() {
            // A wake addressed to the human means an agent asked a question:
            // nudge the UI to refresh its Needs-you surface, don't touch a PTY.
            if w.dir == HUMAN {
                let _ = app.emit("needs-you://changed", w.thread);
                continue;
            }
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
            // Chat-engine worker: deliver the wake as an invisible nudge (no
            // timeline row); a busy engine queues it for after the turn.
            last.insert(dir, now);
            let app2 = app.clone();
            tauri::async_runtime::spawn(async move {
                let Some(db) = app2.try_state::<crate::store::Db>() else { return };
                let db = crate::store::Db(db.0.clone());
                let Ok(Some(s)) = crate::store::repo::latest_session_for_direction(&db, dir).await
                else {
                    return;
                };
                let Some(eng) = app2
                    .state::<crate::lead_chat::engine::LeadChatState>()
                    .get(s.id as i64)
                else {
                    return;
                };
                let _ = crate::lead_chat::engine::nudge(&app2, &db, &eng, WAKE_PROMPT).await;
            });
        }
    });
}
