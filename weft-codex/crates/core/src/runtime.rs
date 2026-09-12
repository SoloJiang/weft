//! Liveness gate for agent-spawning background passes (curator analysis,
//! orchestrator spawns). The daemon installs live mode at boot; tests never
//! do, so a test can never spawn an agent.

use std::sync::OnceLock;

static LIVE: OnceLock<()> = OnceLock::new();

/// Mark the process live. Idempotent.
pub fn set_live() {
    let _ = LIVE.set(());
}

/// True only in the booted daemon.
pub fn agents_allowed() -> bool {
    LIVE.get().is_some()
}

#[cfg(test)]
mod tests {
    #[test]
    fn set_live_enables_agents() {
        super::set_live();
        assert!(super::agents_allowed());
    }
}
