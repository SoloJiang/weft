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

/// Window enumeration + capture, abstracted so the platform-independent
/// logic in `computer/mod.rs` (matching, scaling, file I/O) never touches a
/// real OS API directly and can be driven by [`super::mock::MockBackend`] in
/// tests.
pub trait ComputerBackend: Send + Sync {
    fn list_windows(&self) -> Result<Vec<WindowInfo>, ComputerError>;
    fn capture_window(&self, id: u32) -> Result<CapturedImage, ComputerError>;
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

    fn capture_window(&self, _id: u32) -> Result<CapturedImage, ComputerError> {
        Err(ComputerError::Unsupported(
            "built without computer-os feature".into(),
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
