//! Make a GUI-launched daemon find the Codex CLI installed through a user's
//! shell environment. The login shell is probed once and cached; each child
//! receives the augmented PATH without mutating the process-global environment.

use std::sync::{OnceLock, RwLock};
use std::time::Duration;

const SHELL_PROBE_TIMEOUT: Duration = Duration::from_secs(10);

fn is_supported_login_shell(shell: &str) -> bool {
    matches!(
        std::path::Path::new(shell)
            .file_name()
            .and_then(|name| name.to_str()),
        Some("bash" | "zsh" | "sh" | "dash" | "ksh")
    )
}

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
    let output = wait_with_timeout(&mut child, SHELL_PROBE_TIMEOUT)?;
    let path = String::from_utf8_lossy(&output).trim().to_string();
    (!path.is_empty()).then_some(path)
}

fn wait_with_timeout(child: &mut std::process::Child, timeout: Duration) -> Option<Vec<u8>> {
    use std::io::Read;

    let started = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => {
                let mut output = Vec::new();
                if let Some(mut stdout) = child.stdout.take() {
                    let _ = stdout.read_to_end(&mut output);
                }
                return Some(output);
            }
            Ok(None) if started.elapsed() <= timeout => {
                std::thread::sleep(Duration::from_millis(25));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Err(_) => return None,
        }
    }
}

pub(crate) fn merge_path(base: &str, extra: &str) -> String {
    let mut seen: Vec<&str> = base.split(':').filter(|entry| !entry.is_empty()).collect();
    let mut output = seen.clone();
    for entry in extra.split(':').filter(|entry| !entry.is_empty()) {
        if !seen.contains(&entry) {
            output.push(entry);
            seen.push(entry);
        }
    }
    output.join(":")
}

fn shell_path_cache_file() -> Option<std::path::PathBuf> {
    crate::paths::weft_home()
        .ok()
        .map(|home| home.join("login-shell-path"))
}

fn read_cached_shell_path(file: &std::path::Path) -> Option<String> {
    let value = std::fs::read_to_string(file).ok()?;
    let value = value.trim().to_string();
    (!value.is_empty()).then_some(value)
}

fn write_cached_shell_path(file: &std::path::Path, value: &str) {
    if let Some(parent) = file.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let temporary = file.with_extension(format!("tmp.{}", std::process::id()));
    if std::fs::write(&temporary, value).is_ok() {
        let _ = std::fs::rename(&temporary, file);
    }
}

fn refresh_cache_in_background(file: std::path::PathBuf) {
    std::thread::spawn(move || {
        if let Some(shell_path) = login_shell_path() {
            write_cached_shell_path(&file, &shell_path);
        }
    });
}

static CODEX_PATH: OnceLock<RwLock<String>> = OnceLock::new();

fn path_cell() -> &'static RwLock<String> {
    CODEX_PATH.get_or_init(|| RwLock::new(compute_path()))
}

/// PATH passed to every `codex app-server` child.
pub fn tool_path() -> String {
    match path_cell().read() {
        Ok(path) => path.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    }
}

fn compute_path() -> String {
    let base = std::env::var("PATH").unwrap_or_default();
    if let Some(file) = shell_path_cache_file() {
        if let Some(cached) = read_cached_shell_path(&file) {
            refresh_cache_in_background(file);
            return merge_path(&base, &cached);
        }
    }
    match login_shell_path() {
        Some(shell_path) => {
            if let Some(file) = shell_path_cache_file() {
                write_cached_shell_path(&file, &shell_path);
            }
            merge_path(&base, &shell_path)
        }
        None if !cfg!(windows) => {
            eprintln!("[weftd] login-shell PATH probe unavailable; Codex must already be on PATH");
            base
        }
        None => base,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_appends_only_new_entries() {
        assert_eq!(
            merge_path("/usr/bin:/bin", "/usr/bin:/opt/fnm/bin:/bin"),
            "/usr/bin:/bin:/opt/fnm/bin"
        );
        assert_eq!(merge_path("", "/a::/a"), "/a");
    }

    #[test]
    fn supported_shells_are_explicit() {
        assert!(!is_supported_login_shell("/usr/bin/fish"));
        assert!(is_supported_login_shell("/bin/zsh"));
        assert!(is_supported_login_shell("/usr/bin/bash"));
    }

    #[test]
    fn cached_shell_path_round_trips_atomically() {
        let temporary = tempfile::tempdir().expect("tempdir");
        let file = temporary.path().join("nested/login-shell-path");
        assert!(read_cached_shell_path(&file).is_none());
        write_cached_shell_path(&file, "/opt/homebrew/bin:/usr/bin");
        assert_eq!(
            read_cached_shell_path(&file).as_deref(),
            Some("/opt/homebrew/bin:/usr/bin")
        );
        write_cached_shell_path(&file, "  \n");
        assert!(read_cached_shell_path(&file).is_none());
    }
}
