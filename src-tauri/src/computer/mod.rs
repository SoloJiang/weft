//! OS-level "computer use" core (issue #160). M1 shipped observation only
//! (window enumeration + screenshot); M2 adds input injection — click/type/
//! key/scroll/drag/move — behind the SAME setting gate, plus a process-wide
//! control lock, an input throttle, and a coordinate-mapping layer so an
//! agent's screenshot-space clicks land on the right physical pixel. This
//! module (and `bus/computer_srv.rs`, which drives it) never touches
//! `inject.rs` / `commands.rs` / `engine.rs` — those back a DIFFERENT
//! feature (an engine's own PTY input), not this one.
//!
//! Layout:
//! - This file: the setting gate, the shared error/data types, the
//!   self-exclusion table, the platform-independent list/match/capture/
//!   coordinate-mapping logic that drives a [`backend::ComputerBackend`],
//!   the key-combo grammar ([`parse_key_combo`]), and the process-level
//!   control lock / input throttle / emergency-stop API.
//! - `backend`: the process-level backend singleton + trait.
//! - `os`: the real platform backend (via the `xcap` crate for windows and
//!   the `enigo` crate for input), feature-gated — see the `computer-os`
//!   feature comment in `Cargo.toml`.
//! - `mock`: a test-only backend (`#[doc(hidden)] pub` so integration tests
//!   under `tests/`, a separate crate, can see it).

pub mod backend;
pub mod mock;
#[cfg(feature = "computer-os")]
mod os;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// app_setting key gating the whole feature. Value is the literal string
/// "true"/"false"; anything else (including absent) is treated as disabled —
/// see [`enabled`]. Kept as a named const (not a magic string) since both the
/// Settings UI (which writes it) and this module (which reads it) must agree
/// on the exact key.
pub const K_COMPUTER_USE_ENABLED: &str = "computer_use_enabled";

/// Whether computer use is turned on. Fails CLOSED: a DB error, a missing
/// row, or any value other than the literal "true" all read as disabled —
/// this gate gets no benefit of the doubt, unlike most weft settings.
///
/// issue #160 review R1 #1: the emergency-stop latch ([`EMERGENCY_STOPPED`])
/// is checked FIRST, before the DB is even touched — once tripped, this
/// returns `false` unconditionally, so a `set_setting` write failure inside
/// [`emergency_stop`] (which the latch does NOT depend on — see that
/// function's doc) can never leave the kill switch silently fail-open
/// because the setting row still reads "true".
pub async fn enabled(db: &crate::store::Db) -> bool {
    if EMERGENCY_STOPPED.load(Ordering::SeqCst) {
        return false;
    }
    matches!(
        crate::store::repo::get_setting(db, K_COMPUTER_USE_ENABLED).await,
        Ok(Some(v)) if v == "true"
    )
}

/// Process-level emergency-stop latch (issue #160 review R1 #1). Once
/// tripped by [`emergency_stop`], [`enabled`] returns `false`
/// UNCONDITIONALLY, before it even reads the `computer_use_enabled` DB
/// setting — so this is the actual fail-closed mechanism the kill switch
/// relies on, and the DB write in [`emergency_stop`] is best-effort
/// persistence for the NEXT launch, not something [`enabled`] depends on
/// within THIS process's life. The ONLY function allowed to clear it is
/// [`clear_emergency_stop`], itself called from exactly one place —
/// `commands::set_computer_use_enabled` when a human explicitly re-enables
/// computer use from Settings. `Ordering::SeqCst` throughout: this gates
/// every input action process-wide, so the extra fence cost over a looser
/// ordering is nothing next to a kill switch that could otherwise be
/// reordered into fail-open.
static EMERGENCY_STOPPED: AtomicBool = AtomicBool::new(false);

/// Clear the emergency-stop latch (issue #160 review R1 #1). The ONLY
/// legitimate caller is `commands::set_computer_use_enabled` when `enabled
/// == true` — a human explicitly turning computer use back on from Settings
/// after a kill switch trip. Nothing else in this codebase may call this:
/// there is deliberately no other path back to "computer use may run again"
/// once the latch is tripped.
pub fn clear_emergency_stop() {
    EMERGENCY_STOPPED.store(false, Ordering::SeqCst);
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
    /// An input action's coordinate falls outside the most recent
    /// screenshot's range for this window (issue #160 M2) — see
    /// [`map_to_physical`]. `max_x`/`max_y` are the LAST valid coordinate
    /// (inclusive) — the valid range is `0..=max_x` / `0..=max_y`, NOT the
    /// screenshot's width/height (issue #160 review R1 #2: the upper bound
    /// is exclusive, so `x == width` is one past the last real pixel column
    /// and is itself out of bounds).
    OutOfBounds { x: u32, y: u32, max_x: u32, max_y: u32 },
    /// Someone else's session currently holds the control lease (issue #160
    /// M2) — see [`acquire_control`].
    Busy { thread: i32, dir: String },
    /// The global input throttle rejected this call (issue #160 M2) — see
    /// [`throttle_input`].
    RateLimited { wait_ms: u64 },
    /// An open permission Ask is blocking this (thread, dir) from driving
    /// the desktop (issue #160 M2) — checked in `bus::computer_srv` before
    /// any input action runs.
    SuspendedPendingAsk,
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
            ComputerError::OutOfBounds { x, y, max_x, max_y } => write!(
                f,
                "coordinate ({x}, {y}) is out of bounds — it must fall within the most recent screenshot of this window, 0..={max_x} x 0..={max_y}"
            ),
            ComputerError::Busy { thread, dir } => write!(
                f,
                "another session is controlling the desktop (thread {thread}, dir {dir}) — wait for its control lease to expire and try again"
            ),
            ComputerError::RateLimited { wait_ms } => write!(
                f,
                "computer input is rate-limited — wait {wait_ms}ms and try again"
            ),
            ComputerError::SuspendedPendingAsk => write!(
                f,
                "a permission card is waiting for the human — input is suspended until it is answered"
            ),
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

/// Match `query` against the backend's visible windows down to exactly ONE
/// hit — the same rule [`screenshot_window`] and every window-scoped M2
/// input action share, so a `screenshot`/`left_click`/`type`/... call all
/// report the identical `WindowNotFound`/`AmbiguousWindow` wording for the
/// same query (single source, per this module's exclusion-table precedent).
pub fn resolve_window(backend: &dyn backend::ComputerBackend, query: &str) -> Result<WindowInfo, ComputerError> {
    let windows = visible_windows(backend)?;
    match match_windows(&windows, query).as_slice() {
        [] => Err(ComputerError::WindowNotFound {
            query: query.to_string(),
        }),
        [one] => Ok((*one).clone()),
        many => Err(ComputerError::AmbiguousWindow {
            query: query.to_string(),
            candidates: many.iter().map(|&w| candidate_label(w)).collect(),
        }),
    }
}

/// The downscale factor a screenshot of `w` is saved at, and — because
/// [`map_to_physical`] recomputes the SAME formula from the SAME window
/// fields — the exact factor an agent-given coordinate against that
/// screenshot must be divided by to recover a real screen position. Pulled
/// out as its own function (issue #160 M2) specifically so those two call
/// sites can never drift onto two different formulas: long edge > 1280px
/// gets scaled down to fit (matching [`scale_capture`]'s pre-M2 rule
/// exactly), everything else is 1:1.
pub fn display_scale(w: &WindowInfo) -> f64 {
    const MAX_LONG_EDGE: u32 = 1280;
    let long_edge = w.width.max(w.height);
    if long_edge <= MAX_LONG_EDGE {
        return 1.0;
    }
    f64::from(MAX_LONG_EDGE) / f64::from(long_edge)
}

/// Downscale `rgba`/`width`/`height` by `scale` (`1.0` is a no-op —
/// `FilterType::Triangle`, cheap and good enough for a viewer, not a
/// pixel-perfect archival need). `scale` MUST be [`display_scale`] applied
/// to the window this capture came from — see that function's doc comment
/// for why this takes the scale as an input instead of deriving its own.
fn scale_capture(rgba: Vec<u8>, width: u32, height: u32, scale: f64) -> Result<image::RgbaImage, ComputerError> {
    let buf = image::RgbaImage::from_raw(width, height, rgba).ok_or_else(|| {
        ComputerError::CaptureFailed("captured pixel buffer doesn't match its reported size".into())
    })?;
    if scale >= 1.0 {
        return Ok(buf);
    }
    let new_width = ((f64::from(width) * scale).round() as u32).max(1);
    let new_height = ((f64::from(height) * scale).round() as u32).max(1);
    Ok(image::imageops::resize(&buf, new_width, new_height, image::imageops::FilterType::Triangle))
}

/// Map `(cx, cy)` — a coordinate an agent read off the MOST RECENT
/// screenshot of `w` — back to a physical on-screen coordinate an input
/// backend can click/move to.
///
/// `w` must be a FRESHLY resolved [`WindowInfo`] (this call's own
/// [`resolve_window`], not one cached from screenshot time): the scale is
/// recomputed from `w`'s CURRENT `width`/`height` via [`display_scale`]
/// every single call, so if the window resized between the screenshot and
/// this click the math still reflects the window as it is NOW — and a
/// coordinate that's out of range for that current (rescaled) size is
/// rejected as [`ComputerError::OutOfBounds`] rather than silently landing
/// somewhere the agent never saw. That bounds check is the fallback for
/// resize drift; there is no other reconciliation between screenshot time
/// and click time.
pub fn map_to_physical(w: &WindowInfo, cx: u32, cy: u32) -> Result<(i32, i32), ComputerError> {
    let scale = display_scale(w);
    let scaled_w = (f64::from(w.width) * scale).round() as u32;
    let scaled_h = (f64::from(w.height) * scale).round() as u32;
    // Exclusive upper bound (issue #160 review R1 #2): a screenshot saved at
    // `scaled_w` x `scaled_h` px has valid pixel columns/rows `0..=scaled_w-1`
    // / `0..=scaled_h-1` — `cx == scaled_w` is one past the last real pixel,
    // not a valid click target, and must be rejected exactly like any larger
    // value rather than silently accepted at the edge.
    if cx >= scaled_w || cy >= scaled_h {
        return Err(ComputerError::OutOfBounds {
            x: cx,
            y: cy,
            max_x: scaled_w.saturating_sub(1),
            max_y: scaled_h.saturating_sub(1),
        });
    }
    let px = w.x + (f64::from(cx) / scale).round() as i32;
    let py = w.y + (f64::from(cy) / scale).round() as i32;
    Ok((px, py))
}

/// Process-level nonce appended to every screenshot's filename (issue #160
/// round-3 P2 §4): two `screenshot` calls for the SAME window landing in the
/// SAME millisecond used to compute the IDENTICAL `<unix_ms>-<window_id>.png`
/// path and race each other's `image::save` — whichever call's write lands
/// second silently overwrites the first call's own PNG at that exact path,
/// even though both calls reported a successful save. `fetch_add` is
/// monotonically increasing for the life of the process (never reset, never
/// reused, and atomic across any number of concurrent callers), so no two
/// `screenshot_window` calls can ever collide on a filename again, no matter
/// how close together in time.
static SHOT_SEQ: AtomicU64 = AtomicU64::new(0);

/// Match `query` against the backend's visible windows, capture the ONE hit,
/// downscale it (see [`scale_capture`]/[`display_scale`]), and write it to
/// `out_dir/<unix_ms>-<id>-<seq>.png` (`out_dir` is created if missing) — the
/// trailing `<seq>` is [`SHOT_SEQ`]'s own collision-proofing nonce (issue
/// #160 round-3 P2 §4), not derived from anything about the capture itself.
pub fn screenshot_window(
    backend: &dyn backend::ComputerBackend,
    query: &str,
    out_dir: &Path,
) -> Result<Screenshot, ComputerError> {
    let matched = resolve_window(backend, query)?;
    let scale = display_scale(&matched);
    let captured = backend.capture_window(matched.id)?;
    let image = scale_capture(captured.rgba, captured.width, captured.height, scale)?;

    std::fs::create_dir_all(out_dir).map_err(|e| ComputerError::Io(e.to_string()))?;
    let unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| ComputerError::Io(e.to_string()))?
        .as_millis();
    let seq = SHOT_SEQ.fetch_add(1, Ordering::SeqCst);
    let path = out_dir.join(format!("{unix_ms}-{}-{seq}.png", matched.id));
    image.save(&path).map_err(|e| ComputerError::Io(e.to_string()))?;

    Ok(Screenshot {
        path,
        width: image.width(),
        height: image.height(),
        scale,
    })
}

/// Downscale `captured` to at most `max_long_edge` px on its long edge (the
/// SAME triangle-filter rule [`scale_capture`] uses for the on-disk PNG, but
/// parameterized so a caller can target a DIFFERENT size than the
/// screenshot's own display scale — issue #160 M3-B's MCP `image` content
/// block wants 1280px, the Ask-card thumbnail registry wants a smaller
/// 640px), drop the alpha channel (JPEG has none — `to_rgb8` does this),
/// encode at `quality` (1-100, [`image::codecs::jpeg::JpegEncoder`]'s own
/// range — the caller picks a lower one for the throwaway thumbnail than for
/// the model-facing image), and wrap the bytes as a
/// `data:image/jpeg;base64,...` URI ready to embed inline anywhere a data URI
/// is valid.
///
/// An MCP `image` content block wants the base64 payload WITHOUT this
/// `data:` prefix — `bus::computer_srv` strips it back off rather than this
/// helper never adding it, so every OTHER caller (an Ask card preview, or any
/// future `<img src>` use) gets a ready-to-use URI without having to
/// remember to add the prefix back themselves.
///
/// Requires the `image` crate's `jpeg` feature (see `Cargo.toml`) — enabled
/// unconditionally alongside `png`, not gated behind `computer-os`, since
/// this helper (like [`scale_capture`]) runs against `mock::MockBackend`'s
/// pixels in tests too, not just the real OS backend.
pub fn encode_jpeg_data_uri(
    captured: &CapturedImage,
    max_long_edge: u32,
    quality: u8,
) -> Result<String, ComputerError> {
    let buf = image::RgbaImage::from_raw(captured.width, captured.height, captured.rgba.clone())
        .ok_or_else(|| {
            ComputerError::CaptureFailed("captured pixel buffer doesn't match its reported size".into())
        })?;
    let long_edge = captured.width.max(captured.height);
    let scale = if long_edge > max_long_edge {
        f64::from(max_long_edge) / f64::from(long_edge)
    } else {
        1.0
    };
    let rgba = if scale >= 1.0 {
        buf
    } else {
        let new_width = ((f64::from(captured.width) * scale).round() as u32).max(1);
        let new_height = ((f64::from(captured.height) * scale).round() as u32).max(1);
        image::imageops::resize(&buf, new_width, new_height, image::imageops::FilterType::Triangle)
    };
    let rgb = image::DynamicImage::ImageRgba8(rgba).to_rgb8();
    let mut bytes: Vec<u8> = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, quality)
        .encode(rgb.as_raw(), rgb.width(), rgb.height(), image::ExtendedColorType::Rgb8)
        .map_err(|e| ComputerError::Io(e.to_string()))?;
    use base64::Engine as _;
    let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
    Ok(format!("data:image/jpeg;base64,{b64}"))
}

// —— control lock / input throttle / emergency stop (issue #160 M2) ——
//
// Process-level, not per-thread/dir: the whole point is that exactly ONE
// session drives the ONE real mouse/keyboard weft has access to, so both the
// control lease and the throttle timer are single global statics rather than
// keyed maps.

/// Internal lease record. Not `pub` — [`control_state`] returns the
/// decoupled, `Serialize`-able [`ControlHolder`] instead, so this struct can
/// grow bookkeeping fields later without changing the wire shape the
/// Settings UI's `get_computer_control_state` command returns.
struct ControlHolderState {
    thread: i32,
    dir: String,
    expires_at_ms: u64,
}

/// A snapshot of who currently holds the computer-use control lease, for
/// display (the Settings UI's kill-switch banner) — see [`control_state`].
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct ControlHolder {
    pub thread: i32,
    pub dir: String,
    pub expires_at_ms: u64,
}

/// How long a control lease lasts before it's treated as abandoned. Chosen
/// to comfortably cover one input action's round trip (gate checks + a
/// backend call) while still self-healing quickly if a session crashes or
/// hangs mid-lease — there is no turn-boundary hook to release it
/// explicitly (see [`acquire_control`]'s doc comment).
const CONTROL_LEASE_MS: u64 = 30_000;

fn control_mutex() -> &'static Mutex<Option<ControlHolderState>> {
    static CONTROL: OnceLock<Mutex<Option<ControlHolderState>>> = OnceLock::new();
    CONTROL.get_or_init(|| Mutex::new(None))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Take (or renew) the control lease for `(thread, dir)` — a 30s SLIDING
/// window: every successful acquire (including a same-holder re-acquire on
/// its NEXT input action) pushes `expires_at_ms` forward, so a session
/// mid-task never loses the lock to its own next call. Succeeds immediately
/// when nobody holds it, the previous lease already expired, or the SAME
/// `(thread, dir)` is re-acquiring; fails [`ComputerError::Busy`] when a
/// DIFFERENT, still-live holder has it.
///
/// There is no turn-boundary hook to release this automatically when a
/// session finishes (weft has none available here), so the lease's own
/// expiry IS the cleanup mechanism — a crashed/hung session's lock
/// self-heals within `CONTROL_LEASE_MS` instead of wedging the desktop
/// forever. Callers are not required to pair this with
/// [`release_control`].
///
/// issue #160 review R1 #5: the FIRST successful hold of an otherwise-unheld
/// lease (nobody held it, or the previous lease had expired) — as opposed to
/// a LIVE same-holder renewal — registers the OS-level global Escape
/// shortcut ([`register_global_escape`]), so it exists only while a lease is
/// genuinely live.
pub fn acquire_control(thread: i32, dir: &str) -> Result<(), ComputerError> {
    let now = now_ms();
    let register_needed;
    {
        let mut guard = control_mutex().lock().unwrap_or_else(|e| e.into_inner());
        match guard.as_ref() {
            Some(holder) => {
                let is_same_holder = holder.thread == thread && holder.dir == dir;
                let is_live = holder.expires_at_ms > now;
                if is_live && !is_same_holder {
                    return Err(ComputerError::Busy {
                        thread: holder.thread,
                        dir: holder.dir.clone(),
                    });
                }
                // A LIVE same-holder re-acquire is a renewal: the shortcut
                // from the earlier acquire is still registered, so this
                // skips re-registering it. Every other path that reaches
                // here (nobody held it live, or the previous lease already
                // expired) is a FRESH hold as far as the shortcut is
                // concerned, even when `(thread, dir)` happens to match the
                // previous holder — see `register_global_escape`'s doc for
                // why a redundant registration attempt there is harmless.
                register_needed = !(is_live && is_same_holder);
            }
            None => register_needed = true,
        }
        *guard = Some(ControlHolderState {
            thread,
            dir: dir.to_string(),
            expires_at_ms: now + CONTROL_LEASE_MS,
        });
    }
    // Registered OUTSIDE the mutex guard (issue #160 review R1 #5): the
    // Escape callback spawns a task that eventually calls `clear_control`,
    // which takes this SAME mutex — registering while still holding the
    // lock here would risk a reentrant deadlock if that path ever collapsed
    // onto this call synchronously.
    if register_needed {
        register_global_escape();
    }
    Ok(())
}

/// Release the lease early — a no-op unless `(thread, dir)` is the CURRENT
/// holder (an already-expired/released lease, or one some other session
/// since took over, is left alone rather than clobbered). Not required
/// anywhere today (see [`acquire_control`]'s doc comment on why expiry alone
/// is sufficient); this exists for a caller that knows it's done and wants
/// the next session unblocked sooner than the full 30s lease.
pub fn release_control(thread: i32, dir: &str) {
    let mut guard = control_mutex().lock().unwrap_or_else(|e| e.into_inner());
    if matches!(guard.as_ref(), Some(h) if h.thread == thread && h.dir == dir) {
        *guard = None;
    }
}

/// The current holder, or `None` when nobody holds it OR the lease expired.
/// An expired lease is cleaned up right here (lazily, on read) rather than
/// left for the next [`acquire_control`] to notice — issue #160 review R1
/// #5: that lazy cleanup is also this module's other unregister trigger for
/// the OS-level global Escape shortcut ([`unregister_global_escape`]), since
/// an expired lease means nobody is actually driving the desktop anymore.
pub fn control_state() -> Option<ControlHolder> {
    let (result, expired) = {
        let mut guard = control_mutex().lock().unwrap_or_else(|e| e.into_inner());
        let now = now_ms();
        match guard.as_ref() {
            Some(h) if h.expires_at_ms > now => (
                Some(ControlHolder {
                    thread: h.thread,
                    dir: h.dir.clone(),
                    expires_at_ms: h.expires_at_ms,
                }),
                false,
            ),
            Some(_) => {
                *guard = None;
                (None, true)
            }
            None => (None, false),
        }
    };
    // Unregistered OUTSIDE the mutex guard — same reentrancy concern as
    // `acquire_control`'s registration call.
    if expired {
        unregister_global_escape();
    }
    result
}

/// Unconditionally drop the lease, regardless of who (if anyone) holds it —
/// the emergency-stop escape hatch (see [`emergency_stop`]), which must win
/// even over a session that still believes it's mid-lease. issue #160 review
/// R1 #5: also unregisters the OS-level global Escape shortcut when there
/// WAS a holder to clear (a no-op call when there wasn't one is skipped
/// rather than attempted and swallowed).
pub fn clear_control() {
    let had_holder = {
        let mut guard = control_mutex().lock().unwrap_or_else(|e| e.into_inner());
        let had_holder = guard.is_some();
        *guard = None;
        had_holder
    };
    if had_holder {
        unregister_global_escape();
    }
}

/// issue #160 M2 kill switch: trip the in-memory latch (so every subsequent
/// [`enabled`] call fails closed) AND clear the control lease (in case a
/// call is already mid-flight against the old value), THEN best-effort
/// persist `computer_use_enabled = false` to the DB for next launch.
///
/// issue #160 review R1 #1: latch first, persist second — the latch flip and
/// the lease clear below happen BEFORE the DB write, and [`enabled`] only
/// ever consults the latch first, so a `set_setting` failure (disk full, DB
/// locked, ...) can never leave the kill switch fail-open for the rest of
/// this process's life; it only means the disabled state won't survive a
/// restart until the human retries. That write failure is still surfaced to
/// the caller (`Err`) rather than silently dropped, since a UI that thinks
/// the setting persisted when it didn't is its own kind of foot-gun — it is
/// just not the FAIL-CLOSED mechanism itself anymore.
pub async fn emergency_stop(db: &crate::store::Db) -> Result<(), String> {
    EMERGENCY_STOPPED.store(true, Ordering::SeqCst);
    clear_control();
    crate::store::repo::set_setting(db, K_COMPUTER_USE_ENABLED, "false")
        .await
        .map_err(|e| e.to_string())
}

// —— OS-level global Escape (issue #160 review R1 #5) ——
//
// The WebView's own keydown Esc handler only fires while WEFT ITSELF has
// keyboard focus — but the normal shape of a computer-use session is that
// focus is on the CONTROLLED app, not weft, so that handler alone can miss
// the one moment a human most needs the kill switch to work. This is a
// SECOND, redundant layer on top of it (the WebView listener stays in
// place), driven by `tauri_plugin_global_shortcut`, which grabs Escape at
// the OS level regardless of which window has focus. It is only ever
// registered while a control lease is actually held (see
// `acquire_control`/`clear_control`/`control_state`'s hooks above) — never
// for the whole app lifetime — so it doesn't permanently steal the system
// Escape key from every other app on the human's desktop.

/// The Tauri app handle this module needs to register/unregister the
/// OS-level global Escape shortcut. Set once, from `lib.rs`'s `setup()`
/// closure (see [`set_app_handle`]) — kept as this module's OWN `OnceLock`
/// (rather than reading the crate-level `crate::APP_HANDLE` directly) so
/// this module stays the single owner of the shortcut's registration
/// lifecycle, with no back-reference into `lib.rs` internals.
static APP_HANDLE: OnceLock<tauri::AppHandle> = OnceLock::new();

/// Wire the app handle [`register_global_escape`]/[`unregister_global_escape`]
/// need. Called exactly once, from `lib.rs`'s `setup()` closure, AFTER the
/// `tauri_plugin_global_shortcut` plugin has already run its own setup (Tauri
/// initializes plugins during `.build()`, which completes before `.setup()`
/// ever runs — see that call site's comment) — so by the time this is
/// called, `AppHandle::global_shortcut()` is always safe to use. Before this
/// runs (a unit test; a build that never reaches `setup()`), both
/// register/unregister calls silently no-op instead of touching a `None`
/// handle — see their own doc comments.
pub fn set_app_handle(h: tauri::AppHandle) {
    let _ = APP_HANDLE.set(h);
}

/// The single shortcut this module ever registers: bare `Escape`, no
/// modifiers.
fn escape_shortcut() -> tauri_plugin_global_shortcut::Shortcut {
    tauri_plugin_global_shortcut::Shortcut::new(None, tauri_plugin_global_shortcut::Code::Escape)
}

/// Best-effort: register the OS-level global Escape shortcut. Silently
/// no-ops when [`set_app_handle`] hasn't run yet (`APP_HANDLE` still `None`
/// — the common case in `cargo test --lib`, which never builds a real Tauri
/// app) — this function's whole contract is "try to add the redundant OS
/// layer if we can", never "the kill switch depends on this". A registration
/// failure (already grabbed by another app, unsupported platform/desktop
/// environment, ...) is logged and swallowed the same way, for the same
/// reason: the WebView's own Esc listener and the in-memory
/// [`EMERGENCY_STOPPED`] latch are the mechanisms that actually have to
/// work; this is upside only.
fn register_global_escape() {
    use tauri_plugin_global_shortcut::GlobalShortcutExt;
    let Some(app) = APP_HANDLE.get() else { return };
    let result = app.global_shortcut().on_shortcut(escape_shortcut(), |app, _shortcut, event| {
        if event.state != tauri_plugin_global_shortcut::ShortcutState::Pressed {
            return;
        }
        let app = app.clone();
        tauri::async_runtime::spawn(async move {
            use tauri::Manager as _;
            let db = app.state::<crate::store::Db>().inner().clone();
            // Error ignored: `emergency_stop` sets the in-memory
            // `EMERGENCY_STOPPED` latch BEFORE its own fallible DB write, so
            // the kill switch has already taken effect even on an `Err` here
            // — see that function's doc comment.
            let _ = emergency_stop(&db).await;
        });
    });
    if let Err(err) = result {
        eprintln!("[weft] register global Escape shortcut: {err}");
    }
}

/// Best-effort unregister — the [`register_global_escape`] counterpart,
/// same no-op-without-a-handle and log-and-swallow-on-error behavior, for
/// the same reasons.
fn unregister_global_escape() {
    use tauri_plugin_global_shortcut::GlobalShortcutExt;
    let Some(app) = APP_HANDLE.get() else { return };
    if let Err(err) = app.global_shortcut().unregister(escape_shortcut()) {
        eprintln!("[weft] unregister global Escape shortcut: {err}");
    }
}

fn throttle_mutex() -> &'static Mutex<Option<Instant>> {
    static LAST_INPUT: OnceLock<Mutex<Option<Instant>>> = OnceLock::new();
    LAST_INPUT.get_or_init(|| Mutex::new(None))
}

/// Minimum gap enforced between two input actions, process-wide.
const THROTTLE_MS: u64 = 500;

/// Global input rate limit: at most one input action roughly every
/// [`THROTTLE_MS`], across the WHOLE process (not per-thread/dir — the point
/// is protecting the one physical input device weft drives, and
/// [`acquire_control`] already keeps two different sessions from racing each
/// other here regardless). Only input actions call this — screenshot,
/// list_windows, and cursor_position never touch the input devices, so they
/// are never throttled. On success this also STARTS the next window (i.e.
/// records `now`), so a rapid run of calls is naturally paced rather than
/// bursting once the gap has elapsed.
pub fn throttle_input() -> Result<(), ComputerError> {
    let mut guard = throttle_mutex().lock().unwrap_or_else(|e| e.into_inner());
    let now = Instant::now();
    let min_gap = Duration::from_millis(THROTTLE_MS);
    if let Some(last) = *guard {
        let elapsed = now.duration_since(last);
        if elapsed < min_gap {
            return Err(ComputerError::RateLimited {
                wait_ms: (min_gap - elapsed).as_millis() as u64,
            });
        }
    }
    *guard = Some(now);
    Ok(())
}

/// Process-wide input mutex serializing input-action calls END TO END
/// (issue #160 review R1 #3). [`acquire_control`] only blocks a DIFFERENT
/// `(thread, dir)` from taking over the lease — it does nothing to stop the
/// SAME session from issuing two concurrent `tools/call`s, which would
/// otherwise race each other straight into the OS backend and interleave
/// their clicks/keystrokes on the human's real desktop.
///
/// Contract with `bus::computer_srv` (the sole caller — DO NOT rename this
/// function, its signature is a cross-module contract): the caller MUST
/// `.await` this and hold the returned guard for the FULL duration of one
/// input action's actual backend call — acquire it immediately before
/// invoking `backend::backend()`'s click/type/key/scroll/drag/move method
/// and let it drop only after that call returns (not just around the gate
/// checks that precede it). Two `tools/call`s racing for the SAME
/// `(thread, dir)` then serialize on this mutex instead of interleaving.
pub async fn input_flight_guard() -> tokio::sync::MutexGuard<'static, ()> {
    static FLIGHT: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    FLIGHT.get_or_init(|| tokio::sync::Mutex::new(())).lock().await
}

// —— key combo grammar (issue #160 M2) ——

/// One token out of a `"cmd+s"`-shaped key combo string, parsed by
/// [`parse_key_combo`]. Kept as its own pure enum/function — no `enigo`
/// dependency at all — so the parsing itself is unit-testable without the
/// `computer-os` feature; only `os.rs` (behind that feature) ever maps a
/// `KeyToken` to a real platform key type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyToken {
    /// `cmd` AND `win` both parse to this (see [`parse_key_combo`]) — the
    /// per-platform "which modifier is this really" distinction is left
    /// entirely to `os.rs`/`enigo`, never branched on here.
    Meta,
    Control,
    Alt,
    Shift,
    Named(NamedKey),
    /// A single printable character — anything that isn't a recognized
    /// modifier or named key falls here, ONLY when it's exactly one
    /// character (a multi-character unrecognized word is a typo, not a key,
    /// and is rejected instead — see [`parse_key_combo`]).
    Unicode(char),
}

/// The closed set of named (non-modifier, non-printable) keys this module
/// recognizes in a combo string — exactly the keys issue #160 M2's spec
/// calls out, not an attempt to cover every key `enigo` itself knows about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NamedKey {
    Return,
    Tab,
    Escape,
    Space,
    Backspace,
    Delete,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    F1,
    F2,
    F3,
    F4,
    F5,
    F6,
    F7,
    F8,
    F9,
    F10,
    F11,
    F12,
}

/// Parse a combo like `"cmd+s"` / `"ctrl+shift+t"` / `"Return"` / `"f5"` into
/// an ordered list of [`KeyToken`]s: lower-cased first (combos are case-
/// insensitive), then split on `+`. Every part must resolve to SOMETHING —
/// an empty part (`"cmd+"`, `"+s"`, `"cmd++s"`) or an unrecognized
/// multi-character word (`"bogus_key"`) is [`ComputerError::Unsupported`]
/// with a reason, never silently dropped.
pub fn parse_key_combo(combo: &str) -> Result<Vec<KeyToken>, ComputerError> {
    let lower = combo.trim().to_ascii_lowercase();
    if lower.is_empty() {
        return Err(ComputerError::Unsupported("empty key combo".into()));
    }
    lower.split('+').map(key_token).collect()
}

fn key_token(part: &str) -> Result<KeyToken, ComputerError> {
    use NamedKey::*;
    match part {
        "cmd" | "win" => Ok(KeyToken::Meta),
        "ctrl" => Ok(KeyToken::Control),
        "alt" => Ok(KeyToken::Alt),
        "shift" => Ok(KeyToken::Shift),
        "return" | "enter" => Ok(KeyToken::Named(Return)),
        "tab" => Ok(KeyToken::Named(Tab)),
        "escape" | "esc" => Ok(KeyToken::Named(Escape)),
        "space" => Ok(KeyToken::Named(Space)),
        "backspace" => Ok(KeyToken::Named(Backspace)),
        "delete" => Ok(KeyToken::Named(Delete)),
        "up" => Ok(KeyToken::Named(Up)),
        "down" => Ok(KeyToken::Named(Down)),
        "left" => Ok(KeyToken::Named(Left)),
        "right" => Ok(KeyToken::Named(Right)),
        "home" => Ok(KeyToken::Named(Home)),
        "end" => Ok(KeyToken::Named(End)),
        "pageup" => Ok(KeyToken::Named(PageUp)),
        "pagedown" => Ok(KeyToken::Named(PageDown)),
        "f1" => Ok(KeyToken::Named(F1)),
        "f2" => Ok(KeyToken::Named(F2)),
        "f3" => Ok(KeyToken::Named(F3)),
        "f4" => Ok(KeyToken::Named(F4)),
        "f5" => Ok(KeyToken::Named(F5)),
        "f6" => Ok(KeyToken::Named(F6)),
        "f7" => Ok(KeyToken::Named(F7)),
        "f8" => Ok(KeyToken::Named(F8)),
        "f9" => Ok(KeyToken::Named(F9)),
        "f10" => Ok(KeyToken::Named(F10)),
        "f11" => Ok(KeyToken::Named(F11)),
        "f12" => Ok(KeyToken::Named(F12)),
        _ => {
            let mut chars = part.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) => Ok(KeyToken::Unicode(c)),
                _ => Err(ComputerError::Unsupported(format!(
                    "unrecognized key \"{part}\" in combo"
                ))),
            }
        }
    }
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

    fn window_sized(id: u32, app: &str, title: &str, width: u32, height: u32) -> WindowInfo {
        WindowInfo {
            id,
            app: app.to_string(),
            title: title.to_string(),
            x: 0,
            y: 0,
            width,
            height,
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
            ..Default::default()
        };
        let dir = std::env::temp_dir().join("weft-computer-test-zero-hits");
        let err = screenshot_window(&backend, "nonexistent", &dir).unwrap_err();
        assert!(matches!(err, ComputerError::WindowNotFound { query } if query == "nonexistent"));
    }

    #[test]
    fn multiple_hits_is_ambiguous_with_candidates() {
        let backend = mock::MockBackend {
            windows: vec![window(1, "Notes", "Untitled 1"), window(2, "Notes", "Untitled 2")],
            ..Default::default()
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
    fn display_scale_matches_long_edge_threshold() {
        let big = window_sized(1, "x", "x", 2560, 1440);
        assert_eq!(display_scale(&big), 0.5);
        let small = window_sized(2, "x", "x", 800, 600);
        assert_eq!(display_scale(&small), 1.0);
        // Exactly at the threshold stays 1.0 (the rule is "> 1280", not ">=").
        let at_threshold = window_sized(3, "x", "x", 1280, 720);
        assert_eq!(display_scale(&at_threshold), 1.0);
    }

    #[test]
    fn scale_capture_downscales_by_the_given_scale() {
        let img = scale_capture(vec![7u8; 2560 * 1440 * 4], 2560, 1440, 0.5).unwrap();
        assert_eq!((img.width(), img.height()), (1280, 720));
    }

    #[test]
    fn scale_capture_is_noop_at_scale_one() {
        let img = scale_capture(vec![7u8; 800 * 600 * 4], 800, 600, 1.0).unwrap();
        assert_eq!((img.width(), img.height()), (800, 600));
    }

    // —— file write ——

    #[test]
    fn screenshot_window_writes_a_real_png() {
        // Window dims match the captured image's actual pixel dims (the
        // realistic case — see `display_scale`'s doc comment: the SAME
        // window fields drive both the screenshot's downscale and, later,
        // `map_to_physical`'s coordinate mapping).
        let backend = mock::MockBackend {
            windows: vec![window_sized(9, "Notes", "Untitled", 2560, 1440)],
            image: Some(solid_image(2560, 1440, 200)),
            ..Default::default()
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

    /// issue #160 round-3 P2 §4: two `screenshot_window` calls for the SAME
    /// window with the SAME args (so a naive `<unix_ms>-<id>.png` filename
    /// scheme could genuinely collide if both land in the same millisecond)
    /// must never overwrite each other — each gets its own path, and both
    /// files must actually exist afterward.
    #[test]
    fn screenshot_window_gives_each_call_a_unique_path_even_in_the_same_millisecond() {
        let backend = mock::MockBackend {
            windows: vec![window_sized(9, "Notes", "Untitled", 800, 600)],
            image: Some(solid_image(800, 600, 5)),
            ..Default::default()
        };
        let tmp = tempfile::tempdir().unwrap();
        let shot1 = screenshot_window(&backend, "9", tmp.path()).unwrap();
        let shot2 = screenshot_window(&backend, "9", tmp.path()).unwrap();
        assert_ne!(
            shot1.path, shot2.path,
            "two calls with identical args must never collide on the same filename"
        );
        assert!(shot1.path.exists());
        assert!(shot2.path.exists());
    }

    // —— encode_jpeg_data_uri (issue #160 M3-B) ——

    #[test]
    fn encode_jpeg_data_uri_produces_a_decodable_jpeg_with_the_right_prefix() {
        let captured = solid_image(800, 600, 128);
        let uri = encode_jpeg_data_uri(&captured, 1280, 75).unwrap();
        assert!(uri.starts_with("data:image/jpeg;base64,"), "{uri}");
        let b64 = uri.strip_prefix("data:image/jpeg;base64,").unwrap();
        use base64::Engine as _;
        let bytes = base64::engine::general_purpose::STANDARD.decode(b64).unwrap();
        let decoded = image::load_from_memory_with_format(&bytes, image::ImageFormat::Jpeg).unwrap();
        // No downscale needed (800x600's long edge is under max_long_edge), so
        // the decoded JPEG keeps the source's exact dimensions.
        assert_eq!((decoded.width(), decoded.height()), (800, 600));
    }

    #[test]
    fn encode_jpeg_data_uri_downscales_to_the_requested_long_edge() {
        let captured = solid_image(2560, 1440, 50);
        let uri = encode_jpeg_data_uri(&captured, 640, 60).unwrap();
        let b64 = uri.strip_prefix("data:image/jpeg;base64,").unwrap();
        use base64::Engine as _;
        let bytes = base64::engine::general_purpose::STANDARD.decode(b64).unwrap();
        let decoded = image::load_from_memory_with_format(&bytes, image::ImageFormat::Jpeg).unwrap();
        // 2560x1440 downscaled to a 640px long edge preserves the aspect ratio.
        assert_eq!((decoded.width(), decoded.height()), (640, 360));
    }

    #[test]
    fn encode_jpeg_data_uri_rejects_a_mismatched_buffer() {
        let bad = CapturedImage {
            rgba: vec![0u8; 4], // way too short for 800x600
            width: 800,
            height: 600,
        };
        let err = encode_jpeg_data_uri(&bad, 1280, 75).unwrap_err();
        assert!(matches!(err, ComputerError::CaptureFailed(_)));
    }

    // —— map_to_physical (issue #160 M2) ——

    #[test]
    fn map_to_physical_scales_and_offsets_a_screenshot_coordinate() {
        let w = WindowInfo {
            id: 1,
            app: "x".into(),
            title: "x".into(),
            x: 100,
            y: 50,
            width: 2560,
            height: 1440,
        };
        // scale = 0.5; coordinate (640, 360) in the DOWNSCALED screenshot
        // maps to (1280, 720) in the window's own space, plus the window's
        // origin.
        let (px, py) = map_to_physical(&w, 640, 360).unwrap();
        assert_eq!((px, py), (100 + 1280, 50 + 720));
    }

    #[test]
    fn map_to_physical_rejects_an_out_of_bounds_coordinate() {
        let w = WindowInfo {
            id: 1,
            app: "x".into(),
            title: "x".into(),
            x: 0,
            y: 0,
            width: 2560,
            height: 1440,
        };
        // scale = 0.5 -> scaled_w=1280, scaled_h=720; the last VALID
        // coordinate is (1279, 719) — see the next test — so `max_x`/`max_y`
        // report that inclusive last-valid value, not the width/height.
        let err = map_to_physical(&w, 1281, 0).unwrap_err();
        assert!(matches!(
            err,
            ComputerError::OutOfBounds { x: 1281, y: 0, max_x: 1279, max_y: 719 }
        ));
    }

    #[test]
    fn map_to_physical_rejects_the_exclusive_upper_bound_and_accepts_the_last_valid_coordinate() {
        // issue #160 review R1 #2: `cx == scaled_w` (here 1280) must be
        // rejected — it's one past the last real pixel column — while
        // `cx == scaled_w - 1` (1279), the actual last valid column, is
        // accepted.
        let w = WindowInfo {
            id: 1,
            app: "x".into(),
            title: "x".into(),
            x: 0,
            y: 0,
            width: 2560,
            height: 1440,
        };
        let err = map_to_physical(&w, 1280, 0).unwrap_err();
        assert!(matches!(
            err,
            ComputerError::OutOfBounds { x: 1280, y: 0, max_x: 1279, max_y: 719 }
        ));
        assert!(map_to_physical(&w, 1279, 0).is_ok());
        // Same rule on the y axis.
        let err_y = map_to_physical(&w, 0, 720).unwrap_err();
        assert!(matches!(
            err_y,
            ComputerError::OutOfBounds { x: 0, y: 720, max_x: 1279, max_y: 719 }
        ));
        assert!(map_to_physical(&w, 0, 719).is_ok());
    }

    #[test]
    fn map_to_physical_passes_through_unscaled_for_a_small_window() {
        let w = WindowInfo {
            id: 1,
            app: "x".into(),
            title: "x".into(),
            x: 10,
            y: 20,
            width: 800,
            height: 600,
        };
        let (px, py) = map_to_physical(&w, 100, 50).unwrap();
        assert_eq!((px, py), (110, 70));
    }

    // —— control lock (issue #160 M2) ——

    #[test]
    fn control_lock_busy_expiry_release_and_clear() {
        // Every test in this crate shares the SAME process-wide statics
        // (`control_mutex`), so this one test exercises the whole lifecycle
        // sequentially rather than each phase getting its own `#[test]` —
        // splitting them would let `cargo test`'s parallel test threads
        // stomp on each other's lock state.
        clear_control();
        assert!(control_state().is_none());

        acquire_control(1, "10").unwrap();
        let held = control_state().unwrap();
        assert_eq!((held.thread, held.dir.as_str()), (1, "10"));

        // A different (thread, dir) is blocked while the lease is live.
        let err = acquire_control(2, "20").unwrap_err();
        assert!(matches!(err, ComputerError::Busy { thread: 1, dir } if dir == "10"));

        // The SAME holder re-acquiring (renewing) still succeeds.
        acquire_control(1, "10").unwrap();

        // Releasing a lease you do NOT hold is a no-op.
        release_control(2, "20");
        assert!(control_state().is_some());

        // The real holder releasing frees it up immediately.
        release_control(1, "10");
        assert!(control_state().is_none());

        // A manually-expired lease reads as absent (and is cleaned up) —
        // simulated by acquiring, then reaching into the internal state to
        // force `expires_at_ms` into the past instead of sleeping 30s.
        acquire_control(1, "10").unwrap();
        {
            let mut guard = control_mutex().lock().unwrap();
            if let Some(h) = guard.as_mut() {
                h.expires_at_ms = now_ms().saturating_sub(1);
            }
        }
        assert!(control_state().is_none(), "expired lease must read as absent");
        // ... and once expired, someone else CAN acquire it.
        acquire_control(2, "20").unwrap();

        // clear_control wipes it unconditionally, even mid-lease.
        assert!(control_state().is_some());
        clear_control();
        assert!(control_state().is_none());
    }

    #[tokio::test]
    async fn emergency_stop_disables_the_setting_and_clears_control() {
        let db = Db::connect("sqlite::memory:").await.unwrap();
        crate::store::repo::set_setting(&db, K_COMPUTER_USE_ENABLED, "true")
            .await
            .unwrap();
        acquire_control(9, "90").unwrap();

        let result = emergency_stop(&db).await;

        assert!(result.is_ok(), "emergency_stop must succeed against a healthy in-memory db");
        assert!(!enabled(&db).await);
        assert!(control_state().is_none());
        // Leave the shared process-wide latch cleared for every other test
        // in this binary — see `emergency_stop_latch_wins_over_a_true_setting_until_explicitly_cleared`'s
        // own note on why every latch-touching test must clean up after
        // itself.
        clear_emergency_stop();
    }

    // —— emergency-stop latch (issue #160 review R1 #1) ——

    #[tokio::test]
    async fn emergency_stop_latch_wins_over_a_true_setting_until_explicitly_cleared() {
        // `EMERGENCY_STOPPED` is a process-wide static shared by every test
        // in this binary (same category of shared state as `control_mutex`/
        // `throttle_mutex` above) — kept as ONE test exercising the whole
        // latch lifecycle sequentially, ending with the latch cleared again,
        // rather than splitting it across tests that could interleave with
        // each other (or with `enabled_reads_true_false_and_missing` below)
        // under `cargo test`'s default parallel test threads.
        let db = Db::connect("sqlite::memory:").await.unwrap();
        clear_emergency_stop();
        crate::store::repo::set_setting(&db, K_COMPUTER_USE_ENABLED, "true")
            .await
            .unwrap();
        assert!(enabled(&db).await, "baseline: latch clear, setting true -> enabled");

        EMERGENCY_STOPPED.store(true, Ordering::SeqCst);
        assert!(
            !enabled(&db).await,
            "the latch must win even though the underlying setting is still \"true\""
        );

        clear_emergency_stop();
        assert!(
            enabled(&db).await,
            "clearing the latch restores the underlying setting's value"
        );

        // Leave it cleared for any other test in this binary.
        clear_emergency_stop();
    }

    // —— input throttle (issue #160 M2) ——

    #[test]
    fn throttle_input_rejects_a_second_call_inside_the_window() {
        // Shares the same process-wide static as every other throttle test —
        // reset by consuming the current window with a first call before
        // asserting on the second.
        throttle_input().ok();
        let err = throttle_input().unwrap_err();
        assert!(matches!(err, ComputerError::RateLimited { wait_ms } if wait_ms > 0 && wait_ms <= THROTTLE_MS));
    }

    // —— input flight guard (issue #160 review R1 #3) ——

    #[tokio::test]
    async fn input_flight_guard_serializes_concurrent_acquires() {
        use std::sync::atomic::AtomicU32;
        use std::sync::Arc;

        let concurrent = Arc::new(AtomicU32::new(0));
        let max_concurrent = Arc::new(AtomicU32::new(0));

        let mut handles = Vec::new();
        for _ in 0..2 {
            let concurrent = concurrent.clone();
            let max_concurrent = max_concurrent.clone();
            handles.push(tokio::spawn(async move {
                let _guard = input_flight_guard().await;
                let now = concurrent.fetch_add(1, Ordering::SeqCst) + 1;
                max_concurrent.fetch_max(now, Ordering::SeqCst);
                // Yield/sleep WHILE holding the guard so the second task, if
                // it were (incorrectly) able to acquire concurrently, would
                // have a window to observe `concurrent == 2`.
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                concurrent.fetch_sub(1, Ordering::SeqCst);
            }));
        }
        for h in handles {
            h.await.unwrap();
        }

        assert_eq!(
            max_concurrent.load(Ordering::SeqCst),
            1,
            "the two tasks must never hold the guard at the same time — later one waits for the first to drop it"
        );
    }

    // —— OS-level global Escape (issue #160 review R1 #5) ——

    #[test]
    fn register_and_unregister_global_escape_noop_without_an_app_handle() {
        // `cargo test --lib` never builds a real Tauri app, so
        // `set_app_handle` is never called in this binary and `APP_HANDLE`
        // stays `None` for the whole process — both calls must silently
        // no-op rather than panic (there is no live runtime-behavior test
        // possible here; see the task's own verification-scope note).
        register_global_escape();
        unregister_global_escape();
    }

    // —— key combo parsing (issue #160 M2) ——

    #[test]
    fn parse_key_combo_recognizes_modifiers_named_keys_and_single_chars() {
        assert_eq!(
            parse_key_combo("cmd+s").unwrap(),
            vec![KeyToken::Meta, KeyToken::Unicode('s')]
        );
        assert_eq!(
            parse_key_combo("ctrl+shift+t").unwrap(),
            vec![KeyToken::Control, KeyToken::Shift, KeyToken::Unicode('t')]
        );
        assert_eq!(parse_key_combo("Return").unwrap(), vec![KeyToken::Named(NamedKey::Return)]);
        assert_eq!(parse_key_combo("f5").unwrap(), vec![KeyToken::Named(NamedKey::F5)]);
    }

    #[test]
    fn parse_key_combo_rejects_empty_and_unrecognized_tokens() {
        assert!(parse_key_combo("cmd+").is_err());
        assert!(parse_key_combo("bogus_key").is_err());
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
