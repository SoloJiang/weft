//! User-configurable command overrides for coding-agent CLIs ("aliases").
//!
//! The tool *identity* ("claude"/"codex"/"opencode") selects the adapter, wire
//! dialect, and trust handling, and is stored on every thread/session row — it
//! must stay stable. The *binary* actually spawned is separable: some users have
//! the CLI installed under a different name (e.g. `claude` aliased/renamed to
//! `cc-claude`). This module holds the identity → command map so every spawn
//! resolves the real binary while the identity (and thus all session rows,
//! resume ids, and adapters) is untouched.
//!
//! Resolution is GLOBAL and read at spawn time, so a newly-configured override
//! applies uniformly to NEW *and* EXISTING sessions: an existing thread keeps
//! `lead_tool = "claude"` and simply spawns the configured command on its next
//! run — no migration of old rows. To let the user EXCLUDE existing sessions
//! from a later override, each row carries an optional per-session pin
//! (`thread.lead_command` / `session.command`); when present it wins over the
//! global map (see [`effective`]).
//!
//! The override value is a bare command name resolved on `PATH` (which Weft
//! augments from the login shell at startup, see `detect.rs`), matching how the
//! un-aliased CLIs are found.

use std::collections::HashMap;
use std::sync::{OnceLock, RwLock};

/// `app_setting` key holding the JSON object `{tool: command}` of overrides.
pub const K_TOOL_COMMANDS: &str = "tool_commands";

/// Process-global override map, loaded once at startup and refreshed after a
/// settings write. Sync (`std::sync::RwLock`) so both blocking (`std::process`)
/// and async spawn sites can read it without an `.await`.
fn overrides() -> &'static RwLock<HashMap<String, String>> {
    static O: OnceLock<RwLock<HashMap<String, String>>> = OnceLock::new();
    O.get_or_init(|| RwLock::new(HashMap::new()))
}

/// Replace the in-memory override map (startup load + post-write refresh). A
/// poisoned lock only means a prior panic while holding it; recover the inner
/// guard rather than propagating (overrides are best-effort config).
pub fn set_overrides(map: HashMap<String, String>) {
    let mut g = match overrides().write() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    *g = map;
}

/// The global command for a tool identity: the configured override, else the
/// identity itself (a bare name resolved on `PATH` at spawn).
pub fn command_for(tool: &str) -> String {
    let found = match overrides().read() {
        Ok(g) => g.get(tool).cloned(),
        Err(p) => p.into_inner().get(tool).cloned(),
    };
    found.unwrap_or_else(|| tool.to_string())
}

/// The effective command for a SPECIFIC session: a per-session pin wins (it
/// froze this session to a command when the user excluded existing sessions from
/// a later override), else the global command for the tool identity.
pub fn effective(pin: Option<&str>, tool: &str) -> String {
    match pin {
        Some(p) if !p.trim().is_empty() => p.trim().to_string(),
        _ => command_for(tool),
    }
}

/// Parse the stored JSON object into a clean map: trim values and drop blank or
/// identity (`command == tool`) entries so the map only holds real overrides.
pub fn parse_overrides(json: &str) -> HashMap<String, String> {
    serde_json::from_str::<HashMap<String, String>>(json)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(tool, cmd)| {
            let cmd = cmd.trim().to_string();
            (!cmd.is_empty() && cmd != tool).then_some((tool, cmd))
        })
        .collect()
}

/// Serialize the override map for storage. A `HashMap<String,String>` cannot
/// fail to serialize, but stay panic-free per the production-path policy.
pub fn to_json(map: &HashMap<String, String>) -> String {
    serde_json::to_string(map).unwrap_or_else(|_| "{}".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    // These assertions all mutate the PROCESS-global override map, so they live in
    // ONE test: cargo runs tests in the same binary concurrently by default, and
    // separate tests racing on `set_overrides` would be nondeterministic.
    #[test]
    fn global_override_and_pin_resolution() {
        // No override configured → the identity is its own command.
        set_overrides(HashMap::new());
        assert_eq!(command_for("claude"), "claude");
        assert_eq!(command_for("codex"), "codex");

        // A configured override is returned; unconfigured tools still fall back.
        set_overrides(HashMap::from([("claude".to_string(), "cc-claude".to_string())]));
        assert_eq!(command_for("claude"), "cc-claude");
        assert_eq!(command_for("codex"), "codex");

        // A per-session pin wins over the global override.
        assert_eq!(effective(Some("claude"), "claude"), "claude");
        // Blank/whitespace pin is ignored → falls through to the global override.
        assert_eq!(effective(Some("  "), "claude"), "cc-claude");
        assert_eq!(effective(None, "claude"), "cc-claude");

        set_overrides(HashMap::new()); // leave the global map clean
    }

    #[test]
    fn parse_drops_blank_and_identity_entries() {
        let m = parse_overrides(
            r#"{"claude":" cc-claude ","codex":"codex","opencode":"  "}"#,
        );
        assert_eq!(m.get("claude").map(String::as_str), Some("cc-claude"));
        assert!(!m.contains_key("codex")); // identity → dropped
        assert!(!m.contains_key("opencode")); // blank → dropped
    }

    #[test]
    fn parse_tolerates_malformed_json() {
        assert!(parse_overrides("not json").is_empty());
        assert!(parse_overrides("").is_empty());
    }
}
