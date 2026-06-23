//! Tool readiness: make GUI-launched Weft find CLIs installed via nvm/fnm/volta
//! or native installers, and report each CLI's version. The core fix is
//! augmenting THIS process's PATH at startup — engine spawns inherit this
//! process's env, so one augment makes every later `claude`/`codex`/`opencode`
//! spawn resolvable. PATH is augmented from two sources: a deterministic scan of
//! known version-manager / install dirs (fast, can't time out) and the user's
//! login-shell PATH (best-effort, for custom dirs the scan can't know).

use std::time::Duration;

/// Budget for the `zsh -ilc` PATH probe. Generous because the result is cached on
/// disk — this runs at most once per shell-config change (and off the critical
/// path on a cache hit), so a heavy interactive shell no longer loses the race a
/// tight 3s budget did.
const SHELL_PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// POSIX shells we will invoke as `-ilc`. fish has different syntax → excluded.
fn is_supported_login_shell(shell: &str) -> bool {
    matches!(
        std::path::Path::new(shell)
            .file_name()
            .and_then(|s| s.to_str()),
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
    let out = wait_with_timeout(&mut child, SHELL_PROBE_TIMEOUT)?;
    let path = String::from_utf8_lossy(&out).trim().to_string();
    if path.is_empty() {
        None
    } else {
        Some(path)
    }
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

/// Cache file for the probed login-shell PATH, under the weft home so it follows
/// the same dev/release/`$WEFT_HOME` split as the rest of weft's data.
fn shell_path_cache_file() -> Option<std::path::PathBuf> {
    crate::paths::weft_home()
        .ok()
        .map(|h| h.join("login-shell-path"))
}

/// A cached login-shell PATH, if present and non-empty.
fn read_cached_shell_path(file: &std::path::Path) -> Option<String> {
    let s = std::fs::read_to_string(file).ok()?;
    let s = s.trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// Write the login-shell PATH cache atomically (tmp + rename) so a crash — or a
/// background refresh killed at app exit — can't leave a torn cache file.
fn write_cached_shell_path(file: &std::path::Path, value: &str) {
    if let Some(parent) = file.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let tmp = file.with_extension(format!("tmp.{}", std::process::id()));
    if std::fs::write(&tmp, value).is_ok() {
        let _ = std::fs::rename(&tmp, file);
    }
}

/// Merge `extra` into the process PATH, keeping existing entries' priority (only
/// missing dirs are appended).
fn apply_shell_path(base: &str, extra: &str) {
    let merged = merge_path(base, extra);
    if merged != base {
        std::env::set_var("PATH", merged);
    }
}

/// Re-probe the login shell off the critical path and rewrite the cache — DISK
/// ONLY, never the live process env (mutating env from another thread would race
/// concurrent `Command::spawn`s) — so the NEXT launch reflects a tool installed
/// since this one.
fn spawn_shell_path_refresh(file: std::path::PathBuf) {
    std::thread::spawn(move || {
        if let Some(shell_path) = login_shell_path() {
            write_cached_shell_path(&file, &shell_path);
        }
    });
}

/// Whether any known coding-agent CLI resolves on the current process PATH — a
/// best-effort "is the cache obviously stale?" signal: false means none of
/// codex/claude/opencode is found, so re-probe synchronously rather than wait a
/// launch. It runs early (so the `set_var` in `augment_path` precedes the async
/// runtime — see `lib.rs`), hence it checks bare identities, NOT aliases or the
/// per-session selected tool. That precision would require per-`Command` PATH
/// rather than a process-global env — a deliberate non-goal here; the residual
/// edge self-heals on the next launch.
fn known_agent_resolves() -> bool {
    TOOL_PRIORITY.iter().any(|t| resolve_tool_path(t).is_some())
}

/// Run once at startup: fold the user's login-shell PATH into this process's PATH
/// so a GUI- or dev-launched Weft can spawn nvm/fnm/volta/native CLIs even when
/// the inherited PATH is minimal. Engine and curator spawns inherit this
/// process's env, so one augment fixes every later `claude`/`codex`/`opencode`.
///
/// The login shell is the single authoritative source of what's on the user's
/// PATH — it already knows every version manager — but probing it (`zsh -ilc`) is
/// slow on a heavy shell. So the result is CACHED on disk: a cache hit augments
/// instantly and refreshes the cache in the background (disk only — no
/// cross-thread setenv race) for next launch; a cache miss (first launch) pays
/// the probe once, synchronously, and seeds the cache.
///
/// Staleness: if a cache hit leaves NO known agent resolvable (e.g. one was
/// installed since the cache was written), the cache is treated as stale and
/// re-probed synchronously HERE — on the main thread, before other threads spawn,
/// so applying it is race-free — so a freshly-installed agent is picked up this
/// session, not only after a restart.
pub fn augment_path() {
    let base = std::env::var("PATH").unwrap_or_default();
    let cache = shell_path_cache_file();

    // Fast path: reuse a previous launch's probe.
    if let Some(file) = cache.as_ref() {
        if let Some(cached) = read_cached_shell_path(file) {
            apply_shell_path(&base, &cached);
            if known_agent_resolves() {
                // Cache looks good — refresh in the background for next launch.
                spawn_shell_path_refresh(file.clone());
            } else if let Some(fresh) = login_shell_path() {
                // Stale cache (or a newly-installed agent): re-probe now, merge into
                // the just-applied PATH, and rewrite the cache so the next launch is
                // correct too.
                let now = std::env::var("PATH").unwrap_or_default();
                apply_shell_path(&now, &fresh);
                write_cached_shell_path(file, &fresh);
            }
            return;
        }
    }

    // Cold path (first launch / no cache): probe synchronously once, augment, seed.
    match login_shell_path() {
        Some(shell_path) => {
            apply_shell_path(&base, &shell_path);
            if let Some(file) = cache.as_ref() {
                write_cached_shell_path(file, &shell_path);
            }
        }
        // Windows GUIs inherit PATH fine and have no `-ilc` probe, so only the unix
        // probe failing (unset/unsupported shell or timeout) is worth flagging.
        None if !cfg!(windows) => eprintln!(
            "[weft] login-shell PATH probe unavailable (unset/unsupported shell or timed out); \
             GUI-launched spawns may not find nvm/fnm/volta CLIs until it succeeds"
        ),
        None => {}
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

/// The soft minimum version as a display string ("0.20.0"), or "" if none.
pub(crate) fn min_version_str(tool: &str) -> String {
    min_version(tool)
        .map(|(a, b, c)| format!("{a}.{b}.{c}"))
        .unwrap_or_default()
}

/// Why a CLI probe didn't yield a usable, up-to-date tool — surfaced in the
/// Settings diagnostics panel so a missing/old CLI explains itself.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub enum DiagnosticKind {
    MissingTarget,
    NotExecutable,
    SpawnFailed,
    VersionProbeFailed,
    BelowMinimum,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct ToolDiagnostic {
    pub kind: DiagnosticKind,
    pub message: String,
}

impl ToolDiagnostic {
    pub fn missing_target(tool: &str) -> Self {
        Self {
            kind: DiagnosticKind::MissingTarget,
            message: format!("{tool} is not on PATH. Install it or check your shell profile."),
        }
    }
    pub fn not_executable(path: &str) -> Self {
        Self {
            kind: DiagnosticKind::NotExecutable,
            message: format!("{path} exists but is not executable (permission denied)."),
        }
    }
    pub fn spawn_failed(tool: &str, err: &str) -> Self {
        Self {
            kind: DiagnosticKind::SpawnFailed,
            message: format!("Could not run {tool}: {err}"),
        }
    }
    pub fn version_probe_failed(tool: &str) -> Self {
        Self {
            kind: DiagnosticKind::VersionProbeFailed,
            message: format!("{tool} ran but --version returned no usable version."),
        }
    }
    pub fn below_minimum(tool: &str, version: &str, min: &str) -> Self {
        Self {
            kind: DiagnosticKind::BelowMinimum,
            message: format!(
                "{tool} {version} is below the recommended {min}. Update recommended."
            ),
        }
    }
}

/// Preference order when the user hasn't chosen a tool explicitly.
pub(crate) const TOOL_PRIORITY: [&str; 3] = ["codex", "claude", "opencode"];

/// Pure default-tool decision: an explicit user choice wins when that CLI is
/// installed; otherwise the first installed tool by priority; otherwise codex
/// (nothing can spawn anyway — Settings surfaces the "no CLI" warning).
pub(crate) fn pick_default_tool(user: Option<&str>, installed: impl Fn(&str) -> bool) -> String {
    if let Some(u) = user {
        if installed(u) {
            return u.to_string();
        }
    }
    TOOL_PRIORITY
        .iter()
        .copied()
        .find(|t| installed(t))
        .unwrap_or("codex")
        .to_string()
}

/// Resolve the effective default tool against the real PATH (and the Codex
/// app-bundle fallback), honoring the user's explicit choice when present. A
/// tool counts as installed when its configured command (alias) resolves, so an
/// aliased CLI is eligible as the default.
pub fn resolve_default_tool(user: Option<&str>) -> String {
    pick_default_tool(user, |t| {
        resolve_tool_path(&crate::tool_command::command_for(t)).is_some()
    })
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
    fn missing_target_diagnostic_has_helpful_message() {
        let d = ToolDiagnostic::missing_target("claude");
        assert_eq!(d.kind, DiagnosticKind::MissingTarget);
        assert!(d.message.contains("not on PATH"));
    }

    #[test]
    fn below_minimum_message_contains_versions() {
        let d = ToolDiagnostic::below_minimum("codex", "0.1.0", &min_version_str("codex"));
        assert!(d.message.contains("0.1.0"));
        assert!(d.message.contains("0.20.0"));
    }

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

    #[test]
    fn default_tool_prefers_user_choice_when_installed() {
        let installed = |t: &str| t == "claude" || t == "codex";
        assert_eq!(pick_default_tool(Some("claude"), installed), "claude");
    }

    #[test]
    fn default_tool_falls_back_when_user_choice_missing() {
        let installed = |t: &str| t == "claude";
        assert_eq!(pick_default_tool(Some("codex"), installed), "claude");
    }

    #[test]
    fn default_tool_detects_by_priority() {
        let installed = |t: &str| t == "codex" || t == "opencode";
        assert_eq!(pick_default_tool(None, installed), "codex");
        let only_oc = |t: &str| t == "opencode";
        assert_eq!(pick_default_tool(None, only_oc), "opencode");
    }

    #[test]
    fn default_tool_codex_when_nothing_installed() {
        assert_eq!(pick_default_tool(None, |_| false), "codex");
    }

    #[test]
    fn cached_shell_path_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("login-shell-path");
        // Missing cache → None (cold path: probe synchronously).
        assert!(read_cached_shell_path(&file).is_none());
        write_cached_shell_path(&file, "/opt/homebrew/bin:/usr/bin");
        assert_eq!(
            read_cached_shell_path(&file).as_deref(),
            Some("/opt/homebrew/bin:/usr/bin")
        );
    }

    #[test]
    fn cached_shell_path_rejects_blank_and_creates_parents() {
        let tmp = tempfile::tempdir().unwrap();
        // Parent dirs are created on write.
        let nested = tmp.path().join("a/b/login-shell-path");
        write_cached_shell_path(&nested, "/usr/bin");
        assert_eq!(read_cached_shell_path(&nested).as_deref(), Some("/usr/bin"));
        // A blank/whitespace cache reads back as None, not "".
        let blank = tmp.path().join("blank");
        write_cached_shell_path(&blank, "  \n");
        assert!(read_cached_shell_path(&blank).is_none());
    }

    // `:`-joined PATH + split_paths is unix semantics (Windows uses `;`), and the
    // whole login-shell augmentation is unix-only anyway.
    #[cfg(unix)]
    #[test]
    fn cached_path_with_nvm_bin_resolves_codex_after_merge() {
        // Reproduces the bug at the cache layer: a cached login-shell PATH that
        // includes the nvm bin makes `codex` resolvable when merged into a minimal
        // process PATH that lacked it.
        let tmp = tempfile::tempdir().unwrap();
        let nvm_bin = tmp.path().join("nvm/versions/node/v22.22.0/bin");
        std::fs::create_dir_all(&nvm_bin).unwrap();
        std::fs::write(nvm_bin.join("codex"), b"#!/bin/sh\n").unwrap();
        let cached = format!("/usr/bin:{}", nvm_bin.display());
        let merged = merge_path("/usr/bin:/bin", &cached);
        assert!(std::env::split_paths(&merged).any(|d| d.join("codex").is_file()));
    }
}
