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
/// issue #160 review R1 #1: the emergency-stop latch ([`stop_state`]'s
/// `stopped` field) is checked FIRST, before the DB is even touched — once
/// tripped, this returns `false` unconditionally, so a `set_setting` write
/// failure inside [`emergency_stop`] (which the latch does NOT depend on —
/// see that function's doc) can never leave the kill switch silently
/// fail-open because the setting row still reads "true".
pub async fn enabled(db: &crate::store::Db) -> bool {
    if stop_state().lock().unwrap_or_else(|e| e.into_inner()).stopped {
        return false;
    }
    matches!(
        crate::store::repo::get_setting(db, K_COMPUTER_USE_ENABLED).await,
        Ok(Some(v)) if v == "true"
    )
}

/// Process-level emergency-stop latch state, PLUS the generation counter
/// [`clear_emergency_stop`] needs to detect a stale caller (issue #160
/// round-6 review P1 #1) — kept together behind ONE mutex rather than
/// `stopped` as a lone `AtomicBool` and `generation` as a separate
/// `AtomicU64`: [`clear_emergency_stop`]'s whole job is a check-THEN-act
/// ("is the generation still what the caller expects; if so, clear
/// `stopped`") that two INDEPENDENT atomics cannot make atomic against a
/// concurrent [`emergency_stop`] call landing in the gap between the check
/// and the act (a compare-then-store on two separate atomics still leaves a
/// window where `emergency_stop`'s own two writes could straddle it) — see
/// [`clear_emergency_stop`]'s own doc for the exact race this closes.
///
/// `stopped`, once tripped by [`emergency_stop`], makes [`enabled`] return
/// `false` UNCONDITIONALLY, before it even reads the `computer_use_enabled`
/// DB setting — so this is the actual fail-closed mechanism the kill switch
/// relies on, and the DB write in [`emergency_stop`] is best-effort
/// persistence for the NEXT launch, not something [`enabled`] depends on
/// within THIS process's life. The ONLY function allowed to clear `stopped`
/// is [`clear_emergency_stop`], itself called from exactly one place —
/// `commands::set_computer_use_enabled` when a human explicitly re-enables
/// computer use from Settings.
struct StopState {
    stopped: bool,
    /// Bumped by every [`emergency_stop`] call (issue #160 round-6 review P1
    /// #1). `commands::set_computer_use_enabled(true)` reads this via
    /// [`stop_generation`] BEFORE its own (possibly slow) `set_setting`
    /// write, then only actually clears the latch (via
    /// [`clear_emergency_stop`]) if the generation it read is STILL current
    /// once that write finishes. A Stop that lands WHILE the enable's write
    /// is in flight bumps this, so the enable's later clear attempt is
    /// recognized as stale and refused — an explicit, LATER Stop always
    /// wins over an EARLIER, still-in-flight enable, never the reverse.
    generation: u64,
}

fn stop_state() -> &'static Mutex<StopState> {
    static STATE: OnceLock<Mutex<StopState>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(StopState { stopped: false, generation: 0 }))
}

/// The current stop-generation (issue #160 round-6 review P1 #1) — see
/// [`StopState::generation`]'s own doc for the full read-before-write,
/// clear-only-if-still-current contract this exists for.
pub fn stop_generation() -> u64 {
    stop_state().lock().unwrap_or_else(|e| e.into_inner()).generation
}

/// Clear the emergency-stop latch (issue #160 review R1 #1; generation check
/// added round-6 review P1 #1 — see [`StopState`]'s own doc for why a lone
/// `AtomicBool`/`AtomicU64` pair could not make this fully race-safe). The
/// ONLY legitimate caller is `commands::set_computer_use_enabled` when
/// `enabled == true` — a human explicitly turning computer use back on from
/// Settings after a kill switch trip. Nothing else in this codebase may call
/// this: there is deliberately no other path back to "computer use may run
/// again" once the latch is tripped.
///
/// `expected_gen` must be a [`stop_generation`] value the caller read BEFORE
/// its own `set_setting` write started. This clears the latch (and returns
/// `true`) ONLY when the generation is STILL exactly what the caller
/// expects — i.e. no [`emergency_stop`] ran in between. If a Stop DID land
/// in between (the generation has since moved on), this returns `false` and
/// leaves the latch tripped: an explicit, LATER Stop must never be silently
/// undone by an EARLIER enable request that only just now finished its own
/// DB write (issue #160 round-6 review P1 #1 — this is the exact bug this
/// generation check exists to close). Also resets [`STOP_PERSIST_FAILED`] on
/// an actual clear (issue #160 round-6 review P2 #6): a human explicitly
/// re-enabling computer use from Settings is the closest thing this feature
/// has to an acknowledgment that any earlier persist failure has been dealt
/// with.
pub fn clear_emergency_stop(expected_gen: u64) -> bool {
    let mut guard = stop_state().lock().unwrap_or_else(|e| e.into_inner());
    if guard.generation != expected_gen {
        return false;
    }
    guard.stopped = false;
    drop(guard);
    STOP_PERSIST_FAILED.store(false, Ordering::SeqCst);
    true
}

/// Set (issue #160 round-6 review P2 #6) whenever the MOST RECENT
/// [`emergency_stop`] call's own `set_setting` write failed — covering BOTH
/// paths that call it (`commands::computer_emergency_stop`, the Stop button/
/// dialog, AND the OS-level global Escape shortcut's own spawned call — see
/// [`register_global_escape`]), since neither can otherwise tell a human
/// "the kill switch tripped in-memory, but the setting may still read
/// `true` on disk for the next launch".
///
/// Cleared two ways: a SUCCESSFUL [`clear_emergency_stop`] (a human
/// explicitly re-enabling computer use from Settings), OR — issue #160
/// round-8 P2 #3 — a SUCCESSFUL, still-most-recent [`emergency_stop`] retry
/// (a human clicking Stop again after an earlier attempt's persist failed,
/// this time landing). Either way this always reflects the MOST RECENT
/// call's own outcome, by generation — never an older, slower call's stale
/// result racing in after a newer one already recorded its own.
/// `bus::computer_srv`/the frontend never touch this directly; read it via
/// [`stop_persist_failed`] / `commands::get_computer_stop_persist_failed`.
static STOP_PERSIST_FAILED: AtomicBool = AtomicBool::new(false);

/// Whether the most recent [`emergency_stop`] call's own DB write failed and
/// hasn't since been cleared — see [`STOP_PERSIST_FAILED`]'s own doc.
pub fn stop_persist_failed() -> bool {
    STOP_PERSIST_FAILED.load(Ordering::SeqCst)
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
    /// The id of the window this screenshot ACTUALLY captured — [`screenshot_window`]'s
    /// own `matched.id`, from the SAME resolution it captured against (issue
    /// #160 round-6 review P2 #4). Exists so a caller that needs to key
    /// something off "the window this screenshot came from" (the Ask-card
    /// preview registry in `bus::computer_srv`) never has to re-resolve
    /// `query` a second time — a second resolution can land on a DIFFERENT
    /// window than the one actually captured if it closed, was renamed, or
    /// its id got reused in the gap between the two calls, silently
    /// mis-keying the preview to the wrong window.
    pub window_id: u32,
    /// The SAVED image's own pixels, RGBA, kept in memory for the life of this
    /// `Screenshot` value (round-7 P1) — the SAME `width`x`height`, already-
    /// scaled bytes `screenshot_window` wrote to `path`, captured right around
    /// its own `image.save` call rather than derived separately. Exists so a
    /// caller building a preview/MCP-image payload (`bus::computer_srv`) never
    /// has to re-open `path` off disk to get pixels it already had a moment
    /// ago: `path` is a path INSIDE the worker's own writable worktree, and
    /// the gap between this function's `image.save` and any later re-open is
    /// exactly the window a sandboxed background process could use to swap
    /// that file for a symlink to an arbitrary user-readable image — Weft
    /// would then follow that link with its own permissions and hand the
    /// substituted pixels straight to the model/human as if they were the
    /// real capture. Reading `pixels` here instead closes that reopen-after-
    /// save TOCTOU/symlink race entirely, rather than narrowing it.
    pub pixels: CapturedImage,
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
///
/// issue #160 round-5 review P2 §5: this is a BEST-EFFORT list of terminal
/// emulators known at the time this list was last updated, NOT an exhaustive
/// catalog of every terminal emulator that exists or will ever exist — a
/// brand-new or simply-not-yet-added one is still screenshottable until this
/// list is extended to cover it. `computerUseHint` (`src/i18n/en.ts`/`zh.ts`)
/// is worded to match that reality ("known terminal-emulator windows", not
/// "any terminal window") rather than promising a guarantee this table
/// cannot keep. Round-5 added: Ghostty, foot, Tilix, Rio, st (suckless
/// simple terminal), urxvt/rxvt, Terminator, Guake, Yakuake, xfce4-terminal,
/// LXTerminal, QTerminal, Deepin Terminal, Tabby, Contour, WaveTerm, Cool
/// Retro Term, Eterm, Sakura, Termite, and `kgx` (GNOME Console's own binary
/// name — GNOME Terminal's newer sibling, distinct from `gnome-terminal`
/// above).
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
    "ghostty",
    "foot",
    "tilix",
    "rio",
    "st",
    "urxvt",
    "rxvt",
    "terminator",
    "guake",
    "yakuake",
    "xfce4-terminal",
    "lxterminal",
    "qterminal",
    "deepin-terminal",
    "tabby",
    "contour",
    "waveterm",
    "wave",
    "cool-retro-term",
    "eterm",
    "sakura",
    "termite",
    "kgx",
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
///
/// A thin wrapper over [`display_scale_for_dims`] (round-7 P2) — same
/// formula, just reading its two inputs off a [`WindowInfo`] instead of bare
/// dimensions, for the two callers (`map_to_physical`'s live re-resolve, and
/// any caller that only has a `WindowInfo` on hand) that want it that way.
/// Behavior is unchanged from before the split.
pub fn display_scale(w: &WindowInfo) -> f64 {
    display_scale_for_dims(w.width, w.height)
}

/// The core of [`display_scale`], taking bare dimensions instead of a
/// [`WindowInfo`] (round-7 P2) — so [`screenshot_window`] can derive the
/// scale it records from the CAPTURED frame's own dimensions, not from a
/// window resolved earlier and possibly already stale. See
/// [`screenshot_window`]'s own doc comment for the resize-in-the-gap race
/// this exists to close.
fn display_scale_for_dims(width: u32, height: u32) -> f64 {
    const MAX_LONG_EDGE: u32 = 1280;
    let long_edge = width.max(height);
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
/// downscale it (see [`scale_capture`]/[`display_scale_for_dims`]), and write
/// it to `out_dir/<unix_ms>-<id>-<seq>.png` (`out_dir` is created if missing)
/// — the trailing `<seq>` is [`SHOT_SEQ`]'s own collision-proofing nonce
/// (issue #160 round-3 P2 §4), not derived from anything about the capture
/// itself. The returned [`Screenshot`] carries the saved image's pixels
/// in-memory (`Screenshot::pixels`, round-7 P1) precisely so `bus::
/// computer_srv` never has to re-open `path` off disk to build a preview/MCP-
/// image payload — see that field's own doc for the reopen-after-save
/// symlink/TOCTOU race this closes.
///
/// round-7 P2: the recorded `scale` (and the saved pixel dimensions) are
/// derived from `captured` — the frame actually returned by THIS call's own
/// `capture_window`, taken AFTER `matched` was resolved — rather than from
/// `matched`'s own (pre-capture) `width`/`height`. If the window resized in
/// the gap between `resolve_window` and `capture_window`, `matched`'s
/// dimensions are already stale by the time this runs; deriving the scale
/// from them instead of from the frame actually being scaled and saved would
/// record a `Screenshot::scale` that doesn't match the pixels it describes.
/// The common, no-resize path (`captured.width == matched.width` and same for
/// height, true for both `mock::MockBackend` and the real backend absent an
/// actual mid-call resize) is completely unaffected — same scale, same
/// output, byte for byte — since `scale_capture` already treats `captured`'s
/// own dimensions as the coordinate space to scale from either way. Click-time
/// drift (the window moving/resizing again AFTER this screenshot, before an
/// input action) is a separate, already-handled concern: `map_to_physical`
/// re-resolves a FRESH `WindowInfo` and recomputes the scale from ITS current
/// dimensions on every call, with an out-of-range coordinate rejected rather
/// than silently mismapped — real-machine HiDPI device-pixel-ratio
/// calibration beyond that remains issue #160 §9.
///
/// round-7 P1: every successful save also best-effort prunes this
/// `out_dir` down to [`MAX_RETAINED_SCREENSHOTS`] via
/// [`prune_old_screenshots`] — an unbounded, always-growing set of on-disk
/// PNGs per session directory is its own resource-exhaustion hazard once
/// `screenshot` is Always/Full-granted and no longer needs a card per call.
///
/// issue #160 round-5 review P2 §4: `bus::computer_srv::screenshot_out_dir`
/// already walks every path component up through `out_dir` via
/// `refuse_symlinks` BEFORE ever calling this function — but a worktree is
/// repository-controlled content, so the window between THAT check and the
/// actual write below is still open: anything with write access to the
/// checkout (an agent's own earlier approved write, a background process)
/// could swap `out_dir` itself for a symlink to an arbitrary path in the
/// instant between the two. This re-checks `out_dir` itself, via
/// `symlink_metadata` (never plain `metadata`, so a symlink is caught even
/// when it points at something real), AFTER `create_dir_all` (which is a
/// no-op — and does NOT itself error — when `out_dir` already exists as a
/// symlink to a real directory, so skipping straight to `image.save` without
/// this recheck would silently follow it) and BEFORE `image.save` ever
/// touches the filesystem. This narrows, but does not fully close, the race:
/// a swap landing in the still-open window between THIS check and the actual
/// write remains a residual — see below (round-8 P2 #8 closes THAT leaf).
///
/// issue #160 round-8 P2 #8: the actual on-disk write is no longer a bare
/// `image.save(&path)`. On unix, the PNG is now encoded straight into a file
/// handle opened with `create_new` (O_EXCL) + `O_NOFOLLOW` + mode `0o600` —
/// closing TWO gaps `image.save` alone left open:
///  1. the exact leaf race this function's own doc used to call out as an
///     accepted residual — a swap of the leaf PATH itself (not `out_dir`) for
///     a symlink/pre-existing file in the instant between the `out_dir`
///     recheck above and the write. `O_NOFOLLOW` refuses to follow a symlink
///     leaf; `create_new` (O_EXCL) refuses to write through/into anything
///     already there at all. The per-call `<unix_ms>-<id>-<seq>.png` name is
///     unique by construction (never reused), so `create_new` never spuriously
///     fails against a screenshot's OWN prior, legitimate file.
///  2. worktrees living in a shared/traversable directory with a permissive
///     process umask: `image.save`'s default `0644` leaves screenshots — which
///     can carry mail/browser/password-manager pixels — readable by every
///     OTHER local account for as long as the file is retained. `mode(0o600)`
///     makes them owner-only from the moment of creation, no window where a
///     default-permissive create is later tightened.
/// Non-unix keeps the pre-existing `image.save(&path)` (no owner-only concept
/// there this crate can portably act on).
pub fn screenshot_window(
    backend: &dyn backend::ComputerBackend,
    query: &str,
    out_dir: &Path,
) -> Result<Screenshot, ComputerError> {
    let matched = resolve_window(backend, query)?;
    let captured = backend.capture_window(matched.id)?;
    // round-7 P2: derive the recorded scale from THIS frame (`captured`), not
    // from `matched`'s own pre-capture geometry — see this function's own doc
    // comment for the resize-in-the-gap race this closes.
    let scale = display_scale_for_dims(captured.width, captured.height);
    let image = scale_capture(captured.rgba, captured.width, captured.height, scale)?;

    std::fs::create_dir_all(out_dir).map_err(|e| ComputerError::Io(e.to_string()))?;
    if let Ok(meta) = std::fs::symlink_metadata(out_dir) {
        if meta.file_type().is_symlink() {
            return Err(ComputerError::Io(format!(
                "refusing to write a screenshot through a symlink at {}",
                out_dir.display()
            )));
        }
    }
    let unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| ComputerError::Io(e.to_string()))?
        .as_millis();
    let seq = SHOT_SEQ.fetch_add(1, Ordering::SeqCst);
    let path = out_dir.join(format!("{unix_ms}-{}-{seq}.png", matched.id));
    let width = image.width();
    let height = image.height();
    // round-8 P2 #8: owner-only, no-follow, exclusive-create write — see this
    // function's own doc comment above for exactly which two gaps this closes
    // over the plain `image.save(&path)` this replaces.
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut opt = std::fs::OpenOptions::new();
        opt.write(true).create_new(true).mode(0o600).custom_flags(libc::O_NOFOLLOW);
        let file = opt.open(&path).map_err(|e| ComputerError::Io(e.to_string()))?;
        let mut w = std::io::BufWriter::new(file);
        image
            .write_to(&mut w, image::ImageFormat::Png)
            .map_err(|e| ComputerError::Io(e.to_string()))?;
    }
    #[cfg(not(unix))]
    {
        image.save(&path).map_err(|e| ComputerError::Io(e.to_string()))?;
    }
    // round-7 P1: keep the just-saved pixels in memory instead of ever having
    // a caller re-open `path` to get them back — see `Screenshot::pixels`'s
    // own doc for the symlink/TOCTOU race that reopen would risk. `into_raw`
    // consumes `image`, so this runs only after the write above (which only
    // borrows it) has already finished.
    let pixels = CapturedImage { rgba: image.into_raw(), width, height };

    // round-7 P1: best-effort retention cap — never fails a screenshot that
    // already saved successfully just because pruning stumbled.
    prune_old_screenshots(out_dir, MAX_RETAINED_SCREENSHOTS);

    Ok(Screenshot {
        path,
        width,
        height,
        scale,
        window_id: matched.id,
        pixels,
    })
}

/// Per-session-directory screenshot retention cap (round-7 P1). An agent
/// needs recent visual context, not an unbounded history — once `screenshot`
/// is Always/Full-granted it no longer surfaces a card per call, so nothing
/// else in this module limits how many PNGs a looping agent can accumulate in
/// one session's output directory; left alone this grows without bound until
/// the human's disk fills up. Anything beyond this many, per directory, is
/// deleted by modification time (oldest first) — see [`prune_old_screenshots`].
const MAX_RETAINED_SCREENSHOTS: usize = 20;

/// Delete this module's OWN screenshots in `out_dir` beyond the most recent
/// `keep` (round-7 P1) — best-effort: any I/O error along the way is
/// silently skipped, since a cleanup failure must never turn an
/// already-successful screenshot save into a call failure. Only touches
/// files [`is_own_screenshot_filename`] recognizes as this module's own
/// `<unix_ms>-<window_id>-<seq>.png` naming — anything else in `out_dir`
/// (a `.weft` audit directory, a file some other tool left there) is left
/// alone regardless of age. Uses `symlink_metadata` (never plain `metadata`)
/// so a symlink sitting in `out_dir` is neither followed nor deleted nor
/// counted against `keep` — `remove_file` on a symlink removes the directory
/// entry, never the link's target, but this skips symlinks entirely rather
/// than relying on that alone. The file [`screenshot_window`] itself JUST
/// saved is by construction the newest entry here (it was just written), so
/// it is never among the ones pruned — a caller holding this call's own
/// `Screenshot::path` always still has a file at that path afterward.
fn prune_old_screenshots(out_dir: &Path, keep: usize) {
    let Ok(rd) = std::fs::read_dir(out_dir) else {
        return;
    };
    let mut shots: Vec<(SystemTime, PathBuf)> = Vec::new();
    for entry in rd.flatten() {
        let path = entry.path();
        if !is_own_screenshot_filename(&path) {
            continue;
        }
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if !meta.file_type().is_file() {
            continue; // skip symlinks/directories — never follow, never delete
        }
        let Ok(mtime) = meta.modified() else {
            continue;
        };
        shots.push((mtime, path));
    }
    if shots.len() <= keep {
        return;
    }
    shots.sort_by(|a, b| b.0.cmp(&a.0)); // newest first
    for (_, path) in shots.into_iter().skip(keep) {
        let _ = std::fs::remove_file(&path);
    }
}

/// Whether `path`'s file name matches this module's OWN screenshot naming —
/// `<unix_ms>-<window_id>-<seq>.png`, three all-digit, non-empty dash-
/// separated components with a `.png` extension (round-7 P1). Only a file
/// matching this is ever a candidate for [`prune_old_screenshots`] to delete
/// — anything else living in the same directory (today: nothing; in
/// principle, a `.weft` subdirectory or some unrelated file a human or
/// another tool placed there) is never touched, matched, or counted, no
/// matter how old.
fn is_own_screenshot_filename(path: &Path) -> bool {
    let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
        return false;
    };
    let ext_ok = path.extension().and_then(|s| s.to_str()) == Some("png");
    let parts: Vec<&str> = stem.split('-').collect();
    ext_ok
        && parts.len() == 3
        && parts.iter().all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
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

/// Test-only synchronization (issue #160 round-5 review — flake found while
/// verifying this round's own test suite, not a NEW hazard this round
/// introduces): every test — in THIS module's own `#[cfg(test)] mod tests`,
/// or in `bus::computer_srv`'s separate one — that touches ANY process-wide,
/// un-keyed static this module owns ([`control_mutex`], [`throttle_mutex`],
/// `shortcut_mutex`/the `SHORTCUT_*_ATTEMPTS` counters, [`stop_state`],
/// [`STOP_PERSIST_FAILED`]) must acquire this lock for its own duration. `cargo test`'s default
/// parallel test threads would otherwise interleave two such tests' own
/// acquire/clear/store calls against the exact SAME global state — several of
/// this file's own pre-existing test doc comments already named this exact
/// hazard ("splitting them would let cargo test's parallel test threads stomp
/// on each other's lock state") without actually adding synchronization for
/// it, which is why it surfaced as a real, reproducible flake under `cargo
/// test --lib bus::`/`--lib computer` once enough tests touching these
/// statics existed for the odds to catch up. A test that only touches its
/// OWN isolated (thread, dir)-keyed state (the preview registry,
/// `recent_clicks`, an in-memory `Db`, a `MockBackend` it alone owns) does
/// NOT need this — only the truly global, unkeyed statics do.
///
/// `#[doc(hidden)] pub` (not `#[cfg(test)]`): `bus::computer_srv`'s test
/// module is a DIFFERENT module in the same crate that needs the SAME lock —
/// mirrors `backend::_set_backend_override`'s own reasoning for exposing a
/// test-only item this way rather than cfg(test)-gating it, since an
/// integration/sibling-module test binary can't see a `cfg(test)` item in a
/// different compilation unit.
#[doc(hidden)]
pub fn process_state_test_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
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
/// a LIVE same-holder renewal — syncs the OS-level global Escape shortcut
/// (see [`sync_shortcut_state`]), so it exists only while a lease is
/// genuinely live.
pub fn acquire_control(thread: i32, dir: &str) -> Result<(), ComputerError> {
    let now = now_ms();
    let sync_needed;
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
                // from the earlier acquire is already registered and stays
                // that way, so this skips syncing it again. Every other path
                // that reaches here (nobody held it live, or the previous
                // lease already expired) is a FRESH hold as far as the
                // shortcut is concerned, even when `(thread, dir)` happens to
                // match the previous holder.
                sync_needed = !(is_live && is_same_holder);
            }
            None => sync_needed = true,
        }
        *guard = Some(ControlHolderState {
            thread,
            dir: dir.to_string(),
            expires_at_ms: now + CONTROL_LEASE_MS,
        });
    }
    // Synced OUTSIDE the mutex guard (issue #160 review R1 #5): the Escape
    // callback spawns a task that eventually calls `clear_control`, which
    // takes this SAME mutex — acting while still holding the lock here would
    // risk a reentrant deadlock if that path ever collapsed onto this call
    // synchronously. issue #160 round-4 P2 §4: this is why the DECISION
    // ("does the shortcut need to change") is taken inside the lock above,
    // but the ACTUAL register/unregister call is [`sync_shortcut_state`] —
    // see that function's own doc for how it closes the race a bare
    // `register_global_escape()` call here would reopen.
    if sync_needed {
        sync_shortcut_state();
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

/// The state-mutation half of [`control_state`]'s lazy-expiry cleanup,
/// split out purely so a unit test can drive it and [`sync_shortcut_state`]
/// as two SEPARATE steps with another caller's own full acquire/sync cycle
/// injected in between — deterministically reproducing the exact
/// interleaving issue #160 round-4 P2 §4 fixes (see this section's own top
/// doc comment) without depending on real thread scheduling. Not `pub`:
/// [`control_state`] is still the only production entry point.
fn control_state_detect_and_clear_if_expired() -> (Option<ControlHolder>, bool) {
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
}

/// The current holder, or `None` when nobody holds it OR the lease expired.
/// An expired lease is cleaned up right here (lazily, on read) rather than
/// left for the next [`acquire_control`] to notice — issue #160 review R1
/// #5: that lazy cleanup is also this module's other sync trigger for the
/// OS-level global Escape shortcut (see [`sync_shortcut_state`]), since an
/// expired lease means nobody is actually driving the desktop anymore.
pub fn control_state() -> Option<ControlHolder> {
    let (result, expired) = control_state_detect_and_clear_if_expired();
    // Synced OUTSIDE the mutex guard — same reentrancy concern as
    // `acquire_control`'s own sync call. issue #160 round-4 P2 §4: this is
    // the exact call site the race lived in — see [`sync_shortcut_state`]'s
    // own doc for how re-reading `control_mutex` fresh, right here, closes it.
    if expired {
        sync_shortcut_state();
    }
    result
}

/// Unconditionally drop the lease, regardless of who (if anyone) holds it —
/// the emergency-stop escape hatch (see [`emergency_stop`]), which must win
/// even over a session that still believes it's mid-lease. issue #160 review
/// R1 #5: also syncs the OS-level global Escape shortcut when there WAS a
/// holder to clear (a no-op call when there wasn't one is skipped rather than
/// attempted and swallowed) — issue #160 round-4 P2 §4: this call site has
/// the EXACT SAME "decide, unlock, then act" shape `control_state`'s own
/// lazy-expiry path does (and the OS-level Escape callback itself reaches
/// this via `emergency_stop`), so it goes through the SAME serialized
/// [`sync_shortcut_state`] rather than a bare `unregister_global_escape()`
/// call, for the identical race-closing reason.
pub fn clear_control() {
    let had_holder = {
        let mut guard = control_mutex().lock().unwrap_or_else(|e| e.into_inner());
        let had_holder = guard.is_some();
        *guard = None;
        had_holder
    };
    if had_holder {
        sync_shortcut_state();
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
///
/// issue #160 round-6 review P1 #1: the generation bump happens in the SAME
/// locked critical section as the `stopped` flip (see [`StopState`]'s own
/// doc) — so a `set_computer_use_enabled(true)` request that read
/// [`stop_generation`] before this call started is guaranteed to see a
/// DIFFERENT generation afterward, and its own later
/// [`clear_emergency_stop`] call is refused rather than undoing THIS stop.
///
/// issue #160 round-6 review P2 #6: on a persist failure, this also sets
/// [`STOP_PERSIST_FAILED`] — covering both callers (the Settings/command
/// path AND the OS-level global Escape shortcut's own spawned call below),
/// since round-4 only wired the frontend's error state to the command path's
/// own local `.catch`, leaving the Escape path's failures silently dropped.
///
/// issue #160 round-8 P2 #3: also CLEARS [`STOP_PERSIST_FAILED`] on a
/// successful persist — not just on an explicit [`clear_emergency_stop`], as
/// before. Without this, a first `emergency_stop` call whose write failed
/// (disk full, DB locked, …) would set the flag, and a human clicking Stop
/// AGAIN — the natural "retry" action, whose write this time succeeds — would
/// never see the banner clear: only `clear_emergency_stop` reset it, and
/// nothing routes back through that from a plain Stop retry. The flag is
/// recorded under a generation guard (mirroring [`StopState::generation`]'s
/// own read-before-write, clear-only-if-still-current discipline) so a SLOW
/// call's own (possibly late-arriving) success can never stomp a NEWER,
/// still-failing `emergency_stop`'s `true` — only the MOST RECENT call, by
/// generation, is allowed to record the flag's final value.
pub async fn emergency_stop(db: &crate::store::Db) -> Result<(), String> {
    let my_gen = {
        let mut guard = stop_state().lock().unwrap_or_else(|e| e.into_inner());
        guard.stopped = true;
        guard.generation = guard.generation.wrapping_add(1);
        guard.generation
    };
    clear_control();
    let result = crate::store::repo::set_setting(db, K_COMPUTER_USE_ENABLED, "false")
        .await
        .map_err(|e| e.to_string());
    // round-8 P2 #3: only record THIS call's outcome if no NEWER
    // `emergency_stop` has since bumped the generation again — otherwise that
    // newer call owns writing the flag for its own outcome, and a slow
    // success here must never clear a newer failure it knows nothing about.
    {
        let guard = stop_state().lock().unwrap_or_else(|e| e.into_inner());
        if guard.generation == my_gen {
            STOP_PERSIST_FAILED.store(result.is_err(), Ordering::SeqCst);
        }
    }
    result
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
//
// issue #160 round-4 P2 §4: `control_state`'s lazy-expiry cleanup used to
// decide "expired, nobody holds it anymore" INSIDE the `control_mutex`
// guard, then call `unregister_global_escape()` AFTER releasing it. Between
// that release and the unregister call actually running, a brand-new
// `acquire_control` for a DIFFERENT `(thread, dir)` could acquire the
// now-vacant lease and register its OWN shortcut — and the first caller's
// now-STALE unregister call, still in flight, would then tear down the
// brand-new holder's registration, leaving the new session's global
// emergency-stop key silently gone. `sync_shortcut_state` below closes this:
// every caller that might have changed "is anybody holding the lease"
// (`acquire_control`, `control_state`'s expiry path, `clear_control`) goes
// through this ONE serialized choke point instead of calling
// `register_global_escape`/`unregister_global_escape` directly, and it
// re-reads `control_mutex` FRESH the instant it gets its turn — so whichever
// call actually executes LAST always leaves the OS-level shortcut matching
// the CURRENT truth, never undoing a newer holder's registration with a
// decision that was already stale by the time it ran.

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
/// [`StopState::stopped`] latch are the mechanisms that actually have to
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
            // Error ignored (but not un-observable — see `STOP_PERSIST_FAILED`):
            // `emergency_stop` sets the in-memory latch BEFORE its own
            // fallible DB write, so the kill switch has already taken effect
            // even on an `Err` here — see that function's doc comment.
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

/// Serializes EVERY register/unregister decision+call for the OS-level
/// global Escape shortcut (issue #160 round-4 P2 §4 — see this section's own
/// top doc comment for the race this closes). A mutex of its OWN, SEPARATE
/// from `control_mutex` — never held at the same time as it (only ever
/// locked from inside [`sync_shortcut_state`], which takes and releases
/// `control_mutex` for a quick read before touching this one) and never
/// re-entered from within `register_global_escape`/`unregister_global_escape`
/// themselves: the Escape callback that could reach back into this module
/// runs on a LATER, independent task (`tauri::async_runtime::spawn`), never
/// synchronously during `on_shortcut`/`unregister` itself, so holding this
/// mutex across the OS call is safe.
fn shortcut_mutex() -> &'static Mutex<()> {
    static SHORTCUT: OnceLock<Mutex<()>> = OnceLock::new();
    SHORTCUT.get_or_init(|| Mutex::new(()))
}

/// Test-only observability for [`sync_shortcut_state`]'s own decisions
/// (issue #160 round-4 P2 §4) — `register_global_escape`/
/// `unregister_global_escape` both silently no-op without a
/// `tauri::AppHandle` (see their own doc comments), which `cargo test --lib`
/// never installs, so a test can't observe the real OS-level call directly.
/// These count every time `sync_shortcut_state` DECIDES to attempt one,
/// regardless of what the underlying (no-op-in-tests) OS call itself did —
/// enough to prove the SEQUENCING this fix cares about (did a stale cleanup
/// try to undo a live registration) without a real desktop shortcut manager.
#[cfg(test)]
static SHORTCUT_REGISTER_ATTEMPTS: AtomicU64 = AtomicU64::new(0);
#[cfg(test)]
static SHORTCUT_UNREGISTER_ATTEMPTS: AtomicU64 = AtomicU64::new(0);

/// Bring the OS-level global Escape shortcut's registration in line with the
/// control lease's CURRENT truth (issue #160 round-4 P2 §4) — the ONE choke
/// point [`acquire_control`], [`control_state`], and [`clear_control`] all
/// go through instead of calling `register_global_escape`/
/// `unregister_global_escape` directly. [`shortcut_mutex`] ensures at most
/// one of these runs at a time; each invocation re-reads `control_mutex`
/// FRESH the instant it gets its turn, rather than trusting whatever
/// "expired"/"had a holder" boolean its caller computed further up the call
/// stack (which may already be stale by the time this runs) — so whichever
/// call actually executes LAST always converges the OS-level shortcut to
/// match the truth AT THAT MOMENT, regardless of how many earlier callers
/// raced in with now-outdated decisions.
fn sync_shortcut_state() {
    let _serialize = shortcut_mutex().lock().unwrap_or_else(|e| e.into_inner());
    let now = now_ms();
    let holder_live = {
        let guard = control_mutex().lock().unwrap_or_else(|e| e.into_inner());
        matches!(guard.as_ref(), Some(h) if h.expires_at_ms > now)
    };
    if holder_live {
        #[cfg(test)]
        SHORTCUT_REGISTER_ATTEMPTS.fetch_add(1, Ordering::SeqCst);
        register_global_escape();
    } else {
        #[cfg(test)]
        SHORTCUT_UNREGISTER_ATTEMPTS.fetch_add(1, Ordering::SeqCst);
        unregister_global_escape();
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
///
/// issue #160 round-8 P2 #6: every token EXCEPT THE LAST must itself be a
/// modifier (`KeyToken::Meta`/`Control`/`Alt`/`Shift`) — `"a+b"` or
/// `"ctrl+a+b"` used to pass this check (each individual token is a valid
/// `KeyToken` on its own), but `os.rs`'s own `key()` then blindly treats
/// every token but the last as a modifier to press-and-hold before pressing
/// the final one — so `"a+b"` would silently hold `a` down and press `b`,
/// producing input the caller never asked for instead of being rejected as
/// the malformed shortcut it is. Checked once, here, at the single source
/// every consumer (`os.rs`, any future one) parses through — a single-token
/// combo (`"a"`, `"f5"`, `"return"`) has no non-final tokens to check at all
/// and is unaffected.
pub fn parse_key_combo(combo: &str) -> Result<Vec<KeyToken>, ComputerError> {
    let lower = combo.trim().to_ascii_lowercase();
    if lower.is_empty() {
        return Err(ComputerError::Unsupported("empty key combo".into()));
    }
    let tokens: Vec<KeyToken> = lower.split('+').map(key_token).collect::<Result<_, _>>()?;
    if let Some((_last, mods)) = tokens.split_last() {
        if !mods
            .iter()
            .all(|t| matches!(t, KeyToken::Meta | KeyToken::Control | KeyToken::Alt | KeyToken::Shift))
        {
            return Err(ComputerError::Unsupported(
                "a key combo's non-final tokens must all be modifiers (cmd/ctrl/alt/shift), e.g. \
                 `ctrl+a`, not `a+b`"
                    .into(),
            ));
        }
    }
    Ok(tokens)
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

    /// issue #160 round-5 review P2 §5: the newly-added terminal emulators
    /// are excluded, case-insensitively, exactly like the pre-existing
    /// entries — and an unrelated app is still not swept in by the expansion.
    #[test]
    fn round_5_added_terminal_apps_are_excluded() {
        for app in [
            "ghostty",
            "Ghostty",
            "foot",
            "Tilix",
            "rio",
            "st",
            "urxvt",
            "rxvt",
            "Terminator",
            "guake",
            "yakuake",
            "xfce4-terminal",
            "lxterminal",
            "qterminal",
            "deepin-terminal",
            "Tabby",
            "contour",
            "waveterm",
            "wave",
            "cool-retro-term",
            "Eterm",
            "sakura",
            "termite",
            "kgx",
        ] {
            assert!(is_excluded(app), "{app} must be excluded");
        }
        assert!(!is_excluded("Safari"), "an unrelated app must still not be excluded");
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
        // issue #160 round-6 review P2 #4: the id of the window actually
        // captured, not something a caller must re-resolve `query` to get.
        assert_eq!(shot.window_id, 9);
        let opened = image::open(&shot.path).unwrap();
        assert_eq!((opened.width(), opened.height()), (1280, 720));
    }

    /// issue #160 round-8 P2 #8: the saved PNG must be owner-only (`0600`),
    /// never the `image.save` default (`0644`, world/group-readable modulo
    /// umask) — a worktree in a shared/traversable directory must not leave
    /// screenshots (which can carry mail/browser/password-manager pixels)
    /// readable by any other local account for the retention window.
    #[cfg(unix)]
    #[test]
    fn screenshot_window_writes_the_png_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let backend = mock::MockBackend {
            windows: vec![window_sized(9, "Notes", "Untitled", 800, 600)],
            image: Some(solid_image(800, 600, 5)),
            ..Default::default()
        };
        let tmp = tempfile::tempdir().unwrap();
        let shot = screenshot_window(&backend, "9", tmp.path()).unwrap();

        let mode = std::fs::symlink_metadata(&shot.path).unwrap().permissions().mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "the screenshot PNG must be created owner-only, not `image.save`'s default 0644"
        );
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

    /// issue #160 round-5 review P2 §4: `out_dir` itself is ALREADY a symlink
    /// to an outside directory by the time `screenshot_window` runs — standing
    /// in for the TOCTOU window between `bus::computer_srv::screenshot_out_dir`'s
    /// own EARLIER `refuse_symlinks` check and this function's own write.
    /// `create_dir_all` is a no-op here (the symlink already resolves to a
    /// real, existing directory) and does NOT itself error — so only the
    /// dedicated `symlink_metadata` recheck this round adds can catch it. Must
    /// refuse with `ComputerError::Io`, and must never write the PNG through
    /// the symlink into the outside directory.
    #[cfg(unix)]
    #[test]
    fn screenshot_window_refuses_an_out_dir_thats_a_symlink() {
        let backend = mock::MockBackend {
            windows: vec![window_sized(9, "Notes", "Untitled", 800, 600)],
            image: Some(solid_image(800, 600, 5)),
            ..Default::default()
        };
        let base = std::env::temp_dir().join(format!("weft-screenshot-out-dir-sym-{}", std::process::id()));
        let outside =
            std::env::temp_dir().join(format!("weft-screenshot-out-dir-sym-outside-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::remove_dir_all(&outside);
        std::fs::create_dir_all(&outside).unwrap();
        // `base` itself (the `out_dir` this call receives) is a symlink to an
        // ALREADY-EXISTING outside directory — `create_dir_all(base)` sees a
        // real directory through the symlink and is a no-op, exactly the case
        // that makes a recheck necessary rather than redundant.
        std::os::unix::fs::symlink(&outside, &base).unwrap();

        let err = screenshot_window(&backend, "9", &base).unwrap_err();
        assert!(matches!(err, ComputerError::Io(_)), "{err}");
        assert!(err.to_string().contains("symlink"), "{err}");
        assert!(
            std::fs::read_dir(&outside).unwrap().next().is_none(),
            "must never write the screenshot through the symlinked out_dir"
        );

        let _ = std::fs::remove_dir_all(&outside);
        let _ = std::fs::remove_file(&base);
    }

    // —— Screenshot::pixels stay in memory (issue #160 round-7 P1) ——

    #[test]
    fn screenshot_window_pixels_are_the_in_memory_scaled_image_not_a_disk_reread() {
        let backend = mock::MockBackend {
            windows: vec![window_sized(9, "Notes", "Untitled", 2560, 1440)],
            image: Some(solid_image(2560, 1440, 200)),
            ..Default::default()
        };
        let tmp = tempfile::tempdir().unwrap();
        let shot = screenshot_window(&backend, "9", tmp.path()).unwrap();

        assert_eq!(shot.pixels.width, shot.width);
        assert_eq!(shot.pixels.height, shot.height);
        assert_eq!(shot.pixels.rgba.len(), (shot.width * shot.height * 4) as usize);
        assert!(
            shot.pixels.rgba.iter().all(|&b| b == 200),
            "downscaling a solid-color image with a triangle filter must keep it solid"
        );

        // Hardening: swap the on-disk PNG for a DIFFERENT image AFTER the
        // call already returned. `shot.pixels` must be totally unaffected —
        // proof that nothing downstream needs to (or does) re-open
        // `shot.path` to get pixels, closing the save-then-reopen symlink/
        // TOCTOU race a worker-writable `out_dir` would otherwise expose.
        let different = image::RgbaImage::from_pixel(shot.width, shot.height, image::Rgba([1, 2, 3, 4]));
        different.save(&shot.path).unwrap();
        assert!(
            shot.pixels.rgba.iter().all(|&b| b == 200),
            "shot.pixels must still be the ORIGINAL capture after the disk file was replaced"
        );
    }

    // —— recorded scale comes from the captured frame (issue #160 round-7 P2) ——

    #[test]
    fn screenshot_window_derives_scale_from_the_captured_frame_not_a_stale_resolve() {
        // `matched` (from resolve_window) reports 800x600 — scale 1.0 by the
        // OLD (buggy) formula. The mock's own capture returns a DIFFERENT
        // frame, 2000x1000 (long edge crosses the 1280 threshold) — standing
        // in for a resize landing between resolve_window and capture_window.
        // The recorded scale/dims must come from the CAPTURED frame, never
        // the stale `matched` geometry.
        let backend = mock::MockBackend {
            windows: vec![window_sized(9, "Notes", "Untitled", 800, 600)],
            image: Some(solid_image(2000, 1000, 77)),
            ..Default::default()
        };
        let tmp = tempfile::tempdir().unwrap();
        let shot = screenshot_window(&backend, "9", tmp.path()).unwrap();

        let expected_scale = display_scale_for_dims(2000, 1000);
        assert_ne!(expected_scale, 1.0, "the fixture must actually exercise a downscale to be meaningful");
        assert_eq!(shot.scale, expected_scale, "scale must come from the captured frame, not matched's stale 800x600");
        let expected = scale_capture(vec![77u8; 2000 * 1000 * 4], 2000, 1000, expected_scale).unwrap();
        assert_eq!(shot.width, expected.width());
        assert_eq!(shot.height, expected.height());
        assert_eq!(shot.pixels.width, expected.width());
        assert_eq!(shot.pixels.height, expected.height());
    }

    // —— prune_old_screenshots (issue #160 round-7 P1) ——

    #[cfg(unix)]
    fn set_mtime(path: &Path, seconds_from_epoch: u64) {
        let f = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        f.set_modified(UNIX_EPOCH + Duration::from_secs(seconds_from_epoch)).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn prune_old_screenshots_deletes_the_oldest_by_mtime_and_keeps_the_newest() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let keep = 5usize;
        let total = keep + 3;
        let mut paths = Vec::new();
        for i in 0..total {
            let p = dir.join(format!("{}-9-{i}.png", 1_700_000_000_000u64 + i as u64));
            std::fs::write(&p, b"fake png bytes").unwrap();
            // Strictly increasing mtimes, spaced well apart — deterministic
            // regardless of the filesystem's own mtime-write granularity.
            set_mtime(&p, 1_000_000 + i as u64);
            paths.push(p);
        }

        prune_old_screenshots(dir, keep);

        let remaining: std::collections::HashSet<PathBuf> = std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .collect();
        assert_eq!(remaining.len(), keep, "expected exactly {keep} files left: {remaining:?}");
        for p in &paths[..total - keep] {
            assert!(!remaining.contains(p), "{p:?} is one of the oldest and should have been pruned");
        }
        for p in &paths[total - keep..] {
            assert!(remaining.contains(p), "{p:?} is one of the newest and must survive pruning");
        }
    }

    #[cfg(unix)]
    #[test]
    fn prune_old_screenshots_never_touches_non_own_files_or_symlinks() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();

        // A lone own-named real file.
        let own = dir.join("111-9-0.png");
        std::fs::write(&own, b"fake").unwrap();

        // Non-own files — wrong shape — must survive regardless of age/count.
        let unrelated = dir.join("note.txt");
        std::fs::write(&unrelated, b"hello").unwrap();
        let odd_name = dir.join("abc.png");
        std::fs::write(&odd_name, b"hello").unwrap();

        // A symlink with an OWN-LOOKING name pointing at a real file
        // elsewhere — must never be followed, deleted, or counted against
        // `keep`, even though its name alone would pass `is_own_screenshot_filename`.
        let link_target = dir.join("real-target.png");
        std::fs::write(&link_target, b"real").unwrap();
        let symlink_path = dir.join("222-9-1.png");
        std::os::unix::fs::symlink(&link_target, &symlink_path).unwrap();

        // keep=0: pruning must consider every LEGIT candidate it finds — the
        // strongest possible test that non-own files/symlinks are skipped by
        // construction, not merely left alone because they were under the cap.
        prune_old_screenshots(dir, 0);

        assert!(!own.exists(), "the lone own-named real file should be pruned at keep=0");
        assert!(unrelated.exists(), "a non-own file must never be pruned");
        assert!(odd_name.exists(), "a non-matching .png name must never be pruned");
        assert!(
            std::fs::symlink_metadata(&symlink_path).is_ok(),
            "a symlink must never be pruned, even with an own-looking name and keep=0"
        );
        assert!(link_target.exists(), "the symlink's target must never be deleted either");
    }

    #[test]
    fn is_own_screenshot_filename_matches_only_the_exact_naming_scheme() {
        assert!(is_own_screenshot_filename(Path::new("1700000000000-9-3.png")));
        assert!(!is_own_screenshot_filename(Path::new("note.txt")));
        assert!(!is_own_screenshot_filename(Path::new("abc.png")), "non-digit parts must not match");
        assert!(!is_own_screenshot_filename(Path::new("1-2.png")), "must be exactly three parts");
        assert!(!is_own_screenshot_filename(Path::new("1-2-3.jpg")), "must be a .png extension");
        assert!(!is_own_screenshot_filename(Path::new("1--3.png")), "an empty middle part must not match");
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
        // stomp on each other's lock state. issue #160 round-5 review:
        // `process_state_test_lock` ALSO serializes this against every OTHER
        // test (in this file or `bus::computer_srv`'s) that touches the same
        // family of un-keyed globals — see that function's own doc for why
        // this was needed in addition to being "one sequential test".
        let _guard = process_state_test_lock().lock().unwrap_or_else(|e| e.into_inner());
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

    // —— issue #160 round-4 P2 §4: the Escape-shortcut unregister race ——
    //
    // Shares the SAME process-wide `control_mutex`/`shortcut_mutex` statics
    // as every other control-lock test in this file — kept as its own
    // sequential test(s) rather than splitting scenarios across parallel
    // `#[test]`s, for the identical reason `control_lock_busy_expiry_
    // release_and_clear` above already documents.

    /// The end-to-end property the fix exists for, reproduced deterministically
    /// via [`control_state_detect_and_clear_if_expired`] (the SAME two-phase
    /// split `control_state` itself uses internally): a caller detects an
    /// expired lease and clears the holder, but — before its OWN belated
    /// `sync_shortcut_state` call actually runs — a DIFFERENT `(thread, dir)`
    /// races in, acquires the now-vacant lease, and runs its OWN full
    /// register cycle. The first caller's belated sync must NOT unregister
    /// the new holder's shortcut: `sync_shortcut_state` re-reads
    /// `control_mutex` fresh at the instant it actually runs, rather than
    /// trusting the stale `expired = true` it was handed.
    #[test]
    fn escape_shortcut_sync_survives_a_new_acquire_racing_in_before_a_belated_cleanup_runs() {
        // issue #160 round-5 review: see `process_state_test_lock`'s own doc —
        // this test's own `SHORTCUT_*_ATTEMPTS` counter assertions are exact
        // counts, so it's especially sensitive to another test's `acquire_control`/
        // `clear_control` call incrementing them concurrently.
        let _guard = process_state_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        clear_control();
        SHORTCUT_REGISTER_ATTEMPTS.store(0, Ordering::SeqCst);
        SHORTCUT_UNREGISTER_ATTEMPTS.store(0, Ordering::SeqCst);

        acquire_control(950_001, "a").unwrap();
        assert_eq!(SHORTCUT_REGISTER_ATTEMPTS.load(Ordering::SeqCst), 1, "the first hold must register");

        // Force the lease to look expired, exactly like
        // `control_lock_busy_expiry_release_and_clear` does above.
        {
            let mut guard = control_mutex().lock().unwrap();
            if let Some(h) = guard.as_mut() {
                h.expires_at_ms = now_ms().saturating_sub(1);
            }
        }

        // Phase 1 ONLY: detect the expiry and clear the holder — mirrors
        // exactly what `control_state` does internally BEFORE it calls
        // `sync_shortcut_state`. Its own belated sync call is deliberately
        // NOT made yet.
        let (_, expired) = control_state_detect_and_clear_if_expired();
        assert!(expired, "the manually-expired lease must be detected as expired");
        assert_eq!(
            SHORTCUT_UNREGISTER_ATTEMPTS.load(Ordering::SeqCst), 0,
            "detecting expiry alone must not itself sync anything yet"
        );

        // *** THE RACE ***: a DIFFERENT (thread, dir) acquires the now-vacant
        // lease and runs its OWN full register cycle BEFORE the first
        // caller's belated sync (below) ever runs.
        acquire_control(950_002, "b").unwrap();
        assert_eq!(SHORTCUT_REGISTER_ATTEMPTS.load(Ordering::SeqCst), 2, "the new holder must register too");

        // Phase 2, NOW (late): the ORIGINAL caller's belated sync finally
        // runs. Naive code that trusted the STALE `expired = true` it
        // computed above would unregister here and tear down thread 2's
        // brand-new shortcut; this implementation re-reads `control_mutex`
        // fresh instead, sees thread 2 is live, and (harmlessly) registers
        // again — it must NEVER unregister.
        sync_shortcut_state();

        assert_eq!(
            SHORTCUT_UNREGISTER_ATTEMPTS.load(Ordering::SeqCst), 0,
            "a belated cleanup must never unregister a NEW holder's live shortcut"
        );
        let held = control_state().unwrap();
        assert_eq!((held.thread, held.dir.as_str()), (950_002, "b"), "thread 2 must still hold the lease");

        // Cleanup: an actual clear (nobody live) must still unregister.
        clear_control();
        assert_eq!(
            SHORTCUT_UNREGISTER_ATTEMPTS.load(Ordering::SeqCst), 1,
            "clear_control on a real holder must actually unregister"
        );
    }

    #[tokio::test]
    async fn emergency_stop_disables_the_setting_and_clears_control() {
        // issue #160 round-5 review: see `process_state_test_lock`'s own doc.
        let _guard = process_state_test_lock().lock().unwrap_or_else(|e| e.into_inner());
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
        clear_emergency_stop(stop_generation());
    }

    // —— emergency-stop latch (issue #160 review R1 #1) ——

    #[tokio::test]
    async fn emergency_stop_latch_wins_over_a_true_setting_until_explicitly_cleared() {
        // `stop_state` is a process-wide static shared by every test in this
        // binary (same category of shared state as `control_mutex`/
        // `throttle_mutex` above) — kept as ONE test exercising the whole
        // latch lifecycle sequentially, ending with the latch cleared again,
        // rather than splitting it across tests that could interleave with
        // each other (or with `enabled_reads_true_false_and_missing` below)
        // under `cargo test`'s default parallel test threads. issue #160
        // round-5 review: that risk was previously only DOCUMENTED, not
        // actually enforced — `process_state_test_lock` closes it for real.
        let _guard = process_state_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        let db = Db::connect("sqlite::memory:").await.unwrap();
        clear_emergency_stop(stop_generation());
        crate::store::repo::set_setting(&db, K_COMPUTER_USE_ENABLED, "true")
            .await
            .unwrap();
        assert!(enabled(&db).await, "baseline: latch clear, setting true -> enabled");

        stop_state().lock().unwrap_or_else(|e| e.into_inner()).stopped = true;
        assert!(
            !enabled(&db).await,
            "the latch must win even though the underlying setting is still \"true\""
        );

        clear_emergency_stop(stop_generation());
        assert!(
            enabled(&db).await,
            "clearing the latch restores the underlying setting's value"
        );

        // Leave it cleared for any other test in this binary.
        clear_emergency_stop(stop_generation());
    }

    /// issue #160 round-6 review P1 #1: reproduces the exact race the
    /// generation check exists to close — a `set_computer_use_enabled(true)`
    /// request reads the stop-generation, then (while its own DB write is
    /// still "in flight" here, simulated by simply not having called
    /// `clear_emergency_stop` yet) a real Stop lands. The enable's own later
    /// `clear_emergency_stop` call, using the STALE generation it read
    /// before the Stop, must be refused — the later, explicit Stop wins.
    #[tokio::test]
    async fn clear_emergency_stop_refuses_a_stale_generation_from_an_enable_that_lost_a_race_to_a_later_stop() {
        let _guard = process_state_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        let db = Db::connect("sqlite::memory:").await.unwrap();
        // Known-clean starting point regardless of what an earlier test in
        // this binary left behind.
        clear_emergency_stop(stop_generation());
        crate::store::repo::set_setting(&db, K_COMPUTER_USE_ENABLED, "true")
            .await
            .unwrap();
        assert!(enabled(&db).await);

        // The "enable" request reads the generation BEFORE its own (here,
        // simulated) DB write.
        let stale_gen = stop_generation();

        // A real Stop lands WHILE that write is "in flight".
        emergency_stop(&db).await.unwrap();
        assert!(!enabled(&db).await, "the stop must win immediately");

        // The enable's write finishes and it tries to clear the latch with
        // the now-STALE generation it read before the stop happened.
        let cleared = clear_emergency_stop(stale_gen);
        assert!(!cleared, "a stale generation must not be allowed to clear a NEWER stop");
        assert!(!enabled(&db).await, "the latch must still be tripped — the later Stop wins");

        // The normal, no-race path: reading the CURRENT generation and
        // clearing with it succeeds. `emergency_stop` above persisted
        // "false" to the DB, so re-set it to "true" first (mirroring
        // `commands::set_computer_use_enabled`'s own write-then-clear order)
        // to prove the LATCH, not the DB row, was blocking `enabled`.
        let current_gen = stop_generation();
        assert!(
            clear_emergency_stop(current_gen),
            "clearing with the CURRENT (non-stale) generation must succeed"
        );
        crate::store::repo::set_setting(&db, K_COMPUTER_USE_ENABLED, "true")
            .await
            .unwrap();
        assert!(enabled(&db).await, "clearing the latch with a valid generation restores the setting's value");

        // Leave it cleared for any other test in this binary.
        clear_emergency_stop(stop_generation());
    }

    /// issue #160 round-6 review P2 #6: a persist failure inside
    /// `emergency_stop` (here: a read-only sqlite connection, standing in for
    /// a real disk-full/read-only-filesystem `set_setting` failure) must set
    /// `STOP_PERSIST_FAILED` — and a SUCCESSFUL `clear_emergency_stop` must
    /// reset it again.
    #[tokio::test]
    async fn emergency_stop_sets_stop_persist_failed_on_a_write_failure_and_clear_emergency_stop_resets_it() {
        let _guard = process_state_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        clear_emergency_stop(stop_generation());
        STOP_PERSIST_FAILED.store(false, Ordering::SeqCst);

        let db = Db::connect("sqlite::memory:").await.unwrap();
        use sea_orm::ConnectionTrait;
        // `PRAGMA query_only` fails every write at the SQLite engine level —
        // deterministic regardless of OS file permissions/process privilege
        // (a plain chmod-based trick would NOT reliably fail a write for a
        // process running as root, which this container does).
        db.0.execute_unprepared("PRAGMA query_only = ON;").await.unwrap();

        let result = emergency_stop(&db).await;
        assert!(result.is_err(), "a read-only connection must fail the persist write");
        assert!(stop_persist_failed(), "a failed persist must set the flag");
        // The in-memory latch itself must still have tripped — see
        // `emergency_stop`'s own doc: the latch flips BEFORE the fallible DB
        // write, so persistence failing never leaves the kill switch
        // fail-open.
        assert!(!enabled(&db).await);

        assert!(
            clear_emergency_stop(stop_generation()),
            "no concurrent stop landed here, so this must clear"
        );
        assert!(!stop_persist_failed(), "clearing the latch must reset the persist-failed flag too");

        // Leave it cleared for any other test in this binary.
        clear_emergency_stop(stop_generation());
    }

    /// issue #160 round-8 P2 #3: a successful RETRY of `emergency_stop` (a
    /// human clicking Stop again after an earlier attempt's own persist
    /// failed) must clear `STOP_PERSIST_FAILED` on its own — the horizontal
    /// case `clear_emergency_stop`'s own reset does NOT cover, since nothing
    /// about clicking Stop again routes through that function at all.
    #[tokio::test]
    async fn emergency_stop_clears_stop_persist_failed_on_a_successful_retry_without_clear_emergency_stop() {
        let _guard = process_state_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        clear_emergency_stop(stop_generation());
        STOP_PERSIST_FAILED.store(false, Ordering::SeqCst);

        let db = Db::connect("sqlite::memory:").await.unwrap();
        use sea_orm::ConnectionTrait;
        db.0.execute_unprepared("PRAGMA query_only = ON;").await.unwrap();

        let first = emergency_stop(&db).await;
        assert!(first.is_err(), "the first attempt's write must fail against a read-only connection");
        assert!(stop_persist_failed(), "a failed persist must set the flag");

        // The "retry": same call, now against a connection that can write —
        // standing in for the human clicking Stop again once the transient
        // condition (disk full, DB locked, ...) has cleared.
        db.0.execute_unprepared("PRAGMA query_only = OFF;").await.unwrap();
        let second = emergency_stop(&db).await;
        assert!(second.is_ok(), "the retry's own write must succeed against a writable connection");
        assert!(
            !stop_persist_failed(),
            "round-8 P2 #3: a successful retry must clear the flag on its own — the human's \
             \"try again\" action must be able to make the warning go away without a SEPARATE \
             re-enable-from-Settings round trip"
        );

        clear_emergency_stop(stop_generation());
    }

    /// issue #160 round-8 P2 #3: the generation guard around recording
    /// `STOP_PERSIST_FAILED` — an OLDER, slower `emergency_stop` call finishing
    /// AFTER a NEWER one already landed and recorded its own outcome must
    /// never overwrite that newer outcome. Reproduced by hand (real
    /// concurrent interleaving against an in-memory sqlite connection isn't
    /// reliably schedulable in a test): the "older" call's own generation
    /// bump is taken first, then a genuinely newer `emergency_stop` runs to
    /// completion (failing, recording `true`) before the older call's own
    /// tail (the same guarded store `emergency_stop` itself runs) executes.
    #[tokio::test]
    async fn emergency_stop_generation_guard_lets_a_newer_calls_outcome_win() {
        let _guard = process_state_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        clear_emergency_stop(stop_generation());
        STOP_PERSIST_FAILED.store(false, Ordering::SeqCst);

        let db = Db::connect("sqlite::memory:").await.unwrap();

        // The OLDER call's own generation bump — mirrors the first statement
        // of `emergency_stop`'s body exactly.
        let older_gen = {
            let mut g = stop_state().lock().unwrap_or_else(|e| e.into_inner());
            g.stopped = true;
            g.generation = g.generation.wrapping_add(1);
            g.generation
        };

        // A genuinely NEWER `emergency_stop` call lands and finishes
        // completely (bumps the generation again, fails, records `true`).
        use sea_orm::ConnectionTrait;
        db.0.execute_unprepared("PRAGMA query_only = ON;").await.unwrap();
        let newer_result = emergency_stop(&db).await;
        assert!(newer_result.is_err());
        assert!(stop_persist_failed(), "precondition: the newer call's own failure must be recorded");

        // The OLDER call's own write now finishes (successfully) — but by
        // the time it checks, the generation has moved on past `older_gen`,
        // so its guarded store must be skipped entirely, exactly mirroring
        // `emergency_stop`'s own tail.
        db.0.execute_unprepared("PRAGMA query_only = OFF;").await.unwrap();
        assert!(crate::store::repo::set_setting(&db, K_COMPUTER_USE_ENABLED, "false").await.is_ok());
        {
            let guard = stop_state().lock().unwrap_or_else(|e| e.into_inner());
            if guard.generation == older_gen {
                STOP_PERSIST_FAILED.store(false, Ordering::SeqCst);
            }
        }
        assert!(
            stop_persist_failed(),
            "a stale, OLDER call's own success must never clear a NEWER call's already-recorded failure"
        );

        clear_emergency_stop(stop_generation());
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

    /// issue #160 round-8 P2 #6: a combo whose non-final token is NOT a
    /// modifier must be rejected outright, rather than reaching `os.rs`'s
    /// `key()`, which would otherwise press-and-hold that token as though it
    /// were a modifier before pressing the final one.
    #[test]
    fn parse_key_combo_rejects_a_non_modifier_in_a_non_final_position() {
        assert!(parse_key_combo("a+b").is_err(), "two plain chars — neither is a modifier");
        assert!(parse_key_combo("ctrl+a+b").is_err(), "the middle token (\"a\") is not a modifier");
        assert!(parse_key_combo("return+s").is_err(), "a named key is not a modifier either");
    }

    /// The mirror image: every non-final token IS a modifier, and/or there's
    /// only one token total (nothing to check) — all still accepted exactly
    /// as before.
    #[test]
    fn parse_key_combo_accepts_combos_whose_non_final_tokens_are_all_modifiers() {
        assert!(parse_key_combo("ctrl+shift+t").is_ok());
        assert!(parse_key_combo("cmd+s").is_ok());
        assert!(parse_key_combo("a").is_ok(), "single token — nothing to check");
        assert!(parse_key_combo("f5").is_ok());
        assert!(parse_key_combo("return").is_ok());
    }

    // —— enabled() ——

    #[tokio::test]
    async fn enabled_reads_true_false_and_missing() {
        // `enabled()` reads the process-wide `stop_state` latch FIRST — see
        // `process_state_test_lock`'s own doc for why this must not
        // interleave with a test that flips that latch.
        let _guard = process_state_test_lock().lock().unwrap_or_else(|e| e.into_inner());
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
