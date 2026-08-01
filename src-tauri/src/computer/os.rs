//! Real platform backend (issue #160 M1), via the `xcap` crate —
//! cross-platform window enumeration + capture. Only compiled with the
//! `computer-os` cargo feature (default off; see the dependency comment on
//! `xcap` in `Cargo.toml` for why). Runtime failures here (no X/Wayland
//! server, no display, missing OS capture permission — e.g. macOS Screen
//! Recording TCC) are all expected and map to `ComputerError`, never panic.

use super::backend::ComputerBackend;
use super::{CapturedImage, ComputerError, WindowInfo};

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
