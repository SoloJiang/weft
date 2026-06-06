// Panic-prone code is banned in production paths. clippy enforces it; the
// `not(test)` guard lets test modules use unwrap/expect freely (a failing test
// SHOULD panic). Run `cargo clippy` to check.
#![cfg_attr(
    not(test),
    deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)
)]

pub mod paths;
pub mod slug;
pub mod store;
pub mod git;
pub mod materialize;
mod batch;
pub mod bus;
mod claude;
mod coordinator;
mod drivers;
mod inspect;
mod pty;
mod commands;

/// The bus server's base URL, e.g. "http://127.0.0.1:54321".
pub struct BusBase(pub String);

/// Log a fatal startup error and exit cleanly (no panic/unwind).
fn fatal(context: &str, err: impl std::fmt::Display) -> ! {
    eprintln!("[weft] fatal: {context}: {err}");
    std::process::exit(1);
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Open the DB synchronously before building the app.
    let db = tauri::async_runtime::block_on(async { store::Db::open_default().await })
        .unwrap_or_else(|e| fatal("open weft.db", e));

    // Start the thread-bus HTTP MCP server on an ephemeral port.
    let bus = bus::BusRegistry::new();
    let bus_base: String = {
        let bus = bus.clone();
        tauri::async_runtime::block_on(async move { bus::server::serve(bus).await })
            .map(|(base, _handle)| base) // leak the JoinHandle: server lives for app lifetime
            .unwrap_or_else(|e| fatal("start bus server", e))
    };
    eprintln!("[weft] thread bus on {bus_base}");

    // Wire the coordinator: bus wakes -> nudge the target direction's session.
    let (wake_tx, wake_rx) = std::sync::mpsc::channel::<bus::Wake>();
    bus.set_wake_sender(wake_tx);

    let mut builder = tauri::Builder::default().plugin(tauri_plugin_opener::init());

    #[cfg(debug_assertions)]
    {
        builder = builder.plugin(tauri_plugin_mcp_bridge::init());
    }

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
            commands::create_workspace,
            commands::list_workspaces,
            commands::add_repo_ref,
            commands::create_thread,
            commands::list_threads,
            commands::list_repos,
            commands::list_directions,
            commands::list_direction_repos,
            commands::create_direction,
            commands::list_worktrees,
            commands::repo_diff,
            commands::delete_thread,
            commands::thread_messages,
            commands::bus_post_human,
            commands::needs_you,
            commands::answer_ask,
            pty::open_session,
            pty::resume_session,
            pty::write_pty,
            pty::resize_pty,
            pty::kill_session,
            inspect::open_terminal,
            inspect::reveal_path,
        ])
        .run(tauri::generate_context!())
        .unwrap_or_else(|e| fatal("running tauri application", e));
}
