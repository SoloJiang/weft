// Panic-prone code is banned in production paths. clippy enforces it; the
// `not(test)` guard lets test modules use unwrap/expect freely (a failing test
// SHOULD panic). Run `cargo clippy` to check.
#![cfg_attr(
    not(test),
    deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)
)]

mod adapters;
pub mod ask;
pub mod backup;
mod brief;
pub mod bus;
mod check;
mod claude;
mod codex;
mod codex_app_server;
mod codex_slash;
pub mod commands;
mod commands_backup;
pub mod config;
mod coordinator;
mod curator;
mod detect;
mod gc;
pub mod git;
pub mod im;
mod inspect;
pub mod lead_chat;
pub mod materialize;
mod opencode;
pub mod paths;
mod planner;
mod power;
pub mod profile;
mod sidecar;
pub mod skills;
pub mod slug;
pub mod store;
mod tools;

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

fn mcp_bridge_enabled_from_env(value: Option<&str>) -> bool {
    matches!(
        value.map(str::trim).map(str::to_ascii_lowercase).as_deref(),
        Some("1" | "true" | "yes" | "on")
    )
}

fn mcp_bridge_enabled() -> bool {
    mcp_bridge_enabled_from_env(std::env::var("WEFT_MCP_BRIDGE").ok().as_deref())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Make GUI-launched spawns find nvm/fnm/native-installer CLIs (see detect.rs).
    detect::augment_path_from_login_shell();

    // Pin the rustls CryptoProvider. Several of our transitive deps enable
    // both `ring` and `aws-lc-rs`, which makes rustls 0.23 refuse to auto-pick
    // and panic at the first TLS handshake ("Could not automatically determine
    // the process-level CryptoProvider"). Install ring once here, ignoring the
    // "already installed" error so re-entry (tests, restart) is harmless.
    let _ = rustls::crypto::ring::default_provider().install_default();

    // Open the DB synchronously before building the app.
    let db = tauri::async_runtime::block_on(async { store::Db::open_default().await })
        .unwrap_or_else(|e| fatal("open weft.db", e));

    // App-level backup handle: scheduler + on-exit + commands all share it.
    let backup_svc = backup::BackupService::new(
        db.clone(),
        paths::weft_home().unwrap_or_else(|e| fatal("weft_home for backup", e)),
    );

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
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_updater::Builder::new().build());

    #[cfg(debug_assertions)]
    if mcp_bridge_enabled() {
        builder = builder.plugin(
            tauri_plugin_mcp_bridge::Builder::new()
                .bind_address("127.0.0.1")
                .build(),
        );
    }

    builder
        .manage(db)
        .manage(lead_chat::engine::LeadChatState::default())
        .manage(lead_chat::out_hub::LeadOutHub::default())
        .manage(lead_chat::delta_hub::LeadDeltaHub::default())
        .manage(commands::GuardrailState::default())
        .manage(power::PowerGuard::default())
        .manage(bus)
        .manage(asks)
        .manage(BusBase(bus_base))
        .manage(im::ImBridge::default())
        .manage(backup_svc.clone())
        .on_window_event({
            let svc = backup_svc.clone();
            move |_window, event| {
                if let tauri::WindowEvent::CloseRequested { .. } = event {
                    // Don't block the close path — `run_on_exit` is bounded
                    // at 10s internally, but we still detach so the user
                    // never sees the window hang.
                    let svc = svc.clone();
                    tauri::async_runtime::spawn(async move {
                        backup::scheduler::run_on_exit(&svc).await;
                    });
                }
            }
        })
        .setup(move |app| {
            let _ = APP_HANDLE.set(app.handle().clone());
            coordinator::run(app.handle().clone(), wake_rx);
            lead_chat::engine::spawn_watchdog(app.handle().clone());
            lead_chat::revive::spawn_revive(app.handle().clone());
            power::spawn_sweep(app.handle().clone());
            gc::spawn_periodic(app.handle().clone());
            skills::spawn_periodic(app.handle().clone());
            im::spawn(app.handle().clone());
            backup::scheduler::spawn(backup_svc.clone());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::create_workspace,
            commands::list_workspaces,
            commands::ensure_default_workspace,
            commands::add_repo_ref,
            commands::check_git_repo,
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
            commands::rename_workspace,
            commands::rename_thread,
            commands::rename_direction,
            commands::thread_messages,
            commands::bus_post_human,
            commands::pending_asks,
            commands::workspace_needs_counts,
            commands::answer_permission,
            commands::set_dangerous_mode,
            commands::set_keep_awake,
            commands::db_encryption_status,
            commands::db_enable_encryption,
            commands::db_disable_encryption,
            commands::db_change_password,
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
            lead_chat::commands::list_live_worker_slots,
            lead_chat::commands::auto_verify_check,
            lead_chat::commands::discover_slash,
            lead_chat::commands::post_lead_tool_result,
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
            commands::im_get_settings,
            commands::im_set_settings,
            commands::im_set_enabled,
            commands::im_set_remote_standby,
            commands::im_status,
            commands::im_bind_thread,
            commands::im_unbind_thread,
            commands::im_route_for_thread,
            commands::im_list_routes,
            commands_backup::backup_get_status,
            commands_backup::backup_save_prefs,
            commands_backup::backup_test_remote,
            commands_backup::backup_run_now,
            commands_backup::backup_export_recovery_key,
            commands_backup::backup_restore,
        ])
        .run(tauri::generate_context!())
        .unwrap_or_else(|e| fatal("running tauri application", e));
}

#[cfg(test)]
mod tests {
    use super::mcp_bridge_enabled_from_env;

    #[test]
    fn mcp_bridge_is_opt_in() {
        assert!(!mcp_bridge_enabled_from_env(None));
        assert!(!mcp_bridge_enabled_from_env(Some("")));
        assert!(!mcp_bridge_enabled_from_env(Some("0")));
        assert!(!mcp_bridge_enabled_from_env(Some("false")));
        assert!(mcp_bridge_enabled_from_env(Some("1")));
        assert!(mcp_bridge_enabled_from_env(Some("true")));
        assert!(mcp_bridge_enabled_from_env(Some("YES")));
        assert!(mcp_bridge_enabled_from_env(Some(" on ")));
    }
}
