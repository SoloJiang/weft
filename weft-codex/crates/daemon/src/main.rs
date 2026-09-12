//! weftd — the weft-codex headless daemon (product shell:
//! docs/specs/2026-08-16-weft-third-mode-design.md; protocol spikes:
//! docs/specs/2026-08-08-codex-desktop-migration-design.md §5–6, §9).
//!
//! Boots: live-mode gate → store (fresh ~/.weft-codex schema) → UI event
//! channel → thread-bus MCP server + kanban HTTP API on a STABLE address
//! (default 127.0.0.1:47810, `WEFTD_ADDR` override) → orchestrator bus
//! delivery loop → ctrl-c shutdown.

use axum::middleware::from_fn;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use weft_core::{
    api, bus::BusRegistry, cors, events, mcp, orchestrator::Orchestrator, runtime, store::Store,
};

/// Log a fatal startup error and exit cleanly (no panic/unwind).
fn fatal(context: &str, err: impl std::fmt::Display) -> ! {
    eprintln!("[weftd] fatal: {context}: {err}");
    std::process::exit(1);
}

#[tokio::main]
async fn main() {
    runtime::set_live();

    let home = weft_app_server::paths::weft_home()
        .unwrap_or_else(|e| fatal("resolve weft-codex home", e));
    let store = Store::open(&home.join("weft-codex.db"))
        .await
        .unwrap_or_else(|e| fatal("open store", e));

    // A `running` profile outlived its daemon — fail it so the UI recovers.
    match store.reset_running_profiles().await {
        Ok(0) => {}
        Ok(n) => eprintln!("[weftd] marked {n} interrupted repo analyses failed"),
        Err(e) => eprintln!("[weftd] reset running profiles failed: {e:#}"),
    }

    let (tx, _rx) = tokio::sync::broadcast::channel(256);
    events::install(tx);

    let addr = std::env::var("WEFTD_ADDR").unwrap_or_else(|_| "127.0.0.1:47810".to_string());
    let bus_base = format!("http://{addr}");

    let bus = BusRegistry::new();
    let orch = Orchestrator::new(store.clone(), bus.clone(), bus_base, home.clone());

    let restored_bus_messages = match orch.restore_pending_bus().await {
        Ok(count) => count,
        Err(error) => {
            eprintln!("[weftd] restore durable bus inboxes failed: {error:#}");
            0
        }
    };

    tokio::spawn(orch.clone().run_bus_delivery());

    // Threads persist in ~/.codex across restarts; watchers don't.
    match orch.reattach_all().await {
        Ok(0) => {}
        Ok(n) => eprintln!("[weftd] re-attached {n} thread watchers"),
        Err(e) => eprintln!("[weftd] re-attach failed (bus delivery still works): {e:#}"),
    }

    // Lead-created tasks dispatch automatically. A single queue preserves
    // creation order while each started worker continues independently.
    let (task_dispatch, mut task_dispatch_rx) = tokio::sync::mpsc::unbounded_channel::<i64>();
    let dispatch_orch = orch.clone();
    tokio::spawn(async move {
        while let Some(direction_id) = task_dispatch_rx.recv().await {
            if let Err(error) = dispatch_orch.dispatch_direction(direction_id).await {
                eprintln!(
                    "[weftd] automatic worker dispatch failed for task {direction_id}: {error:#}"
                );
            }
        }
    });

    // Read crash leftovers before the store moves into router state. They are
    // queued only after the HTTP listener is bound, so per-thread MCP setup
    // cannot race daemon startup.
    let undispatched = match store.list_undispatched_directions().await {
        Ok(directions) => directions,
        Err(error) => {
            eprintln!("[weftd] list undispatched tasks failed: {error:#}");
            Vec::new()
        }
    };

    let pending_flush_orch = orch.clone();
    let state = mcp::McpState {
        bus,
        store: store.clone(),
        task_dispatch: task_dispatch.clone(),
    };
    let app = mcp::router(state)
        .merge(api::router(api::ApiState { store, orch }))
        .route("/", get(web_index))
        .route("/web/{*path}", get(web_file))
        .layer(from_fn(cors::layer));

    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .unwrap_or_else(|e| fatal(&format!("bind {addr}"), e));
    eprintln!("[weftd] thread bus MCP + kanban API + web app on http://{addr}");

    if restored_bus_messages > 0 {
        eprintln!("[weftd] restored {restored_bus_messages} pending bus messages");
        tokio::spawn(async move {
            pending_flush_orch.flush_pending_bus().await;
        });
    }

    for direction in undispatched {
        if task_dispatch.send(direction.id).is_err() {
            eprintln!(
                "[weftd] automatic worker dispatcher stopped before task {} was queued",
                direction.id
            );
            break;
        }
    }

    let server = axum::serve(listener, app);
    tokio::select! {
        result = server => {
            if let Err(error) = result {
                eprintln!("[weftd] server error: {error}");
            }
        }
        _ = tokio::signal::ctrl_c() => {
            eprintln!("[weftd] shutting down");
        }
    }
    weft_app_server::client::shutdown_global().await;
}

/// Web app root directory: `WEFT_WEB_DIR` override, else the crate's web/.
fn web_dir() -> std::path::PathBuf {
    match std::env::var("WEFT_WEB_DIR") {
        Ok(dir) => std::path::PathBuf::from(dir),
        Err(_) => std::path::PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/web")),
    }
}

async fn web_index() -> Response {
    web_serve("index.html").await
}

async fn web_file(axum::extract::Path(path): axum::extract::Path<String>) -> Response {
    web_serve(&path).await
}

async fn web_serve(rel: &str) -> Response {
    let clean = rel.trim_start_matches('/');
    if clean.is_empty() || clean.split('/').any(|seg| seg == "..") {
        return axum::http::StatusCode::BAD_REQUEST.into_response();
    }
    let path = web_dir().join(clean);
    let bytes = match tokio::fs::read(&path).await {
        Ok(b) => b,
        Err(_) => return axum::http::StatusCode::NOT_FOUND.into_response(),
    };
    let mime = match path.extension().and_then(|e| e.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("svg") => "image/svg+xml",
        _ => "application/octet-stream",
    };
    ([("content-type", mime)], bytes).into_response()
}
