//! Durable transcript trail for resolved Asks.
//!
//! Permission cards (the Ask Bridge) and human questions (the bus) close the
//! loop by vanishing from their dock once answered, leaving no record in the
//! transcript where the human acted. This consumer turns each *resolution
//! event* into a persisted `settled` lead_message so that record survives.
//!
//! Driving it off the registries' events (not the answer command) is what makes
//! it complete: every answer path — desktop, remote `weft_global`, IM — resolves
//! through `AskRegistry::answer` / `BusRegistry::answer_ask`, which emit these
//! events, and a broad grant (`always` / `full`) emits one `Resolved` per ask it
//! sweeps up. So all of those leave a row, uniformly, with no per-call-site code.
//!
//! Each row is routed to the transcript where the human answered: a worker
//! question/permission lands in that worker's session timeline; a lead/planning
//! ask lands in the lead console (session_id NULL).

use crate::ask::{AskEvent, AskRegistry};
use crate::bus::state::HumanAskEvent;
use crate::bus::BusRegistry;
use crate::store::{repo, Db};
use tauri::{AppHandle, Emitter, Manager};

/// Install the trail consumer's notifiers and spawn its event loop. Called once
/// at startup, independent of (and in addition to) the IM bridge's notifiers.
pub fn spawn(app: AppHandle) {
    let (ask_tx, mut ask_rx) = tokio::sync::mpsc::unbounded_channel::<AskEvent>();
    let (hum_tx, mut hum_rx) = tokio::sync::mpsc::unbounded_channel::<HumanAskEvent>();
    app.state::<AskRegistry>().set_trail_notifier(ask_tx);
    app.state::<BusRegistry>().set_ask_trail_notifier(hum_tx);
    let db = app.state::<Db>().inner().clone();

    tauri::async_runtime::spawn(async move {
        loop {
            tokio::select! {
                ev = ask_rx.recv() => match ev {
                    // Permission decided (allow / deny / always / full). Each
                    // ask covered by a broad grant arrives as its own event.
                    Some(AskEvent::Resolved { ask, answer }) => {
                        let content = serde_json::json!({
                            "variant": "permission",
                            "summary": ask.summary,
                            "answer": answer.as_str(),
                        })
                        .to_string();
                        record(&app, &db, ask.thread, &ask.dir, content).await;
                    }
                    // Opened: still pending. Cancelled: timed out, not answered.
                    Some(_) => {}
                    None => break,
                },
                ev = hum_rx.recv() => match ev {
                    Some(HumanAskEvent::Answered { thread, from, question, text, .. }) => {
                        let content = serde_json::json!({
                            "variant": "ask",
                            "text": question,
                            "answer": text,
                        })
                        .to_string();
                        record(&app, &db, thread, &from, content).await;
                    }
                    Some(_) => {}
                    None => break,
                },
            }
        }
    });
}

/// Persist one settled row into the asking direction's transcript and push it
/// live. Best-effort: a failed insert never breaks the (already completed) answer.
async fn record(app: &AppHandle, db: &Db, thread_id: i32, dir: &str, content: String) {
    let session_id = session_for_dir(db, dir).await;
    let turn = repo::next_turn_id(db, thread_id)
        .await
        .unwrap_or(1)
        .saturating_sub(1)
        .max(1);
    let Ok(m) = repo::insert_lead_message(
        db, thread_id, session_id, turn, "system", "settled", &content, "complete",
    )
    .await
    else {
        return;
    };
    let _ = app.emit(
        crate::lead_chat::engine::EVENT,
        crate::lead_chat::engine::Push::Message {
            thread_id,
            message: m,
        },
    );
}

/// Resolve an asking direction id (as carried on an Ask) to its latest worker
/// session, so the row lands where the human answered. None for a lead/planning
/// ask (dir "" or non-numeric) → the row goes to the lead console (session NULL).
async fn session_for_dir(db: &Db, dir: &str) -> Option<i32> {
    let dir_id: i32 = dir.parse().ok()?;
    repo::latest_session_for_direction(db, dir_id)
        .await
        .ok()
        .flatten()
        .map(|s| s.id)
}
