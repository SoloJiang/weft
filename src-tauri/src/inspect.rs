//! Escape-hatch commands for the Inspect surface (architecture §4.7) and for
//! file references inside chat: the product hides plumbing (worktree paths,
//! branches) but ALWAYS offers a real way out — open the working copy in a
//! terminal/file manager, or open a file the agent mentioned with the OS default
//! app. Opening files/URLs + revealing go through `tauri-plugin-opener`, so they
//! work on macOS/Windows/Linux. (`open_terminal` is still macOS-only — later pass.)

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

/// Open a URL or app deep link with the OS handler — `https://…`, app schemes
/// like `ms-settings:…`/`vscode-insiders://…`, or `codex://threads/<id>` to jump
/// to a session in the Codex app. Cross-platform via the opener plugin.
#[tauri::command]
pub fn open_url(url: String) -> Result<(), String> {
    tauri_plugin_opener::open_url(url, None::<&str>).map_err(err)
}

/// Open a file the agent referenced in chat with the OS default app. `path` may
/// be relative (resolved against the session `cwd`), absolute, `~`-prefixed, or a
/// `file://` URI, and may carry a trailing `:line[:col]` (stripped — the default
/// app can't seek). Errors `"not_found"` so the UI can show a quiet toast.
#[tauri::command]
pub fn open_path(path: String, cwd: Option<String>, is_url: bool) -> Result<(), String> {
    let abs = resolve_chat_path(&path, cwd.as_deref(), is_url).map_err(map_resolve_err)?;
    tauri_plugin_opener::open_path(&abs, None::<&str>).map_err(err)
}

/// Reveal a file the agent referenced in chat — resolves the token (relative to
/// `cwd`) like `open_path`, then selects it in the OS file manager.
#[tauri::command]
pub fn reveal_path_in(path: String, cwd: Option<String>, is_url: bool) -> Result<(), String> {
    let abs = resolve_chat_path(&path, cwd.as_deref(), is_url).map_err(map_resolve_err)?;
    tauri_plugin_opener::reveal_item_in_dir(&abs).map_err(err)
}

/// Reveal a real, already-resolved filesystem path (the Inspect working copy):
/// opens the PARENT and selects the item. The path is taken verbatim — it is NOT
/// a chat URI token, so NO scheme/percent/fragment/`:line` normalization runs
/// (a worktree under e.g. `/work/C#Service` must reveal as-is).
#[tauri::command]
pub fn reveal_path(path: String) -> Result<(), String> {
    if !std::path::Path::new(&path).exists() {
        return Err("not_found".into());
    }
    tauri_plugin_opener::reveal_item_in_dir(&path).map_err(err)
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
fn resolve_chat_path(token: &str, cwd: Option<&str>, is_url: bool) -> Result<PathBuf, ResolveErr> {
    let trimmed = token.trim();
    // URL tokens (markdown link hrefs) carry URI syntax: a `file://` scheme +
    // optional `localhost` authority, a `#fragment`/`?query`, percent-encoding,
    // and the `/C:/…` drive form. Bare prose / inline-code tokens are LITERAL
    // filesystem paths, where `#` and `%` are valid filename characters — so URI
    // normalization runs ONLY for URL tokens.
    let decoded: String = if is_url {
        let body = strip_fragment(strip_file_scheme(trimmed));
        strip_leading_drive_slash(percent_decode(body))
    } else {
        trimmed.to_string()
    };
    let bare = strip_line_suffix(&decoded);
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
    // Return the LEXICAL absolute path — do NOT canonicalize. Canonicalizing
    // resolves symlinks, so revealing/opening a symlinked ref would jump to the
    // target (possibly outside the repo) instead of the path the agent named.
    // (tauri-plugin-opener keeps symlinks too — it uses dunce::simplified.)
    Ok(abs)
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

/// Expand a leading `~`, `~/`, or `~\` to the user's home directory; else verbatim.
fn expand_tilde(s: &str) -> PathBuf {
    if s == "~" {
        return home_dir().unwrap_or_else(|| PathBuf::from(s));
    }
    if let Some(rest) = s.strip_prefix("~/").or_else(|| s.strip_prefix("~\\")) {
        if let Some(home) = home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(s)
}

/// Strip a `file:` scheme, an optional `//` authority marker, and an optional
/// `localhost` host, leaving the path body. Handles all RFC 8089 forms:
/// `file:///p` → `/p`, `file://localhost/p` → `/p`, and the minimal `file:/p`
/// → `/p`. The scheme + host match case-insensitively. Non-URI tokens (and a
/// relative `localhost/…` dir) are returned untouched.
fn strip_file_scheme(token: &str) -> &str {
    let after_scheme = match token.get(..5) {
        Some(prefix) if prefix.eq_ignore_ascii_case("file:") => &token[5..],
        _ => return token,
    };
    // Optional `//` authority marker (present in file:// / file:///, absent in file:/).
    let rest = after_scheme.strip_prefix("//").unwrap_or(after_scheme);
    // The host authority is case-insensitive: strip a `localhost` host (any case)
    // when it's followed by the absolute path, e.g. `file://LOCALHOST/Users/…`.
    match rest.get(..9) {
        Some(host) if host.eq_ignore_ascii_case("localhost") && rest[9..].starts_with('/') => {
            &rest[9..]
        }
        _ => rest,
    }
}

/// Drop a trailing URL fragment/query (`#L42`, `#usage`, `?v=1`). The opener
/// can't act on it and it would otherwise break the existence check.
fn strip_fragment(s: &str) -> &str {
    match s.find(['#', '?']) {
        Some(i) => &s[..i],
        None => s,
    }
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

/// Decode `%XX` percent-escapes in a `file://` URI body. Lone/invalid `%`
/// sequences are passed through untouched.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                out.push(hi * 16 + lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Drop a leading `/` before a Windows drive letter (`/C:/…` → `C:/…`) — the
/// form `file:///C:/…` leaves once the scheme is stripped. No-op for POSIX paths.
fn strip_leading_drive_slash(s: String) -> String {
    let b = s.as_bytes();
    if b.len() >= 4
        && b[0] == b'/'
        && b[1].is_ascii_alphabetic()
        && b[2] == b':'
        && (b[3] == b'/' || b[3] == b'\\')
    {
        return s[1..].to_string();
    }
    s
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
            assert!(expand_tilde("~\\a").starts_with(&home)); // Windows-style ~\
        }
        assert_eq!(expand_tilde("plain/rel"), PathBuf::from("plain/rel"));
    }

    #[test]
    fn resolves_absolute_existing() {
        let abs = manifest().join("Cargo.toml");
        let got = resolve_chat_path(abs.to_str().unwrap(), None, true).unwrap();
        assert!(got.ends_with("Cargo.toml"));
        assert!(got.is_absolute());
    }

    #[test]
    fn resolves_relative_against_cwd() {
        let cwd = manifest();
        let got = resolve_chat_path("Cargo.toml", cwd.to_str(), false).unwrap();
        assert!(got.ends_with("Cargo.toml"));
    }

    #[test]
    fn strips_file_scheme_and_line_then_resolves() {
        let abs = manifest().join("Cargo.toml");
        let token = format!("file://{}:10", abs.to_str().unwrap());
        let got = resolve_chat_path(&token, None, true).unwrap();
        assert!(got.ends_with("Cargo.toml"));
    }

    #[test]
    fn decodes_percent_escapes() {
        assert_eq!(percent_decode("My%20Repo/App.tsx"), "My Repo/App.tsx");
        assert_eq!(percent_decode("a%2Fb"), "a/b");
        assert_eq!(percent_decode("plain"), "plain");
        assert_eq!(percent_decode("bad%2"), "bad%2"); // truncated escape passes through
        assert_eq!(percent_decode("100%"), "100%");
    }

    #[test]
    fn decodes_file_uri_then_resolves() {
        let dir = manifest();
        // Encode the leading 'C' of Cargo.toml as %43 to exercise URI decoding.
        let token = format!("file://{}/%43argo.toml", dir.to_str().unwrap());
        let got = resolve_chat_path(&token, None, true).unwrap();
        assert!(got.ends_with("Cargo.toml"));
    }

    #[test]
    fn decodes_percent_escapes_in_url_token() {
        // A non-`file://` link href is still URL-encoded — must decode when is_url.
        let dir = manifest();
        let token = format!("{}/%43argo.toml", dir.to_str().unwrap());
        let got = resolve_chat_path(&token, None, true).unwrap();
        assert!(got.ends_with("Cargo.toml"));
    }

    #[test]
    fn literal_paths_keep_hash_but_urls_strip_fragment() {
        // A real file whose name contains '#': literal tokens must keep it; URL
        // tokens treat '#' as a fragment. (No cross-test env mutation — temp file.)
        let p = std::env::temp_dir().join("weft_test_a#b.txt");
        std::fs::write(&p, b"x").unwrap();
        let token = p.to_str().unwrap();
        assert!(resolve_chat_path(token, None, false).is_ok()); // literal → found
        assert_eq!(resolve_chat_path(token, None, true), Err(ResolveErr::NotFound)); // url → '#' fragment
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn strips_leading_drive_slash() {
        assert_eq!(
            strip_leading_drive_slash("/C:/repo/main.rs".into()),
            "C:/repo/main.rs"
        );
        assert_eq!(strip_leading_drive_slash("/Users/me/x".into()), "/Users/me/x");
        assert_eq!(strip_leading_drive_slash("relative/x".into()), "relative/x");
    }

    #[test]
    fn strips_file_scheme_and_localhost_authority() {
        assert_eq!(strip_file_scheme("file:///Users/me/x"), "/Users/me/x");
        assert_eq!(strip_file_scheme("file:/tmp/App.tsx"), "/tmp/App.tsx"); // minimal RFC 8089 form
        assert_eq!(strip_file_scheme("FILE:/tmp/x"), "/tmp/x"); // minimal + case-insensitive
        assert_eq!(strip_file_scheme("file://localhost/Users/me/x"), "/Users/me/x");
        assert_eq!(strip_file_scheme("file://localhost/C:/repo"), "/C:/repo");
        assert_eq!(strip_file_scheme("FILE:///tmp/x"), "/tmp/x"); // scheme is case-insensitive
        assert_eq!(strip_file_scheme("File://localhost/y"), "/y");
        assert_eq!(strip_file_scheme("file://LOCALHOST/Users/me/x"), "/Users/me/x"); // host case-insensitive
        assert_eq!(strip_file_scheme("file://localhostish/x"), "localhostish/x"); // not the localhost host
        assert_eq!(strip_file_scheme("localhost/foo"), "localhost/foo"); // non-URI, untouched
        assert_eq!(strip_file_scheme("/plain/path"), "/plain/path");
    }

    #[test]
    fn strips_url_fragment_and_query() {
        assert_eq!(strip_fragment("README.md#usage"), "README.md");
        assert_eq!(strip_fragment("/repo/App.tsx#L42"), "/repo/App.tsx");
        assert_eq!(strip_fragment("a/b?v=1"), "a/b");
        assert_eq!(strip_fragment("plain/path.rs"), "plain/path.rs");
    }

    #[test]
    fn missing_path_is_not_found() {
        let got = resolve_chat_path("definitely/not/here.xyz", manifest().to_str(), false);
        assert_eq!(got, Err(ResolveErr::NotFound));
    }

    #[test]
    fn relative_without_cwd_errors() {
        let got = resolve_chat_path("src/foo.ts", None, false);
        assert_eq!(got, Err(ResolveErr::RelativeWithoutCwd));
    }
}
