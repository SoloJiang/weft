//! Real platform backend (issue #160 M1 window enumeration/capture via
//! `xcap`, M2 mouse/keyboard input injection via `enigo`). Only compiled
//! with the `computer-os` cargo feature (default off; see the dependency
//! comments on `xcap`/`enigo` in `Cargo.toml` for why). Runtime failures
//! here (no X/Wayland server, no display, missing OS capture/input
//! permission — e.g. macOS Screen Recording / Accessibility TCC) are all
//! expected and map to `ComputerError`, never panic.

use super::backend::{ComputerBackend, MouseButton};
use super::{CapturedImage, ComputerError, KeyToken, NamedKey, WindowInfo};
use enigo::{Axis, Button, Coordinate, Direction, Enigo, Key, Keyboard, Mouse, Settings};

pub struct OsBackend;

impl ComputerBackend for OsBackend {
    fn list_windows(&self) -> Result<Vec<WindowInfo>, ComputerError> {
        let windows = xcap::Window::all().map_err(|e| ComputerError::Unsupported(e.to_string()))?;
        // Any one window failing to report itself (a transient handle from a
        // window that closed between enumeration and inspection, a
        // permission-scoped window we can see but not introspect, ...) skips
        // just that window rather than failing the whole list.
        Ok(windows.iter().filter_map(|w| window_info(w).ok()).collect())
    }

    fn capture_window(&self, id: u32) -> Result<CapturedImage, ComputerError> {
        let windows = xcap::Window::all().map_err(|e| ComputerError::Unsupported(e.to_string()))?;
        let target = windows
            .iter()
            .find(|w| matches!(w.id(), Ok(wid) if wid == id))
            .ok_or_else(|| ComputerError::CaptureFailed(format!("window {id} is no longer visible")))?;
        let image = target
            .capture_image()
            .map_err(|e| ComputerError::CaptureFailed(e.to_string()))?;
        let width = image.width();
        let height = image.height();
        Ok(CapturedImage {
            rgba: image.into_raw(),
            width,
            height,
        })
    }

    fn click(&self, x: i32, y: i32, button: MouseButton, count: u32) -> Result<(), ComputerError> {
        let mut enigo = new_enigo()?;
        move_abs(&mut enigo, x, y)?;
        let btn = match button {
            MouseButton::Left => Button::Left,
            MouseButton::Right => Button::Right,
        };
        // `count` clicks in a row (2 = double-click, 3 = triple-click) — the
        // caller passes 1 for a plain click; guard against a stray 0 so this
        // never silently no-ops an intended click.
        for _ in 0..count.max(1) {
            enigo
                .button(btn, Direction::Click)
                .map_err(|e| ComputerError::Unsupported(e.to_string()))?;
        }
        Ok(())
    }

    fn type_text(&self, text: &str) -> Result<(), ComputerError> {
        let mut enigo = new_enigo()?;
        enigo
            .text(text)
            .map_err(|e| ComputerError::Unsupported(e.to_string()))
    }

    fn key(&self, combo: &str) -> Result<(), ComputerError> {
        let tokens = super::parse_key_combo(combo)?;
        let keys: Vec<Key> = tokens.into_iter().map(map_key_token).collect();
        let Some((last, modifiers)) = keys.split_last() else {
            return Err(ComputerError::Unsupported("empty key combo".into()));
        };
        let mut enigo = new_enigo()?;
        // Press each modifier in order, but track exactly how many
        // succeeded (`pressed`) instead of using `?` to bail out on the
        // first failure (issue #160 review R1 #4): a `?` on a LATER
        // modifier's press would abandon every EARLIER modifier physically
        // held on the real keyboard with nothing left to release it. Every
        // failure path below — a modifier press failing partway through, OR
        // the trailing (real) key's own click failing — releases every
        // modifier that's actually held, in reverse press order,
        // best-effort (a release call's own error is swallowed: a
        // doubly-stuck key can't get any worse, and bailing out on one
        // release would abandon releasing the rest). Mirrors `drag`'s
        // press-move-release-regardless discipline below.
        let mut pressed: Vec<&Key> = Vec::with_capacity(modifiers.len());
        let mut result: Result<(), ComputerError> = Ok(());
        for m in modifiers {
            match enigo.key(*m, Direction::Press) {
                Ok(()) => pressed.push(m),
                Err(e) => {
                    result = Err(ComputerError::Unsupported(e.to_string()));
                    break;
                }
            }
        }
        if result.is_ok() {
            result = enigo
                .key(*last, Direction::Click)
                .map_err(|e| ComputerError::Unsupported(e.to_string()));
        }
        for m in pressed.iter().rev() {
            let _ = enigo.key(**m, Direction::Release);
        }
        result
    }

    fn scroll(&self, x: i32, y: i32, dx: i32, dy: i32) -> Result<(), ComputerError> {
        let mut enigo = new_enigo()?;
        move_abs(&mut enigo, x, y)?;
        if dy != 0 {
            enigo
                .scroll(dy, Axis::Vertical)
                .map_err(|e| ComputerError::Unsupported(e.to_string()))?;
        }
        if dx != 0 {
            enigo
                .scroll(dx, Axis::Horizontal)
                .map_err(|e| ComputerError::Unsupported(e.to_string()))?;
        }
        Ok(())
    }

    fn drag(&self, from: (i32, i32), to: (i32, i32)) -> Result<(), ComputerError> {
        let mut enigo = new_enigo()?;
        move_abs(&mut enigo, from.0, from.1)?;
        enigo
            .button(Button::Left, Direction::Press)
            .map_err(|e| ComputerError::Unsupported(e.to_string()))?;
        let move_result = enigo.move_mouse(to.0, to.1, Coordinate::Abs);
        // Always release, even if the move partway through failed — an
        // undropped mouse button held down on the real desktop is a much
        // worse stuck state than a masked move error.
        let release_result = enigo.button(Button::Left, Direction::Release);
        move_result
            .and(release_result)
            .map_err(|e| ComputerError::Unsupported(e.to_string()))
    }

    fn move_cursor(&self, x: i32, y: i32) -> Result<(), ComputerError> {
        let mut enigo = new_enigo()?;
        move_abs(&mut enigo, x, y)
    }

    fn cursor_position(&self) -> Result<(i32, i32), ComputerError> {
        let enigo = new_enigo()?;
        enigo
            .location()
            .map_err(|e| ComputerError::Unsupported(e.to_string()))
    }

    fn activate_window(&self, id: u32) -> Result<(), ComputerError> {
        activate_window_impl(id)
    }
}

// —— window activation (issue #160 round-4 P1 §2) ——
//
// Neither `xcap` (window enumeration/capture) nor `enigo` (input injection —
// this module's other dependency) exposes a "raise/focus this window" API,
// and this crate cannot add a new Cargo dependency just for this (see the
// allow-list this fix was scoped against), so each platform below reaches
// for whatever ALREADY-available system mechanism can do it: an OS binary
// every real install of that OS ships (macOS/Linux), or a direct FFI call
// into a system DLL that is always present and linkable without a new crate
// (Windows' own `user32.dll`). `id` is always the SAME value this module's
// own `list_windows`/`resolve_window` already hand back for this window (see
// `window_info` below and `xcap`'s own per-platform `Window::id()`), so
// every platform's own implementation reconstructs whatever it needs FROM
// that id rather than threading a second identifier through.

#[cfg(target_os = "macos")]
fn activate_window_impl(id: u32) -> Result<(), ComputerError> {
    // macOS has no public API to raise a SPECIFIC window within a
    // multi-window app without the Accessibility (AX) API's own window
    // handle, which `xcap` does not expose — so this activates at the APP
    // level first (`NSRunningApplication`/System Events process
    // activation), the best-effort approximation the round-4 review's own
    // "NSRunningApplication ... 或 CGWindow 层激活" guidance calls for.
    // Resolves `id`'s owning pid via `xcap` (the SAME window list this
    // module's own `list_windows`/`capture_window` already re-fetch on every
    // call — see those methods above), then shells out to `osascript`
    // (System Events) rather than raw `NSRunningApplication` Objective-C
    // runtime FFI: `osascript` ships on every real macOS install, needs no
    // new Cargo dependency, and needs no hand-rolled `objc_msgSend` calls
    // this crate cannot verify compile *or* run in this container (see this
    // module's own build-time note).
    //
    // issue #160 round-5 review P1 §7 (issue #160 #2): app-level frontmost
    // alone does NOT guarantee THIS window — as opposed to some OTHER window
    // of the same multi-window app — ends up the one actually raised to the
    // front; that was Codex's own finding on round-4's fix. So this now
    // ALSO attempts a per-window raise on top of the app-level activation —
    // see `raise_specific_window`'s own doc for the fail-closed policy that
    // governs it.
    let windows = xcap::Window::all().map_err(|e| ComputerError::Unsupported(e.to_string()))?;
    let target = windows
        .iter()
        .find(|w| matches!(w.id(), Ok(wid) if wid == id))
        .ok_or_else(|| ComputerError::CaptureFailed(format!("window {id} is no longer visible")))?;
    let pid = target.pid().map_err(|e| ComputerError::Unsupported(e.to_string()))?;

    let script = format!(
        "tell application \"System Events\" to set frontmost of (first process whose unix id is {pid}) to true"
    );
    let output = std::process::Command::new("osascript")
        .arg("-e")
        .arg(&script)
        .output()
        .map_err(|e| ComputerError::Unsupported(format!("osascript: {e}")))?;
    if !output.status.success() {
        return Err(ComputerError::Unsupported(format!(
            "osascript activation failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }

    raise_specific_window(pid, target)
}

/// Best-effort, FAIL-CLOSED per-window raise on top of the app-level
/// `frontmost` activation above (issue #160 round-5 review P1 §7 / issue
/// #160 #2 — Codex's own finding that app-level activation doesn't pick a
/// SPECIFIC window within a multi-window app). Matches by TITLE — the only
/// window handle `xcap::Window` exposes; there is no AXUIElement/window-id
/// bridge between `xcap`'s id space and System Events' own accessibility
/// tree without a new Cargo dependency, which is out of scope for this fix
/// (see this module's own top-of-file dependency note) — via System Events'
/// accessibility API (`perform action "AXRaise"`), but ONLY when that title
/// identifies EXACTLY ONE window of the owning process:
///
///  - an EMPTY title (real — a borderless palette, some tool/utility
///    windows genuinely have none) can't be matched to a specific window at
///    all;
///  - a title matching ZERO or MORE THAN ONE window of that process (stale
///    since `xcap`'s own window list was read a moment ago and the window
///    closed/renamed, or two windows of the same app that happen to share a
///    title) is likewise not something this can raise with any confidence.
///
/// Both cases return `Err` (`Unsupported`) rather than silently leaving the
/// human with only the app-level frontmost `activate_window_impl` already
/// performed above — a caller relying on this to reactivate a SPECIFIC
/// window (`bus::computer_srv::activate_target`) must not be told "done"
/// when only the wrong window (or an arbitrary one) ended up on top; it fails
/// closed and the caller's own input action never reaches the OS (see
/// `activate_target`'s own fail-closed contract).
///
/// UNVERIFIED ON REAL MACOS HARDWARE (recorded here and tracked in issue
/// #160 §9): this container has no macOS toolchain to build OR run this
/// against — `#[cfg(target_os = "macos")]` code is stripped before
/// type-checking on any other host target, so even `cargo check --features
/// computer-os` on this Linux container cannot catch a wrong method name or
/// a System Events dictionary mistake here, only a gross syntax error. The
/// AppleScript below is written to the best available knowledge of System
/// Events' scripting dictionary (a `window`'s `name` property is its
/// AXTitle; `perform action "AXRaise"` is the standard accessibility raise
/// action) — a real-machine check is required before trusting this in
/// production.
#[cfg(target_os = "macos")]
fn raise_specific_window(pid: u32, target: &xcap::Window) -> Result<(), ComputerError> {
    let title = target.title().map_err(|e| ComputerError::Unsupported(e.to_string()))?;
    if title.trim().is_empty() {
        return Err(ComputerError::Unsupported(
            "window has no title to match for a per-window raise — the app itself was brought \
             forward, but which of its windows ended up on top could not be confirmed"
                .into(),
        ));
    }
    let title_lit = applescript_string_literal(&title);

    // Count how many of this process's windows share this exact title BEFORE
    // attempting to raise one of them — an ambiguous title (0, or more than
    // 1, match) fails closed rather than raising an arbitrary one of several
    // same-titled windows or silently no-op'ing on a stale title.
    let count_script = format!(
        "tell application \"System Events\" to count (every window of (first process whose unix id \
         is {pid}) whose name is {title_lit})"
    );
    let count_output = std::process::Command::new("osascript")
        .arg("-e")
        .arg(&count_script)
        .output()
        .map_err(|e| ComputerError::Unsupported(format!("osascript (window count): {e}")))?;
    if !count_output.status.success() {
        return Err(ComputerError::Unsupported(format!(
            "couldn't count windows titled {title:?}: {}",
            String::from_utf8_lossy(&count_output.stderr)
        )));
    }
    let count_text = String::from_utf8_lossy(&count_output.stdout);
    let count: usize = count_text.trim().parse().map_err(|_| {
        ComputerError::Unsupported(format!(
            "couldn't parse the window-count reply as a number: {count_text:?}"
        ))
    })?;
    if count != 1 {
        return Err(ComputerError::Unsupported(format!(
            "window title {title:?} matches {count} window(s) of this app — refusing an \
             ambiguous per-window raise"
        )));
    }

    let raise_script = format!(
        "tell application \"System Events\" to tell (first process whose unix id is {pid}) to \
         perform action \"AXRaise\" of (first window whose name is {title_lit})"
    );
    let raise_output = std::process::Command::new("osascript")
        .arg("-e")
        .arg(&raise_script)
        .output()
        .map_err(|e| ComputerError::Unsupported(format!("osascript (window raise): {e}")))?;
    if !raise_output.status.success() {
        return Err(ComputerError::Unsupported(format!(
            "per-window raise failed — the app was brought forward, but this specific window \
             couldn't be confirmed on top: {}",
            String::from_utf8_lossy(&raise_output.stderr)
        )));
    }
    Ok(())
}

/// Escape a string for embedding as an AppleScript double-quoted string
/// literal (issue #160 round-5 review P1 §7): a window TITLE is arbitrary,
/// attacker-influenceable text (whatever the target app chose to display),
/// so it must never be spliced into the script unescaped — a title
/// containing `"` would otherwise close the literal early and inject
/// arbitrary AppleScript. Backslash is escaped FIRST so escaping the quote
/// right after doesn't double-escape the backslashes it just introduced.
#[cfg(target_os = "macos")]
fn applescript_string_literal(s: &str) -> String {
    let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

#[cfg(target_os = "windows")]
fn activate_window_impl(id: u32) -> Result<(), ComputerError> {
    // Direct `user32.dll` FFI rather than a new Cargo dependency on the
    // `windows` crate `xcap` itself uses internally but does not re-export
    // (see `xcap::lib`'s own `pub use` list) — `user32.dll` ships on every
    // Windows install and is always linkable without adding anything to
    // `Cargo.toml`. `id` is the SAME truncated-to-`u32` `HWND` value `xcap`'s
    // own Windows backend already hands back as `Window::id()` (it does
    // `self.hwnd.0 as u32` — see `xcap::windows::impl_window`), so widening
    // it back to a pointer-sized value here reconstructs the real handle for
    // every window this module can already resolve by id in the first place
    // — the SAME assumption every other action in this file already makes
    // (there is no OTHER id this crate's own window list could hand back).
    #[allow(non_snake_case)]
    #[link(name = "user32")]
    extern "system" {
        fn SetForegroundWindow(hwnd: isize) -> i32;
        fn ShowWindow(hwnd: isize, cmd: i32) -> i32;
        fn IsIconic(hwnd: isize) -> i32;
    }
    const SW_RESTORE: i32 = 9;
    let hwnd = id as isize;
    // Safety: `SetForegroundWindow`/`ShowWindow`/`IsIconic` are ordinary
    // `user32.dll` calls taking a plain window handle and returning a status
    // code — no raw pointers are dereferenced on the Rust side, and passing
    // a stale/invalid handle is defined by Win32 to fail the call (returning
    // 0/`FALSE`), never undefined behavior.
    unsafe {
        // A minimized window can't become the foreground window until it's
        // restored first — best-effort: ignore `ShowWindow`'s own result,
        // the `SetForegroundWindow` call right after is the one whose
        // success actually matters here.
        if IsIconic(hwnd) != 0 {
            ShowWindow(hwnd, SW_RESTORE);
        }
        if SetForegroundWindow(hwnd) == 0 {
            return Err(ComputerError::Unsupported(
                "SetForegroundWindow failed — the window may no longer exist, or Windows refused \
                 the foreground request (it enforces its own focus-stealing rules)"
                    .into(),
            ));
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn activate_window_impl(id: u32) -> Result<(), ComputerError> {
    // Shells out to whichever of `wmctrl`/`xdotool` is actually installed,
    // rather than linking `libX11` directly: `xcap`'s own Linux backend
    // talks to the X server via the pure-Rust `xcb` crate (see
    // `xcap::linux::impl_window`), not `libX11.so`, so there is no guarantee
    // that shared library is even linkable on every machine this runs on.
    // These two command-line tools are the standard, widely-packaged way
    // desktop scripts already raise/focus an X11 window by id — `wmctrl -i
    // -a` sends the exact `_NET_ACTIVE_WINDOW` client message the round-4
    // review names; `xdotool windowactivate` is the fallback when it isn't
    // installed. Neither is guaranteed present (a hard requirement no NEW
    // Cargo dependency could fix anyway on a Wayland desktop with neither
    // tool packaged), so any failure here is `Unsupported`, never a panic or
    // a silent no-op. `id` is the raw X11 window id `xcap`'s own Linux
    // backend already hands back unchanged as `Window::id()` (see
    // `xcap::linux::impl_window`), so both tools can address it directly —
    // `wmctrl -i` wants it as a `0x`-prefixed hex string.
    let hex_id = format!("0x{id:08x}");
    if let Ok(output) = std::process::Command::new("wmctrl").args(["-i", "-a", &hex_id]).output() {
        if output.status.success() {
            return Ok(());
        }
    }
    if let Ok(output) = std::process::Command::new("xdotool")
        .args(["windowactivate", "--sync", &id.to_string()])
        .output()
    {
        if output.status.success() {
            return Ok(());
        }
    }
    Err(ComputerError::Unsupported(
        "couldn't activate the window — neither `wmctrl` nor `xdotool` is available/succeeded \
         (install one of them, or grant this window an Always approval to avoid needing focus \
         reclaim at all)"
            .into(),
    ))
}

#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
fn activate_window_impl(_id: u32) -> Result<(), ComputerError> {
    Err(ComputerError::Unsupported(
        "window activation isn't implemented for this platform".into(),
    ))
}

fn new_enigo() -> Result<Enigo, ComputerError> {
    Enigo::new(&Settings::default()).map_err(|e| ComputerError::Unsupported(e.to_string()))
}

fn move_abs(enigo: &mut Enigo, x: i32, y: i32) -> Result<(), ComputerError> {
    enigo
        .move_mouse(x, y, Coordinate::Abs)
        .map_err(|e| ComputerError::Unsupported(e.to_string()))
}

/// [`KeyToken`] -> `enigo::Key`. `cmd`/`win` both parse to [`KeyToken::Meta`]
/// (see `parse_key_combo`'s doc comment) and both map here to `Key::Meta` —
/// enigo's own per-platform modifier naming (`Command`/`Super`/`LWin`/...)
/// stays entirely inside enigo; this module never branches on target OS.
fn map_key_token(token: KeyToken) -> Key {
    match token {
        KeyToken::Meta => Key::Meta,
        KeyToken::Control => Key::Control,
        KeyToken::Alt => Key::Alt,
        KeyToken::Shift => Key::Shift,
        KeyToken::Unicode(c) => Key::Unicode(c),
        KeyToken::Named(named) => map_named_key(named),
    }
}

fn map_named_key(named: NamedKey) -> Key {
    match named {
        NamedKey::Return => Key::Return,
        NamedKey::Tab => Key::Tab,
        NamedKey::Escape => Key::Escape,
        NamedKey::Space => Key::Space,
        NamedKey::Backspace => Key::Backspace,
        NamedKey::Delete => Key::Delete,
        NamedKey::Up => Key::UpArrow,
        NamedKey::Down => Key::DownArrow,
        NamedKey::Left => Key::LeftArrow,
        NamedKey::Right => Key::RightArrow,
        NamedKey::Home => Key::Home,
        NamedKey::End => Key::End,
        NamedKey::PageUp => Key::PageUp,
        NamedKey::PageDown => Key::PageDown,
        NamedKey::F1 => Key::F1,
        NamedKey::F2 => Key::F2,
        NamedKey::F3 => Key::F3,
        NamedKey::F4 => Key::F4,
        NamedKey::F5 => Key::F5,
        NamedKey::F6 => Key::F6,
        NamedKey::F7 => Key::F7,
        NamedKey::F8 => Key::F8,
        NamedKey::F9 => Key::F9,
        NamedKey::F10 => Key::F10,
        NamedKey::F11 => Key::F11,
        NamedKey::F12 => Key::F12,
    }
}

/// Map one `xcap::Window` to `WindowInfo`. Any single accessor erroring
/// drops the whole window (see the `list_windows` doc comment above) rather
/// than surfacing a half-populated row.
fn window_info(w: &xcap::Window) -> Result<WindowInfo, ComputerError> {
    let describe = |e: xcap::XCapError| ComputerError::CaptureFailed(e.to_string());
    Ok(WindowInfo {
        id: w.id().map_err(describe)?,
        app: w.app_name().map_err(describe)?,
        title: w.title().map_err(describe)?,
        x: w.x().map_err(describe)?,
        y: w.y().map_err(describe)?,
        width: w.width().map_err(describe)?,
        height: w.height().map_err(describe)?,
    })
}
