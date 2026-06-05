mod batch;
mod claude;
mod git;
mod paths;
mod pty;
mod slug;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let mut builder = tauri::Builder::default()
        .plugin(tauri_plugin_opener::init());

    // Debug-only WebSocket bridge (port 9223) for @hypothesi/tauri-mcp-server.
    // Never compiled into release builds.
    #[cfg(debug_assertions)]
    {
        builder = builder.plugin(tauri_plugin_mcp_bridge::init());
    }

    builder
        .manage(pty::PtyState::default())
        .invoke_handler(tauri::generate_handler![
            pty::open_session,
            pty::resume_session,
            pty::write_pty,
            pty::resize_pty,
            pty::kill_session,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
