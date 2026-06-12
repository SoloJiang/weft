//! Generic agent-adapter facade (Phase B of agent-integration-hardening). Each
//! tool's spawn-argv / wire-dialect / interrupt specifics live behind one trait
//! so the engine can stop matching on tool strings. The adapters REPLICATE the
//! exact arg/parse logic already in `engine.rs`/`proto.rs` (verified by the unit
//! tests below), so wiring the engine to them is a behavior-preserving structural
//! change — NOT a behavior change.
//!
//! This module is intentionally NOT yet called by the live engine: it's the
//! reviewed, tested foundation. The engine cutover (route `ensure_running` /
//! `spawn_turn` / `spawn_reader` / `interrupt` through `adapter_for`) is a
//! follow-up to land with GUI validation, so it can't regress the working
//! claude/codex/opencode paths.
#![allow(dead_code)]

use std::path::Path;
use std::sync::Arc;

use crate::lead_chat::proto::{self, ChatEvent, SlashCmd};

/// Everything an adapter needs to build a spawn command for one turn.
pub struct AdapterContext<'a> {
    pub cwd: &'a Path,
    pub system_prompt: &'a str,
    pub extra_args: &'a [String],
    pub native_id: Option<&'a str>,
    /// The user message. Per-turn tools append it to argv; the long-lived claude
    /// path ignores it here and writes it to stdin instead.
    pub message: &'a str,
    pub slash_commands: &'a [SlashCmd],
}

/// How an in-flight turn is stopped.
#[derive(Debug, PartialEq, Eq)]
pub enum Interrupt {
    /// Write a protocol payload to stdin, then kill after a grace period (claude).
    Protocol,
    /// Kill the per-turn child; EOF finalizes the turn (codex exec, opencode).
    Kill,
    /// Send `turn/interrupt` over the persistent connection (codex app-server).
    Connection,
}

pub trait AgentAdapter: Send + Sync {
    fn tool(&self) -> &'static str;
    /// No resident process per session — a fresh child per turn (codex exec, opencode).
    fn per_turn(&self) -> bool;
    /// Multiplexed over one persistent connection, not a per-session child
    /// (codex app-server). Mutually exclusive with `per_turn`.
    fn is_connection(&self) -> bool {
        false
    }
    /// Accepts inline base64 image blocks (claude); per-turn dialects spill to files.
    fn supports_inline_images(&self) -> bool {
        false
    }

    /// Side-effecting pre-spawn step — folder-trust pre-accept. No-op by default;
    /// kept separate from `build_argv` so arg-building stays pure + testable.
    fn prepare(&self, _cwd: &Path) {}

    /// `(program, argv)` to spawn. Returns Err for connection adapters. Pure (no
    /// side effects — call `prepare` first for trust writes).
    fn build_argv(&self, ctx: &AdapterContext) -> anyhow::Result<(String, Vec<String>)>;

    fn parse_line(&self, line: &str) -> ChatEvent;
    fn extract_native_id(&self, line: &str) -> Option<String>;

    fn interrupt(&self) -> Interrupt;
    /// Stdin payload for a `Protocol` interrupt (claude); empty otherwise.
    fn interrupt_payload(&self, _generation: u64) -> String {
        String::new()
    }
}

// ───────────────────────────── claude ─────────────────────────────

pub struct ClaudeAdapter;

impl AgentAdapter for ClaudeAdapter {
    fn tool(&self) -> &'static str {
        "claude"
    }
    fn per_turn(&self) -> bool {
        false
    }
    fn supports_inline_images(&self) -> bool {
        true
    }

    fn prepare(&self, cwd: &Path) {
        crate::claude::ensure_trusted(cwd);
    }

    fn build_argv(&self, ctx: &AdapterContext) -> anyhow::Result<(String, Vec<String>)> {
        // Mirrors engine::build_args exactly (claude reads the message from stdin,
        // so `ctx.message` is not appended here).
        let mut a: Vec<String> = vec![
            "-p".into(),
            "--input-format".into(),
            "stream-json".into(),
            "--output-format".into(),
            "stream-json".into(),
            "--include-partial-messages".into(),
            "--verbose".into(),
        ];
        if !ctx.system_prompt.is_empty() {
            a.push("--append-system-prompt".into());
            a.push(ctx.system_prompt.to_string());
        }
        if let Some(id) = ctx.native_id {
            a.push("--resume".into());
            a.push(id.to_string());
        }
        a.extend(ctx.extra_args.iter().cloned());
        Ok(("claude".into(), a))
    }

    fn parse_line(&self, line: &str) -> ChatEvent {
        proto::parse_line(line)
    }
    fn extract_native_id(&self, line: &str) -> Option<String> {
        proto::extract_native("claude", line)
    }
    fn interrupt(&self) -> Interrupt {
        Interrupt::Protocol
    }
    fn interrupt_payload(&self, generation: u64) -> String {
        let req = serde_json::json!({
            "type": "control_request",
            "request_id": format!("weft-int-{generation}"),
            "request": { "subtype": "interrupt" }
        });
        format!("{req}\n")
    }
}

// ───────────────────────────── codex (exec) ─────────────────────────────

pub struct CodexExecAdapter;

impl AgentAdapter for CodexExecAdapter {
    fn tool(&self) -> &'static str {
        "codex"
    }
    fn per_turn(&self) -> bool {
        true
    }

    fn prepare(&self, cwd: &Path) {
        crate::codex::ensure_codex_trusted(cwd);
    }

    fn build_argv(&self, ctx: &AdapterContext) -> anyhow::Result<(String, Vec<String>)> {
        // Mirrors engine::spawn_turn's codex branch (message rides the argv).
        let mut a: Vec<String> = vec!["exec".into()];
        a.extend(ctx.extra_args.iter().cloned());
        a.push("--json".into());
        a.push("--cd".into());
        a.push(ctx.cwd.to_string_lossy().into_owned());
        let first_turn = ctx.native_id.is_none();
        if let Some(id) = ctx.native_id {
            a.push("resume".into());
            a.push(id.to_string());
        }
        // codex has no `--append-system-prompt`: on the first turn, prepend the
        // system prompt (e.g. the Concierge prompt) to the message. Resume turns
        // already carry it in the conversation history, so only the first.
        let msg = if first_turn && !ctx.system_prompt.is_empty() {
            format!("{}\n\n{}", ctx.system_prompt, ctx.message)
        } else {
            ctx.message.to_string()
        };
        a.push(msg);
        Ok(("codex".into(), a))
    }

    fn parse_line(&self, line: &str) -> ChatEvent {
        proto::parse_line_for("codex", line)
    }
    fn extract_native_id(&self, line: &str) -> Option<String> {
        proto::extract_native("codex", line)
    }
    fn interrupt(&self) -> Interrupt {
        Interrupt::Kill
    }
}

// ───────────────────────── codex (app-server) ─────────────────────────

pub struct CodexAppServerAdapter;

impl AgentAdapter for CodexAppServerAdapter {
    fn tool(&self) -> &'static str {
        "codex"
    }
    fn per_turn(&self) -> bool {
        false
    }
    fn is_connection(&self) -> bool {
        true
    }

    fn build_argv(&self, _ctx: &AdapterContext) -> anyhow::Result<(String, Vec<String>)> {
        anyhow::bail!(
            "codex app-server is a connection adapter — drive it via codex_app_server::client()"
        )
    }

    fn parse_line(&self, _line: &str) -> ChatEvent {
        ChatEvent::Other
    }
    fn extract_native_id(&self, _line: &str) -> Option<String> {
        None
    }
    fn interrupt(&self) -> Interrupt {
        Interrupt::Connection
    }
}

// ───────────────────────────── opencode ─────────────────────────────

pub struct OpenCodeAdapter;

/// Same `/cmd args` split the engine uses to route opencode slash commands.
fn split_slash(text: &str) -> Option<(String, String)> {
    let mut it = text.strip_prefix('/')?.splitn(2, char::is_whitespace);
    let name = it.next().unwrap_or_default().to_string();
    if name.is_empty() {
        return None;
    }
    let rest = it.next().unwrap_or("").trim_start().to_string();
    Some((name, rest))
}

impl AgentAdapter for OpenCodeAdapter {
    fn tool(&self) -> &'static str {
        "opencode"
    }
    fn per_turn(&self) -> bool {
        true
    }

    fn build_argv(&self, ctx: &AdapterContext) -> anyhow::Result<(String, Vec<String>)> {
        // Mirrors engine::spawn_turn's opencode branch, incl. --command dispatch.
        let mut a: Vec<String> = vec!["run".into(), "--format".into(), "json".into()];
        if let Some(id) = ctx.native_id {
            a.push("--session".into());
            a.push(id.to_string());
        }
        let mut message = ctx.message.to_string();
        if let Some((name, rest)) = split_slash(ctx.message) {
            if ctx.slash_commands.iter().any(|c| c.name == name) {
                a.push("--command".into());
                a.push(name);
                message = rest;
            }
        }
        a.push(message);
        Ok(("opencode".into(), a))
    }

    fn parse_line(&self, line: &str) -> ChatEvent {
        proto::parse_line_for("opencode", line)
    }
    fn extract_native_id(&self, line: &str) -> Option<String> {
        proto::extract_native("opencode", line)
    }
    fn interrupt(&self) -> Interrupt {
        Interrupt::Kill
    }
}

// ───────────────────────────── resolver ─────────────────────────────

/// Codex app-server is opt-in because its initialized handshake can initialize
/// ChatGPT Apps (`/backend-api/wham/apps`) and fail on network/auth state before
/// a normal Weft turn starts. Exec is the safe default.
pub(crate) fn codex_prefers_appserver_from_env(value: Option<&str>) -> bool {
    matches!(
        value.map(str::trim).map(str::to_ascii_lowercase).as_deref(),
        Some("1" | "true" | "yes" | "on")
    )
}

pub(crate) fn codex_prefers_appserver() -> bool {
    codex_prefers_appserver_from_env(std::env::var("WEFT_CODEX_APPSERVER").ok().as_deref())
}

/// The PROCESS adapter for a tool identity, used by the engine's spawn/parse/
/// interrupt paths. For codex this is always the exec adapter — the app-server
/// transport never flows through those paths; it's driven by
/// `engine::spawn_codex_turn`, selected by the send() gating. Unknown → None.
pub fn adapter_for(tool: &str) -> Option<Arc<dyn AgentAdapter>> {
    match tool {
        "claude" => Some(Arc::new(ClaudeAdapter)),
        "codex" => Some(Arc::new(CodexExecAdapter)),
        "opencode" => Some(Arc::new(OpenCodeAdapter)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn ctx<'a>(
        cwd: &'a Path,
        native: Option<&'a str>,
        msg: &'a str,
        slash: &'a [SlashCmd],
    ) -> AdapterContext<'a> {
        AdapterContext {
            cwd,
            system_prompt: "be lead",
            extra_args: &[],
            native_id: native,
            message: msg,
            slash_commands: slash,
        }
    }

    #[test]
    fn claude_argv_matches_engine_shape() {
        let cwd = PathBuf::from("/tmp");
        let (prog, a) = ClaudeAdapter
            .build_argv(&ctx(&cwd, Some("sess-1"), "hi", &[]))
            .unwrap();
        assert_eq!(prog, "claude");
        assert!(a.contains(&"--include-partial-messages".to_string()));
        assert!(a.contains(&"--verbose".to_string()));
        // resume id present; message NOT on argv (claude reads stdin).
        let i = a.iter().position(|x| x == "--resume").unwrap();
        assert_eq!(a[i + 1], "sess-1");
        assert!(!a.contains(&"hi".to_string()));
    }

    #[test]
    fn codex_exec_argv_carries_message_and_resume() {
        let cwd = PathBuf::from("/repo");
        let (prog, a) = CodexExecAdapter
            .build_argv(&ctx(&cwd, Some("t1"), "do it", &[]))
            .unwrap();
        assert_eq!(prog, "codex");
        assert_eq!(a[0], "exec");
        assert!(a.contains(&"--json".to_string()));
        assert_eq!(a.last().unwrap(), "do it");
        let i = a.iter().position(|x| x == "resume").unwrap();
        assert_eq!(a[i + 1], "t1");
    }

    #[test]
    fn opencode_argv_routes_known_slash_to_command() {
        let cwd = PathBuf::from("/repo");
        let cmds = vec![SlashCmd::bare("review")];
        let (_p, a) = OpenCodeAdapter
            .build_argv(&ctx(&cwd, None, "/review fix it", &cmds))
            .unwrap();
        let i = a.iter().position(|x| x == "--command").unwrap();
        assert_eq!(a[i + 1], "review");
        assert_eq!(a.last().unwrap(), "fix it");
        // unknown slash stays literal
        let (_p, b) = OpenCodeAdapter
            .build_argv(&ctx(&cwd, None, "/nope hi", &cmds))
            .unwrap();
        assert!(!b.contains(&"--command".to_string()));
        assert_eq!(b.last().unwrap(), "/nope hi");
    }

    #[test]
    fn codex_appserver_is_opt_in() {
        assert!(!codex_prefers_appserver_from_env(None));
        assert!(!codex_prefers_appserver_from_env(Some("")));
        assert!(!codex_prefers_appserver_from_env(Some("0")));
        assert!(!codex_prefers_appserver_from_env(Some("false")));
        assert!(codex_prefers_appserver_from_env(Some("1")));
        assert!(codex_prefers_appserver_from_env(Some("true")));
        assert!(codex_prefers_appserver_from_env(Some("YES")));
        assert!(codex_prefers_appserver_from_env(Some(" on ")));
    }

    #[test]
    fn transport_flags_and_resolver_map_correctly() {
        // Process adapters never also claim a connection, and vice-versa.
        assert!(CodexExecAdapter.per_turn() && !CodexExecAdapter.is_connection());
        assert!(!CodexAppServerAdapter.per_turn() && CodexAppServerAdapter.is_connection());
        assert!(!ClaudeAdapter.per_turn() && !ClaudeAdapter.is_connection());
        // Resolver maps tool identities; codex → the exec (process) adapter.
        assert_eq!(adapter_for("claude").unwrap().tool(), "claude");
        assert_eq!(adapter_for("opencode").unwrap().tool(), "opencode");
        let codex = adapter_for("codex").unwrap();
        assert_eq!(codex.tool(), "codex");
        assert!(codex.per_turn() && !codex.is_connection());
        assert!(adapter_for("mystery").is_none());
    }

    #[test]
    fn interrupt_kinds_match_transport() {
        assert_eq!(ClaudeAdapter.interrupt(), Interrupt::Protocol);
        assert_eq!(CodexExecAdapter.interrupt(), Interrupt::Kill);
        assert_eq!(OpenCodeAdapter.interrupt(), Interrupt::Kill);
        assert_eq!(CodexAppServerAdapter.interrupt(), Interrupt::Connection);
        assert!(ClaudeAdapter.interrupt_payload(7).contains("weft-int-7"));
    }
}
