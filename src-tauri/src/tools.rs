//! Detect which coding-agent CLIs are installed locally (Settings display + the
//! default-tool picker). Resolution goes through detect.rs so it matches how
//! sessions spawn (PATH, augmented from the login shell at startup) and includes
//! the Codex app-bundle fallback.

use serde::Serialize;

#[derive(Serialize, Clone)]
pub struct ToolStatus {
    pub tool: String,
    pub installed: bool,
    pub version: Option<String>,
    pub path: Option<String>,
    pub meets_min: bool,
}

const TOOLS: [&str; 3] = ["claude", "codex", "opencode"];

fn probe(tool: &str) -> ToolStatus {
    let Some(path) = crate::detect::resolve_tool_path(tool) else {
        return ToolStatus { tool: tool.into(), installed: false, version: None, path: None, meets_min: true };
    };
    let version = std::process::Command::new(&path)
        .arg("--version")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| {
            String::from_utf8_lossy(&o.stdout)
                .trim()
                .lines()
                .next()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        });
    let meets_min = version.as_deref().map(|v| crate::detect::meets_min(tool, v)).unwrap_or(true);
    ToolStatus {
        tool: tool.into(),
        installed: true,
        version,
        path: Some(path.to_string_lossy().to_string()),
        meets_min,
    }
}

#[tauri::command]
pub async fn detect_tools() -> Result<Vec<ToolStatus>, String> {
    tokio::task::spawn_blocking(|| TOOLS.iter().map(|t| probe(t)).collect::<Vec<_>>())
        .await
        .map_err(|e| e.to_string())
}
