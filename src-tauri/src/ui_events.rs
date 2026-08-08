//! UI event fan-out (Stage 1 of the Codex Desktop migration).
//!
//! One emit path, two optional sinks: the Tauri frontend (app) and a broadcast
//! channel (weftd, whose web surface subscribes over SSE in Stage 3). Events
//! are advisory UI signals — with no sink installed, emit is a no-op and never
//! load-bearing for correctness.

use serde::Serialize;
use serde_json::Value;
use std::sync::OnceLock;
use tokio::sync::broadcast;

static CHANNEL: OnceLock<broadcast::Sender<(String, Value)>> = OnceLock::new();

/// weftd installs the channel at boot. Double-install (tests) is ignored.
pub fn install_channel(tx: broadcast::Sender<(String, Value)>) {
    let _ = CHANNEL.set(tx);
}

/// Subscribe to the daemon event channel; None in the app and in tests.
pub fn subscribe() -> Option<broadcast::Receiver<(String, Value)>> {
    CHANNEL.get().map(broadcast::Sender::subscribe)
}

/// Emit to every installed sink. The Tauri path re-serializes the same JSON
/// the frontend has always received — the wire format is unchanged.
pub fn emit(event: &str, payload: impl Serialize) {
    let value = serde_json::to_value(payload).unwrap_or(Value::Null);
    if let Some(app) = crate::APP_HANDLE.get() {
        use tauri::Emitter;
        let _ = app.emit(event, value.clone());
    }
    if let Some(tx) = CHANNEL.get() {
        let _ = tx.send((event.to_string(), value));
    }
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn emit_reaches_channel_when_installed() {
        // OnceLock: another test may already have installed a channel; either
        // way, subscribe() must see the emit that follows.
        let (tx, _keep) = tokio::sync::broadcast::channel(8);
        super::install_channel(tx);
        let mut rx = super::subscribe().expect("channel installed");
        super::emit("weft-test-event", serde_json::json!({ "k": 1 }));
        let (event, value) = rx.recv().await.expect("event received");
        assert_eq!(event, "weft-test-event");
        assert_eq!(value["k"], 1);
    }
}
