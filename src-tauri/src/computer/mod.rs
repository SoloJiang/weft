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

/// SYNCHRONOUS read of the in-memory stop latch alone (issue #160 round-15
/// P1, Codex commands.rs:1619) — `true` the instant [`trip_stop_latch`] runs,
/// with no DB read and no `.await`. [`enabled`] already consults this latch
/// first, but is `async` (it falls through to the persisted setting);
/// `bus::computer_srv::approve` needs a latch-only check it can run INSIDE a
/// synchronous window right after publishing an Ask card — see that
/// function's post-insert self-check for the disable-transition race this
/// closes. Deliberately latch-only: a `false` here does NOT mean computer use
/// is enabled (the persisted setting may still be off) — every real
/// authorization path still goes through [`enabled`].
pub fn stop_latched() -> bool {
    stop_state().lock().unwrap_or_else(|e| e.into_inner()).stopped
}

/// A held lock on the stop state (issue #160 round-17 P1, Codex ask.rs:2174)
/// — for the ONE caller that must make "observe the latch, then act on that
/// observation" ATOMIC against a concurrently-tripping Stop:
/// `AskRegistry::answer`'s grant recording. [`stop_latched`] releases the
/// stop mutex the instant it returns, so a Stop could trip in the gap between
/// that read and the grant insertion it gated; holding THIS guard across the
/// insertion means [`trip_stop_latch`] cannot complete until the answer has
/// fully recorded — the answer then strictly precedes the Stop, which is the
/// legitimate "human answered, then stopped" ordering, never a grant minted
/// concurrently with (or after) the stop it should have been blocked by.
/// Callers must NEVER hold this across an `.await` (it is a sync
/// `MutexGuard`), and lock ordering is one-way — a caller may take this
/// while holding the ask-registry lock, but nothing anywhere takes the
/// registry lock while holding this — so no deadlock is possible.
pub struct StopLatchGuard(std::sync::MutexGuard<'static, StopState>);

impl StopLatchGuard {
    /// Whether the latch is tripped, read under the held lock — stable for
    /// the guard's whole lifetime.
    pub fn latched(&self) -> bool {
        self.0.stopped
    }
}

/// Acquire the stop-state lock and hold it — see [`StopLatchGuard`].
pub fn hold_stop_state() -> StopLatchGuard {
    StopLatchGuard(stop_state().lock().unwrap_or_else(|e| e.into_inner()))
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
    /// The OS-level global Escape shortcut — the kill switch's redundant
    /// layer for while a CONTROLLED app (not weft's own WebView) holds real
    /// OS focus — could not be registered (issue #160 round-10 P1 #7): some
    /// other app already claimed it, or the desktop/platform doesn't support
    /// it. Fail-closed: [`acquire_control`] refuses to grant control at all
    /// rather than let input proceed with no way for a human to Escape out
    /// while the target app has focus. Never constructed when no
    /// `tauri::AppHandle` subsystem exists at all (unit tests, a headless
    /// build) — see [`register_global_escape`]'s own doc for that
    /// distinction.
    EscapeUnavailable,
    /// A control lease exists for this exact `(thread, dir)` but its
    /// kill-switch registration has not yet been CONFIRMED live (issue #160
    /// round-15 P1, Codex mod.rs:1395) — see [`acquire_control`]'s own doc
    /// for the pending-registration race this closes. Distinct from
    /// [`EscapeUnavailable`](ComputerError::EscapeUnavailable): registration
    /// has not necessarily FAILED here, it just has not finished yet (or, in
    /// the rare case where it finished successfully but this exact lease was
    /// lost to expiry/takeover in the meantime), so the right move is a
    /// short retry rather than treating the kill switch as permanently
    /// unregisterable. Never returned for a renewal once `escape_ready` is
    /// `true` — only for a same-holder call that raced in before the FIRST
    /// hold's own registration round-trip confirmed it.
    EscapeRegistrationPending,
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
            ComputerError::EscapeUnavailable => write!(
                f,
                "the global emergency-stop shortcut (Escape) could not be registered — it's likely \
                 already claimed by another app — so computer control is refused; release whatever \
                 is holding Escape, or drive this from weft's own window instead"
            ),
            ComputerError::EscapeRegistrationPending => write!(
                f,
                "the emergency-stop kill switch is still being registered for this session's control \
                 lease — wait a moment and retry this call; it is not safe to inject input until the \
                 Escape shortcut is confirmed live"
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

// —— issue #160 round-11 P1 #D: map by SAVED screenshot geometry, not the
// window's CURRENT size ——
//
// [`map_to_physical`] above recomputes `display_scale` from `w`'s CURRENT
// `width`/`height` on every call — correct for a window that only MOVED
// since the screenshot an agent is reading coordinates off of, but wrong for
// one that RESIZED: a coordinate read off a screenshot taken at, say,
// 1280px wide is expressed in THAT image's own pixel grid, not "whatever
// grid a FRESH scale computed from the window's current (possibly now
// different) width would produce". Example the round-11 review named: a
// 2000px window captured at scale 0.64 (1280px saved), then resized down to
// 1000px BEFORE the next click — `map_to_physical`'s old math would treat
// screenshot-space `x=640` as if it were still "0.64 of the window's
// current width", landing on physical `x=640/0.64=1000` (the window's own
// current right edge) instead of the correct mid-point (`x=500`, half of
// the NOW-1000px-wide window — since 640 was the horizontal MIDPOINT of the
// 1280px-wide screenshot the agent actually looked at).
//
// [`map_screenshot_coord`] fixes this by taking the screenshot's OWN saved
// `shot_w`/`shot_h` as an explicit input (recorded at capture time — see
// [`record_shot_dims`]/[`shot_dims`] below) instead of re-deriving a scale
// from `current_w`'s present size, then mapping `(cx, cy)` as a FRACTION of
// that screenshot's own dimensions onto `current_w`'s CURRENT rectangle.
// Whatever `current_w` looks like NOW (moved, resized, or both since the
// screenshot), the same relative position within the window is what gets
// clicked — never a position derived from treating the screenshot as if it
// were sized to match the window's present dimensions.
pub fn map_screenshot_coord(
    current_w: &WindowInfo,
    shot_w: u32,
    shot_h: u32,
    cx: u32,
    cy: u32,
) -> Result<(i32, i32), ComputerError> {
    // Bounds are judged against the SCREENSHOT's own space, not the current
    // window's — an agent only ever saw pixels `0..shot_w` x `0..shot_h`, so
    // that (not whatever the window's current size happens to be) is the
    // honest range to reject outside of. This also makes `shot_w`/`shot_h`
    // of `0` (never produced by a real `screenshot_window` capture, but not
    // this function's job to assume) safely reject every coordinate rather
    // than divide by zero below.
    if cx >= shot_w || cy >= shot_h {
        return Err(ComputerError::OutOfBounds {
            x: cx,
            y: cy,
            max_x: shot_w.saturating_sub(1),
            max_y: shot_h.saturating_sub(1),
        });
    }
    // issue #160 round-23 P2 (Codex computer/mod.rs:677): clamp each mapped
    // offset to `width-1`/`height-1`. `cx`/`cy` are bounds-checked only against
    // the SCREENSHOT's own space (`cx < shot_w`), so a far-edge coordinate on a
    // window that SHRANK since the capture can round UP to exactly `width` —
    // e.g. `cx=1279` of `shot_w=1280` mapped onto a current width of `100`
    // rounds to `100`, landing on `current_w.x + width`, the EXCLUSIVE right
    // edge that sits one pixel OUTSIDE the approved window (valid offsets are
    // `0..=width-1`). Clamping keeps the click inside the window while leaving
    // every interior mapping bit-identical.
    let off_x = ((f64::from(cx) / f64::from(shot_w)) * f64::from(current_w.width)).round() as i32;
    let off_y = ((f64::from(cy) / f64::from(shot_h)) * f64::from(current_w.height)).round() as i32;
    let off_x = off_x.min(current_w.width.saturating_sub(1) as i32);
    let off_y = off_y.min(current_w.height.saturating_sub(1) as i32);
    Ok((current_w.x + off_x, current_w.y + off_y))
}

/// Process-level "most recent screenshot's OWN saved dimensions, per window"
/// registry (issue #160 round-11 P1 #D) — keyed by the FULL `(thread, dir,
/// window_id)` triple (not just `(thread, dir)`, unlike [`screenshot_previews`]
/// above): an agent may screenshot more than one window for the same
/// session, and a later input action against a DIFFERENT window than the
/// most-recently-screenshotted one must never silently reuse that other
/// window's dimensions. [`bus::computer_srv`]'s own screenshot arm calls
/// [`record_shot_dims`] once per successful capture (with `Screenshot::
/// width`/`height`/`window_id` — the SAVED image's own dimensions, already
/// downscaled if `display_scale` applied one, and the id
/// `screenshot_window` itself resolved and captured against); every
/// coordinate-taking input arm calls [`shot_dims`] before mapping and fails
/// CLOSED (never silently falls back to the window's current size) when
/// nothing is on file — see [`map_screenshot_coord`]'s own doc for why
/// reusing "current size" as a stand-in for "the screenshot's size" is
/// exactly the bug this round closes, so falling back to it here would
/// silently reopen it. In-memory only, like every other per-session
/// registry in this module (`recent_clicks`, `screenshot_previews`) — a
/// restart starting empty just means the FIRST input action after restart
/// needs a fresh screenshot first, which is the correct, safe default
/// anyway.
/// issue #160 round-12 P1 #2: the stored value now ALSO carries the window's
/// own `app`+`title` at capture time (`(width, height, app, title, ts)`), not
/// just its dimensions — see [`shot_dims_for`]'s own doc for why a bare
/// `window_id` key is not enough on its own to answer "is this still the
/// window that screenshot was taken of".
///
/// issue #160 round-12 P1 #C: the stored value now ALSO carries the window's
/// own `(x, y, width, height)` geometry AT CAPTURE TIME (see
/// [`ShotDimsEntry`]) — the strongest per-instance identity signal this
/// module has access to on top of `app`+`title`, given `xcap` (the backend
/// this feature is built on) exposes no stable per-window instance token
/// across separate calls (see [`shot_dims_for`]'s own doc for the honest
/// residual this leaves, and why it is recorded but deliberately NOT folded
/// into that function's own pass/fail gate).
fn recent_shot_dims() -> &'static Mutex<std::collections::HashMap<(i32, String, Option<i32>, u32), ShotDimsEntry>> {
    static DIMS: OnceLock<Mutex<std::collections::HashMap<(i32, String, Option<i32>, u32), ShotDimsEntry>>> = OnceLock::new();
    DIMS.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

/// One [`recent_shot_dims`] entry: the saved screenshot's own pixel
/// dimensions, the window's identity (`app`/`title`) at capture time (issue
/// #160 round-12 P1 #2), its full `(x, y, width, height)` GEOMETRY at that
/// same instant (round-12 P1 #C — see [`record_shot_dims`]'s own doc), and
/// the insertion timestamp [`evict_oldest_shot_dims_if_full`] evicts by.
#[derive(Debug, Clone)]
struct ShotDimsEntry {
    shot_width: u32,
    shot_height: u32,
    app: String,
    title: String,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    ts: u64,
}

/// This registry's capacity — mirrors [`MAX_PREVIEWS`]'s own reasoning (an
/// unbounded, never-evicted map is its own resource-exhaustion hazard once a
/// session can screenshot indefinitely many distinct windows), just keyed
/// one field wider (`window_id` included), so a single busy session
/// screenshotting many different windows can occupy more of this table's
/// capacity than it would of the coarser `(thread, dir)`-keyed one. Sized
/// generously above any realistic number of DISTINCT (session, window) pairs
/// simultaneously in flight.
const MAX_SHOT_DIMS: usize = 128;

/// Evict the single oldest entry (by insertion timestamp) if `map` is
/// already at capacity — same shape as [`evict_oldest_if_full`] above, kept
/// as its own function (rather than reusing that one) since the key/value
/// shapes differ (a `window_id`-widened key, no preview payload).
fn evict_oldest_shot_dims_if_full(map: &mut std::collections::HashMap<(i32, String, Option<i32>, u32), ShotDimsEntry>) {
    if map.len() < MAX_SHOT_DIMS {
        return;
    }
    if let Some(oldest_key) = map.iter().min_by_key(|(_, entry)| entry.ts).map(|(k, _)| k.clone()) {
        map.remove(&oldest_key);
    }
}

/// Record a SAVED screenshot's own dimensions AND the window identity +
/// geometry it was captured against, for `(thread, dir, wt, window_id)`,
/// refreshing whatever was on file for that exact key. `wt` (issue #160
/// round-26 P2, Codex mod.rs:797) isolates SIBLING worker sessions of one
/// multi-repo direction — same `(thread, dir)`, different worktree — so one
/// sibling's fresh screenshot can never overwrite the dimensions another
/// sibling's approved input is about to map coordinates against (a
/// wrong-geometry click on a resized window), matching the wt-precise
/// bearer and control lease (issue #160
/// round-11 P1 #D; `app`/`title` added round-12 P1 #2, geometry added
/// round-12 P1 #C — see [`shot_dims_for`]'s own doc for both). `w` is the
/// window `screenshot_window` actually captured against, resolved as close
/// to the capture as this call site can manage (see `bus::computer_srv`'s
/// screenshot arm, round-12 P1 #I) — its `app`/`title`/`x`/`y`/`width`/
/// `height` are what get stored; `shot_width`/`shot_height` are the SAVED
/// IMAGE's own (possibly display-scaled) pixel dimensions, a SEPARATE number
/// from `w`'s own on-screen geometry. Called ONLY from `bus::computer_srv`'s
/// screenshot arm, right after a capture actually succeeds — see
/// [`recent_shot_dims`]'s own doc for the full contract. `#[doc(hidden)]
/// pub`: `bus::computer_srv` is a sibling module (not a child of this one)
/// and `tests/computer_mcp.rs` is a separate integration-test crate — both
/// need to call this directly (production code calls it after a real
/// capture; tests seed it directly to exercise an input action's mapping
/// without driving an actual screenshot round-trip first) — mirrors
/// `mock::MockBackend`'s own doc comment on why a cross-module/cross-crate
/// test-visible item is `#[doc(hidden)] pub` rather than
/// `pub(crate)`/`#[cfg(test)]`.
#[doc(hidden)]
pub fn record_shot_dims(thread: i32, dir: &str, wt: Option<i32>, window_id: u32, shot_width: u32, shot_height: u32, w: &WindowInfo) {
    let mut g = recent_shot_dims().lock().unwrap_or_else(|e| e.into_inner());
    let key = (thread, dir.to_string(), wt, window_id);
    if !g.contains_key(&key) {
        evict_oldest_shot_dims_if_full(&mut g);
    }
    g.insert(
        key,
        ShotDimsEntry {
            shot_width,
            shot_height,
            app: w.app.clone(),
            title: w.title.clone(),
            x: w.x,
            y: w.y,
            width: w.width,
            height: w.height,
            ts: now_ms(),
        },
    );
}

/// The most recently recorded screenshot dimensions for `w` under `(thread,
/// dir, w.id)`, if any AND if the recorded capture's own `app`+`title` still
/// match `w`'s CURRENT identity — issue #160 round-12 P1 #2 (Codex 1298):
/// before this, the registry was keyed by `(thread, dir, window_id)` alone,
/// so once a window closed and the OS/window manager reused its numeric id
/// for an entirely unrelated window, a coordinate-taking input arm would
/// silently look up (and map against) the OLD window's saved dimensions —
/// bypassing the "screenshot this window first" fail-closed rule and mapping
/// the agent's screenshot-space coordinate onto the WRONG window's geometry.
/// Verifying `app`+`title` here (the same two fields `bus::computer_srv::
/// ApprovedWindow`/`verify_approved_target` already use to detect an id-reuse
/// swap at the approval layer — see that struct's own doc) closes the
/// identical gap at the coordinate-mapping layer: an id match with a
/// DIFFERENT app/title reads as "no record at all" (`None`), the exact
/// fail-closed outcome a window that was never screenshotted gets, rather
/// than a stale hit. `None` for every case `shot_dims`'s old contract already
/// covered (never screenshotted this session, evicted) PLUS this new one
/// (screenshotted, but a DIFFERENT window now holds that id) — every
/// coordinate-taking input arm in `bus::computer_srv` treats any `None` the
/// same way, so this is a strict narrowing of what counts as a hit, never a
/// behavior change for the common case (see [`map_screenshot_coord`]'s own
/// doc).
///
/// issue #160 round-12 P1 #C — geometry is recorded (see
/// [`ShotDimsEntry`]/[`record_shot_dims`]) but DELIBERATELY NOT compared
/// here, even though an id+app+title match with DIFFERENT geometry is
/// exactly the "same id, coincidentally same app/title, actually a
/// different window" case this round set out to narrow. Gating on it would
/// regress TWO already-shipped, deliberately tested properties this same
/// module relies on, both driven through this exact function:
///  - round-10 P1 #B (`bus::computer_srv::
///    left_click_uses_the_windows_geometry_as_of_after_activation_not_before`):
///    activation legitimately moves a window (a window manager
///    un-minimizing/refocusing it) — the ORIGIN changes — and the click
///    must still land correctly against its NEW position.
///  - round-11 P1 #D (`bus::computer_srv::
///    left_click_maps_the_screenshot_coordinate_proportionally_after_a_resize`,
///    "the end-to-end property this whole round exists for"): the window is
///    legitimately RESIZED between an earlier screenshot and this click —
///    the SIZE changes — and the click must still map proportionally
///    against the new rectangle.
/// Between them, EVERY field of `(x, y, width, height)` is exercised as a
/// legitimate, required-to-tolerate change by one test or the other — there
/// is no subset of geometry this function could gate on without breaking
/// one of the two. Closing the remaining gap for real needs a stable
/// per-window instance identity `xcap` does not expose across separate
/// calls (see this module's own top-of-file doc and issue #160 §9) — a
/// protocol-level "screenshot token" the agent round-trips, or an OS-level
/// window handle, either of which is a larger change than this function's
/// signature. This residual — an id the OS reuses for a genuinely different
/// window, with a coincidentally identical app+title AND a query that
/// resolves to it — is unchanged from round-12 P1 #2's own accepted
/// residual; recording geometry here keeps it available for a future,
/// protocol-aware fix (or for audit/forensic use) without regressing either
/// tested tolerance today.
#[doc(hidden)]
pub fn shot_dims_for(thread: i32, dir: &str, wt: Option<i32>, w: &WindowInfo) -> Option<(u32, u32)> {
    let g = recent_shot_dims().lock().unwrap_or_else(|e| e.into_inner());
    let entry = g.get(&(thread, dir.to_string(), wt, w.id))?;
    if entry.app == w.app && entry.title == w.title {
        Some((entry.shot_width, entry.shot_height))
    } else {
        None
    }
}

/// The window geometry [`record_shot_dims`] saved for `(thread, dir, w.id)`
/// at capture time, if any AND if `app`+`title` still match — the SAME
/// identity gate [`shot_dims_for`] itself applies, exposed as its own
/// accessor (issue #160 round-12 P1 #C) so a caller that wants to REASON
/// about geometry drift (audit/forensics, or a future stricter check once a
/// real per-window instance token exists — see [`shot_dims_for`]'s own doc
/// for why that check does not live in this round's hot path) can do so
/// without duplicating the lookup. Not consumed by any production caller
/// yet — see [`shot_dims_for`]'s own doc for the two shipped, tested
/// tolerances (round-10 P1 #B, round-11 P1 #D) that keep this deliberately
/// unwired from the coordinate-mapping path today.
#[doc(hidden)]
pub fn shot_geometry_for(thread: i32, dir: &str, wt: Option<i32>, w: &WindowInfo) -> Option<(i32, i32, u32, u32)> {
    let g = recent_shot_dims().lock().unwrap_or_else(|e| e.into_inner());
    let entry = g.get(&(thread, dir.to_string(), wt, w.id))?;
    if entry.app == w.app && entry.title == w.title {
        Some((entry.x, entry.y, entry.width, entry.height))
    } else {
        None
    }
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
///
/// issue #160 round-14 P2: `create_new` above only guarantees the leaf didn't
/// exist a moment ago — it says nothing about what happens if the
/// SUBSEQUENT `write_to`/`flush` (or, on non-unix, `image.save`, which
/// creates AND writes in one call) then fails. A full disk or a hit quota
/// can trip well after the file itself was successfully created, and until
/// this round that left an empty or truncated PNG sitting on disk under a
/// name [`SHOT_SEQ`] guarantees this process never reuses — so nothing ever
/// cleaned it up again: not a retry (the next call picks a brand-new name),
/// and not [`prune_old_screenshots`] (it doesn't even run on this error
/// path, since this whole write block returns before reaching that call
/// below). A caller retrying in a loop against a persistently full disk
/// could accumulate one corrupt file per attempt without bound, at worst
/// exhausting inodes rather than just bytes. [`cleanup_on_err`] closes this:
/// wrapped around the write step on both platforms, it best-effort deletes
/// whatever ended up at `path` the instant that step errors, then lets the
/// ORIGINAL error propagate untouched — a screenshot call that fails never
/// again leaves a file behind for a future `prune_old_screenshots` pass (or
/// a human) to have to notice and reap.
pub fn screenshot_window(
    backend: &dyn backend::ComputerBackend,
    query: &str,
    out_dir: &Path,
) -> Result<Screenshot, ComputerError> {
    let matched = resolve_window(backend, query)?;
    screenshot_resolved(backend, &matched, out_dir)
}

/// Capture an ALREADY-RESOLVED window — issue #160 round-17 P1 (Codex
/// computer_srv.rs:793). [`screenshot_window`] above resolves its query
/// itself, which was one resolution too many for `bus::computer_srv`'s
/// screenshot arm: that caller had ALREADY resolved and identity-verified its
/// target (`resolve_and_verify_target`) an instant earlier, and handing this
/// module the raw QUERY back meant a second, independent enumeration — a
/// window closing and a same-query replacement appearing between those two
/// enumerations would be captured under an approval shown only for the
/// original. Taking the verified [`WindowInfo`] instead pins the capture to
/// the exact identity the caller verified: there is no re-resolution left to
/// drift, and if that window is gone — or its numeric id was REUSED by a
/// different window (issue #160 round-32 P1, Codex os.rs:30: the backend's
/// own capture-time enumeration now re-verifies `app`/`title` on the exact
/// handle it is about to capture, not the id alone) — by capture time,
/// `backend.capture_window(matched)` fails closed rather than falling back
/// to a lookalike.
pub fn screenshot_resolved(
    backend: &dyn backend::ComputerBackend,
    matched: &WindowInfo,
    out_dir: &Path,
) -> Result<Screenshot, ComputerError> {
    let captured = backend.capture_window(matched)?;
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
        // round-14 P2: from this point on, `path` definitely exists on disk
        // (just created by `create_new` above) — `cleanup_on_err` wraps the
        // write+flush so any failure below best-effort deletes that
        // now-incomplete file before this function's `?` returns the
        // ORIGINAL error. See this function's own doc comment above for why.
        cleanup_on_err(&path, || {
            image
                .write_to(&mut w, image::ImageFormat::Png)
                .map_err(|e| ComputerError::Io(e.to_string()))?;
            // issue #160 round-10 P2 #G: `write_to` only writes through the
            // `BufWriter` — it does NOT itself guarantee the final buffered
            // bytes reach the underlying file, and `BufWriter::drop` (which would
            // otherwise run implicitly at the end of this block) SILENTLY
            // SWALLOWS a flush error rather than propagating it. On a full disk
            // or a hit quota (especially likely here: the whole PNG can still be
            // sitting in the buffer for a small screenshot), that dropped error
            // would let this function return `Ok` — reporting a successful save
            // — for a file that is actually empty or truncated. An explicit,
            // propagated `flush` closes that: a flush failure now surfaces as
            // this function's own `Err` instead of a silently-corrupt `Ok`.
            use std::io::Write as _;
            w.flush().map_err(|e| ComputerError::Io(e.to_string()))
        })?;
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        // issue #160 round-20 P2 (Codex computer/mod.rs:1089): create the PNG
        // with an owner-only DACL BEFORE any pixels are written — the Windows
        // analog of the unix `0o600` create above. Otherwise `image.save`
        // creates it under the (possibly permissive) inherited directory ACL,
        // leaving captured mail/browser/password-manager pixels readable by
        // other local accounts on a shared/traversable `WEFT_HOME`. Fail-CLOSED:
        // if the file can't be locked down, remove it and error rather than
        // leave an unprotected capture on disk.
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|e| ComputerError::Io(e.to_string()))?;
        if !crate::bus::inject::restrict_handle_to_owner(file.as_raw_handle()) {
            drop(file);
            let _ = std::fs::remove_file(&path);
            return Err(ComputerError::Io(
                "could not apply an owner-only ACL to the screenshot file".into(),
            ));
        }
        let mut w = std::io::BufWriter::new(file);
        // Same `cleanup_on_err` + explicit-flush guarantee as the unix branch.
        cleanup_on_err(&path, || {
            image
                .write_to(&mut w, image::ImageFormat::Png)
                .map_err(|e| ComputerError::Io(e.to_string()))?;
            use std::io::Write as _;
            w.flush().map_err(|e| ComputerError::Io(e.to_string()))
        })?;
    }
    #[cfg(not(any(unix, windows)))]
    {
        // `save` both creates and writes in one call, so a failure here may
        // still have left an empty/truncated file behind — same
        // `cleanup_on_err` guarantee as the branches above.
        cleanup_on_err(&path, || image.save(&path).map_err(|e| ComputerError::Io(e.to_string())))?;
    }
    // round-7 P1: keep the just-saved pixels in memory instead of ever having
    // a caller re-open `path` to get them back — see `Screenshot::pixels`'s
    // own doc for the symlink/TOCTOU race that reopen would risk. `into_raw`
    // consumes `image`, so this runs only after the write above (which only
    // borrows it) has already finished.
    let pixels = CapturedImage { rgba: image.into_raw(), width, height };

    // round-7 P1: best-effort retention cap — never fails a screenshot that
    // already saved successfully just because pruning stumbled. round-13 P2:
    // pass `&path` as `just_written` so THIS call's own file is exempt from
    // pruning no matter how it sorts — see `prune_old_screenshots`'s own doc.
    prune_old_screenshots(out_dir, MAX_RETAINED_SCREENSHOTS, Some(&path));

    Ok(Screenshot {
        path,
        width,
        height,
        scale,
        window_id: matched.id,
        pixels,
    })
}

/// Best-effort corrupt-file cleanup wrapper for [`screenshot_window`]'s
/// on-disk PNG write (issue #160 round-14 P2). `write` performs the actual
/// platform write (unix: `write_to` + `flush` into an already-`create_new`'d
/// file handle; non-unix: `image.save`, which creates and writes in one
/// call) and may fail AFTER it has already put bytes — or an empty file — on
/// disk at `path`. On `Err`, this deletes whatever ended up at `path` and
/// then returns the ORIGINAL error completely untouched: `remove_file`'s own
/// outcome is deliberately discarded (`let _ =`), including the case where
/// `write` failed before ever creating anything and there's nothing there to
/// remove — a cleanup failure (or no-op) must never mask or replace the real
/// I/O error the caller needs to see. On `Ok`, this never touches the
/// filesystem at all.
fn cleanup_on_err<T>(path: &Path, write: impl FnOnce() -> Result<T, ComputerError>) -> Result<T, ComputerError> {
    write().map_err(|e| {
        let _ = std::fs::remove_file(path);
        e
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
/// than relying on that alone.
///
/// `just_written` is [`screenshot_window`]'s own freshly-saved `path`, if
/// this call is happening right after a save (it is, at this function's one
/// call site) — `None` from a test that just wants the plain retention-cap
/// behavior. When `Some`, that EXACT path is never removed by this call, no
/// matter where it lands in the sort below — a caller holding this call's
/// own `Screenshot::path` is guaranteed a file still exists there afterward.
///
/// issue #160 round-13 P2: sorting used to be mtime-descending ONLY. On a
/// filesystem with coarse mtime resolution (some FAT-family/network
/// filesystems round to whole seconds or worse), or simply several
/// screenshots landing in the same tick, multiple entries can compare
/// EQUAL by mtime — and since `read_dir`'s own iteration order is
/// unspecified, a `sort_by` that never breaks that tie could put the
/// just-saved file anywhere among the equal-mtime group, including past
/// `keep` and into the deleted tail. The two fixes below close that:
///  1. a deterministic tiebreak — mtime descending, THEN the filename's own
///     trailing `<seq>` component (parsed by [`own_screenshot_seq`])
///     descending. `SHOT_SEQ` is a monotonic per-process counter, so within
///     one mtime tie, higher `seq` really is newer. This only disambiguates
///     WITHIN an mtime tie: mtimes across separate process runs essentially
///     never collide (each run starts `SHOT_SEQ` back at 0), so mtime
///     ordering still dominates across runs the way it always has — `seq`
///     is purely a same-tick tiebreak, never a substitute for it.
///  2. `just_written`, above — an explicit, absolute guarantee independent
///     of sort order entirely, for the one file this exact call is
///     responsible for having just created.
/// Both belong together: (1) makes the common case (many files, one real
/// mtime tie) sort correctly without needing the escape hatch at all; (2)
/// is the hard backstop for the pathological case (2) can't fully rule out
/// on its own — e.g. `keep` itself being 0.
fn prune_old_screenshots(out_dir: &Path, keep: usize, just_written: Option<&Path>) {
    let Ok(rd) = std::fs::read_dir(out_dir) else {
        return;
    };
    let mut shots: Vec<(SystemTime, u64, PathBuf)> = Vec::new();
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
        let seq = own_screenshot_seq(&path);
        shots.push((mtime, seq, path));
    }
    if shots.len() <= keep {
        return;
    }
    // newest first: mtime descending, then round-13 P2's seq tiebreak.
    shots.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
    for (_, _, path) in shots.into_iter().skip(keep) {
        if just_written.is_some_and(|p| p == path) {
            // round-13 P2: this is the file THIS call's own caller just
            // wrote — never delete it, even though it sorted past `keep`.
            continue;
        }
        let _ = std::fs::remove_file(&path);
    }
}

/// Parse the trailing `<seq>` component out of a filename already confirmed
/// by [`is_own_screenshot_filename`] to be this module's own
/// `<unix_ms>-<window_id>-<seq>.png` naming — [`prune_old_screenshots`]'s
/// round-13 P2 mtime-tie tiebreak; see that function's own doc. Never
/// panics: a parse failure (shouldn't happen, since the caller only reaches
/// this after `is_own_screenshot_filename` already confirmed all three
/// components are non-empty and all-digit) falls back to `0` — worst case,
/// a malformed name that slipped past that check loses the tiebreak rather
/// than crashing pruning entirely.
fn own_screenshot_seq(path: &Path) -> u64 {
    path.file_stem()
        .and_then(|s| s.to_str())
        .and_then(|stem| stem.split('-').nth(2))
        .and_then(|seq| seq.parse::<u64>().ok())
        .unwrap_or(0)
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
    /// The exact worktree this holder's session materialized into (issue #160
    /// round-25 P1, Codex mod.rs:1552) — `None` for the lead lane (no
    /// worktree). Two SIBLING worker sessions of one multi-repo direction share
    /// a single `(thread, dir)` and differ ONLY by `wt`; without it in the
    /// holder identity the second sibling read as the SAME holder and RENEWED
    /// the first's lease instead of getting `Busy`, letting the two agents
    /// interleave click/focus/type against the same physical desktop under one
    /// lease. Folded into every identity check ([`acquire_control`],
    /// [`release_control`], and `bus::computer_srv`'s post-queue rechecks) so a
    /// sibling is a DIFFERENT holder — the same `(thread, dir, wt)` triple the
    /// per-session bearer already binds (round-13/14).
    wt: Option<i32>,
    /// The AUTHORITATIVE lease deadline, on the monotonic clock (issue #160
    /// round-29 P2, Codex mod.rs:1625). This used to be the epoch-ms value
    /// below: a wall-clock correction BACKWARD after acquisition (NTP step, a
    /// manual change, resume-time correction) then kept the nominal 30s lease
    /// live for the whole rollback — the old session could keep renewing while
    /// every other session got `Busy`, and the Escape registration/banner
    /// stayed armed far longer than promised. `Instant` is immune to clock
    /// adjustments, so every LIVENESS judgment ([`holder_is_live`]'s callers)
    /// now reads this; the epoch-ms field below survives ONLY as the
    /// serialized display estimate.
    expires_at: std::time::Instant,
    /// Display-only wall-clock estimate of the same deadline, for the
    /// serialized [`ControlHolder`] snapshot the Settings banner reads. NEVER
    /// consulted for liveness — see `expires_at` above.
    expires_at_ms: u64,
    /// Whether THIS hold's OS-level global Escape shortcut registration has
    /// actually finished and passed [`escape_guard_permits_control`] (issue
    /// #160 round-15 P1, Codex mod.rs:1395) — `false` from the instant a
    /// FRESH hold is written in [`acquire_control`]'s locked section until
    /// its later, unlocked `sync_shortcut_state`/`escape_guard_permits_
    /// control` round-trip confirms the kill switch is actually live, then
    /// flips to `true`. A LIVE same-holder re-acquire while this is still
    /// `false` is refused outright (see [`acquire_control`]'s own doc) rather
    /// than treated as an ordinary renewal — otherwise a second call from the
    /// SAME session, racing in while the first call's registration is still
    /// in flight (e.g. stalled past the 500ms throttle window), would read
    /// "live same holder" and start injecting input with no confirmed Escape
    /// kill switch at all. Not part of the public [`ControlHolder`] snapshot
    /// the Settings banner reads — that UI only cares who holds the lease,
    /// not this in-between bookkeeping state.
    escape_ready: bool,
}

/// A snapshot of who currently holds the computer-use control lease, for
/// display (the Settings UI's kill-switch banner) — see [`control_state`].
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct ControlHolder {
    pub thread: i32,
    pub dir: String,
    /// The exact worktree the holder's session materialized into (issue #160
    /// round-25 P1) — `None` for the lead lane. Carried so the Settings banner
    /// and the post-queue lease rechecks in `bus::computer_srv` identify the
    /// holder by the SAME `(thread, dir, wt)` triple [`acquire_control`] keys
    /// on, and a sibling worker (same `(thread, dir)`, different `wt`) is never
    /// mistaken for the current holder.
    pub wt: Option<i32>,
    pub expires_at_ms: u64,
}

/// How long a control lease lasts before it's treated as abandoned. Chosen
/// to comfortably cover one input action's round trip (gate checks + a
/// backend call) while still self-healing quickly if a session crashes or
/// hangs mid-lease — there is no turn-boundary hook to release it
/// explicitly (see [`acquire_control`]'s doc comment).
const CONTROL_LEASE_MS: u64 = 30_000;

/// True while [`input_flight_guard`]'s returned guard is held — i.e. an input
/// action is ACTIVELY injecting via the real backend right now (issue #160
/// round-12 P1 #4). A bare `AtomicBool`, not a counter: `input_flight_guard`
/// is backed by a single global `tokio::sync::Mutex<()>`, so at most one
/// caller can ever hold it at a time — there is no multi-holder overlap to
/// count.
static INPUT_IN_FLIGHT: AtomicBool = AtomicBool::new(false);

/// Whether an input action is, RIGHT NOW, actively injecting via
/// [`input_flight_guard`]'s guard. Read by [`holder_is_live`] — see that
/// function's own doc for why this must keep a control-lease holder from
/// reading as expired while true.
fn input_in_flight() -> bool {
    INPUT_IN_FLIGHT.load(Ordering::SeqCst)
}

/// Whether a control-lease holder with monotonic deadline `expires_at` should
/// still be treated as live at `now` — issue #160 round-12 P1 #4 (Codex 1198);
/// both instants come from the monotonic clock since round-29 P2 (see
/// [`ControlHolderState::expires_at`]'s doc for the backward-correction hazard
/// the epoch-ms comparison had).
/// Normally just "hasn't hit its `CONTROL_LEASE_MS` sliding-window expiry
/// yet", but ALSO true, regardless of the deadline, whenever
/// [`input_in_flight`] is true: a synchronous injection (a long `type`, or a
/// blocking OS backend call) can legitimately run past `CONTROL_LEASE_MS`
/// while still actively driving the desktop, and treating that as "expired"
/// mid-injection is exactly the bug this round closes — the lease would read
/// as abandoned, [`sync_shortcut_state`] would unregister the OS-level Escape
/// kill switch, and the Settings banner (which polls [`control_state`]) would
/// read "no holder" — BOTH Stop surfaces vanishing while an action is still
/// actively running, the one moment a human is most likely to want them.
/// The ONE place all three call sites that judge liveness
/// ([`acquire_control`]'s own is-it-busy check, [`control_state_detect_and_
/// clear_if_expired`], and [`sync_shortcut_state`]'s own `holder_live`) make
/// this judgment, so they can never drift apart on what "live" means.
///
/// KNOWN, ACCEPTED residual (documented here and at [`input_flight_guard`]'s
/// own call sites in `bus::computer_srv`): this keeps the KILL-SWITCH
/// SURFACES alive for the FULL duration of an in-flight injection — it does
/// NOT make the injection itself interruptible. `enigo` (the real backend)
/// has no cancellation hook, so the one synchronous call already in flight
/// when a human hits Stop still runs to completion; what this closes is
/// "Stop's own UI vanishes and the lease looks abandoned while that's
/// happening", not "make an in-progress `type` stoppable mid-keystroke" —
/// that would need the backend's own input calls broken into cancellable
/// chunks, a larger change tracked separately (see round-5's own notes on
/// this).
fn holder_is_live(expires_at: std::time::Instant, now: std::time::Instant) -> bool {
    expires_at > now || input_in_flight()
}

/// `now + CONTROL_LEASE_MS` on the monotonic clock. `checked_add` (never a
/// bare `+`, which panics on overflow): unreachable in practice — the
/// monotonic clock would have to sit within 30s of its representable maximum
/// — but the fallback is `now` itself, i.e. an ALREADY-EXPIRED lease, which
/// fails closed (denies control) rather than ever granting one that outlives
/// its window.
fn lease_deadline(now: std::time::Instant) -> std::time::Instant {
    now.checked_add(std::time::Duration::from_millis(CONTROL_LEASE_MS))
        .unwrap_or(now)
}

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
/// [`STOP_PERSIST_FAILED`], and `INPUT_IN_FLIGHT` — set for as long as any
/// [`input_flight_guard`] guard lives, and read by every lease-liveness
/// judgment) must acquire this lock for its own duration. `cargo test`'s default
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
/// `(thread, dir)` is re-acquiring with a CONFIRMED kill switch already in
/// place; fails [`ComputerError::Busy`] when a DIFFERENT, still-live holder
/// has it, and fails [`ComputerError::EscapeRegistrationPending`] when the
/// SAME holder's own kill-switch registration is still in flight (see below).
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
///
/// issue #160 round-15 P1 (Codex mod.rs:1395): a control-lease acquire and
/// the OS-level Escape registration it triggers were not serialized against
/// EACH OTHER — a fresh hold writes the new `ControlHolderState` and releases
/// the lock BEFORE its own `sync_shortcut_state`/`escape_guard_permits_
/// control` round-trip (register a real OS shortcut, possibly slow, possibly
/// failing) runs. A second call from the exact SAME `(thread, dir)` racing in
/// during that window used to read "live same holder" and renew — sailing
/// straight through the flight guard and injecting input — while the kill
/// switch was still unregistered, or had just failed to register outright;
/// the first call's own eventual rollback (on failure) cannot undo input the
/// second call already sent. [`ControlHolderState::escape_ready`] closes
/// this: a fresh hold starts `escape_ready: false`, and a live same-holder
/// re-acquire while it is still `false` is REFUSED (not renewed) with
/// `EscapeRegistrationPending` — no state changes on that path, so the
/// original in-flight registration is left completely alone to finish on its
/// own. `escape_ready` only flips to `true` once THIS call's own
/// registration round-trip below actually confirms the kill switch, at which
/// point ordinary renewals resume. Deliberately does NOT change
/// [`holder_is_live`]: a pending hold still counts as live within its
/// `expires_at_ms` window, so a DIFFERENT `(thread, dir)` racing in during
/// the same registration window still gets `Busy` (never steals the slot),
/// and a registration that hangs forever still self-heals via the normal 30s
/// lease expiry rather than wedging the desktop.
pub fn acquire_control(thread: i32, dir: &str, wt: Option<i32>) -> Result<(), ComputerError> {
    let now = std::time::Instant::now();
    let sync_needed;
    {
        let mut guard = control_mutex().lock().unwrap_or_else(|e| e.into_inner());
        // issue #160 round-34 P2 (Codex computer_srv.rs:2800): refuse any
        // acquisition — fresh hold or renewal — while the stop latch is set,
        // checked INSIDE this lock so it is atomic against
        // [`trip_stop_latch`]'s own set-latch-then-`clear_control` sequence:
        // either this check sees the latch (refused, nothing installed), or
        // the acquisition completes first and the trip's `clear_control`
        // (which takes this same mutex) sweeps the just-installed lease.
        // No interleaving leaves a post-Stop lease standing — before this, a
        // stale already-approved request racing the kill switch could
        // install a fresh 30s lease AFTER a successful Stop, resurrecting
        // the control banner and the global Escape interception with nothing
        // actually running (its own action was rejected downstream, but
        // nothing released the lease it left behind).
        if stop_latched() {
            return Err(ComputerError::Disabled);
        }
        match guard.as_ref() {
            Some(holder) => {
                // issue #160 round-25 P1 (Codex mod.rs:1552): `wt` is part of
                // the holder identity, so a SIBLING worker session (same
                // `(thread, dir)`, different worktree) is a DIFFERENT holder and
                // gets `Busy` below rather than silently renewing this lease.
                let is_same_holder =
                    holder.thread == thread && holder.dir == dir && holder.wt == wt;
                let is_live = holder_is_live(holder.expires_at, now);
                if is_live && !is_same_holder {
                    return Err(ComputerError::Busy {
                        thread: holder.thread,
                        dir: holder.dir.clone(),
                    });
                }
                if is_live && is_same_holder {
                    // issue #160 round-15 P1: this exact holder's own
                    // registration round-trip has not confirmed the kill
                    // switch yet — refuse the renewal outright rather than
                    // let a racing-in second call from the SAME session
                    // inherit an unconfirmed lease. Nothing is mutated on
                    // this path: the original still-registering acquire is
                    // left untouched.
                    if !holder.escape_ready {
                        return Err(ComputerError::EscapeRegistrationPending);
                    }
                    // An ordinary renewal: the shortcut from the earlier
                    // acquire is already registered AND confirmed, so this
                    // skips syncing it again.
                    sync_needed = false;
                } else {
                    // Nobody held it live (expired — `is_live && !is_same_
                    // holder` above already returned for the only OTHER way
                    // to reach here): a FRESH hold as far as the shortcut is
                    // concerned, even when `(thread, dir)` happens to match
                    // the previous holder.
                    sync_needed = true;
                }
            }
            None => sync_needed = true,
        }
        // Apply the decision made above. A renewal (`sync_needed == false`)
        // only advances the sliding window IN PLACE, preserving `escape_
        // ready: true` — rewriting the whole struct here (as this used to,
        // pre-round-15) would silently reset it to `false` on every single
        // renewal, permanently reopening the exact race this round closes.
        // Every other reachable path is a fresh hold and always starts
        // `escape_ready: false`.
        if sync_needed {
            *guard = Some(ControlHolderState {
                thread,
                dir: dir.to_string(),
                wt,
                expires_at: lease_deadline(now),
                expires_at_ms: now_ms().saturating_add(CONTROL_LEASE_MS),
                escape_ready: false,
            });
        } else if let Some(holder) = guard.as_mut() {
            holder.expires_at = lease_deadline(now);
            holder.expires_at_ms = now_ms().saturating_add(CONTROL_LEASE_MS);
        }
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
        // issue #160 round-10 P1 #7 (Codex 1220): fail CLOSED if THIS fresh
        // hold needed a real Escape registration and it didn't actually
        // succeed — see `escape_guard_permits_control`'s own doc for the (no
        // subsystem vs. subsystem-but-rejected) distinction it encodes. A
        // renewal (`sync_needed == false`) skips this: it never attempted a
        // NEW registration to begin with, and round-4 P2 §4's own design
        // already assumes an already-registered shortcut stays registered
        // for the life of a live lease.
        let permitted = escape_guard_permits_control(
            APP_HANDLE.get().is_some(),
            ESCAPE_REGISTER_OK.load(Ordering::SeqCst),
        );
        if !permitted {
            // Roll back: never grant control without a working kill switch
            // once a real subsystem exists and it just failed to register.
            // Only clears OUR OWN just-stored hold — if some other caller
            // already raced in and overwrote it, this leaves that one alone.
            let mut guard = control_mutex().lock().unwrap_or_else(|e| e.into_inner());
            if matches!(guard.as_ref(), Some(h) if h.thread == thread && h.dir == dir && h.wt == wt) {
                *guard = None;
            }
            return Err(ComputerError::EscapeUnavailable);
        }
        // issue #160 round-15 P1: registration for THIS fresh hold is now
        // CONFIRMED — flip `escape_ready` so a same-holder renewal (any that
        // raced in earlier were refused with `EscapeRegistrationPending`
        // above and must simply retry) can proceed normally from here on.
        // Re-lock and re-check identity rather than trusting the guard
        // released above: this exact `(thread, dir)` slot is not guaranteed
        // to still be ours — the registration round-trip (a real OS call)
        // can run long enough for `CONTROL_LEASE_MS` to lapse and let a
        // DIFFERENT session legitimately win the now-vacant lease in the
        // meantime (this is exactly why `escape_ready` exists rather than
        // `holder_is_live` alone: an unconfirmed hold still counts as live
        // for `Busy` purposes, but that says nothing about who owns the slot
        // once this call resumes).
        let mut guard = control_mutex().lock().unwrap_or_else(|e| e.into_inner());
        match guard.as_mut() {
            Some(h) if h.thread == thread && h.dir == dir && h.wt == wt => {
                h.escape_ready = true;
            }
            _ => {
                // Someone else's fresh hold won this slot while our own
                // registration was in flight — our lease is gone.
                // `EscapeRegistrationPending` here (rather than
                // `EscapeUnavailable`) is deliberate: nothing failed
                // PERMANENTLY — the OS-level registration this call just ran
                // actually succeeded — so the right instruction to the
                // caller is the same "retry me" contract as the pending-
                // renewal refusal above, not "the kill switch itself is
                // unavailable".
                return Err(ComputerError::EscapeRegistrationPending);
            }
        }
    }
    Ok(())
}

/// Release the lease early — a no-op unless `(thread, dir, wt)` is the CURRENT
/// holder (an already-expired/released lease, or one some other session
/// since took over, is left alone rather than clobbered). Used by
/// `commands::clear_control_if_session_doomed` (round-32) to drop exactly one
/// doomed session's lease during a repo delete; also available to any caller
/// that knows it's done and wants the next session unblocked sooner than the
/// full 30s lease.
///
/// issue #160 round-33 P2 (Codex mod.rs:1746): a successful release also
/// syncs the OS-level global Escape shortcut — OUTSIDE the lock, the same
/// decide-unlock-act shape as [`clear_control`] and [`control_state`]'s
/// lazy-expiry path (see [`sync_shortcut_state`]'s own doc for the race that
/// shape closes). Without it, releasing the last holder left bare Escape
/// globally intercepted indefinitely: `control_state` thereafter sees `None`
/// (not an expired holder), so its own repair path never fires, the banner
/// disappears, and pressing Escape could still trip Emergency Stop with no
/// active controller.
pub fn release_control(thread: i32, dir: &str, wt: Option<i32>) {
    let released;
    {
        let mut guard = control_mutex().lock().unwrap_or_else(|e| e.into_inner());
        released =
            matches!(guard.as_ref(), Some(h) if h.thread == thread && h.dir == dir && h.wt == wt);
        if released {
            *guard = None;
        }
    }
    if released {
        sync_shortcut_state();
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
    let now = std::time::Instant::now();
    match guard.as_ref() {
        Some(h) if holder_is_live(h.expires_at, now) => (
            Some(ControlHolder {
                thread: h.thread,
                dir: h.dir.clone(),
                wt: h.wt,
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
///
/// issue #160 round-12 P1 #B/#F: split into [`trip_stop_latch`] (synchronous,
/// no lock held across an `.await`, no DB touched at all — flips `stopped`,
/// bumps the generation, clears the control lease) and [`persist_stop`] (the
/// awaited DB write, now serialized on the SAME [`enable_serialize_mutex`] a
/// `commands::set_computer_use_enabled_inner` enable request uses). Two
/// production callers need that split, not just this combined wrapper:
///  - `commands::computer_emergency_stop` and this module's own
///    `register_global_escape` callback both need `AskRegistry::
///    cancel_gui_asks` to run strictly BEFORE the awaited DB write below —
///    see that method's own doc for the stale-card-then-Always race left
///    open when cancellation only ran AFTER this whole function returned
///    (round-12 P1 #B). Both now call `trip_stop_latch` (latch flips
///    immediately, no await), then `cancel_gui_asks` (also immediate), then
///    `persist_stop` (the awaited write) — never this combined function.
///  - `persist_stop` joining `enable_serialize_mutex` (round-12 P2 #F) is
///    what makes the LAST call's own write win the DB row regardless of
///    which call's write happens to finish I/O first — see
///    `enable_serialize_mutex`'s own doc for the compensating-write race
///    this closes on the Stop side (before this round, Stop's write ran
///    OUTSIDE that lock entirely, so a slower Stop write could still land
///    AFTER a newer, explicit enable's write and silently revert it).
///
/// This function itself stays the single, unsplit entry point for every
/// OTHER caller (tests driving the kill switch end to end; any future
/// caller that has no `AskRegistry` to cancel against) — it is simply
/// `trip_stop_latch` immediately followed by an awaited `persist_stop`, with
/// no behavior change from before this round for a caller that never
/// observed the gap between the two.
pub async fn emergency_stop(db: &crate::store::Db) -> Result<(), String> {
    let my_gen = trip_stop_latch();
    persist_stop(db, my_gen).await
}

/// The synchronous half of [`emergency_stop`] (issue #160 round-12 P1 #B/#F)
/// — flips the in-memory `stopped` latch, bumps the stop-generation, and
/// clears the control lease, all WITHOUT touching the DB or crossing an
/// `.await` point. Returns the NEW generation, to be threaded into
/// [`persist_stop`] afterward (mirrors [`StopState::generation`]'s own
/// read-before-write contract — this IS that read). Callers that need the
/// cut-in (and any GUI ask cancellation) to happen strictly before the
/// slower, awaited DB write call this directly instead of [`emergency_stop`]
/// — see that function's own doc for the two production call sites and why.
pub fn trip_stop_latch() -> u64 {
    let my_gen = {
        let mut guard = stop_state().lock().unwrap_or_else(|e| e.into_inner());
        guard.stopped = true;
        guard.generation = guard.generation.wrapping_add(1);
        guard.generation
    };
    clear_control();
    my_gen
}

/// The awaited half of [`emergency_stop`] (issue #160 round-12 P1 #B/#F):
/// best-effort persists `computer_use_enabled = false` for `my_gen` (the
/// generation [`trip_stop_latch`] just minted) and records
/// [`STOP_PERSIST_FAILED`] under the SAME generation guard `emergency_stop`
/// always used. Round-12 P2 #F: now serialized on [`enable_serialize_mutex`]
/// — the SAME lock `commands::set_computer_use_enabled_inner` holds across
/// its own read-generation/write/reconcile sequence — so Stop's own
/// persisted write and an overlapping explicit enable's persisted write can
/// never land out of CALL order. The in-memory latch is already tripped (by
/// [`trip_stop_latch`], before this ever acquires the lock), so `enabled()`
/// fails closed for the ENTIRE time this sits queued behind an in-flight
/// enable — queuing the PERSISTED write behind that lock never reopens the
/// fail-open window round-6/round-8's own fixes closed, it only decides
/// which value the DISK ends up with once both writes land.
pub async fn persist_stop(db: &crate::store::Db, my_gen: u64) -> Result<(), String> {
    let _serialize = enable_serialize_mutex().lock().await;
    // issue #160 round-22 P2 (Codex computer/mod.rs:1849): only persist "false"
    // if THIS Stop is STILL the current truth. `enable_serialize_mutex` orders
    // lock ATTEMPTS, not the user-visible calls that preceded them — so a later
    // Settings re-enable that reached the lock FIRST has already written "true"
    // and cleared the latch, and writing "false" now would silently revert that
    // newer, explicit enable on disk while the UI reports it succeeded. Re-read
    // the latch under its own lock: if it is no longer tripped (a re-enable
    // cleared it) or a NEWER Stop has since bumped the generation (that call
    // owns writing "false" for its OWN outcome), skip this stale write.
    {
        let guard = stop_state().lock().unwrap_or_else(|e| e.into_inner());
        if !guard.stopped || guard.generation != my_gen {
            return Ok(());
        }
    }
    let result = crate::store::repo::set_setting(db, K_COMPUTER_USE_ENABLED, "false")
        .await
        .map_err(|e| e.to_string());
    // round-8 P2 #3: only record THIS call's outcome if no NEWER
    // `emergency_stop`/`trip_stop_latch` has since bumped the generation
    // again — otherwise that newer call owns writing the flag for its own
    // outcome, and a slow success here must never clear a newer failure it
    // knows nothing about.
    {
        let guard = stop_state().lock().unwrap_or_else(|e| e.into_inner());
        if guard.generation == my_gen {
            STOP_PERSIST_FAILED.store(result.is_err(), Ordering::SeqCst);
        }
    }
    result
}

/// issue #160 round-10 P2 #D / round-12 P2 #F: serializes every
/// `commands::set_computer_use_enabled_inner` call AND every [`persist_stop`]
/// call — see `set_computer_use_enabled_inner`'s own doc for the enable-vs-
/// enable compensating-write race this originally existed for, and
/// [`persist_stop`]'s own doc for why Stop's persisted write joined this same
/// queue (round-12 P2 #F): without it, Stop's write ran OUTSIDE this lock
/// entirely, so an enable request already past its own read-generation step
/// could still have its `"true"` write land AFTER a slower Stop's `"false"`
/// write, silently reverting an explicit, more recent Stop. Held across
/// EVERY caller's own full read/write/reconcile (or write/record) sequence,
/// so whichever call is issued LAST is also guaranteed to WRITE last —
/// `tokio::sync::Mutex`, not `std::sync::Mutex`, because both callers hold it
/// across their own `.await`s.
///
/// Deliberately does NOT gate [`trip_stop_latch`]/the in-memory latch flip
/// itself — only the two functions' own DB writes join this queue. A human's
/// Stop must always cut in on the LATCH immediately; queuing that behind an
/// in-flight enable would delay the one thing this whole feature's safety
/// property depends on. Only the PERSISTED write is serialized here.
pub fn enable_serialize_mutex() -> &'static tokio::sync::Mutex<()> {
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
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

/// Register the OS-level global Escape shortcut. `Ok(())` when
/// [`set_app_handle`] hasn't run yet (`APP_HANDLE` still `None` — the common
/// case in `cargo test --lib`, which never builds a real Tauri app, and any
/// headless/no-GUI build that never reaches `lib.rs`'s `setup()`): there is
/// no subsystem here at all to have failed, so [`sync_shortcut_state`]/
/// [`acquire_control`] treat that as "nothing to fail on" — see
/// [`escape_guard_permits_control`]'s own doc for the full (no subsystem vs.
/// subsystem-but-rejected) split this feeds. `Err` (issue #160 round-10 P1
/// #7, Codex 1220) ONLY when an app handle IS installed and the OS itself
/// refused the registration (already grabbed by another app, unsupported
/// platform/desktop environment, ...) — logged here (same as before this
/// round) AND now propagated so [`acquire_control`] can fail CLOSED instead
/// of silently granting control with no working kill switch.
fn register_global_escape() -> Result<(), String> {
    use tauri_plugin_global_shortcut::GlobalShortcutExt;
    let Some(app) = APP_HANDLE.get() else { return Ok(()) };
    // issue #160 round-22 P1 (Codex computer/mod.rs:2081): clear any prior
    // bare-Escape registration FIRST. A control lease that EXPIRED between the
    // banner's cleanup polls leaves the OLD shortcut still registered at the OS;
    // registering the same accelerator again then fails ("already registered"),
    // so a fresh `acquire_control` rolls back — clearing its new holder WITHOUT
    // ever unregistering the stale shortcut, which would then stay globally
    // intercepting Escape with no live lease until restart. A best-effort
    // unregister here is idempotent (a no-op when nothing is registered) and
    // makes the re-registration below deterministic. `unregister` is safe to
    // call here — it never re-enters this module synchronously (the Escape
    // callback runs on a later spawned task), same as `sync_shortcut_state`'s
    // own reasoning.
    let _ = app.global_shortcut().unregister(escape_shortcut());
    let result = app.global_shortcut().on_shortcut(escape_shortcut(), |app, _shortcut, event| {
        if event.state != tauri_plugin_global_shortcut::ShortcutState::Pressed {
            return;
        }
        let app = app.clone();
        tauri::async_runtime::spawn(async move {
            use tauri::Manager as _;
            let db = app.state::<crate::store::Db>().inner().clone();
            // issue #160 round-12 P1 #B: `trip_stop_latch` (immediate, no
            // await) THEN `cancel_gui_asks` (also immediate) THEN
            // `persist_stop` (the awaited DB write) — never the combined
            // `emergency_stop` wrapper. Before this split, cancellation ran
            // AFTER the awaited DB write returned; an Always/Full answer
            // landing on a still-open GUI card DURING that write recorded a
            // standing grant `answer()` never re-checked the kill switch for
            // — see `AskRegistry::cancel_gui_asks`'s own doc for the full
            // race this closes. The latch is already tripped by the time
            // `cancel_gui_asks` runs, so `computer::enabled` fails closed for
            // the ENTIRE persist below regardless of how long it takes.
            let my_gen = trip_stop_latch();
            // Reached via the app handle (this callback has no `State`
            // extraction the way a `#[tauri::command]` does) — this is the
            // OS-level global Escape's own entry point into the same
            // cancellation `commands::computer_emergency_stop` performs.
            let asks = app.state::<crate::ask::AskRegistry>().inner().clone();
            asks.cancel_gui_asks();
            // Error ignored (but not un-observable — see `STOP_PERSIST_FAILED`):
            // the in-memory latch (and the GUI-ask cancellation above) already
            // took effect before this fallible DB write even starts — see
            // `persist_stop`'s own doc comment.
            let _ = persist_stop(&db, my_gen).await;
        });
    });
    if let Err(err) = &result {
        eprintln!("[weft] register global Escape shortcut: {err}");
    }
    result.map_err(|e| e.to_string())
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

/// Whether the LAST time [`sync_shortcut_state`] attempted a registration
/// (i.e. found a live holder), that attempt actually succeeded — issue #160
/// round-10 P1 #7. `true` is also the resting/default value for the
/// no-live-holder case (`sync_shortcut_state`'s `unregister` branch resets it
/// there): with no lease held, there is nothing for a NEXT `acquire_control`
/// to have inherited a stale failure from — that next acquire runs its OWN
/// fresh registration attempt and sets this itself. Read by
/// [`escape_guard_permits_control`] (via [`acquire_control`]), which also
/// needs to know whether a real subsystem exists at all — see that
/// function's own doc for why the two questions are kept separate rather
/// than folded into one bit.
static ESCAPE_REGISTER_OK: AtomicBool = AtomicBool::new(true);

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
    let now = std::time::Instant::now();
    let holder_live = {
        let guard = control_mutex().lock().unwrap_or_else(|e| e.into_inner());
        matches!(guard.as_ref(), Some(h) if holder_is_live(h.expires_at, now))
    };
    if holder_live {
        #[cfg(test)]
        SHORTCUT_REGISTER_ATTEMPTS.fetch_add(1, Ordering::SeqCst);
        let result = register_global_escape();
        // issue #160 round-10 P1 #7: recorded regardless of whether THIS
        // particular sync came from `acquire_control` — `control_state`'s
        // lazy-expiry path and `clear_control` only ever call this when
        // `holder_live` is false (their own holder just went away), so the
        // `holder_live` branch here is, in practice, always reached via a
        // fresh `acquire_control` hold.
        ESCAPE_REGISTER_OK.store(result.is_ok(), Ordering::SeqCst);
    } else {
        #[cfg(test)]
        SHORTCUT_UNREGISTER_ATTEMPTS.fetch_add(1, Ordering::SeqCst);
        unregister_global_escape();
        // No live holder means nothing needs the Escape guarantee right now
        // — reset to the permissive default so a stale failure can't outlive
        // the lease it belonged to and wrongly block some LATER, unrelated
        // acquire that hasn't attempted its own registration yet.
        ESCAPE_REGISTER_OK.store(true, Ordering::SeqCst);
    }
}

/// Whether an acquire that just (re)synced the OS-level Escape shortcut may
/// proceed granting control, given (a) whether a real `tauri::AppHandle`
/// subsystem is even installed and (b) — only when it is — whether the
/// actual registration attempt succeeded. issue #160 round-10 P1 #7 (Codex
/// 1220). Pure/synchronous so this exact fail-closed judgment call is
/// unit-testable without a real `tauri::AppHandle`/global-shortcut backend
/// (`cargo test --lib` never has one — see [`register_global_escape`]'s own
/// doc) — `acquire_control` composes it with `APP_HANDLE.get().is_some()` and
/// [`ESCAPE_REGISTER_OK`] at its one real call site, which isn't itself
/// practical to drive from a plain `#[test]` with an actually-failing OS
/// registration.
///
///  - no subsystem installed (`has_app_handle == false`) — the common case in
///    every unit test and any headless/no-GUI build that never reaches
///    `lib.rs`'s `setup()` — always permits: `register_global_escape` itself
///    no-ops in this case, so there was never a real kill-switch guarantee to
///    begin with, and fail-closing every unit test in this crate would be a
///    far worse regression than the gap this fixes.
///  - a subsystem IS installed and its own registration attempt came back
///    `Err` (grabbed by another app, unsupported desktop, ...) is the ONLY
///    case that refuses: a real desktop session where the redundant OS-level
///    Escape genuinely could not be wired up must not silently grant control
///    anyway — the human would have no way to Escape out while a CONTROLLED
///    app (not weft) holds real OS focus.
fn escape_guard_permits_control(has_app_handle: bool, register_ok: bool) -> bool {
    !has_app_handle || register_ok
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

/// The instant the last input action was actually DISPATCHED to the OS
/// backend — a separate clock from [`throttle_mutex`], which records
/// ADMISSION time (issue #160 round-26 P2, Codex computer_srv.rs:1006).
fn dispatch_pace_mutex() -> &'static Mutex<Option<Instant>> {
    static PACE: OnceLock<Mutex<Option<Instant>>> = OnceLock::new();
    PACE.get_or_init(|| Mutex::new(None))
}

/// Enforce the [`THROTTLE_MS`] gap at DISPATCH time, on the blocking thread
/// that is about to call the OS backend (issue #160 round-26 P2, Codex
/// computer_srv.rs:1006). The admission throttle ([`throttle_input`]) alone
/// cannot hold the advertised ~2-actions/second promise: while one slow
/// action holds the flight guard, later calls can be ADMITTED ≥500ms apart,
/// queue on the mutex, and then — once the slow holder releases — execute
/// back-to-back with no gap at all. This paces the actual injections: it
/// SLEEPS out whatever remains of the gap since the last dispatch (bounded
/// by `THROTTLE_MS`, on the blocking pool where sleeping is fine — the
/// flight guard already serializes input end-to-end, so pacing here is the
/// pacing of the whole pipeline), then records this dispatch. Callers run
/// their final stop/lease recheck AFTER this returns, so a kill switch
/// tripped during the sleep is still honored before the backend is touched.
pub fn pace_backend_dispatch() {
    let sleep_needed = {
        let guard = dispatch_pace_mutex().lock().unwrap_or_else(|e| e.into_inner());
        let min_gap = Duration::from_millis(THROTTLE_MS);
        match *guard {
            Some(last) => min_gap.checked_sub(last.elapsed()),
            None => None,
        }
        // Lock released before sleeping — a Stop/Escape path must never queue
        // behind a pacing sleep.
    };
    if let Some(wait) = sleep_needed {
        std::thread::sleep(wait);
    }
    let mut guard = dispatch_pace_mutex().lock().unwrap_or_else(|e| e.into_inner());
    *guard = Some(Instant::now());
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
pub async fn input_flight_guard() -> InputFlightGuard {
    static FLIGHT: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    let inner = FLIGHT.get_or_init(|| tokio::sync::Mutex::new(())).lock().await;
    // issue #160 round-12 P1 #4: flip the in-flight latch the INSTANT this
    // guard is actually acquired (not merely requested) — see
    // `holder_is_live`'s own doc for what this keeps alive while true.
    INPUT_IN_FLIGHT.store(true, Ordering::SeqCst);
    InputFlightGuard { _inner: inner }
}

/// The guard [`input_flight_guard`] returns — wraps the underlying
/// `tokio::sync::MutexGuard` purely so `Drop` can clear [`INPUT_IN_FLIGHT`]
/// the instant the caller's injection finishes (issue #160 round-12 P1 #4),
/// symmetric with the `store(true, ..)` in `input_flight_guard` itself. Every
/// existing caller (`bus::computer_srv`'s input arms, this module's own
/// tests) only ever binds this to a `let _guard = ...` / `let held = ...`
/// and lets it drop — nothing depended on the old bare `MutexGuard` type
/// itself, so this is a transparent replacement.
pub struct InputFlightGuard {
    _inner: tokio::sync::MutexGuard<'static, ()>,
}

impl Drop for InputFlightGuard {
    fn drop(&mut self) {
        INPUT_IN_FLIGHT.store(false, Ordering::SeqCst);
    }
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

    /// A monotonic deadline strictly in the past — the round-29 replacement
    /// for the old `now_ms().saturating_sub(1)` expiry-forcing writes. The
    /// `checked_sub` fallback (`now` itself) still reads as expired:
    /// [`holder_is_live`] requires `expires_at > now`, and the clock can only
    /// have advanced by the time it re-reads `now`.
    fn expired_deadline() -> std::time::Instant {
        let now = std::time::Instant::now();
        now.checked_sub(std::time::Duration::from_millis(1)).unwrap_or(now)
    }

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

    // —— cleanup_on_err (issue #160 round-14 P2) ——

    #[test]
    fn cleanup_on_err_removes_a_file_left_behind_by_a_failed_write() {
        let tmp = tempfile::tempdir().unwrap();
        // Stand in for the file `create_new` already made on disk before the
        // injected write below fails partway through — exactly the shape of
        // the unix `screenshot_window` write path this guards.
        let path = tmp.path().join("123-9-0.png");
        std::fs::write(&path, b"partial, corrupt PNG bytes").unwrap();
        assert!(path.exists(), "test setup: the file must exist before cleanup runs");

        let result: Result<(), ComputerError> =
            cleanup_on_err(&path, || Err(ComputerError::Io("disk full".to_string())));

        assert!(result.is_err(), "the original error must still propagate");
        assert!(
            matches!(result, Err(ComputerError::Io(msg)) if msg == "disk full"),
            "the ORIGINAL error must pass through untouched, not be replaced by a cleanup outcome"
        );
        assert!(!path.exists(), "a file left behind by a failed write must be cleaned up");
    }

    #[test]
    fn cleanup_on_err_is_a_noop_when_the_write_never_created_anything() {
        // A write that fails before ever touching the filesystem (e.g. the
        // unix `create_new` open itself failing) leaves nothing at `path` —
        // cleanup's own `remove_file` failing in that case must never mask
        // the original error.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("does-not-exist-9-0.png");
        assert!(!path.exists());

        let result: Result<(), ComputerError> =
            cleanup_on_err(&path, || Err(ComputerError::Io("open failed".to_string())));

        assert!(
            matches!(result, Err(ComputerError::Io(msg)) if msg == "open failed"),
            "the original error must still surface even though there was nothing to clean up"
        );
        assert!(!path.exists());
    }

    #[test]
    fn cleanup_on_err_leaves_a_successful_write_untouched() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("123-9-0.png");
        std::fs::write(&path, b"a complete, valid PNG").unwrap();

        let result: Result<(), ComputerError> = cleanup_on_err(&path, || Ok(()));

        assert!(result.is_ok());
        assert!(path.exists(), "cleanup must never touch a file after a successful write");
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

        prune_old_screenshots(dir, keep, None);

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
        prune_old_screenshots(dir, 0, None);

        assert!(!own.exists(), "the lone own-named real file should be pruned at keep=0");
        assert!(unrelated.exists(), "a non-own file must never be pruned");
        assert!(odd_name.exists(), "a non-matching .png name must never be pruned");
        assert!(
            std::fs::symlink_metadata(&symlink_path).is_ok(),
            "a symlink must never be pruned, even with an own-looking name and keep=0"
        );
        assert!(link_target.exists(), "the symlink's target must never be deleted either");
    }

    #[cfg(unix)]
    #[test]
    fn prune_old_screenshots_breaks_mtime_ties_by_seq_descending() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let keep = 2usize;

        // Four own-named files, all given the EXACT same mtime — standing in
        // for a coarse-mtime filesystem, or several screenshots landing in
        // the same tick, where mtime alone can't order them (issue #160
        // round-13 P2). `seq` (the filename's third component) is the only
        // thing that still tells them apart, and higher `seq` is newer.
        let mut paths = Vec::new();
        for seq in 0..4u64 {
            let p = dir.join(format!("1700000000000-9-{seq}.png"));
            std::fs::write(&p, b"fake").unwrap();
            set_mtime(&p, 1_000_000); // identical for all four
            paths.push(p);
        }

        prune_old_screenshots(dir, keep, None);

        let remaining: std::collections::HashSet<PathBuf> = std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .collect();
        assert_eq!(remaining.len(), keep, "expected exactly {keep} files left: {remaining:?}");
        assert!(remaining.contains(&paths[3]), "highest-seq file in an mtime tie must survive");
        assert!(remaining.contains(&paths[2]), "second-highest-seq file in an mtime tie must survive");
        assert!(!remaining.contains(&paths[0]), "lowest-seq file in an mtime tie must be pruned");
        assert!(!remaining.contains(&paths[1]), "second-lowest-seq file in an mtime tie must be pruned");
    }

    #[cfg(unix)]
    #[test]
    fn prune_old_screenshots_never_deletes_the_just_written_file_even_if_it_sorts_oldest() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let keep = 2usize;

        // `a` is the file THIS call's caller "just wrote" — but it's given
        // the OLDEST mtime, exactly the case that would otherwise get it
        // sorted into the pruned tail by mtime order alone.
        let a = dir.join("1700000000000-9-0.png");
        std::fs::write(&a, b"fake").unwrap();
        set_mtime(&a, 1_000_000);

        let b = dir.join("1700000000001-9-1.png");
        std::fs::write(&b, b"fake").unwrap();
        set_mtime(&b, 1_000_001);

        let c = dir.join("1700000000002-9-2.png");
        std::fs::write(&c, b"fake").unwrap();
        set_mtime(&c, 1_000_002);

        let d = dir.join("1700000000003-9-3.png");
        std::fs::write(&d, b"fake").unwrap();
        set_mtime(&d, 1_000_003);

        prune_old_screenshots(dir, keep, Some(&a));

        assert!(a.exists(), "the just-written file must survive even though it sorts oldest by mtime");
        assert!(d.exists(), "the newest file must survive under the normal keep cap");
        assert!(c.exists(), "the second-newest file must survive under the normal keep cap");
        assert!(!b.exists(), "a non-exempt file beyond the keep cap must still be pruned");
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

    #[test]
    fn own_screenshot_seq_parses_the_trailing_component_and_never_panics() {
        assert_eq!(own_screenshot_seq(Path::new("1700000000000-9-42.png")), 42);
        assert_eq!(own_screenshot_seq(Path::new("1700000000000-9-0.png")), 0);
        // Malformed shapes fall back to 0 rather than panicking — this
        // function is only ever called on names `is_own_screenshot_filename`
        // already accepted, but must stay panic-free regardless.
        assert_eq!(own_screenshot_seq(Path::new("abc.png")), 0);
        assert_eq!(own_screenshot_seq(Path::new("1-2.png")), 0);
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

    // —— map_screenshot_coord / recent_shot_dims (issue #160 round-11 P1 #D) ——

    /// The end-to-end property this fix exists for: a screenshot taken at
    /// 1280x800, the window later resized DOWN to 1000x600 (only the SIZE
    /// changed — `map_to_physical`'s old scale-from-current-size math would
    /// mismap this; see this function's own doc) — a screenshot-space
    /// coordinate maps to the SAME relative position in the window's CURRENT
    /// rectangle, not a position derived from the window's now-stale
    /// original size.
    #[test]
    fn map_screenshot_coord_scales_a_screenshot_coordinate_to_the_current_window_rect() {
        let current = WindowInfo {
            id: 1,
            app: "x".into(),
            title: "x".into(),
            x: 0,
            y: 0,
            width: 1000,
            height: 600,
        };
        // (640, 400) is the exact midpoint of a 1280x800 screenshot (0.5,
        // 0.5) — must land at the midpoint of the CURRENT 1000x600 rect,
        // (500, 300), regardless of the screenshot's own original size.
        let (px, py) = map_screenshot_coord(&current, 1280, 800, 640, 400).unwrap();
        assert_eq!((px, py), (500, 300));
    }

    /// Same as above, but the window also MOVED (non-zero origin) — the
    /// origin offset must still be added on top of the proportional mapping.
    #[test]
    fn map_screenshot_coord_adds_the_current_windows_origin() {
        let current = WindowInfo {
            id: 1,
            app: "x".into(),
            title: "x".into(),
            x: 200,
            y: 100,
            width: 1000,
            height: 600,
        };
        let (px, py) = map_screenshot_coord(&current, 1280, 800, 640, 400).unwrap();
        assert_eq!((px, py), (200 + 500, 100 + 300));
    }

    /// Out-of-bounds is judged against the SCREENSHOT's own dimensions, never
    /// the current window's — a coordinate at/beyond the screenshot's own
    /// edge is rejected even though the current window might be far larger
    /// (and vice versa: this test's own current window is SMALLER than the
    /// screenshot, so a naive "bounds = current window size" check would
    /// reject valid screenshot-space coordinates that this correctly accepts).
    #[test]
    fn map_screenshot_coord_bounds_are_judged_by_screenshot_size_not_current_window_size() {
        let current = WindowInfo {
            id: 1,
            app: "x".into(),
            title: "x".into(),
            x: 0,
            y: 0,
            width: 200,
            height: 100,
        };
        // In-bounds for a 1280x800 screenshot even though it's far bigger
        // than the current (200x100) window.
        assert!(map_screenshot_coord(&current, 1280, 800, 1279, 799).is_ok());
        // Out of bounds for that SAME screenshot — rejected using the
        // screenshot's own size, not the (smaller) current window's.
        let err = map_screenshot_coord(&current, 1280, 800, 1280, 0).unwrap_err();
        assert!(matches!(
            err,
            ComputerError::OutOfBounds { x: 1280, y: 0, max_x: 1279, max_y: 799 }
        ));
    }

    #[test]
    fn map_screenshot_coord_clamps_the_far_edge_inside_a_shrunk_window() {
        // issue #160 round-23 P2 (Codex computer/mod.rs:677): a window that
        // SHRANK to 100x100 since a 1280x800 screenshot. The far in-bounds
        // screenshot corner (1279, 799) proportionally rounds UP to the window's
        // EXCLUSIVE edge (offset 100 on a 100-wide window) — clamping holds it at
        // width-1/height-1 so the click lands INSIDE the approved window, never
        // one pixel past it onto an overlapping foreground app.
        let current = WindowInfo {
            id: 1,
            app: "x".into(),
            title: "x".into(),
            x: 500,
            y: 300,
            width: 100,
            height: 100,
        };
        let (px, py) = map_screenshot_coord(&current, 1280, 800, 1279, 799).unwrap();
        assert_eq!(px, 500 + 99, "x clamped to width-1, inside the window");
        assert_eq!(py, 300 + 99, "y clamped to height-1, inside the window");
        // Interior mappings are untouched by the clamp: the screenshot midpoint
        // still maps to the window midpoint.
        let (mx, my) = map_screenshot_coord(&current, 1280, 800, 640, 400).unwrap();
        assert_eq!(mx, 500 + 50);
        assert_eq!(my, 300 + 50);
    }

    /// A `WindowInfo` for `shot_dims_for`/`record_shot_dims` identity tests.
    /// `id`/`app`/`title` decide `shot_dims_for`'s own hit/miss gate;
    /// geometry (`x`/`y`/`width`/`height`) is recorded alongside them (issue
    /// #160 round-12 P1 #C) and readable via `shot_geometry_for`, but is
    /// NOT part of `shot_dims_for`'s own gate — see that function's own doc
    /// for why. Defaults to `(0, 0, 0, 0)`; tests that care about geometry
    /// pass their own explicit values instead of this helper.
    fn shot_win(id: u32, app: &str, title: &str) -> WindowInfo {
        WindowInfo { id, app: app.into(), title: title.into(), x: 0, y: 0, width: 0, height: 0 }
    }

    #[test]
    fn shot_dims_round_trips_what_was_recorded_and_is_isolated_per_window_id() {
        record_shot_dims(920_001, "lead", None, 7, 1280, 800, &shot_win(7, "Notes", "Untitled"));
        record_shot_dims(920_001, "lead", None, 8, 640, 480, &shot_win(8, "Other", "Untitled"));
        assert_eq!(shot_dims_for(920_001, "lead", None, &shot_win(7, "Notes", "Untitled")), Some((1280, 800)));
        assert_eq!(shot_dims_for(920_001, "lead", None, &shot_win(8, "Other", "Untitled")), Some((640, 480)));
        // A DIFFERENT (thread, dir, window_id) triple that was never recorded
        // must fail closed with `None` — never fall back to some other
        // window's dims.
        assert_eq!(
            shot_dims_for(920_001, "lead", None, &shot_win(9, "Notes", "Untitled")),
            None,
            "no record for window 9 must be None"
        );
        assert_eq!(
            shot_dims_for(920_002, "lead", None, &shot_win(7, "Notes", "Untitled")),
            None,
            "a different thread must not see thread 920_001's record"
        );
    }

    /// issue #160 round-26 P2 (Codex mod.rs:797): SIBLING worker sessions of
    /// one multi-repo direction — same `(thread, dir)`, different `wt` — keep
    /// SEPARATE dimension records for the same window. Worker A screenshotting
    /// after a resize must never overwrite the dims worker B's approved input
    /// is about to map coordinates against.
    #[test]
    fn shot_dims_are_isolated_per_worktree_for_sibling_workers() {
        let w = || shot_win(21, "Notes", "shared window");
        record_shot_dims(920_010, "44", Some(1), 21, 1280, 800, &w());
        record_shot_dims(920_010, "44", Some(2), 21, 640, 480, &w());
        assert_eq!(
            shot_dims_for(920_010, "44", Some(1), &w()),
            Some((1280, 800)),
            "worker A's dims survive worker B's later screenshot of the same window"
        );
        assert_eq!(shot_dims_for(920_010, "44", Some(2), &w()), Some((640, 480)));
        assert_eq!(
            shot_dims_for(920_010, "44", None, &w()),
            None,
            "the absent-wt (lead-shaped) key is a DISTINCT record, never a fallback"
        );
    }

    #[test]
    fn shot_dims_refreshing_the_same_window_overwrites_rather_than_duplicates() {
        record_shot_dims(920_003, "lead", None, 1, 1280, 800, &shot_win(1, "Steady", "steady window"));
        record_shot_dims(920_003, "lead", None, 1, 640, 480, &shot_win(1, "Steady", "steady window"));
        assert_eq!(
            shot_dims_for(920_003, "lead", None, &shot_win(1, "Steady", "steady window")),
            Some((640, 480)),
            "a second screenshot of the SAME window must replace the earlier dims, not stack"
        );
    }

    /// issue #160 round-12 P1 #2: the exact property this round closes — an
    /// id REUSED by a different window (same numeric id, different app/title)
    /// must read as no record at all, fail-closed, never a stale hit against
    /// the OLD window's saved geometry.
    #[test]
    fn shot_dims_for_fails_closed_when_the_window_id_was_reused_by_a_different_window() {
        record_shot_dims(920_004, "lead", None, 5, 1280, 800, &shot_win(5, "Original App", "Original Title"));
        // Same (thread, dir, id) — a DIFFERENT app+title, standing in for the
        // OS reusing a closed window's id for an unrelated new window.
        let replaced = shot_win(5, "Different App", "Different Title");
        assert_eq!(
            shot_dims_for(920_004, "lead", None, &replaced),
            None,
            "an id reused by a different window must never return the old window's dims"
        );
        // The ORIGINAL identity still hits, unaffected by the check above.
        assert_eq!(
            shot_dims_for(920_004, "lead", None, &shot_win(5, "Original App", "Original Title")),
            Some((1280, 800))
        );
    }

    // —— issue #160 round-12 P1 #C: capture-time geometry is recorded and
    // readable, deliberately NOT folded into `shot_dims_for`'s own gate ——

    /// The data-layer half of round-12 P1 #C: `record_shot_dims` now ALSO
    /// captures `w`'s own `(x, y, width, height)` at capture time, and
    /// `shot_geometry_for` returns EXACTLY that — round-tripped, not derived
    /// or approximated.
    #[test]
    fn shot_geometry_for_round_trips_the_windows_capture_time_geometry() {
        let w = WindowInfo {
            id: 11,
            app: "Geo".into(),
            title: "geo window".into(),
            x: 50,
            y: 60,
            width: 800,
            height: 600,
        };
        record_shot_dims(921_001, "lead", None, 11, 1280, 800, &w);
        assert_eq!(shot_geometry_for(921_001, "lead", None, &w), Some((50, 60, 800, 600)));
    }

    /// `shot_geometry_for` shares `shot_dims_for`'s own app/title identity
    /// gate — an id reused by a different window reads as no geometry on
    /// file either, the same fail-closed answer `shot_dims_for` gives.
    #[test]
    fn shot_geometry_for_fails_closed_when_the_window_id_was_reused_by_a_different_window() {
        let original = WindowInfo {
            id: 12,
            app: "Original".into(),
            title: "original window".into(),
            x: 10,
            y: 20,
            width: 800,
            height: 600,
        };
        record_shot_dims(921_002, "lead", None, 12, 1280, 800, &original);
        let replaced = shot_win(12, "Different", "different window");
        assert_eq!(shot_geometry_for(921_002, "lead", None, &replaced), None);
        assert_eq!(
            shot_geometry_for(921_002, "lead", None, &original),
            Some((10, 20, 800, 600)),
            "the original identity's recorded geometry is unaffected by the check above"
        );
    }

    /// The exact residual `shot_dims_for`'s own doc calls out BY DESIGN: an
    /// id+app+title match with DIFFERENT geometry (the narrow slice round-12
    /// P1 #C set out to close) still returns `Some` — deliberately, because
    /// this exact shape is ALSO what a legitimate resize (round-11 P1 #D) or
    /// activation-driven reposition (round-10 P1 #B) produces, and neither
    /// of those may regress. `shot_geometry_for`, called alongside, proves
    /// the drift is at least OBSERVABLE (not silently lost) even though
    /// `shot_dims_for` does not act on it — seeded to make this
    /// intentional, not an oversight.
    #[test]
    fn shot_dims_for_still_hits_on_an_id_app_title_match_with_different_geometry_by_design() {
        let at_capture = WindowInfo {
            id: 13,
            app: "Same".into(),
            title: "same window".into(),
            x: 0,
            y: 0,
            width: 1280,
            height: 800,
        };
        record_shot_dims(921_003, "lead", None, 13, 1280, 800, &at_capture);

        // A DIFFERENT geometry under the SAME id+app+title — standing in
        // for either a legitimate move/resize OR a coincidentally identical
        // id/app/title replacement; `shot_dims_for` cannot tell these apart
        // without a real per-window instance token (see its own doc), so it
        // still returns the recorded dims here.
        let moved = WindowInfo {
            id: 13,
            app: "Same".into(),
            title: "same window".into(),
            x: 400,
            y: 300,
            width: 1000,
            height: 600,
        };
        assert_eq!(
            shot_dims_for(921_003, "lead", None, &moved),
            Some((1280, 800)),
            "id+app+title still match — shot_dims_for must not regress round-10/round-11's \
             resize/move tolerance"
        );
        // But the geometry drift itself is NOT hidden — a caller that wants
        // to reason about it (audit, or a future stricter check) can see it
        // changed.
        assert_eq!(
            shot_geometry_for(921_003, "lead", None, &moved),
            Some((0, 0, 1280, 800)),
            "the RECORDED (capture-time) geometry, not the queried window's current one"
        );
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

        acquire_control(1, "10", None).unwrap();
        let held = control_state().unwrap();
        assert_eq!((held.thread, held.dir.as_str()), (1, "10"));

        // A different (thread, dir) is blocked while the lease is live.
        let err = acquire_control(2, "20", None).unwrap_err();
        assert!(matches!(err, ComputerError::Busy { thread: 1, dir } if dir == "10"));

        // The SAME holder re-acquiring (renewing) still succeeds.
        acquire_control(1, "10", None).unwrap();

        // Releasing a lease you do NOT hold is a no-op.
        release_control(2, "20", None);
        assert!(control_state().is_some());

        // The real holder releasing frees it up immediately.
        release_control(1, "10", None);
        assert!(control_state().is_none());

        // A manually-expired lease reads as absent (and is cleaned up) —
        // simulated by acquiring, then reaching into the internal state to
        // force the monotonic deadline into the past instead of sleeping 30s.
        acquire_control(1, "10", None).unwrap();
        {
            let mut guard = control_mutex().lock().unwrap();
            if let Some(h) = guard.as_mut() {
                h.expires_at = expired_deadline();
            }
        }
        assert!(control_state().is_none(), "expired lease must read as absent");
        // ... and once expired, someone else CAN acquire it.
        acquire_control(2, "20", None).unwrap();

        // clear_control wipes it unconditionally, even mid-lease.
        assert!(control_state().is_some());
        clear_control();
        assert!(control_state().is_none());
    }

    /// issue #160 round-25 P1 (Codex mod.rs:1552): two SIBLING worker sessions
    /// of one multi-repo direction share a single `(thread, dir)` and differ
    /// ONLY by `wt`. The second sibling must be a DIFFERENT holder — it gets
    /// `Busy` while the first holds the lease, never a silent renewal that
    /// would let the two agents interleave input under one lease. A same-`wt`
    /// re-acquire still renews, and `release_control` frees only the exact
    /// `(thread, dir, wt)`.
    #[test]
    fn sibling_workers_differing_only_by_wt_are_distinct_lease_holders() {
        let _guard = process_state_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        clear_control();

        // Worker A (worktree 1) takes the lease.
        acquire_control(7, "42", Some(1)).unwrap();
        let held = control_state().unwrap();
        assert_eq!((held.thread, held.dir.as_str(), held.wt), (7, "42", Some(1)));

        // Sibling worker B (SAME thread+dir, worktree 2) is a DIFFERENT holder
        // and is blocked — not treated as a same-holder renewal.
        let err = acquire_control(7, "42", Some(2)).unwrap_err();
        assert!(
            matches!(err, ComputerError::Busy { thread: 7, ref dir } if dir == "42"),
            "a sibling worker (different wt) must get Busy, got {err:?}"
        );

        // Worker A renewing its OWN lease (same wt) still succeeds.
        acquire_control(7, "42", Some(1)).unwrap();

        // Releasing under the WRONG wt is a no-op — B cannot free A's lease.
        release_control(7, "42", Some(2));
        assert!(
            control_state().is_some(),
            "a sibling's release must not free the holder's lease"
        );

        // A releasing its own exact (thread, dir, wt) frees it, and B can then
        // take it.
        release_control(7, "42", Some(1));
        assert!(control_state().is_none());
        acquire_control(7, "42", Some(2)).unwrap();
        assert_eq!(control_state().unwrap().wt, Some(2));
        clear_control();
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

        acquire_control(950_001, "a", None).unwrap();
        assert_eq!(SHORTCUT_REGISTER_ATTEMPTS.load(Ordering::SeqCst), 1, "the first hold must register");

        // Force the lease to look expired, exactly like
        // `control_lock_busy_expiry_release_and_clear` does above.
        {
            let mut guard = control_mutex().lock().unwrap();
            if let Some(h) = guard.as_mut() {
                h.expires_at = expired_deadline();
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
        acquire_control(950_002, "b", None).unwrap();
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
        acquire_control(9, "90", None).unwrap();

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

        // Holding the guard sets the process-wide `INPUT_IN_FLIGHT` static,
        // which `holder_is_live` consults — an unlocked run of this test
        // (which keeps the flag raised across two 20ms sleeps) made every
        // CONCURRENT lease-expiry assertion in the suite read a lapsed lease
        // as still live (observed as a real one-off failure of
        // `lease_liveness_ignores_the_wall_clock_mirror`). Exactly the
        // un-keyed-static rule `process_state_test_lock`'s own doc states.
        let _guard = process_state_test_lock().lock().unwrap_or_else(|e| e.into_inner());
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

    /// issue #160 round-12 P1 #4: the property this round exists for — while
    /// `input_flight_guard`'s guard is held, a control lease whose own timer
    /// already lapsed must keep reporting as live (both to `control_state`'s
    /// own poll, the Settings banner's source, AND to the OS-level Escape
    /// shortcut's own sync decision), and must go back to ordinary expiry
    /// judgment the instant the injection finishes.
    #[tokio::test]
    async fn control_lease_and_escape_registration_stay_live_while_an_input_is_in_flight() {
        let _guard = process_state_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        clear_control();
        acquire_control(1, "10", None).unwrap();

        // Force the lease's own timer into the past — standing in for a
        // synchronous injection that ran past CONTROL_LEASE_MS, exactly like
        // `control_lock_busy_expiry_release_and_clear`'s own manufactured
        // expiry above (no real 30s sleep).
        {
            let mut g = control_mutex().lock().unwrap_or_else(|e| e.into_inner());
            if let Some(h) = g.as_mut() {
                h.expires_at = expired_deadline();
            }
        }

        let flight = input_flight_guard().await;

        // The control-state poll (the Settings banner's own source) must
        // still report the holder — never "no holder" — while the injection
        // is in flight, even though the lease's own timer already lapsed.
        let held = control_state().unwrap();
        assert_eq!((held.thread, held.dir.as_str()), (1, "10"));

        // The OS-level Escape shortcut's own sync decision must ALSO still
        // see a live holder while in flight — observed via
        // `sync_shortcut_state`'s own attempt counters (no real `AppHandle`
        // in `cargo test --lib`, so the underlying OS call itself always
        // no-ops regardless — see `register_global_escape`'s own doc — but
        // the DECISION of which branch to take is exactly what this round
        // changed).
        let unregister_before = SHORTCUT_UNREGISTER_ATTEMPTS.load(Ordering::SeqCst);
        sync_shortcut_state();
        assert_eq!(
            SHORTCUT_UNREGISTER_ATTEMPTS.load(Ordering::SeqCst),
            unregister_before,
            "an in-flight injection must never let the Escape shortcut be unregistered, even past the lease's own expiry"
        );

        // Releasing the guard restores ordinary expiry judgment for the SAME
        // already-lapsed timer — `control_state`'s own lazy-cleanup path
        // detects the true expiry and unregisters the shortcut.
        drop(flight);
        assert!(
            control_state().is_none(),
            "once the injection finishes, the SAME already-expired lease must go back to reading as expired"
        );
        assert_eq!(
            SHORTCUT_UNREGISTER_ATTEMPTS.load(Ordering::SeqCst),
            unregister_before + 1,
            "once no longer in flight, the now-truly-expired lease must let the Escape shortcut unregister"
        );

        clear_control();
    }

    // —— OS-level global Escape (issue #160 review R1 #5) ——

    #[test]
    fn register_and_unregister_global_escape_noop_without_an_app_handle() {
        // `cargo test --lib` never builds a real Tauri app, so
        // `set_app_handle` is never called in this binary and `APP_HANDLE`
        // stays `None` for the whole process — both calls must silently
        // no-op rather than panic (there is no live runtime-behavior test
        // possible here; see the task's own verification-scope note).
        // issue #160 round-10 P1 #7: `register_global_escape` now returns a
        // `Result` — the no-`APP_HANDLE` case must still be `Ok`, never an
        // `Err` that would fail-close every single-threaded test in this
        // crate closed (see `escape_guard_permits_control`'s own doc).
        assert!(register_global_escape().is_ok());
        unregister_global_escape();
    }

    // —— issue #160 round-10 P1 #7: fail-closed Escape registration ——

    #[test]
    fn escape_guard_permits_control_only_fails_closed_when_a_real_subsystem_rejected_it() {
        // No subsystem at all (every unit test, any headless/no-GUI build):
        // always permits, regardless of what `register_ok` says — there was
        // never a real registration attempt to have failed.
        assert!(escape_guard_permits_control(false, true));
        assert!(
            escape_guard_permits_control(false, false),
            "no subsystem installed must permit control even if `register_ok` were somehow false"
        );
        // A real subsystem exists: its own registration result decides.
        assert!(escape_guard_permits_control(true, true));
        assert!(
            !escape_guard_permits_control(true, false),
            "a real subsystem that rejected registration must fail closed"
        );
    }

    #[test]
    fn acquire_control_grants_when_no_app_handle_subsystem_exists() {
        // `cargo test --lib` never installs a real `tauri::AppHandle`
        // (`APP_HANDLE` stays `None` all binary long — see
        // `register_and_unregister_global_escape_noop_without_an_app_handle`'s
        // own note), so every `acquire_control` in this whole test binary
        // exercises exactly this path: `escape_guard_permits_control(false,
        // _)` always permits. This test names that property explicitly
        // rather than leaving it merely implied by every OTHER
        // `acquire_control`-touching test incidentally succeeding.
        let _guard = process_state_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        clear_control();
        assert!(acquire_control(920_001, "esc-no-handle", None).is_ok());
        clear_control();
    }

    // —— issue #160 round-15 P1: registration-pending leases (Codex mod.rs:1395) ——

    /// A fresh hold in THIS test binary (no `APP_HANDLE`, so
    /// `escape_guard_permits_control(false, _)` always permits — see
    /// `acquire_control_grants_when_no_app_handle_subsystem_exists`'s own
    /// note) must still flip `escape_ready` to `true` once its own
    /// registration round-trip completes, not leave it `false` forever —
    /// otherwise EVERY renewal in this whole test binary would wrongly hit
    /// `EscapeRegistrationPending`.
    #[test]
    fn acquire_control_fresh_hold_marks_escape_ready_when_no_subsystem_exists() {
        let _guard = process_state_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        clear_control();
        assert!(acquire_control(920_101, "esc-ready-fresh", None).is_ok());
        {
            let guard = control_mutex().lock().unwrap_or_else(|e| e.into_inner());
            let holder = guard.as_ref().expect("fresh acquire must have stored a holder");
            assert_eq!(holder.thread, 920_101);
            assert_eq!(holder.dir, "esc-ready-fresh");
            assert!(holder.escape_ready, "a completed fresh-hold registration must mark escape_ready");
        }
        clear_control();
    }

    /// The end-to-end property round-15 exists for: a holder whose OWN
    /// registration is still in flight (`escape_ready: false`) must not be
    /// inherited by ANY later `acquire_control` call — not a same-holder
    /// renewal (would race straight past the flight guard with no confirmed
    /// kill switch), and not a different holder either (still `Busy`, exactly
    /// as an ordinary live lease would be). Once registration completes
    /// (`escape_ready` flips to `true`, simulated here directly rather than
    /// via a real OS callback), the same holder's renewal succeeds normally.
    #[test]
    fn acquire_control_pending_registration_blocks_same_holder_renewal() {
        let _guard = process_state_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        clear_control();
        {
            let mut guard = control_mutex().lock().unwrap_or_else(|e| e.into_inner());
            *guard = Some(ControlHolderState {
                thread: 920_201,
                dir: "esc-pending".to_string(),
                wt: None,
                expires_at: lease_deadline(std::time::Instant::now()),
                expires_at_ms: now_ms().saturating_add(CONTROL_LEASE_MS),
                escape_ready: false,
            });
        }

        // (a) The SAME holder racing back in while registration is still
        // pending must be refused, not silently renewed.
        let err = acquire_control(920_201, "esc-pending", None).unwrap_err();
        assert!(
            matches!(err, ComputerError::EscapeRegistrationPending),
            "a same-holder re-acquire against a pending lease must be refused, got {err:?}"
        );

        // (b) A DIFFERENT holder still reads this pending lease as live and
        // gets the ordinary Busy error — it must not be able to steal the
        // slot just because registration hasn't confirmed yet.
        let err = acquire_control(920_202, "esc-other", None).unwrap_err();
        assert!(
            matches!(err, ComputerError::Busy { thread: 920_201, ref dir } if dir == "esc-pending"),
            "a different holder must still see Busy against a pending lease, got {err:?}"
        );

        // The pending holder's own state must be untouched by either of the
        // rejected calls above.
        {
            let guard = control_mutex().lock().unwrap_or_else(|e| e.into_inner());
            let holder = guard.as_ref().expect("the pending holder must still be present");
            assert_eq!((holder.thread, holder.dir.as_str()), (920_201, "esc-pending"));
            assert!(!holder.escape_ready, "a rejected renewal attempt must not flip escape_ready itself");
        }

        // (c) Once registration completes, the SAME holder's renewal
        // succeeds normally.
        {
            let mut guard = control_mutex().lock().unwrap_or_else(|e| e.into_inner());
            if let Some(h) = guard.as_mut() {
                h.escape_ready = true;
            }
        }
        assert!(
            acquire_control(920_201, "esc-pending", None).is_ok(),
            "a same-holder renewal must succeed once escape_ready is true"
        );

        clear_control();
    }

    /// issue #160 round-34 P2 (Codex computer_srv.rs:2800): no lease can be
    /// installed while the stop latch is set — a stale already-approved
    /// request racing Emergency Stop must not resurrect the control banner
    /// and global Escape interception after a successful Stop. Normal
    /// acquisition resumes once the latch is cleared.
    #[test]
    fn acquire_control_refuses_while_the_stop_latch_is_set() {
        let _guard = process_state_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        clear_control();
        let my_gen = trip_stop_latch();

        let err = acquire_control(920_401, "post-stop", None).unwrap_err();
        assert!(matches!(err, ComputerError::Disabled), "{err:?}");
        assert!(control_state().is_none(), "no lease may be installed after Stop");

        assert!(clear_emergency_stop(my_gen));
        assert!(
            acquire_control(920_401, "post-stop", None).is_ok(),
            "acquisition resumes normally once the latch is cleared"
        );
        clear_control();
    }

    /// issue #160 round-29 P2 (Codex mod.rs:1625): lease liveness is judged on
    /// the MONOTONIC deadline alone — the serialized wall-clock estimate is
    /// display-only. Simulated backward clock correction: the wall-clock
    /// mirror claims the lease has practically forever left while the
    /// monotonic deadline has passed; the holder must read as EXPIRED (a
    /// wall-clock comparison would have kept it live for the whole rollback,
    /// wedging every other session on `Busy` and keeping the Escape
    /// registration/banner armed past the promised 30s).
    #[test]
    fn lease_liveness_ignores_the_wall_clock_mirror() {
        let _guard = process_state_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        clear_control();
        acquire_control(920_301, "clock-rollback", None).unwrap();
        {
            let mut guard = control_mutex().lock().unwrap_or_else(|e| e.into_inner());
            if let Some(h) = guard.as_mut() {
                h.expires_at = expired_deadline();
                h.expires_at_ms = u64::MAX;
            }
        }

        assert!(
            control_state().is_none(),
            "an expired monotonic deadline must expire the lease no matter what the \
             wall-clock mirror claims"
        );
        // And the slot is genuinely free: a different session acquires it.
        assert!(
            acquire_control(920_302, "clock-rollback-next", None).is_ok(),
            "the expired lease must not hold the next session to Busy"
        );
        clear_control();
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
