//! weftd — the headless Weft daemon (Stage 1 of the Codex Desktop migration;
//! spec: docs/superpowers/specs/2026-08-08-codex-desktop-migration-design.md).
//!
//! Boots the product core WITHOUT the Tauri shell: store, tool-command
//! overrides, the thread-bus MCP server on a STABLE address (so the Codex
//! `mcp_servers` config and the Stage 3 web app can point at it across
//! restarts), and the UI event channel the web surface will subscribe to.
//!
//! The desktop app keeps working unchanged. Both share ~/.weft/weft.db;
//! SQLite (WAL) tolerates multi-process readers/writers, but during the
//! migration the app remains the owner of lead/worker session orchestration —
//! weftd Stage 1 serves the bus + repo-map MCP surface only.

use weft::ask::AskRegistry;
use weft::bus::{server, BusRegistry};
use weft::{detect, runtime, store, tool_command, ui_events};

/// Log a fatal startup error and exit cleanly (no panic/unwind).
fn fatal(context: &str, err: impl std::fmt::Display) -> ! {
    eprintln!("[weftd] fatal: {context}: {err}");
    std::process::exit(1);
}

#[tokio::main]
async fn main() {
    // Mark the process before anything can consult runtime::agents_allowed().
    runtime::set_mode(runtime::Mode::Daemon);

    // Same rustls CryptoProvider pin as the app (mixed ring/aws-lc-rs deps).
    let _ = rustls::crypto::ring::default_provider().install_default();

    // Tool PATH prewarm + user-configured command overrides, same as app boot.
    detect::augment_path();

    let db = store::Db::open_default()
        .await
        .unwrap_or_else(|e| fatal("open weft.db", e));
    {
        let map = store::repo::get_tool_commands(&db).await.unwrap_or_default();
        tool_command::set_overrides(map);
    }

    // Daemon-side UI event sink (the Stage 3 web app's SSE feed subscribes here).
    let (tx, _rx) = tokio::sync::broadcast::channel(256);
    ui_events::install_channel(tx);

    let bus = BusRegistry::new();
    let asks = AskRegistry::new();
    let addr = std::env::var("WEFTD_ADDR").unwrap_or_else(|_| "127.0.0.1:47810".to_string());
    let (base, _handle) = server::serve_on(bus, db, asks, &addr)
        .await
        .unwrap_or_else(|e| fatal("start bus server", e));
    eprintln!("[weftd] thread bus MCP on {base} (http + SSE)");

    match tokio::signal::ctrl_c().await {
        Ok(()) => eprintln!("[weftd] shutting down"),
        Err(error) => eprintln!("[weftd] ctrl-c listener error: {error}"),
    }
}
