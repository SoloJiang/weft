//! Detect which coding-agent CLIs are installed locally (Settings display + the
//! default-tool picker). Resolution goes through detect.rs so it matches how
//! sessions spawn (PATH, augmented from the login shell at startup) and includes
//! the Codex app-bundle fallback.

use serde::Serialize;

#[derive(Serialize, Clone)]
pub struct ToolStatus {
    pub tool: String,
    pub installed: bool,
    /// Whether the configured command can be reached by the same bare spawn
    /// path used by agent sessions. This is false for the macOS Codex.app
    /// diagnostic fallback even when `installed` is true.
    pub spawnable: bool,
    pub version: Option<String>,
    pub path: Option<String>,
    pub meets_min: bool,
    /// Why the tool is missing / unusable / outdated, for the diagnostics panel.
    /// Empty when the CLI is present and current.
    pub diagnostics: Vec<crate::detect::ToolDiagnostic>,
}

// Display order for Settings (default-tool picker + diagnostics): mirrors the
// default-tool priority (codex > claude > opencode).
const TOOLS: [&str; 3] = ["codex", "claude", "opencode"];

/// The executable used for the version probe, plus whether that exact path is
/// reachable by a bare session spawn. Prefer the spawnable PATH match over a
/// diagnostics-only match: an earlier non-executable file must not hide a later
/// working CLI with the same name.
fn probe_target(
    spawnable_path: Option<std::path::PathBuf>,
    diagnostic_path: Option<std::path::PathBuf>,
) -> Option<(std::path::PathBuf, bool)> {
    let spawnable = spawnable_path.is_some();
    spawnable_path
        .or(diagnostic_path)
        .map(|path| (path, spawnable))
}

fn probe(tool: &str) -> ToolStatus {
    use crate::detect::ToolDiagnostic as Diag;
    let mut diagnostics = Vec::new();
    // Probe the user-configured command (alias) for this identity, so Settings
    // reports install status for the binary sessions actually spawn.
    let command = crate::tool_command::command_for(tool);
    let spawnable_path = crate::detect::resolve_spawnable_tool_path(&command);
    // Only fall back to the diagnostics resolver when a session cannot spawn
    // this command. It may point at the macOS Codex.app bundle or a
    // non-executable PATH file, both useful to report but not to select.
    let diagnostic_path = if spawnable_path.is_none() {
        crate::detect::resolve_tool_path(&command)
    } else {
        None
    };
    let Some((path, path_is_spawnable)) = probe_target(spawnable_path, diagnostic_path) else {
        diagnostics.push(Diag::missing_target(tool));
        return ToolStatus {
            tool: tool.into(),
            installed: false,
            spawnable: false,
            version: None,
            path: None,
            meets_min: true,
            diagnostics,
        };
    };
    let path_str = path.to_string_lossy().to_string();
    // Classify the --version probe: it ran (with/without a version), or it
    // couldn't spawn (not executable vs other OS error).
    // nvm/fnm shims are `#!/usr/bin/env node` — the --version probe needs the
    // augmented PATH to find that node, or it false-reports the tool as missing.
    let (installed, version) = match std::process::Command::new(&path)
        .arg("--version")
        .env("PATH", crate::detect::tool_path())
        .output()
    {
        Ok(o) if o.status.success() => {
            let v = String::from_utf8_lossy(&o.stdout)
                .trim()
                .lines()
                .next()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());
            if v.is_none() {
                diagnostics.push(Diag::version_probe_failed(tool));
            }
            (true, v)
        }
        Ok(_) => {
            diagnostics.push(Diag::version_probe_failed(tool));
            (true, None)
        }
        Err(e) => {
            if e.raw_os_error() == Some(13) || e.kind() == std::io::ErrorKind::PermissionDenied {
                diagnostics.push(Diag::not_executable(&path_str));
            } else {
                diagnostics.push(Diag::spawn_failed(tool, &e.to_string()));
            }
            (false, None)
        }
    };
    let spawnable = installed && path_is_spawnable;
    let meets_min = version
        .as_deref()
        .map(|v| crate::detect::meets_min(tool, v))
        .unwrap_or(true);
    if installed && !meets_min {
        if let Some(v) = &version {
            diagnostics.push(Diag::below_minimum(
                tool,
                v,
                &crate::detect::min_version_str(tool),
            ));
        }
    }
    ToolStatus {
        tool: tool.into(),
        installed,
        spawnable,
        version,
        path: Some(path_str),
        meets_min,
        diagnostics,
    }
}

#[tauri::command]
pub async fn detect_tools() -> Result<Vec<ToolStatus>, String> {
    tokio::task::spawn_blocking(|| TOOLS.iter().map(|t| probe(t)).collect::<Vec<_>>())
        .await
        .map_err(|e| e.to_string())
}

/// The effective default coding tool: the Settings choice when that CLI is
/// installed, else the first installed CLI by priority (codex > claude >
/// opencode). Reads app_setting "default_tool"; resolution is detect.rs's.
pub async fn default_tool(db: &crate::store::Db) -> String {
    let configured = crate::store::repo::get_setting(db, "default_tool")
        .await
        .ok()
        .flatten();
    crate::detect::resolve_default_tool(configured.as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_target_prefers_a_spawnable_path_over_diagnostic_only_match() {
        let diagnostic = std::path::PathBuf::from("/first/codex");
        let runnable = std::path::PathBuf::from("/second/codex");

        assert_eq!(
            probe_target(Some(runnable.clone()), Some(diagnostic)),
            Some((runnable, true))
        );
    }

    #[test]
    fn probe_target_keeps_a_diagnostic_only_path_non_spawnable() {
        let diagnostic = std::path::PathBuf::from("/Applications/Codex.app/codex");

        assert_eq!(probe_target(None, Some(diagnostic.clone())), Some((diagnostic, false)));
    }
}
