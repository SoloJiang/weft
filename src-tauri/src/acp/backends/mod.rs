//! Per-CLI ACP backend descriptors.
//!
//! The runtime is generic; only this layer knows binary names and capability
//! quirks. Adding agent N+1 = new file + register + product enum entries.

use std::sync::{Arc, LazyLock};

use serde_json::Value;

mod omp;

pub use omp::OmpBackend;

/// HTTP MCP server Weft injects into `session/new|resume`.
#[derive(Debug, Clone)]
pub struct McpServerSpec {
    pub name: String,
    pub url: String,
}

/// Thin per-tool ACP identity. No process I/O.
pub trait AcpBackend: Send + Sync + 'static {
    /// Stable tool identity (also the process-pool key), e.g. `"omp"`.
    fn id(&self) -> &'static str;

    /// `(program, args)` to spawn the ACP server. `command` is the resolved
    /// binary/alias from Settings (e.g. `"omp"` or a user override).
    fn spawn_argv(&self, command: &str) -> (String, Vec<String>);

    /// `clientCapabilities` object for `initialize`.
    fn client_capabilities(&self) -> Value;

    /// Whether `session/fork` is advertised/usable for rewind helpers.
    fn supports_fork(&self) -> bool {
        true
    }

    /// Paint Weft MCP specs into the wire `mcpServers` array shape this agent accepts.
    fn paint_mcp_servers(&self, servers: Vec<McpServerSpec>) -> Vec<Value> {
        servers
            .into_iter()
            .map(|s| {
                serde_json::json!({
                    "type": "http",
                    "name": s.name,
                    "url": s.url,
                })
            })
            .collect()
    }
}

static REGISTRY: LazyLock<Vec<Arc<dyn AcpBackend>>> =
    LazyLock::new(|| vec![Arc::new(OmpBackend) as Arc<dyn AcpBackend>]);

fn registry() -> &'static Vec<Arc<dyn AcpBackend>> {
    &REGISTRY
}

/// Look up an ACP backend by tool identity. `None` → not an ACP tool.
pub fn backend_for(tool: &str) -> Option<Arc<dyn AcpBackend>> {
    registry().iter().find(|b| b.id() == tool).cloned()
}

/// All registered ACP tool ids (for detect/tests).
pub fn registered_ids() -> Vec<&'static str> {
    registry().iter().map(|b| b.id()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn omp_is_registered() {
        let b = backend_for("omp").expect("omp backend");
        assert_eq!(b.id(), "omp");
        let (prog, args) = b.spawn_argv("omp");
        assert_eq!(prog, "omp");
        assert_eq!(args, vec!["acp"]);
        assert!(b.supports_fork());
        let caps = b.client_capabilities();
        assert_eq!(caps["session"]["requestPermission"], true);
    }

    #[test]
    fn unknown_tool_is_none() {
        assert!(backend_for("claude").is_none());
        assert!(backend_for("codex").is_none());
    }

    #[test]
    fn paints_http_mcp() {
        let b = backend_for("omp").unwrap();
        let painted = b.paint_mcp_servers(vec![McpServerSpec {
            name: "weft_bus".into(),
            url: "http://127.0.0.1:9/bus/1/1/mcp".into(),
        }]);
        assert_eq!(painted.len(), 1);
        assert_eq!(painted[0]["type"], "http");
        assert_eq!(painted[0]["name"], "weft_bus");
    }
}
