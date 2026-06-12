//! `opencode.rs` — a lazily-started, app-lifetime `opencode serve` used ONLY to
//! discover a project's slash commands (GET /command). Turn dispatch goes
//! through `opencode run --command <name>` in the engine, never this server.
//!
//! `opencode run` exposes no command list, so the registry (built-ins + config +
//! markdown + MCP/skills) is only reachable via the HTTP server. One serve
//! multiplexes every worktree through the `?directory=` query param, so a single
//! process answers all sessions. It binds 127.0.0.1 on an ephemeral port
//! (`--port 0`) and we learn the bound URL from its "listening on http://…" line.

use std::process::Stdio;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::Command;
use tokio::sync::Mutex;

use crate::lead_chat::proto::SlashCmd;

#[derive(Default)]
struct Serve {
    base: Option<String>,
    child: Option<tokio::process::Child>,
}

fn serve() -> Arc<Mutex<Serve>> {
    static S: OnceLock<Arc<Mutex<Serve>>> = OnceLock::new();
    S.get_or_init(|| Arc::new(Mutex::new(Serve::default())))
        .clone()
}

/// Discover slash commands for `cwd`, or an empty list on any failure (opencode
/// not installed, serve won't start, endpoint error). Discovery is best-effort
/// and must never block the composer.
pub async fn discover_commands(cwd: &str) -> Vec<SlashCmd> {
    match discover_inner(cwd).await {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[weft][opencode] discover commands: {e}");
            vec![]
        }
    }
}

async fn discover_inner(cwd: &str) -> anyhow::Result<Vec<SlashCmd>> {
    let base = ensure_base().await?;
    let arr: Vec<serde_json::Value> = reqwest::Client::new()
        .get(format!("{base}/command"))
        .query(&[("directory", cwd)])
        .timeout(Duration::from_secs(10))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    Ok(arr
        .iter()
        .filter_map(|v| {
            let name = v.get("name")?.as_str()?.to_string();
            let description = v
                .get("description")
                .and_then(|d| d.as_str())
                .filter(|s| !s.is_empty())
                .map(String::from);
            Some(SlashCmd {
                name,
                description,
                arg_hint: None,
            })
        })
        .collect())
}

/// Return the base URL of a live serve, (re)spawning if the prior one died.
async fn ensure_base() -> anyhow::Result<String> {
    let s = serve();
    let mut g = s.lock().await;

    // Reuse the running server if its child is still alive.
    if let (Some(base), Some(child)) = (g.base.clone(), g.child.as_mut()) {
        if matches!(child.try_wait(), Ok(None)) {
            return Ok(base);
        }
    }
    g.base = None;
    g.child = None;

    let mut child = Command::new("opencode")
        .args(["serve", "--hostname", "127.0.0.1", "--port", "0"])
        // Force an unsecured server on our private localhost port: we never set a
        // password, so the user's global OPENCODE_SERVER_PASSWORD (if any) does
        // not apply and discovery needs no auth.
        .env_remove("OPENCODE_SERVER_PASSWORD")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;

    let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(2);
    drain_for_url(child.stdout.take(), tx.clone());
    drain_for_url(child.stderr.take(), tx);
    let base = tokio::time::timeout(Duration::from_secs(15), rx.recv())
        .await
        .map_err(|_| anyhow::anyhow!("opencode serve did not report a listening URL in 15s"))?
        .ok_or_else(|| anyhow::anyhow!("opencode serve exited before listening"))?;

    g.base = Some(base.clone());
    g.child = Some(child);
    Ok(base)
}

/// Read a child stream to EOF, sending the first `http://…` URL found and
/// discarding the rest (so the child never blocks on a full pipe).
fn drain_for_url<R: AsyncRead + Unpin + Send + 'static>(
    stream: Option<R>,
    tx: tokio::sync::mpsc::Sender<String>,
) {
    let Some(stream) = stream else { return };
    tauri::async_runtime::spawn(async move {
        let mut lines = BufReader::new(stream).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if let Some(url) = parse_url(&line) {
                let _ = tx.try_send(url);
            }
        }
    });
}

/// Extract `http://host:port` from a serve log line (drops a trailing slash).
fn parse_url(line: &str) -> Option<String> {
    let rest = &line[line.find("http://")?..];
    let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
    Some(rest[..end].trim_end_matches('/').to_string())
}

#[cfg(test)]
mod tests {
    use super::parse_url;

    #[test]
    fn parses_listening_url() {
        assert_eq!(
            parse_url("opencode server listening on http://127.0.0.1:54321").as_deref(),
            Some("http://127.0.0.1:54321")
        );
        assert_eq!(
            parse_url("listening on http://127.0.0.1:4096/ now").as_deref(),
            Some("http://127.0.0.1:4096")
        );
        assert_eq!(parse_url("no url here"), None);
    }
}
