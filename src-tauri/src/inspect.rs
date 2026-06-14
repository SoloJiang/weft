//! Escape-hatch commands for the Inspect surface (architecture §4.7) and for
//! file references inside chat: the product hides plumbing (worktree paths,
//! branches) but ALWAYS offers a real way out — open the working copy in a
//! terminal/file manager, or open a file the agent mentioned with the OS default
//! app. Opening + revealing go through `tauri-plugin-opener`, so they work on
//! macOS/Windows/Linux. (`open_terminal` is still macOS-only — a later adapt pass.)

use std::path::PathBuf;
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

/// Open a file the agent referenced in chat with the OS default app. `path` may
/// be relative (resolved against the session `cwd`), absolute, `~`-prefixed, or a
/// `file://` URI, and may carry a trailing `:line[:col]` (stripped — the default
/// app can't seek). Errors `"not_found"` so the UI can show a quiet toast.
#[tauri::command]
pub fn open_path(path: String, cwd: Option<String>) -> Result<(), String> {
    let abs = resolve_chat_path(&path, cwd.as_deref()).map_err(map_resolve_err)?;
    tauri_plugin_opener::open_path(&abs, None::<&str>).map_err(err)
}

/// Reveal `path` in the OS file manager — opens the PARENT and selects the item.
/// `cwd` resolves relative chat paths; absolute paths (e.g. the Inspect working
/// copy) ignore it.
#[tauri::command]
pub fn reveal_path(path: String, cwd: Option<String>) -> Result<(), String> {
    let abs = resolve_chat_path(&path, cwd.as_deref()).map_err(map_resolve_err)?;
    tauri_plugin_opener::reveal_item_in_dir(&abs).map_err(err)
}

/// Why a chat path token couldn't be turned into an openable absolute path.
#[derive(Debug, PartialEq, Eq)]
enum ResolveErr {
    /// The resolved path does not exist on disk.
    NotFound,
    /// The token is relative but no session working directory was supplied.
    RelativeWithoutCwd,
}

fn map_resolve_err(e: ResolveErr) -> String {
    match e {
        // Both surface as "not_found" — the UI just needs "couldn't open it".
        ResolveErr::NotFound | ResolveErr::RelativeWithoutCwd => "not_found".into(),
    }
}

/// Turn a raw path token from chat into an absolute, existing path.
fn resolve_chat_path(token: &str, cwd: Option<&str>) -> Result<PathBuf, ResolveErr> {
    let trimmed = token.trim();
    let unscheme = trimmed.strip_prefix("file://").unwrap_or(trimmed);
    let bare = strip_line_suffix(unscheme);
    let expanded = expand_tilde(bare);

    let abs = if expanded.is_absolute() {
        expanded
    } else {
        match cwd {
            Some(c) if !c.is_empty() => PathBuf::from(c).join(&expanded),
            _ => return Err(ResolveErr::RelativeWithoutCwd),
        }
    };

    if !abs.exists() {
        return Err(ResolveErr::NotFound);
    }
    // Canonicalize for a tidy absolute path; fall back to the lexical join if the
    // OS can't (e.g. permission quirks) — it still exists per the check above.
    Ok(abs.canonicalize().unwrap_or(abs))
}

/// Strip a trailing `:line` or `:line:col` (digits only) editor suffix. Leaves a
/// bare Windows drive letter (`C:` head) and non-numeric suffixes untouched.
fn strip_line_suffix(s: &str) -> &str {
    let mut result = s;
    for _ in 0..2 {
        match result.rfind(':') {
            Some(idx) => {
                let head = &result[..idx];
                let tail = &result[idx + 1..];
                let numeric = !tail.is_empty() && tail.bytes().all(|b| b.is_ascii_digit());
                // `head.len() > 1` rejects a lone drive letter like "C".
                if numeric && head.len() > 1 {
                    result = head;
                } else {
                    break;
                }
            }
            None => break,
        }
    }
    result
}

/// Expand a leading `~` / `~/` to the user's home directory; otherwise verbatim.
fn expand_tilde(s: &str) -> PathBuf {
    if s == "~" {
        return home_dir().unwrap_or_else(|| PathBuf::from(s));
    }
    if let Some(rest) = s.strip_prefix("~/") {
        if let Some(home) = home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(s)
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    }

    #[test]
    fn strips_line_and_col_suffix() {
        assert_eq!(strip_line_suffix("src/foo.ts:42"), "src/foo.ts");
        assert_eq!(strip_line_suffix("src/foo.ts:42:7"), "src/foo.ts");
        assert_eq!(strip_line_suffix("src/foo.ts"), "src/foo.ts");
        assert_eq!(strip_line_suffix("/a/b/c"), "/a/b/c");
    }

    #[test]
    fn keeps_non_numeric_and_drive_suffix() {
        assert_eq!(strip_line_suffix("a:b"), "a:b");
        assert_eq!(strip_line_suffix("C:5"), "C:5"); // lone drive letter, not :line
        assert_eq!(strip_line_suffix("foo:"), "foo:");
    }

    #[test]
    fn expands_tilde() {
        // Read the real home (never mutate the shared process env — other tests
        // depend on HOME, and cargo runs them in the same process in parallel).
        if let Some(home) = home_dir() {
            assert_eq!(expand_tilde("~"), home);
            assert_eq!(expand_tilde("~/a/b"), home.join("a/b"));
        }
        assert_eq!(expand_tilde("plain/rel"), PathBuf::from("plain/rel"));
    }

    #[test]
    fn resolves_absolute_existing() {
        let abs = manifest().join("Cargo.toml");
        let got = resolve_chat_path(abs.to_str().unwrap(), None).unwrap();
        assert!(got.ends_with("Cargo.toml"));
        assert!(got.is_absolute());
    }

    #[test]
    fn resolves_relative_against_cwd() {
        let cwd = manifest();
        let got = resolve_chat_path("Cargo.toml", cwd.to_str()).unwrap();
        assert!(got.ends_with("Cargo.toml"));
    }

    #[test]
    fn strips_file_scheme_and_line_then_resolves() {
        let abs = manifest().join("Cargo.toml");
        let token = format!("file://{}:10", abs.to_str().unwrap());
        let got = resolve_chat_path(&token, None).unwrap();
        assert!(got.ends_with("Cargo.toml"));
    }

    #[test]
    fn missing_path_is_not_found() {
        let got = resolve_chat_path("definitely/not/here.xyz", manifest().to_str());
        assert_eq!(got, Err(ResolveErr::NotFound));
    }

    #[test]
    fn relative_without_cwd_errors() {
        let got = resolve_chat_path("src/foo.ts", None);
        assert_eq!(got, Err(ResolveErr::RelativeWithoutCwd));
    }
}
