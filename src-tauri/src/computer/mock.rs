//! Test-only [`backend::ComputerBackend`] impl. `#[doc(hidden)] pub` (not
//! `#[cfg(test)]`) because integration tests under `tests/` are a separate
//! crate and need to construct this from outside the library's own test
//! builds — see `backend::_set_backend_override`'s doc comment for the same
//! reasoning.

use super::backend::ComputerBackend;
use super::{CapturedImage, ComputerError, WindowInfo};

#[doc(hidden)]
pub struct MockBackend {
    pub windows: Vec<WindowInfo>,
    pub image: Option<CapturedImage>,
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
}
