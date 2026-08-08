//! Process runtime mode (Stage 1 of the Codex Desktop migration; spec:
//! docs/superpowers/specs/2026-08-08-codex-desktop-migration-design.md).
//!
//! The desktop app and the headless daemon (weftd) BOTH allow agent-spawning
//! background passes (e.g. curator analysis). Unit tests install no mode, which
//! keeps them from ever spawning an agent — this replaces the old
//! `APP_HANDLE.is_some()` guard, which conflated "has a UI" with "may spawn".

use std::sync::OnceLock;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    App,
    Daemon,
}

static MODE: OnceLock<Mode> = OnceLock::new();

/// Install the process-wide mode once at boot. Re-entry (tests) is harmless.
pub fn set_mode(mode: Mode) {
    let _ = MODE.set(mode);
}

/// True in the app and in weftd; false in tests (no mode installed).
pub fn agents_allowed() -> bool {
    MODE.get().is_some()
}

#[cfg(test)]
mod tests {
    #[test]
    fn installing_a_mode_allows_agents() {
        // OnceLock is process-wide and shared across test threads, so only
        // assert the direction this test controls: after set, agents allowed.
        super::set_mode(super::Mode::Daemon);
        assert!(super::agents_allowed());
    }
}
