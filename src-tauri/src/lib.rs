pub mod paths;
pub mod slug;
pub mod store;
pub mod git;
pub mod materialize;
mod batch;
mod claude;
mod pty;
mod commands;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Open the DB synchronously before building the app.
    let db = tauri::async_runtime::block_on(async {
        store::Db::open_default().await.expect("open weft.db")
    });

    let mut builder = tauri::Builder::default().plugin(tauri_plugin_opener::init());

    #[cfg(debug_assertions)]
    {
        builder = builder.plugin(tauri_plugin_mcp_bridge::init());
    }

    builder
        .manage(db)
        .manage(pty::PtyState::default())
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
            pty::open_session,
            pty::resume_session,
            pty::write_pty,
            pty::resize_pty,
            pty::kill_session,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
