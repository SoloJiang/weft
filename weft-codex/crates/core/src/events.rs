//! UI event channel (Stage 3's SSE feed subscribes here). Advisory only:
//! events are UI signals, never load-bearing for correctness.

use serde::Serialize;
use serde_json::Value;
use std::sync::OnceLock;
use tokio::sync::broadcast;

static CHANNEL: OnceLock<broadcast::Sender<(String, Value)>> = OnceLock::new();

pub fn install(tx: broadcast::Sender<(String, Value)>) {
    let _ = CHANNEL.set(tx);
}

pub fn subscribe() -> Option<broadcast::Receiver<(String, Value)>> {
    CHANNEL.get().map(broadcast::Sender::subscribe)
}

pub fn emit(event: &str, payload: impl Serialize) {
    if let Some(tx) = CHANNEL.get() {
        let value = serde_json::to_value(payload).unwrap_or(Value::Null);
        let _ = tx.send((event.to_string(), value));
    }
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn emit_reaches_subscriber() {
        let (tx, _keep) = tokio::sync::broadcast::channel(8);
        super::install(tx);
        let mut rx = super::subscribe().expect("channel installed");
        super::emit("test-event", serde_json::json!({ "k": 1 }));
        let (event, value) = rx.recv().await.expect("event");
        assert_eq!(event, "test-event");
        assert_eq!(value["k"], 1);
    }
}
