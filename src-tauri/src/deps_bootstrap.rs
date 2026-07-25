//! Best-effort JS dependency install for freshly materialized worktrees.
//!
//! Weft creates repo-native git worktrees but does not share one `node_modules`
//! across them (isolation, checkpoint, and concurrent agents all need a private
//! tree). For Node repos, the expensive part is usually re-downloading packages
//! per worktree. pnpm already optimizes that with a content-addressable store +
//! hard links, and its experimental Global Virtual Store is aimed at multi-
//! worktree / multi-agent checkouts.
//!
//! This module runs a package-manager install inside the worktree after
//! materialize succeeds:
//! - never fails materialize (worker can still install itself)
//! - skips non-Node trees and worktrees that already look installed
//! - honors the repo's lockfile package manager (pnpm / yarn / npm)
//! - for pnpm, prefers offline store reuse and may enable the global virtual
//!   store via env (no write into the user's repo config)

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

/// Env-backed knobs for deps bootstrap. Tests pass an explicit `Config` so they
/// never race on process-global environment variables.
#[derive(Clone, Debug)]
pub(crate) struct Config {
    pub enabled: bool,
    /// None = wait unbounded.
    pub timeout: Option<Duration>,
    pub pnpm_global_virtual_store: bool,
}

impl Config {
    /// Production defaults from env:
    /// - `WEFT_DEPS_BOOTSTRAP=0|false|off|no` disables
    /// - `WEFT_DEPS_BOOTSTRAP_TIMEOUT_SECS` (default 600; 0 = unbounded)
    /// - `WEFT_PNPM_GLOBAL_VIRTUAL_STORE=0|false|off|no` disables experimental GVS
    pub fn from_env() -> Self {
        Self {
            enabled: env_flag_enabled("WEFT_DEPS_BOOTSTRAP", true),
            timeout: {
                let secs = std::env::var("WEFT_DEPS_BOOTSTRAP_TIMEOUT_SECS")
                    .ok()
                    .and_then(|v| v.trim().parse::<u64>().ok())
                    .unwrap_or(600);
                if secs == 0 {
                    None
                } else {
                    Some(Duration::from_secs(secs))
                }
            },
            pnpm_global_virtual_store: env_flag_enabled("WEFT_PNPM_GLOBAL_VIRTUAL_STORE", true),
        }
    }
}

fn env_flag_enabled(key: &str, default: bool) -> bool {
    match std::env::var(key) {
        Ok(v) => {
            let t = v.trim().to_ascii_lowercase();
            !matches!(t.as_str(), "0" | "false" | "off" | "no")
        }
        Err(_) => default,
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PackageManager {
    Pnpm,
    Yarn,
    Npm,
}

impl PackageManager {
    fn as_str(&self) -> &'static str {
        match self {
            PackageManager::Pnpm => "pnpm",
            PackageManager::Yarn => "yarn",
            PackageManager::Npm => "npm",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct InstallPlan {
    pub cwd: PathBuf,
    pub program: String,
    pub args: Vec<String>,
    /// Extra env pairs applied only for this install (e.g. pnpm virtual store).
    pub env: Vec<(String, String)>,
}

/// Detect the package manager from lockfiles. None when there is no package.json.
pub(crate) fn detect_package_manager(dir: &Path) -> Option<PackageManager> {
    if !dir.join("package.json").is_file() {
        return None;
    }
    if dir.join("pnpm-lock.yaml").is_file() {
        return Some(PackageManager::Pnpm);
    }
    if dir.join("yarn.lock").is_file() {
        return Some(PackageManager::Yarn);
    }
    Some(PackageManager::Npm)
}

/// True when the worktree already has a usable node_modules tree.
/// pnpm is ready when `.modules.yaml` exists; yarn/npm when node_modules is non-empty.
pub(crate) fn node_modules_ready(dir: &Path) -> bool {
    let nm = dir.join("node_modules");
    if !nm.is_dir() {
        return false;
    }
    if dir.join("pnpm-lock.yaml").is_file() {
        return nm.join(".modules.yaml").is_file();
    }
    match std::fs::read_dir(&nm) {
        Ok(mut entries) => entries.next().is_some(),
        Err(_) => false,
    }
}

/// Build the install plan from process env, or None when bootstrap should be skipped.
pub(crate) fn plan_install(dir: &Path) -> Option<InstallPlan> {
    plan_install_with(dir, &Config::from_env())
}

/// Build the install plan with an explicit config (used by tests to avoid env races).
pub(crate) fn plan_install_with(dir: &Path, cfg: &Config) -> Option<InstallPlan> {
    if !cfg.enabled {
        return None;
    }
    if !dir.is_dir() {
        return None;
    }
    let pm = detect_package_manager(dir)?;
    if node_modules_ready(dir) {
        return None;
    }

    let mut env = Vec::new();
    let (program, args) = match pm {
        PackageManager::Pnpm => {
            // Prefer the content-addressable store (skip network when possible).
            // Frozen only when a lockfile is present — without one, frozen would fail.
            let mut args = vec!["install".to_string(), "--prefer-offline".to_string()];
            if dir.join("pnpm-lock.yaml").is_file() {
                args.push("--frozen-lockfile".to_string());
            }
            if cfg.pnpm_global_virtual_store {
                // pnpm reads npm_config_* / package-manager config from env without
                // mutating the repo. Experimental, scoped to this process only.
                env.push((
                    "npm_config_enable_global_virtual_store".to_string(),
                    "true".to_string(),
                ));
            }
            ("pnpm".to_string(), args)
        }
        PackageManager::Yarn => {
            (
                "yarn".to_string(),
                vec!["install".to_string(), "--prefer-offline".to_string()],
            )
        }
        PackageManager::Npm => {
            // `npm ci` is faster and lockfile-faithful when package-lock exists.
            if dir.join("package-lock.json").is_file() || dir.join("npm-shrinkwrap.json").is_file()
            {
                (
                    "npm".to_string(),
                    vec!["ci".to_string(), "--prefer-offline".to_string()],
                )
            } else {
                (
                    "npm".to_string(),
                    vec!["install".to_string(), "--prefer-offline".to_string()],
                )
            }
        }
    };

    Some(InstallPlan {
        cwd: dir.to_path_buf(),
        program,
        args,
        env,
    })
}

fn output_tail(bytes: &[u8], max: usize) -> String {
    let s = String::from_utf8_lossy(bytes);
    if s.len() <= max {
        return s.trim().to_string();
    }
    let start = s.len() - max;
    let slice = &s[start..];
    let slice = slice.find('\n').map(|i| &slice[i + 1..]).unwrap_or(slice);
    format!("…{}", slice.trim())
}

/// Run one install plan. Returns Ok(()) on success. On frozen-lockfile failure
/// for pnpm, retries once without `--frozen-lockfile` (lock may be slightly
/// out of sync while packages still resolve from the shared store).
fn run_plan(plan: &InstallPlan) -> Result<(), String> {
    let first = run_command(plan)?;
    if first.status.success() {
        return Ok(());
    }
    let can_retry_unfrozen =
        plan.program == "pnpm" && plan.args.iter().any(|a| a == "--frozen-lockfile");
    if can_retry_unfrozen {
        let mut retry = plan.clone();
        retry.args.retain(|a| a != "--frozen-lockfile");
        let second = run_command(&retry)?;
        if second.status.success() {
            return Ok(());
        }
        return Err(format!(
            "{} {} failed (retry without frozen-lockfile): exit {} — {}",
            retry.program,
            retry.args.join(" "),
            second.status.code().unwrap_or(-1),
            output_tail(&second.stderr, 400)
        ));
    }
    Err(format!(
        "{} {} failed: exit {} — {}",
        plan.program,
        plan.args.join(" "),
        first.status.code().unwrap_or(-1),
        output_tail(&first.stderr, 400)
    ))
}

fn run_command(plan: &InstallPlan) -> Result<Output, String> {
    let mut cmd = Command::new(&plan.program);
    cmd.args(&plan.args)
        .current_dir(&plan.cwd)
        .env("PATH", crate::detect::tool_path())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    for (k, v) in &plan.env {
        cmd.env(k, v);
    }

    let timeout = Config::from_env().timeout;
    let child = cmd
        .spawn()
        .map_err(|e| format!("could not spawn {}: {e}", plan.program))?;

    // Optional timeout killer: waits on a flag so a finished install is not
    // re-killed after pid reuse. The main thread drains pipes via wait_with_output
    // (avoids the try_wait + full-pipe deadlock).
    let finished = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let killer = timeout.map(|limit| {
        let finished = finished.clone();
        let pid = child.id();
        std::thread::spawn(move || {
            let start = Instant::now();
            while start.elapsed() < limit {
                if finished.load(std::sync::atomic::Ordering::SeqCst) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            if finished.load(std::sync::atomic::Ordering::SeqCst) {
                return;
            }
            kill_process(pid);
        })
    });

    let out = child
        .wait_with_output()
        .map_err(|e| format!("could not run {}: {e}", plan.program));
    finished.store(true, std::sync::atomic::Ordering::SeqCst);
    if let Some(h) = killer {
        // Best-effort join so tests don't leak threads indefinitely.
        let _ = h.join();
    }
    // If the timeout killer fired, status reflects the kill; callers format it.
    let _ = timeout;
    out
}

#[cfg(unix)]
fn kill_process(pid: u32) {
    // SIGKILL so a stuck package manager can't ignore us.
    unsafe {
        libc::kill(pid as i32, libc::SIGKILL);
    }
}

#[cfg(not(unix))]
fn kill_process(pid: u32) {
    // Best-effort on non-unix: taskkill if available; ignore failures.
    let _ = Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}


/// Best-effort install for one worktree path. Never panics; logs and returns.
pub fn maybe_bootstrap(worktree: &Path) {
    let Some(plan) = plan_install(worktree) else {
        return;
    };
    let label = format!(
        "{} {} in {}",
        plan.program,
        plan.args.join(" "),
        worktree.display()
    );
    let started = Instant::now();
    match run_plan(&plan) {
        Ok(()) => {
            eprintln!(
                "[weft] deps bootstrap ok ({:.1}s): {label}",
                started.elapsed().as_secs_f64()
            );
        }
        Err(err) => {
            // Non-fatal: the worker can still install itself. Materialize must
            // not fail because a package registry blipped.
            eprintln!(
                "[weft] deps bootstrap skipped after {:.1}s: {label}: {err}",
                started.elapsed().as_secs_f64()
            );
        }
    }
}

/// Bootstrap every worktree path in a materialize result (best-effort).
#[allow(dead_code)]
pub fn maybe_bootstrap_worktrees<'a, I>(paths: I)
where
    I: IntoIterator<Item = &'a str>,
{
    for p in paths {
        maybe_bootstrap(Path::new(p));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn tmp() -> PathBuf {
        static N: AtomicU64 = AtomicU64::new(0);
        let id = N.fetch_add(1, Ordering::Relaxed);
        let d = std::env::temp_dir().join(format!(
            "weft-deps-{}-{}-{}",
            std::process::id(),
            id,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn write(dir: &Path, rel: &str, body: &str) {
        let p = dir.join(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(p, body).unwrap();
    }

    #[test]
    fn detect_pm_from_lockfiles() {
        let d = tmp();
        write(&d, "package.json", "{}");
        assert_eq!(detect_package_manager(&d), Some(PackageManager::Npm));

        write(&d, "yarn.lock", "");
        assert_eq!(detect_package_manager(&d), Some(PackageManager::Yarn));

        write(&d, "pnpm-lock.yaml", "");
        assert_eq!(detect_package_manager(&d), Some(PackageManager::Pnpm));
    }

    #[test]
    fn detect_pm_none_without_package_json() {
        let d = tmp();
        write(&d, "README.md", "hi");
        assert_eq!(detect_package_manager(&d), None);
    }

    #[test]
    fn node_modules_ready_pnpm_requires_modules_yaml() {
        let d = tmp();
        write(&d, "package.json", "{}");
        write(&d, "pnpm-lock.yaml", "");
        assert!(!node_modules_ready(&d));
        std::fs::create_dir_all(d.join("node_modules")).unwrap();
        assert!(!node_modules_ready(&d));
        write(&d, "node_modules/.modules.yaml", "hoistPattern: []\n");
        assert!(node_modules_ready(&d));
    }

    #[test]
    fn node_modules_ready_npm_needs_nonempty() {
        let d = tmp();
        write(&d, "package.json", "{}");
        assert!(!node_modules_ready(&d));
        std::fs::create_dir_all(d.join("node_modules")).unwrap();
        assert!(!node_modules_ready(&d));
        write(&d, "node_modules/left-pad/package.json", "{}");
        assert!(node_modules_ready(&d));
    }

    fn test_cfg(enabled: bool, gvs: bool) -> Config {
        Config {
            enabled,
            timeout: Some(Duration::from_secs(30)),
            pnpm_global_virtual_store: gvs,
        }
    }

    #[test]
    fn plan_install_skips_when_ready() {
        let d = tmp();
        write(&d, "package.json", "{}");
        write(&d, "pnpm-lock.yaml", "");
        std::fs::create_dir_all(d.join("node_modules")).unwrap();
        write(&d, "node_modules/.modules.yaml", "x\n");
        assert!(plan_install_with(&d, &test_cfg(true, true)).is_none());
    }

    #[test]
    fn plan_install_pnpm_uses_frozen_and_prefer_offline() {
        let d = tmp();
        write(&d, "package.json", r#"{"name":"x"}"#);
        write(&d, "pnpm-lock.yaml", "lockfileVersion: '9.0'\n");
        let plan = plan_install_with(&d, &test_cfg(true, true)).expect("plan");
        assert_eq!(plan.program, "pnpm");
        assert!(plan.args.iter().any(|a| a == "install"));
        assert!(plan.args.iter().any(|a| a == "--prefer-offline"));
        assert!(plan.args.iter().any(|a| a == "--frozen-lockfile"));
        assert!(plan
            .env
            .iter()
            .any(|(k, v)| k == "npm_config_enable_global_virtual_store" && v == "true"));
    }

    #[test]
    fn plan_install_npm_ci_when_lock_present() {
        let d = tmp();
        write(&d, "package.json", "{}");
        write(&d, "package-lock.json", "{}");
        let plan = plan_install_with(&d, &test_cfg(true, false)).expect("plan");
        assert_eq!(plan.program, "npm");
        assert_eq!(plan.args[0], "ci");
    }

    #[test]
    fn plan_install_disabled_by_config() {
        let d = tmp();
        write(&d, "package.json", "{}");
        write(&d, "pnpm-lock.yaml", "");
        assert!(plan_install_with(&d, &test_cfg(false, true)).is_none());
    }

    #[test]
    fn package_manager_as_str() {
        assert_eq!(PackageManager::Pnpm.as_str(), "pnpm");
        assert_eq!(PackageManager::Yarn.as_str(), "yarn");
        assert_eq!(PackageManager::Npm.as_str(), "npm");
    }
}
