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
pub mod ask;
mod brief;
pub mod bus;
mod check;
mod claude;
mod codex;
pub mod config;
pub mod skills;
mod coordinator;
mod curator;
mod detect;
mod gc;
mod inspect;
pub mod lead_chat;
mod planner;
mod power;
pub mod profile;
mod sidecar;
mod tools;
mod commands;

/// The bus server's base URL, e.g. "http://127.0.0.1:54321".
pub struct BusBase(pub String);

/// The app handle, for emitting events from contexts that predate the app
/// (the bus server starts before the Tauri builder finishes). Set in setup().
pub static APP_HANDLE: std::sync::OnceLock<tauri::AppHandle> = std::sync::OnceLock::new();

/// Log a fatal startup error and exit cleanly (no panic/unwind).
fn fatal(context: &str, err: impl std::fmt::Display) -> ! {
    eprintln!("[weft] fatal: {context}: {err}");
    std::process::exit(1);
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Make GUI-launched spawns find nvm/fnm/native-installer CLIs (see detect.rs).
    detect::augment_path_from_login_shell();

    // Open the DB synchronously before building the app.
    let db = tauri::async_runtime::block_on(async { store::Db::open_default().await })
        .unwrap_or_else(|e| fatal("open weft.db", e));

    // Start the local HTTP server (thread bus MCP + planner MCP + Ask Bridge).
    let bus = bus::BusRegistry::new();
    let asks = ask::AskRegistry::new();
    let bus_base: String = {
        let bus = bus.clone();
        let db = db.clone();
        let asks = asks.clone();
        tauri::async_runtime::block_on(async move { bus::server::serve(bus, db, asks).await })
            .map(|(base, _handle)| base) // leak the JoinHandle: server lives for app lifetime
            .unwrap_or_else(|e| fatal("start bus server", e))
    };
    eprintln!("[weft] thread bus on {bus_base}");

    // Wire the coordinator: bus wakes -> nudge the target direction's session.
    let (wake_tx, wake_rx) = std::sync::mpsc::channel::<bus::Wake>();
    bus.set_wake_sender(wake_tx);

    let mut builder = tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init());

    #[cfg(debug_assertions)]
    {
        builder = builder.plugin(tauri_plugin_mcp_bridge::init());
    }

    builder
        .manage(db)
        .manage(lead_chat::engine::LeadChatState::default())
        .manage(commands::GuardrailState::default())
        .manage(power::PowerGuard::default())
        .manage(bus)
        .manage(asks)
        .manage(BusBase(bus_base))
        .setup(move |app| {
            let _ = APP_HANDLE.set(app.handle().clone());
            coordinator::run(app.handle().clone(), wake_rx);
            lead_chat::engine::spawn_watchdog(app.handle().clone());
            power::spawn_sweep(app.handle().clone());
            gc::spawn_periodic(app.handle().clone());
            skills::spawn_periodic(app.handle().clone());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::create_workspace,
            commands::list_workspaces,
            commands::add_repo_ref,
            commands::clone_repo,
            commands::create_repo,
            commands::create_thread,
            commands::list_threads,
            commands::workspace_overview,
            commands::list_repos,
            commands::list_repo_profiles,
            commands::repo_graph,
            commands::reprofile_repo,
            commands::update_repo_profile,
            commands::list_directions,
            commands::set_task_status,
            commands::read_transcript,
            commands::worktree_diff,
            commands::get_proposal,
            commands::save_proposal,
            commands::confirm_proposal,
            commands::preview_brief,
            commands::verify_direction,
            commands::create_direction,
            commands::list_worktrees,
            commands::repo_diff,
            commands::delete_thread,
            commands::thread_messages,
            commands::bus_post_human,
            commands::pending_asks,
            commands::workspace_needs_counts,
            commands::answer_permission,
            commands::set_dangerous_mode,
            commands::set_keep_awake,
            commands::set_guardrails,
            commands::session_for,
            commands::effective_config,
            commands::needs_you,
            commands::write_triggers,
            commands::approve_write_trigger,
            commands::deny_write_trigger,
            commands::answer_ask,
            lead_chat::commands::lead_send,
            lead_chat::commands::lead_interrupt,
            lead_chat::commands::lead_ensure,
            lead_chat::commands::lead_stop,
            lead_chat::commands::lead_state,
            lead_chat::commands::list_lead_messages,
            lead_chat::commands::chat_open_worker,
            lead_chat::commands::chat_send,
            lead_chat::commands::chat_interrupt,
            lead_chat::commands::chat_stop,
            lead_chat::commands::flag_session_skill_refresh,
            lead_chat::commands::flag_lead_skill_refresh,
            inspect::open_terminal,
            inspect::reveal_path,
            inspect::open_url,
            tools::detect_tools,
            commands::get_default_tool,
            commands::set_default_tool,
            commands::list_skill_sources,
            commands::add_skill_source,
            commands::remove_skill_source,
            commands::sync_skill_source,
            commands::sync_all_skill_sources,
            commands::list_parsed_skills,
            commands::set_skill_enabled,
            commands::workspace_skills,
        ])
        .run(tauri::generate_context!())
        .unwrap_or_else(|e| fatal("running tauri application", e));
}
