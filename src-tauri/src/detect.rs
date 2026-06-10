//! Tool readiness: make GUI-launched Weft find CLIs installed via nvm/fnm/volta
//! or native installers, and report each CLI's version. The core fix is
//! augmenting THIS process's PATH from the user's login shell at startup —
//! engine spawns inherit this process's env, so one augment makes every later
//! `claude`/`codex`/`opencode` spawn resolvable.

use std::time::Duration;

/// POSIX shells we will invoke as `-ilc`. fish has different syntax → excluded.
fn is_supported_login_shell(shell: &str) -> bool {
    matches!(
        std::path::Path::new(shell).file_name().and_then(|s| s.to_str()),
        Some("bash" | "zsh" | "sh" | "dash" | "ksh")
    )
}

/// Ask the user's login shell for its full PATH. None if unavailable / unsupported
/// / times out. macOS+Linux only (Windows GUI inherits PATH fine).
fn login_shell_path() -> Option<String> {
    if cfg!(windows) {
        return None;
    }
    let shell = std::env::var("SHELL").ok()?;
    if !is_supported_login_shell(&shell) {
        return None;
    }
    let mut child = std::process::Command::new(&shell)
        .args(["-ilc", "printf '%s' \"$PATH\""])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .stdin(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let out = wait_with_timeout(&mut child, Duration::from_secs(3))?;
    let path = String::from_utf8_lossy(&out).trim().to_string();
    if path.is_empty() { None } else { Some(path) }
}

/// Wait up to `dur` for the child; kill + return None on timeout. Reads stdout
/// after exit.
fn wait_with_timeout(child: &mut std::process::Child, dur: Duration) -> Option<Vec<u8>> {
    use std::io::Read;
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => {
                let mut buf = Vec::new();
                if let Some(mut so) = child.stdout.take() {
                    let _ = so.read_to_end(&mut buf);
                }
                return Some(buf);
            }
            Ok(None) => {
                if start.elapsed() > dur {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(_) => return None,
        }
    }
}

/// Merge `extra` PATH entries into `base`, preserving base order and appending
/// only entries not already present. Pure — unit tested.
pub(crate) fn merge_path(base: &str, extra: &str) -> String {
    let mut seen: Vec<&str> = base.split(':').filter(|s| !s.is_empty()).collect();
    let mut out = seen.clone();
    for e in extra.split(':').filter(|s| !s.is_empty()) {
        if !seen.contains(&e) {
            out.push(e);
            seen.push(e);
        }
    }
    out.join(":")
}

/// Run once at startup: fold the login shell's PATH into this process's PATH.
pub fn augment_path_from_login_shell() {
    let Some(shell_path) = login_shell_path() else { return };
    let base = std::env::var("PATH").unwrap_or_default();
    let merged = merge_path(&base, &shell_path);
    if merged != base {
        std::env::set_var("PATH", merged);
    }
}

/// Soft minimum versions — surfaced as an "update recommended" hint in Settings,
/// NOT a hard spawn gate. Reasons are the features Weft relies on.
pub(crate) fn min_version(tool: &str) -> Option<(u32, u32, u32)> {
    match tool {
        "claude" => Some((1, 0, 0)),
        "codex" => Some((0, 20, 0)),
        "opencode" => Some((0, 1, 0)),
        _ => None,
    }
}

/// Extract (major, minor, patch), tolerating "2.1.100 (Claude Code)" or "v" prefix.
pub(crate) fn parse_semver(raw: &str) -> Option<(u32, u32, u32)> {
    let bytes = raw.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_digit() {
            let rest = &raw[i..];
            let nums: Vec<u32> = rest
                .split(|c: char| !c.is_ascii_digit())
                .filter(|s| !s.is_empty())
                .take(3)
                .filter_map(|s| s.parse().ok())
                .collect();
            if nums.len() == 3 {
                return Some((nums[0], nums[1], nums[2]));
            }
        }
        i += 1;
    }
    None
}

pub fn meets_min(tool: &str, version: &str) -> bool {
    match (min_version(tool), parse_semver(version)) {
        (Some(min), Some(v)) => v >= min,
        _ => true,
    }
}

fn codex_app_bundle_paths() -> Vec<std::path::PathBuf> {
    let mut v = vec![std::path::PathBuf::from(
        "/Applications/Codex.app/Contents/Resources/codex",
    )];
    if let Some(home) = std::env::var_os("HOME") {
        v.push(std::path::Path::new(&home).join("Applications/Codex.app/Contents/Resources/codex"));
    }
    v
}

/// Resolve a tool to an executable path: PATH first (now augmented), then the
/// Codex app-bundle fallback. None if not found.
pub fn resolve_tool_path(tool: &str) -> Option<std::path::PathBuf> {
    if let Some(p) = which_on_path(tool) {
        return Some(p);
    }
    if tool == "codex" {
        for p in codex_app_bundle_paths() {
            if p.exists() {
                return Some(p);
            }
        }
    }
    None
}

fn which_on_path(tool: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let cand = dir.join(tool);
        if cand.is_file() {
            return Some(cand);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_path_appends_only_new_entries() {
        let merged = merge_path("/usr/bin:/bin", "/usr/bin:/opt/fnm/bin:/bin");
        assert_eq!(merged, "/usr/bin:/bin:/opt/fnm/bin");
    }

    #[test]
    fn merge_path_handles_empty_and_dups() {
        assert_eq!(merge_path("/a", ""), "/a");
        assert_eq!(merge_path("", "/a::/a"), "/a");
        assert_eq!(merge_path("/a:/b", "/b:/a"), "/a:/b");
    }

    #[test]
    fn unsupported_shell_rejected() {
        assert!(!is_supported_login_shell("/usr/bin/fish"));
        assert!(is_supported_login_shell("/bin/zsh"));
        assert!(is_supported_login_shell("/usr/bin/bash"));
    }

    #[test]
    fn parse_semver_tolerates_suffix_and_v() {
        assert_eq!(parse_semver("v2.1.100"), Some((2, 1, 100)));
        assert_eq!(parse_semver("2.1.100 (Claude Code)"), Some((2, 1, 100)));
        assert_eq!(parse_semver("codex 0.20.3"), Some((0, 20, 3)));
        assert_eq!(parse_semver("nope"), None);
    }

    #[test]
    fn meets_min_logic() {
        assert!(meets_min("codex", "0.21.0"));
        assert!(!meets_min("codex", "0.19.9"));
        assert!(meets_min("unknown-tool", "0.0.1"));
    }
}
