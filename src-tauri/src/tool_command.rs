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
    *g = map
        .into_iter()
        .filter(|(_tool, cmd)| validate_command(cmd).is_ok())
        .collect();
}

/// Reject commands that are clearly not bare PATH-resolved binary names.
/// Allows typical aliases like `cc-claude` but forbids absolute/relative paths,
/// shell metacharacters, and whitespace that could switch to a different command.
fn validate_command(cmd: &str) -> Result<(), String> {
    if cmd.is_empty() {
        return Err("command cannot be empty".into());
    }
    if cmd.starts_with('/') || cmd.starts_with('\\') || cmd.contains("..") {
        return Err("command must be a bare name resolved on PATH, not a path".into());
    }
    if cmd.contains('/') || cmd.contains('\\') {
        return Err("command cannot contain path separators".into());
    }
    if cmd.chars().any(|c| c.is_whitespace()) {
        return Err("command cannot contain whitespace".into());
    }
    // Conservative allow-list: letters, digits, dot, dash, underscore.
    if !cmd.chars().all(|c| c.is_alphanumeric() || c == '.' || c == '-' || c == '_') {
        return Err(
            "command may only contain letters, digits, '.', '-', '_'".into(),
        );
    }
    Ok(())
}

/// The global command for a tool identity: the configured override, else the
/// identity itself (a bare name resolved on `PATH` at spawn).
pub fn command_for(tool: &str) -> String {
    let found = match overrides().read() {
        Ok(g) => g.get(tool).cloned(),
        Err(p) => p.into_inner().get(tool).cloned(),
    };
    let cmd = found.unwrap_or_else(|| tool.to_string());
    if let Err(err) = validate_command(&cmd) {
        eprintln!("[weft][tool_command] invalid override for {tool}: {err}; falling back to {tool}");
        return tool.to_string();
    }
    cmd
}

/// The effective command for a SPECIFIC session: a per-session pin wins (it
/// froze this session to a command when the user excluded existing sessions from
/// a later override), else the global command for the tool identity.
pub fn effective(pin: Option<&str>, tool: &str) -> String {
    let cmd = match pin {
        Some(p) if !p.trim().is_empty() => p.trim().to_string(),
        _ => return command_for(tool),
    };
    if let Err(err) = validate_command(&cmd) {
        eprintln!("[weft][tool_command] invalid pin for {tool}: {err}; falling back to configured command");
        return command_for(tool);
    }
    cmd
}

/// Parse the stored JSON object into a clean map: trim values and drop blank,
/// identity (`command == tool`), or invalid entries so the map only holds real,
/// safe overrides.
pub fn parse_overrides(json: &str) -> HashMap<String, String> {
    serde_json::from_str::<HashMap<String, String>>(json)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(tool, cmd)| {
            let cmd = cmd.trim().to_string();
            if cmd.is_empty() || cmd == tool || validate_command(&cmd).is_err() {
                return None;
            }
            Some((tool, cmd))
        })
        .collect()
}

/// Serialize the override map for storage. A `HashMap<String,String>` cannot
/// fail to serialize, but stay panic-free per the production-path policy.
pub fn to_json(map: &HashMap<String, String>) -> String {
    serde_json::to_string(map).unwrap_or_else(|_| "{}".to_string())
}

/// A surfacing-ready message for a coding-agent spawn failure. A missing binary
/// (`ErrorKind::NotFound`/ENOENT) becomes the stable `agent-not-found:<tool>`
/// code the UI localizes into an actionable message; any other spawn error keeps
/// the resolved command + OS text so it stays diagnosable. `tool` is the stable
/// identity (codex/claude/opencode), NOT the alias, so the message reads the same
/// whether or not a command override is set. Callers wrap this in their error type.
pub fn spawn_error_message(tool: &str, command: &str, err: &std::io::Error) -> String {
    if err.kind() == std::io::ErrorKind::NotFound {
        format!("agent-not-found:{tool}")
    } else {
        format!("could not run {command}: {err}")
    }
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
        // Invalid/legacy pins also fall back to the configured command, not the bare identity.
        assert_eq!(effective(Some("/opt/claude"), "claude"), "cc-claude");
        assert_eq!(effective(Some("claude --evil"), "claude"), "cc-claude");

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

    #[test]
    fn spawn_error_message_maps_missing_binary_to_stable_code() {
        let enoent = std::io::Error::from(std::io::ErrorKind::NotFound);
        assert_eq!(
            spawn_error_message("codex", "codex", &enoent),
            "agent-not-found:codex"
        );
        // The stable identity drives the code, not the resolved (aliased) binary.
        assert_eq!(
            spawn_error_message("claude", "cc-claude", &enoent),
            "agent-not-found:claude"
        );
    }

    #[test]
    fn spawn_error_message_keeps_other_failures_diagnosable() {
        let denied = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        let msg = spawn_error_message("codex", "/opt/codex", &denied);
        assert!(msg.contains("/opt/codex"), "{msg}");
        assert!(!msg.contains("agent-not-found"), "{msg}");
    }

    #[test]
    fn command_validation_rejects_paths_and_shell_meta() {
        assert!(validate_command("claude").is_ok());
        assert!(validate_command("cc-claude").is_ok());
        assert!(validate_command("claude.exe").is_ok());
        assert!(validate_command("/usr/bin/claude").is_err());
        assert!(validate_command("./claude").is_err());
        assert!(validate_command("claude;rm -rf /").is_err());
        assert!(validate_command("claude --danger").is_err());
        assert!(validate_command("").is_err());
    }

    #[test]
    fn parse_drops_invalid_overrides() {
        let m = parse_overrides(
            r#"{"claude":"cc-claude","codex":"/opt/codex","opencode":"claude;evil"}"#,
        );
        assert_eq!(m.get("claude").map(String::as_str), Some("cc-claude"));
        assert!(!m.contains_key("codex"));
        assert!(!m.contains_key("opencode"));
    }

    #[test]
    fn effective_falls_back_on_invalid_pin() {
        set_overrides(HashMap::new());
        // With no global override, invalid pins fall back to the bare identity.
        assert_eq!(effective(Some("/opt/claude"), "claude"), "claude");
        assert_eq!(effective(Some("claude --evil"), "claude"), "claude");
        // Valid bare pins are honored.
        assert_eq!(effective(Some("cc-claude"), "claude"), "cc-claude");
        set_overrides(HashMap::new()); // leave the global map clean
    }
}
