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
        // Press every modifier, click the trailing (real) key, then release
        // the modifiers in reverse order — a combo with no modifiers at all
        // (e.g. "f5") is just `modifiers == []` and this clicks the single
        // key, same as any other key press.
        for m in modifiers {
            enigo
                .key(*m, Direction::Press)
                .map_err(|e| ComputerError::Unsupported(e.to_string()))?;
        }
        let click_result = enigo.key(*last, Direction::Click);
        for m in modifiers.iter().rev() {
            // Best-effort release even if the click itself failed midway —
            // leaving a modifier physically "held" on the real keyboard
            // because we bailed out early would be a worse failure mode
            // than a masked release error.
            let _ = enigo.key(*m, Direction::Release);
        }
        click_result.map_err(|e| ComputerError::Unsupported(e.to_string()))
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
