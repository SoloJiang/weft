//! OS-level "computer use" core (issue #160, M1: observation only — window
//! enumeration + screenshot, NO input injection). Input injection is a
//! separate, later task (`inject.rs` / `commands.rs` / `engine.rs` stay
//! untouched by this module); everything here is read-only w.r.t. the user's
//! screen.
//!
//! Layout:
//! - This file: the setting gate, the shared error/data types, the
//!   self-exclusion table, and the platform-independent list/match/capture
//!   logic that drives a [`backend::ComputerBackend`].
//! - `backend`: the process-level backend singleton + trait.
//! - `os`: the real platform backend (via the `xcap` crate), feature-gated —
//!   see the `computer-os` feature comment in `Cargo.toml`.
//! - `mock`: a test-only backend (`#[doc(hidden)] pub` so integration tests
//!   under `tests/`, a separate crate, can see it).

pub mod backend;
pub mod mock;
#[cfg(feature = "computer-os")]
mod os;

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// app_setting key gating the whole feature. Value is the literal string
/// "true"/"false"; anything else (including absent) is treated as disabled —
/// see [`enabled`]. Kept as a named const (not a magic string) since both the
/// Settings UI (which writes it) and this module (which reads it) must agree
/// on the exact key.
pub const K_COMPUTER_USE_ENABLED: &str = "computer_use_enabled";

/// Whether computer use is turned on. Fails CLOSED: a DB error, a missing
/// row, or any value other than the literal "true" all read as disabled —
/// this gate gets no benefit of the doubt, unlike most weft settings.
pub async fn enabled(db: &crate::store::Db) -> bool {
    matches!(
        crate::store::repo::get_setting(db, K_COMPUTER_USE_ENABLED).await,
        Ok(Some(v)) if v == "true"
    )
}

/// One discriminated error value for the whole module (CLAUDE.md: derive ONE
/// discriminated value, map it exhaustively) — every fallible computer-use
/// operation returns this, and [`std::fmt::Display`] below is the single
/// place that turns it into the human-readable text the MCP tool result
/// carries back to the calling agent.
#[derive(Debug)]
pub enum ComputerError {
    /// Computer use is turned off in Settings. Callers that already gate on
    /// [`enabled`] before doing any work (the MCP server does) will rarely
    /// construct this themselves; it exists so the fail-closed message has
    /// exactly one authored copy (see the `Display` impl) instead of being a
    /// separately hand-typed literal at every call site.
    Disabled,
    /// No visible, non-excluded window matched the query.
    WindowNotFound { query: String },
    /// More than one visible window matched the query; `candidates` is one
    /// human-readable line per match (`"{app} — {title} (id {id})"`).
    AmbiguousWindow { query: String, candidates: Vec<String> },
    /// The backend found the window but capturing its pixels failed.
    CaptureFailed(String),
    /// The platform/build can't do this at all: no `computer-os` feature,
    /// missing OS permission (e.g. macOS Screen Recording TCC), no display
    /// server, etc. The string is a human-readable reason.
    Unsupported(String),
    /// Reading/writing the screenshot file itself failed.
    Io(String),
}

impl std::fmt::Display for ComputerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ComputerError::Disabled => write!(
                f,
                "computer use is disabled in weft settings — ask the human to enable it in Settings"
            ),
            ComputerError::WindowNotFound { query } => {
                write!(f, "no visible window matched \"{query}\"")
            }
            ComputerError::AmbiguousWindow { query, candidates } => write!(
                f,
                "\"{query}\" matched more than one window — be more specific: {}",
                candidates.join("; ")
            ),
            ComputerError::CaptureFailed(msg) => write!(f, "screenshot capture failed: {msg}"),
            ComputerError::Unsupported(msg) => {
                write!(f, "computer use isn't supported here: {msg}")
            }
            ComputerError::Io(msg) => write!(f, "couldn't save the screenshot: {msg}"),
        }
    }
}

/// One on-screen window, as reported by a [`backend::ComputerBackend`].
#[derive(Debug, Clone, serde::Serialize)]
pub struct WindowInfo {
    pub id: u32,
    pub app: String,
    pub title: String,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

/// A captured window's raw pixels, straight from the backend — RGBA8,
/// row-major, `rgba.len() == width * height * 4`.
#[derive(Debug, Clone)]
pub struct CapturedImage {
    pub rgba: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// A screenshot written to disk.
#[derive(Debug)]
pub struct Screenshot {
    pub path: PathBuf,
    /// The SAVED image's dimensions (post-downscale — see [`screenshot_window`]).
    pub width: u32,
    pub height: u32,
    /// `saved_size / original_size`, e.g. `0.5` when a 2560px-wide capture was
    /// downscaled to 1280px. `1.0` when no downscale happened. A future M2
    /// action layer (click/type coordinates) must divide agent-given
    /// coordinates — expressed against this SAVED image — by this value to
    /// recover real screen coordinates; recorded here so that mapping has
    /// something authoritative to read instead of re-deriving it.
    pub scale: f64,
}

/// Apps excluded from window enumeration/screenshots, for two independent
/// reasons kept in one table since both need the same "never show me this"
/// treatment:
///  (a) Weft itself — an agent screenshotting its own host app is a
///      self-referential capture with no useful signal (and a foot-gun: it
///      could see UI state, like an unrelated Needs-you card, that has
///      nothing to do with the task it's working).
///  (b) terminal emulators — the same self-reference risk one level removed:
///      most of weft's engines run headless inside a terminal-less pty, but a
///      human-launched terminal on the same desktop is showing raw agent
///      output/scrollback, which is a worse, laggier substitute for the
///      transcript weft already gives the agent directly. Mirrors Claude
///      Code's own "Terminal excluded from screenshots" guard for its native
///      computer-use tool.
/// Closed list, lower-cased exact-name comparison (not a substring — "Safari"
/// must not accidentally match nothing here, and "iTerm2 Preferences" is a
/// different app from "iTerm2").
const EXCLUDED_TERMINAL_APPS: &[&str] = &[
    "terminal",
    "iterm2",
    "warp",
    "alacritty",
    "kitty",
    "wezterm",
    "hyper",
    "console",
    "windows terminal",
    "cmd",
    "powershell",
    "gnome-terminal",
    "konsole",
    "xterm",
];

/// Weft's own current process name (lower-cased), best-effort. `None` if it
/// can't be resolved (e.g. `/proc/self/exe` unreadable) — the literal
/// `"weft"` check in [`is_excluded`] still covers the common case then.
fn current_process_name() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    let stem = exe.file_stem()?;
    Some(stem.to_string_lossy().to_ascii_lowercase())
}

/// Whether `app` (a window's reported app name) must never be listed or
/// screenshotted. See [`EXCLUDED_TERMINAL_APPS`] for why.
pub fn is_excluded(app: &str) -> bool {
    let lower = app.to_ascii_lowercase();
    if lower == "weft" {
        return true;
    }
    if let Some(me) = current_process_name() {
        if lower == me {
            return true;
        }
    }
    EXCLUDED_TERMINAL_APPS.contains(&lower.as_str())
}

/// The windows an agent is allowed to see: the backend's raw list, minus
/// self-exclusions ([`is_excluded`]) and minus zero-sized windows (minimized/
/// off-screen placeholders a real desktop reports but that carry no useful
/// pixels).
pub fn visible_windows(backend: &dyn backend::ComputerBackend) -> Result<Vec<WindowInfo>, ComputerError> {
    let windows = backend.list_windows()?;
    Ok(windows
        .into_iter()
        .filter(|w| !is_excluded(&w.app))
        .filter(|w| w.width > 0 && w.height > 0)
        .collect())
}

/// One human-readable candidate line for an [`ComputerError::AmbiguousWindow`].
fn candidate_label(w: &WindowInfo) -> String {
    format!("{} — {} (id {})", w.app, w.title, w.id)
}

/// Resolve `query` against `windows`: a purely-numeric query matches by exact
/// `id`; anything else is a case-insensitive substring match against `app`
/// OR `title`. Both branches search the SAME (already-filtered) list, so an
/// id match still has to be one of the visible, non-excluded windows.
fn match_windows<'a>(windows: &'a [WindowInfo], query: &str) -> Vec<&'a WindowInfo> {
    if let Ok(id) = query.trim().parse::<u32>() {
        return windows.iter().filter(|w| w.id == id).collect();
    }
    let needle = query.to_ascii_lowercase();
    windows
        .iter()
        .filter(|w| {
            w.app.to_ascii_lowercase().contains(&needle) || w.title.to_ascii_lowercase().contains(&needle)
        })
        .collect()
}

/// Downscale `rgba`/`width`/`height` so the long edge is at most 1280px
/// (`FilterType::Triangle` — cheap and good enough for a viewer, not a
/// pixel-perfect archival need). Returns the (possibly unchanged) image plus
/// the scale factor applied (`1.0` when no downscale was needed).
fn scale_capture(
    rgba: Vec<u8>,
    width: u32,
    height: u32,
) -> Result<(image::RgbaImage, f64), ComputerError> {
    let buf = image::RgbaImage::from_raw(width, height, rgba).ok_or_else(|| {
        ComputerError::CaptureFailed("captured pixel buffer doesn't match its reported size".into())
    })?;
    const MAX_LONG_EDGE: u32 = 1280;
    let long_edge = width.max(height);
    if long_edge <= MAX_LONG_EDGE {
        return Ok((buf, 1.0));
    }
    let scale = f64::from(MAX_LONG_EDGE) / f64::from(long_edge);
    let new_width = ((f64::from(width) * scale).round() as u32).max(1);
    let new_height = ((f64::from(height) * scale).round() as u32).max(1);
    let resized = image::imageops::resize(&buf, new_width, new_height, image::imageops::FilterType::Triangle);
    Ok((resized, scale))
}

/// Match `query` against the backend's visible windows, capture the ONE hit,
/// downscale it (see [`scale_capture`]), and write it to
/// `out_dir/<unix_ms>-<id>.png` (`out_dir` is created if missing).
pub fn screenshot_window(
    backend: &dyn backend::ComputerBackend,
    query: &str,
    out_dir: &Path,
) -> Result<Screenshot, ComputerError> {
    let windows = visible_windows(backend)?;
    let matched = match match_windows(&windows, query).as_slice() {
        [] => {
            return Err(ComputerError::WindowNotFound {
                query: query.to_string(),
            })
        }
        [one] => *one,
        many => {
            return Err(ComputerError::AmbiguousWindow {
                query: query.to_string(),
                candidates: many.iter().map(|&w| candidate_label(w)).collect(),
            })
        }
    };

    let captured = backend.capture_window(matched.id)?;
    let (image, scale) = scale_capture(captured.rgba, captured.width, captured.height)?;

    std::fs::create_dir_all(out_dir).map_err(|e| ComputerError::Io(e.to_string()))?;
    let unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| ComputerError::Io(e.to_string()))?
        .as_millis();
    let path = out_dir.join(format!("{unix_ms}-{}.png", matched.id));
    image.save(&path).map_err(|e| ComputerError::Io(e.to_string()))?;

    Ok(Screenshot {
        path,
        width: image.width(),
        height: image.height(),
        scale,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Db;

    fn window(id: u32, app: &str, title: &str) -> WindowInfo {
        WindowInfo {
            id,
            app: app.to_string(),
            title: title.to_string(),
            x: 0,
            y: 0,
            width: 800,
            height: 600,
        }
    }

    fn solid_image(width: u32, height: u32, byte: u8) -> CapturedImage {
        CapturedImage {
            rgba: vec![byte; (width * height * 4) as usize],
            width,
            height,
        }
    }

    // —— exclusion table ——

    #[test]
    fn weft_and_current_process_and_iterm_are_excluded() {
        assert!(is_excluded("Weft"));
        assert!(is_excluded("WEFT"));
        assert!(is_excluded("iTerm2"));
        let me = current_process_name().expect("test binary has a resolvable exe path");
        assert!(is_excluded(&me));
    }

    #[test]
    fn unrelated_app_is_not_excluded() {
        assert!(!is_excluded("Safari"));
    }

    // —— matching ——

    #[test]
    fn match_by_exact_id() {
        let windows = vec![window(1, "Safari", "Apple"), window(2, "Notes", "Untitled")];
        let hits = match_windows(&windows, "2");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, 2);
    }

    #[test]
    fn match_by_case_insensitive_substring() {
        let windows = vec![window(1, "Safari", "Apple — Home")];
        let hits = match_windows(&windows, "safari");
        assert_eq!(hits.len(), 1);
        let hits_title = match_windows(&windows, "HOME");
        assert_eq!(hits_title.len(), 1);
    }

    #[test]
    fn zero_hits_is_window_not_found() {
        let backend = mock::MockBackend {
            windows: vec![window(1, "Safari", "Apple")],
            image: None,
        };
        let dir = std::env::temp_dir().join("weft-computer-test-zero-hits");
        let err = screenshot_window(&backend, "nonexistent", &dir).unwrap_err();
        assert!(matches!(err, ComputerError::WindowNotFound { query } if query == "nonexistent"));
    }

    #[test]
    fn multiple_hits_is_ambiguous_with_candidates() {
        let backend = mock::MockBackend {
            windows: vec![window(1, "Notes", "Untitled 1"), window(2, "Notes", "Untitled 2")],
            image: None,
        };
        let dir = std::env::temp_dir().join("weft-computer-test-ambiguous");
        let err = screenshot_window(&backend, "notes", &dir).unwrap_err();
        match err {
            ComputerError::AmbiguousWindow { query, candidates } => {
                assert_eq!(query, "notes");
                assert_eq!(candidates.len(), 2);
                assert!(candidates[0].contains("Notes — Untitled 1 (id 1)"));
                assert!(candidates[1].contains("Notes — Untitled 2 (id 2)"));
            }
            other => panic!("expected AmbiguousWindow, got {other}"),
        }
    }

    // —— scaling ——

    #[test]
    fn oversized_capture_downscales_to_1280_long_edge() {
        let (img, scale) = scale_capture(vec![7u8; 2560 * 1440 * 4], 2560, 1440).unwrap();
        assert_eq!((img.width(), img.height()), (1280, 720));
        assert_eq!(scale, 0.5);
    }

    #[test]
    fn small_capture_is_not_scaled() {
        let (img, scale) = scale_capture(vec![7u8; 800 * 600 * 4], 800, 600).unwrap();
        assert_eq!((img.width(), img.height()), (800, 600));
        assert_eq!(scale, 1.0);
    }

    // —— file write ——

    #[test]
    fn screenshot_window_writes_a_real_png() {
        let backend = mock::MockBackend {
            windows: vec![window(9, "Notes", "Untitled")],
            image: Some(solid_image(2560, 1440, 200)),
        };
        let tmp = tempfile::tempdir().unwrap();
        let shot = screenshot_window(&backend, "9", tmp.path()).unwrap();
        assert!(shot.path.exists());
        assert_eq!(shot.width, 1280);
        assert_eq!(shot.height, 720);
        assert_eq!(shot.scale, 0.5);
        let opened = image::open(&shot.path).unwrap();
        assert_eq!((opened.width(), opened.height()), (1280, 720));
    }

    // —— enabled() ——

    #[tokio::test]
    async fn enabled_reads_true_false_and_missing() {
        let db = Db::connect("sqlite::memory:").await.unwrap();
        assert!(!enabled(&db).await, "missing setting must fail closed");

        crate::store::repo::set_setting(&db, K_COMPUTER_USE_ENABLED, "true")
            .await
            .unwrap();
        assert!(enabled(&db).await);

        crate::store::repo::set_setting(&db, K_COMPUTER_USE_ENABLED, "false")
            .await
            .unwrap();
        assert!(!enabled(&db).await);
    }
}
