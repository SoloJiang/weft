//! Escape-hatch commands for the Inspect surface (architecture §4.7): the
//! product hides plumbing (worktree paths, branches) but ALWAYS offers a real
//! way out — open the actual working copy in a terminal or the file manager.
//! macOS for now (uses `open`); other platforms are a later adapt pass.

use std::process::Command;

fn err<E: ToString>(e: E) -> String {
    e.to_string()
}

/// Open a new Terminal window at `path` (the isolated working copy).
#[tauri::command]
pub fn open_terminal(path: String) -> Result<(), String> {
    if !std::path::Path::new(&path).exists() {
        return Err("that working copy no longer exists".into());
    }
    #[cfg(target_os = "macos")]
    {
        Command::new("open")
            .args(["-a", "Terminal", &path])
            .status()
            .map_err(err)?;
        Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    {
        Err("opening a terminal is only supported on macOS for now".into())
    }
}

/// Open a URL or app deep link with the OS handler (e.g. `codex://threads/<id>`
/// to jump to a session in the Codex app). Best-effort.
#[tauri::command]
pub fn open_url(url: String) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        Command::new("open").arg(&url).status().map_err(err)?;
        Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    {
        Err("opening a url is only supported on macOS for now".into())
    }
}

/// Reveal `path` in the OS file manager — opens the PARENT and selects the item
/// (Finder `open -R`), rather than opening into the folder.
#[tauri::command]
pub fn reveal_path(path: String) -> Result<(), String> {
    if !std::path::Path::new(&path).exists() {
        return Err("that path no longer exists".into());
    }
    #[cfg(target_os = "macos")]
    {
        Command::new("open")
            .args(["-R", &path])
            .status()
            .map_err(err)?;
        Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    {
        Err("revealing a path is only supported on macOS for now".into())
    }
}
