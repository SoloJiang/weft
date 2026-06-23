//! Tool readiness: make GUI-launched Weft find CLIs installed via nvm/fnm/volta
//! or native installers, and report each CLI's version. The core fix is
//! augmenting THIS process's PATH at startup — engine spawns inherit this
//! process's env, so one augment makes every later `claude`/`codex`/`opencode`
//! spawn resolvable. PATH is augmented from two sources: a deterministic scan of
//! known version-manager / install dirs (fast, can't time out) and the user's
//! login-shell PATH (best-effort, for custom dirs the scan can't know).

use std::time::Duration;

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
    let out = wait_with_timeout(&mut child, Duration::from_secs(3))?;
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

/// Run once at startup: fold known tool-install dirs AND the login shell's PATH
/// into this process's PATH, so a GUI- or dev-launched Weft can spawn
/// nvm/fnm/volta/native CLIs even when the inherited PATH is minimal. Engine and
/// curator spawns inherit this process's env, so one augment fixes every later
/// `claude`/`codex`/`opencode` spawn.
///
/// Two sources, merged with EXISTING entries kept at higher priority (we only
/// ADD what's missing): (1) a deterministic scan of version-manager + native
/// install dirs — needs no shell, so it can't time out; (2) the login shell's
/// full PATH, a best-effort secondary for custom dirs the scan can't know.
/// Source (2) used to fail SILENTLY on a slow shell (leaving an installed CLI
/// unresolvable with no clue in the log), so a probe miss now logs a diagnostic.
pub fn augment_path() {
    let base = std::env::var("PATH").unwrap_or_default();
    let mut merged = base.clone();

    let scanned = tool_bin_dirs(home_dir().as_deref());
    if !scanned.is_empty() {
        let extra = scanned
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(":");
        merged = merge_path(&merged, &extra);
    }

    match login_shell_path() {
        Some(shell_path) => merged = merge_path(&merged, &shell_path),
        // Windows GUIs inherit PATH fine and have no `-ilc` probe, so only the
        // unix probe failing is worth flagging.
        None if !cfg!(windows) => eprintln!(
            "[weft] login-shell PATH probe unavailable (unset/unsupported shell or timed out); \
             relying on direct tool-dir scan for nvm/fnm/volta/native CLIs"
        ),
        None => {}
    }

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

/// The user's home directory (HOME, or USERPROFILE on Windows). None if unset.
fn home_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(std::path::PathBuf::from)
}

/// Existing `bin` dirs under an nvm tree (`<nvm_dir>/versions/node/<v>/bin`).
/// nvm installs each node version's global CLIs there, so a CLI installed under
/// ANY version is discoverable regardless of which one is "current" — the login
/// shell may not have run `nvm use`, but the binary is still on disk.
fn nvm_node_bin_dirs(nvm_dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let Ok(entries) = std::fs::read_dir(nvm_dir.join("versions/node")) else {
        return Vec::new();
    };
    entries
        .filter_map(|e| e.ok())
        .map(|e| e.path().join("bin"))
        .filter(|p| p.is_dir())
        .collect()
}

/// Existing node `installation/bin` dirs across fnm's known on-disk layouts
/// (XDG data dir, `~/.fnm`, macOS Application Support).
fn fnm_node_bin_dirs(home: &std::path::Path) -> Vec<std::path::PathBuf> {
    let roots = [
        home.join(".local/share/fnm/node-versions"),
        home.join(".fnm/node-versions"),
        home.join("Library/Application Support/fnm/node-versions"),
    ];
    let mut out = Vec::new();
    for root in roots {
        let Ok(entries) = std::fs::read_dir(&root) else {
            continue;
        };
        for e in entries.filter_map(|e| e.ok()) {
            let bin = e.path().join("installation/bin");
            if bin.is_dir() {
                out.push(bin);
            }
        }
    }
    out
}

/// Home-relative dirs where native installers / package managers drop CLIs.
/// Pure: returns CANDIDATES (not existence-filtered) so the mapping is testable;
/// the orchestrator filters to existing dirs.
fn static_tool_bin_dirs(home: &std::path::Path) -> Vec<std::path::PathBuf> {
    [
        ".local/bin",
        ".cargo/bin",
        ".bun/bin",
        ".deno/bin",
        ".volta/bin",
        ".opencode/bin",
        ".npm-global/bin",
        "Library/pnpm",
    ]
    .iter()
    .map(|s| home.join(s))
    .collect()
}

/// System bin dirs a minimal GUI PATH often omits (Homebrew, /usr/local).
const SYSTEM_TOOL_BIN_DIRS: [&str; 4] = [
    "/opt/homebrew/bin",
    "/opt/homebrew/sbin",
    "/usr/local/bin",
    "/usr/local/sbin",
];

/// All EXISTING tool-install bin dirs discovered WITHOUT a shell: version
/// managers (nvm/fnm) + native-installer / package-manager dirs + system dirs.
/// Deterministic and fast — the reliable half of PATH augmentation, so a slow or
/// timed-out login-shell probe can no longer hide an installed CLI.
fn tool_bin_dirs(home: Option<&std::path::Path>) -> Vec<std::path::PathBuf> {
    let mut out: Vec<std::path::PathBuf> = Vec::new();
    if let Some(h) = home {
        // nvm respects $NVM_DIR; default to ~/.nvm when the env var is unset
        // (a GUI launch won't have sourced the shell that exports it).
        let nvm = std::env::var_os("NVM_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| h.join(".nvm"));
        out.extend(nvm_node_bin_dirs(&nvm));
        out.extend(fnm_node_bin_dirs(h));
        out.extend(static_tool_bin_dirs(h));
    }
    out.extend(SYSTEM_TOOL_BIN_DIRS.iter().map(std::path::PathBuf::from));
    out.into_iter().filter(|p| p.is_dir()).collect()
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
    fn nvm_node_bin_dirs_finds_versioned_bins_and_skips_binless() {
        let tmp = tempfile::tempdir().unwrap();
        let nvm = tmp.path();
        for v in ["v22.22.0", "v18.19.0"] {
            std::fs::create_dir_all(nvm.join("versions/node").join(v).join("bin")).unwrap();
        }
        // A version dir with no bin/ must be skipped, not blindly added.
        std::fs::create_dir_all(nvm.join("versions/node/v16.0.0")).unwrap();
        let dirs = nvm_node_bin_dirs(nvm);
        assert!(dirs.contains(&nvm.join("versions/node/v22.22.0/bin")));
        assert!(dirs.contains(&nvm.join("versions/node/v18.19.0/bin")));
        assert!(!dirs.contains(&nvm.join("versions/node/v16.0.0/bin")));
        // A missing nvm dir yields nothing and never panics.
        assert!(nvm_node_bin_dirs(&nvm.join("does-not-exist")).is_empty());
    }

    #[test]
    fn fnm_node_bin_dirs_finds_installation_bins() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let bin = home.join(".local/share/fnm/node-versions/v22.22.0/installation/bin");
        std::fs::create_dir_all(&bin).unwrap();
        assert!(fnm_node_bin_dirs(home).contains(&bin));
    }

    #[test]
    fn static_tool_bin_dirs_lists_known_home_install_dirs() {
        let home = std::path::Path::new("/home/u");
        let dirs = static_tool_bin_dirs(home);
        for sub in [".cargo/bin", ".local/bin", ".volta/bin", ".bun/bin"] {
            assert!(dirs.contains(&home.join(sub)), "missing candidate: {sub}");
        }
    }

    #[test]
    fn discovered_nvm_bin_makes_codex_resolvable_after_merge() {
        // Reproduces the real bug: the inherited PATH lacks the nvm dir where
        // `codex` lives; after merging the DISCOVERED nvm bin, codex resolves.
        let tmp = tempfile::tempdir().unwrap();
        let nvm = tmp.path();
        let bin = nvm.join("versions/node/v22.22.0/bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join("codex"), b"#!/bin/sh\n").unwrap();
        let extra = nvm_node_bin_dirs(nvm)
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(":");
        let merged = merge_path("/usr/bin:/bin", &extra);
        let found = std::env::split_paths(&merged).any(|d| d.join("codex").is_file());
        assert!(found, "codex must resolve after merging the discovered nvm bin");
    }
}
