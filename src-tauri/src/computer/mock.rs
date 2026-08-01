//! Test-only [`backend::ComputerBackend`] impl. `#[doc(hidden)] pub` (not
//! `#[cfg(test)]`) because integration tests under `tests/` are a separate
//! crate and need to construct this from outside the library's own test
//! builds — see `backend::_set_backend_override`'s doc comment for the same
//! reasoning.

use super::backend::{ComputerBackend, MouseButton};
use super::{CapturedImage, ComputerError, WindowInfo};
use std::sync::atomic::AtomicBool;
use std::sync::Mutex;

#[doc(hidden)]
#[derive(Default)]
pub struct MockBackend {
    pub windows: Vec<WindowInfo>,
    pub image: Option<CapturedImage>,
    /// One line per input method call (issue #160 M2), in call order — e.g.
    /// `"click 100,200 Left x1"`. Lets a test assert an action actually
    /// reached the backend without a real OS input device. `Mutex` (not
    /// `RefCell`) because `ComputerBackend` requires `Send + Sync` and this
    /// struct is shared behind an `Arc<dyn ComputerBackend>`.
    pub actions: Mutex<Vec<String>>,
    /// Backing store for `cursor_position`/`move_cursor` — starts at
    /// `(0, 0)`, configurable by writing it directly before installing the
    /// backend, and updated by `move_cursor` the same way a real backend's
    /// cursor would move.
    pub cursor: Mutex<(i32, i32)>,
    /// When `true`, `activate_window` fails with `Unsupported` instead of
    /// recording an `"activate {id}"` action (issue #160 round-4 P1 §2) — a
    /// test-controlled switch simulating a real backend that couldn't find
    /// any window-activation API at all (`StubBackend`'s own permanent
    /// case), so a test can exercise `bus::computer_srv::
    /// activate_if_interactive`'s fail-closed path without a second
    /// `ComputerBackend` impl. `false` (the `Default` value) behaves exactly
    /// like every other successful mock action.
    pub fail_activate: AtomicBool,
}

impl MockBackend {
    fn record(&self, line: String) {
        self.actions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(line);
    }
}

impl ComputerBackend for MockBackend {
    fn list_windows(&self) -> Result<Vec<WindowInfo>, ComputerError> {
        Ok(self.windows.clone())
    }

    /// Ignores `id` — tests configure at most one canned image per
    /// `MockBackend`, which is all the current test matrix needs (matching
    /// already narrowed to exactly one window before this is called).
    fn capture_window(&self, _id: u32) -> Result<CapturedImage, ComputerError> {
        self.image
            .clone()
            .ok_or_else(|| ComputerError::CaptureFailed("mock backend has no image configured".into()))
    }

    fn click(&self, x: i32, y: i32, button: MouseButton, count: u32) -> Result<(), ComputerError> {
        self.record(format!("click {x},{y} {button:?} x{count}"));
        Ok(())
    }

    fn type_text(&self, text: &str) -> Result<(), ComputerError> {
        self.record(format!("type {text}"));
        Ok(())
    }

    fn key(&self, combo: &str) -> Result<(), ComputerError> {
        self.record(format!("key {combo}"));
        Ok(())
    }

    fn scroll(&self, x: i32, y: i32, dx: i32, dy: i32) -> Result<(), ComputerError> {
        self.record(format!("scroll {x},{y} {dx},{dy}"));
        Ok(())
    }

    fn drag(&self, from: (i32, i32), to: (i32, i32)) -> Result<(), ComputerError> {
        self.record(format!("drag {},{} -> {},{}", from.0, from.1, to.0, to.1));
        Ok(())
    }

    fn move_cursor(&self, x: i32, y: i32) -> Result<(), ComputerError> {
        self.record(format!("move_cursor {x},{y}"));
        *self.cursor.lock().unwrap_or_else(|e| e.into_inner()) = (x, y);
        Ok(())
    }

    fn cursor_position(&self) -> Result<(i32, i32), ComputerError> {
        Ok(*self.cursor.lock().unwrap_or_else(|e| e.into_inner()))
    }

    fn activate_window(&self, id: u32) -> Result<(), ComputerError> {
        if self.fail_activate.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(ComputerError::Unsupported(
                "mock backend: activate_window forced to fail".into(),
            ));
        }
        self.record(format!("activate {id}"));
        Ok(())
    }
}
