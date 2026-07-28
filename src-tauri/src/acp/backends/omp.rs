//! omp (Oh My Pi) ACP backend descriptor.

use serde_json::{json, Value};

use super::AcpBackend;

pub struct OmpBackend;

impl AcpBackend for OmpBackend {
    fn id(&self) -> &'static str {
        "omp"
    }

    fn spawn_argv(&self, command: &str) -> (String, Vec<String>) {
        (command.to_string(), vec!["acp".into()])
    }

    fn client_capabilities(&self) -> Value {
        // fs/terminal off — agent runs tools locally in the worktree.
        // requestPermission on so bash/edit gates surface to Weft Ask.
        json!({
            "fs": {
                "readTextFile": false,
                "writeTextFile": false,
            },
            "session": {
                "requestPermission": true,
            },
        })
    }

    fn supports_fork(&self) -> bool {
        // Full-history fork only; cut-before rewind uses jsonl rewrite + load.
        true
    }
}
