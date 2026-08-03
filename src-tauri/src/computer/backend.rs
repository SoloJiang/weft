//! The process-level backend that serves window enumeration + capture.
//!
//! Exactly one backend serves the whole process: the real platform
//! implementation (`super::os::OsBackend`, only compiled with the
//! `computer-os` cargo feature) or, without that feature, a stub that always
//! reports [`super::ComputerError::Unsupported`]. Tests swap in
//! [`super::mock::MockBackend`] via [`_set_backend_override`] — a
//! production code path must never call it (see its own doc comment).

use super::{CapturedImage, ComputerError, WindowInfo};
use std::sync::{Arc, OnceLock};

/// Which physical mouse button a click/drag drives. Only the
/// two buttons the MCP surface exposes (`left_click`/`right_click`) — not a
/// stand-in for every button `enigo::Button` knows about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Right,
}

/// Window enumeration + capture + input injection, abstracted so the
/// platform-independent logic in `computer/mod.rs` (matching, scaling,
/// coordinate mapping, file I/O) never touches a real OS API directly and
/// can be driven by [`super::mock::MockBackend`] in tests. The input methods
///  all take PHYSICAL screen coordinates — a caller maps an
/// agent-given, screenshot-space coordinate through
/// [`super::map_to_physical`] BEFORE calling any of them; the backend itself
/// never rescales.
pub trait ComputerBackend: Send + Sync {
    fn list_windows(&self) -> Result<Vec<WindowInfo>, ComputerError>;
    /// Capture `target`, verifying its FULL identity — `id` AND `app`/`title`
    /// — on the exact window about to be captured
    /// . This method performs its own enumeration to find the
    /// window handle, and an OS window id is reusable: selecting by `id`
    /// alone let a window that closed after the caller's own
    /// resolve-and-verify be silently replaced by whatever unrelated (or
    /// deliberately excluded) window inherited its number in the gap, its
    /// pixels then saved/previewed/returned under the ORIGINAL approval. An
    /// identity mismatch fails closed with `CaptureFailed`, disclosing
    /// nothing about the replacement.
    fn capture_window(&self, target: &WindowInfo) -> Result<CapturedImage, ComputerError>;
    /// Click at a physical `(x, y)`, `count` times in a row (2 = double,
    /// 3 = triple) — the caller does NOT need to move the cursor there
    /// first, this moves it as part of the click.
    fn click(&self, x: i32, y: i32, button: MouseButton, count: u32) -> Result<(), ComputerError>;
    fn type_text(&self, text: &str) -> Result<(), ComputerError>;
    /// `combo` is a raw string like `"cmd+s"` / `"ctrl+shift+t"` / `"Return"`
    /// — see [`super::parse_key_combo`] for the grammar every implementation
    /// must parse it with (the real backend maps the parsed tokens to its
    /// own key type; [`super::mock::MockBackend`] just records the raw
    /// string).
    fn key(&self, combo: &str) -> Result<(), ComputerError>;
    /// Move the cursor to physical `(x, y)` first, then scroll by `(dx, dy)`.
    fn scroll(&self, x: i32, y: i32, dx: i32, dy: i32) -> Result<(), ComputerError>;
    /// Press-move-release: mouse down at `from` (physical), move to `to`
    /// (physical), mouse up.
    fn drag(&self, from: (i32, i32), to: (i32, i32)) -> Result<(), ComputerError>;
    fn move_cursor(&self, x: i32, y: i32) -> Result<(), ComputerError>;
    /// The cursor's CURRENT physical position — independent of any window
    /// (there is exactly one system cursor), so this never needs a window
    /// query.
    fn cursor_position(&self) -> Result<(i32, i32), ComputerError>;
    /// Raise/focus `target`, verifying its FULL identity — `id` AND
    /// `app`/`title` — as close to the raise as the platform allows (issue
    /// the capture-side verification): activation addresses
    /// the window by its reusable numeric id, so an id-only boundary could
    /// raise and focus whatever unrelated application inherited the number
    /// after the caller's own resolve-and-verify — a foreground-steal side
    /// effect the caller's post-activation re-resolve only catches AFTER it
    /// happened. Same discipline as [`Self::capture_window`].
    /// Bring window `id` to the foreground and give it OS keyboard/mouse
    /// focus — the ONLY mechanism
    /// `bus::computer_srv::activate_if_interactive` uses to hand focus back
    /// to a target window after a Needs-you approval card took the
    /// foreground away from it. Replaces the earlier "replay the last click"
    /// hack: an absolute-coordinate replay risks landing on whatever now
    /// covers that physical point (Weft's own card, if it's still up), and
    /// has its own side effects (collapsing a text selection, re-toggling a
    /// button) a pure window-raise does not. `Err(ComputerError::Unsupported)`
    /// when the backend has no way to do this at all (`StubBackend`'s
    /// permanent case; a real backend that couldn't find any activation API
    /// on this platform) — callers fail closed on that, never falling
    /// through to drive input into a window that may not actually have the
    /// foreground.
    fn activate_window(&self, target: &WindowInfo) -> Result<(), ComputerError>;
}

/// Test-only override, set at most once per process (see
/// [`_set_backend_override`]).
static OVERRIDE: OnceLock<Arc<dyn ComputerBackend>> = OnceLock::new();
/// The real, lazily-built backend singleton.
static REAL: OnceLock<Arc<dyn ComputerBackend>> = OnceLock::new();

/// The process-wide backend: whatever a test installed via
/// [`_set_backend_override`], else the real platform backend (built once,
/// reused for the life of the process).
pub fn backend() -> Arc<dyn ComputerBackend> {
    if let Some(b) = OVERRIDE.get() {
        return b.clone();
    }
    REAL.get_or_init(build_real_backend).clone()
}

#[cfg(feature = "computer-os")]
fn build_real_backend() -> Arc<dyn ComputerBackend> {
    Arc::new(super::os::OsBackend)
}

#[cfg(not(feature = "computer-os"))]
fn build_real_backend() -> Arc<dyn ComputerBackend> {
    Arc::new(StubBackend)
}

/// Built without the `computer-os` cargo feature (the default — see the
/// dependency comment on `xcap` in `Cargo.toml`): every call fails closed
/// with `Unsupported` instead of silently doing nothing or, worse, panicking
/// on a platform API that was never linked in.
#[cfg(not(feature = "computer-os"))]
struct StubBackend;

#[cfg(not(feature = "computer-os"))]
impl ComputerBackend for StubBackend {
    fn list_windows(&self) -> Result<Vec<WindowInfo>, ComputerError> {
        Err(ComputerError::Unsupported(
            "built without computer-os feature".into(),
        ))
    }

    fn capture_window(&self, _target: &WindowInfo) -> Result<CapturedImage, ComputerError> {
        Err(ComputerError::Unsupported(
            "built without computer-os feature".into(),
        ))
    }

    fn click(&self, _x: i32, _y: i32, _button: MouseButton, _count: u32) -> Result<(), ComputerError> {
        Err(ComputerError::Unsupported(
            "built without computer-os feature".into(),
        ))
    }

    fn type_text(&self, _text: &str) -> Result<(), ComputerError> {
        Err(ComputerError::Unsupported(
            "built without computer-os feature".into(),
        ))
    }

    fn key(&self, _combo: &str) -> Result<(), ComputerError> {
        Err(ComputerError::Unsupported(
            "built without computer-os feature".into(),
        ))
    }

    fn scroll(&self, _x: i32, _y: i32, _dx: i32, _dy: i32) -> Result<(), ComputerError> {
        Err(ComputerError::Unsupported(
            "built without computer-os feature".into(),
        ))
    }

    fn drag(&self, _from: (i32, i32), _to: (i32, i32)) -> Result<(), ComputerError> {
        Err(ComputerError::Unsupported(
            "built without computer-os feature".into(),
        ))
    }

    fn move_cursor(&self, _x: i32, _y: i32) -> Result<(), ComputerError> {
        Err(ComputerError::Unsupported(
            "built without computer-os feature".into(),
        ))
    }

    fn cursor_position(&self) -> Result<(i32, i32), ComputerError> {
        Err(ComputerError::Unsupported(
            "built without computer-os feature".into(),
        ))
    }

    fn activate_window(&self, _target: &WindowInfo) -> Result<(), ComputerError> {
        Err(ComputerError::Unsupported(
            "window activation needs the computer-os backend".into(),
        ))
    }
}

/// Test-only backend injection point. NEVER call this from production code:
/// it permanently overrides the process-wide backend for the rest of the
/// process's life (backed by the same kind of `OnceLock` singleton as
/// [`backend`] itself — the whole point is "one backend for the process",
/// and this is the one deliberate escape hatch from that for tests).
/// `#[doc(hidden)] pub` rather than `#[cfg(test)]` because integration tests
/// under `tests/` compile as a separate crate and cannot see a
/// `cfg(test)`-gated item in this library.
#[doc(hidden)]
pub fn _set_backend_override(b: Arc<dyn ComputerBackend>) {
    let _ = OVERRIDE.set(b);
}
