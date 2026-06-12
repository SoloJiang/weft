//! Codex slash command discovery for weft's composer palette.
//!
//! Codex does NOT expose a "list slash commands" RPC over `app-server` — the
//! TUI keeps its `/`-menu in a hardcoded enum (codex-rs/tui/src/slash_command.rs).
//! We mirror that enum here as a static table so the weft composer shows the
//! same palette the codex TUI does. The dynamic dimension — user-installed
//! skills — comes via `skills/list` over the shared app-server connection
//! when reachable; failures degrade silently to just the built-ins (palette
//! discovery must never block the composer).
//!
//! When codex bumps the enum, re-sync this table:
//!   curl -fsSL https://raw.githubusercontent.com/openai/codex/main/codex-rs/tui/src/slash_command.rs

use crate::lead_chat::proto::SlashCmd;

struct Builtin {
    name: &'static str,
    description: &'static str,
    visible: bool,
}

// Order matches the SlashCommand enum = TUI popup order. The visibility flags
// mirror SlashCommand::is_visible() (platform / debug-build gates).
fn builtins() -> Vec<Builtin> {
    let macos_or_windows = cfg!(any(target_os = "macos", target_os = "windows"));
    let windows = cfg!(target_os = "windows");
    let not_android = !cfg!(target_os = "android");
    let debug = cfg!(debug_assertions);
    vec![
        Builtin {
            name: "model",
            description: "choose what model and reasoning effort to use",
            visible: true,
        },
        Builtin {
            name: "ide",
            description: "include current selection, open files, and other context from your IDE",
            visible: true,
        },
        Builtin {
            name: "permissions",
            description: "choose what Codex is allowed to do",
            visible: true,
        },
        Builtin {
            name: "keymap",
            description: "remap TUI shortcuts",
            visible: true,
        },
        Builtin {
            name: "vim",
            description: "toggle Vim mode for the composer",
            visible: true,
        },
        Builtin {
            name: "setup-default-sandbox",
            description: "set up elevated agent sandbox",
            visible: true,
        },
        Builtin {
            name: "sandbox-add-read-dir",
            description: "let sandbox read a directory: /sandbox-add-read-dir <absolute_path>",
            visible: windows,
        },
        Builtin {
            name: "experimental",
            description: "toggle experimental features",
            visible: true,
        },
        Builtin {
            name: "approve",
            description: "approve one retry of a recent auto-review denial",
            visible: true,
        },
        Builtin {
            name: "memories",
            description: "configure memory use and generation",
            visible: true,
        },
        Builtin {
            name: "skills",
            description: "use skills to improve how Codex performs specific tasks",
            visible: true,
        },
        Builtin {
            name: "import",
            description: "import setup, this project, and recent chats from another coding agent",
            visible: true,
        },
        Builtin {
            name: "hooks",
            description: "view and manage lifecycle hooks",
            visible: true,
        },
        Builtin {
            name: "review",
            description: "review my current changes and find issues",
            visible: true,
        },
        Builtin {
            name: "rename",
            description: "rename the current thread",
            visible: true,
        },
        Builtin {
            name: "new",
            description: "start a new chat during a conversation",
            visible: true,
        },
        Builtin {
            name: "archive",
            description: "archive this session and exit",
            visible: true,
        },
        Builtin {
            name: "delete",
            description: "permanently delete this session and exit",
            visible: true,
        },
        Builtin {
            name: "resume",
            description: "resume a saved chat",
            visible: true,
        },
        Builtin {
            name: "fork",
            description: "fork the current chat",
            visible: true,
        },
        Builtin {
            name: "app",
            description: "continue this session in Codex Desktop",
            visible: macos_or_windows,
        },
        Builtin {
            name: "init",
            description: "create an AGENTS.md file with instructions for Codex",
            visible: true,
        },
        Builtin {
            name: "compact",
            description: "summarize conversation to prevent hitting the context limit",
            visible: true,
        },
        Builtin {
            name: "plan",
            description: "switch to Plan mode",
            visible: true,
        },
        Builtin {
            name: "goal",
            description: "set or view the goal for a long-running task",
            visible: true,
        },
        Builtin {
            name: "agent",
            description: "switch the active agent thread",
            visible: true,
        },
        Builtin {
            name: "side",
            description: "start a side conversation in an ephemeral fork",
            visible: true,
        },
        Builtin {
            name: "btw",
            description: "start a side conversation in an ephemeral fork",
            visible: true,
        },
        Builtin {
            name: "copy",
            description: "copy last response as markdown",
            visible: not_android,
        },
        Builtin {
            name: "raw",
            description: "toggle raw scrollback mode for copy-friendly terminal selection",
            visible: true,
        },
        Builtin {
            name: "diff",
            description: "show git diff (including untracked files)",
            visible: true,
        },
        Builtin {
            name: "mention",
            description: "mention a file",
            visible: true,
        },
        Builtin {
            name: "status",
            description: "show current session configuration and token usage",
            visible: true,
        },
        Builtin {
            name: "debug-config",
            description: "show config layers and requirement sources for debugging",
            visible: true,
        },
        Builtin {
            name: "title",
            description: "configure which items appear in the terminal title",
            visible: true,
        },
        Builtin {
            name: "statusline",
            description: "configure which items appear in the status line",
            visible: true,
        },
        Builtin {
            name: "theme",
            description: "choose a syntax highlighting theme",
            visible: true,
        },
        Builtin {
            name: "pets",
            description: "choose or hide the terminal pet",
            visible: true,
        },
        Builtin {
            name: "mcp",
            description: "list configured MCP tools; use /mcp verbose for details",
            visible: true,
        },
        Builtin {
            name: "apps",
            description: "manage apps",
            visible: true,
        },
        Builtin {
            name: "plugins",
            description: "browse plugins",
            visible: true,
        },
        Builtin {
            name: "logout",
            description: "log out of Codex",
            visible: true,
        },
        Builtin {
            name: "quit",
            description: "exit Codex",
            visible: true,
        },
        Builtin {
            name: "exit",
            description: "exit Codex",
            visible: true,
        },
        Builtin {
            name: "feedback",
            description: "send logs to maintainers",
            visible: true,
        },
        Builtin {
            name: "rollout",
            description: "print the rollout file path",
            visible: debug,
        },
        Builtin {
            name: "ps",
            description: "list background terminals",
            visible: true,
        },
        Builtin {
            name: "stop",
            description: "stop all background terminals",
            visible: true,
        },
        Builtin {
            name: "clear",
            description: "clear the terminal and start a new chat",
            visible: true,
        },
        Builtin {
            name: "personality",
            description: "choose a communication style for Codex",
            visible: true,
        },
        Builtin {
            name: "realtime",
            description: "toggle realtime voice mode (experimental)",
            visible: true,
        },
        Builtin {
            name: "settings",
            description: "configure realtime microphone/speaker",
            visible: true,
        },
        Builtin {
            name: "test-approval",
            description: "test approval request",
            visible: debug,
        },
        Builtin {
            name: "subagents",
            description: "switch the active agent thread",
            visible: true,
        },
    ]
}

/// Built-in + dynamic-skills slash command list for the codex composer palette.
/// Skills come from `skills/list` over the shared `codex app-server` connection
/// when reachable; failures degrade silently to just the built-ins so the
/// composer never blocks on discovery.
pub async fn discover_commands() -> Vec<SlashCmd> {
    let mut out: Vec<SlashCmd> = builtins()
        .into_iter()
        .filter(|b| b.visible)
        .map(|b| SlashCmd {
            name: b.name.into(),
            description: Some(b.description.into()),
            arg_hint: None,
        })
        .collect();

    if let Ok(skills) = fetch_skills().await {
        let seen: std::collections::HashSet<String> = out.iter().map(|c| c.name.clone()).collect();
        for s in skills {
            if !seen.contains(&s.name) {
                out.push(s);
            }
        }
    }
    out
}

async fn fetch_skills() -> anyhow::Result<Vec<SlashCmd>> {
    let client = crate::codex_app_server::client().await?;
    let v = client.request("skills/list", serde_json::json!({})).await?;
    // `skills/list` shape isn't pinned across codex versions; accept either
    // `{ skills: [...] }` or a top-level array, pulling (name, description) per
    // entry. Anything unrecognised yields no skills (silent fallback).
    let arr = v
        .get("skills")
        .and_then(|x| x.as_array())
        .cloned()
        .or_else(|| v.as_array().cloned())
        .unwrap_or_default();
    Ok(arr
        .iter()
        .filter_map(|e| {
            let name = e.get("name")?.as_str()?.trim();
            if name.is_empty() {
                return None;
            }
            let desc = e
                .get("shortDescription")
                .or_else(|| e.get("description"))
                .and_then(|d| d.as_str())
                .filter(|s| !s.is_empty())
                .map(String::from);
            Some(SlashCmd {
                name: name.to_string(),
                description: desc,
                arg_hint: None,
            })
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn builtins_carry_unique_names_and_nonempty_descriptions() {
        let mut seen = HashSet::new();
        for b in builtins() {
            assert!(seen.insert(b.name), "duplicate builtin: {}", b.name);
            assert!(
                !b.description.is_empty(),
                "empty description for {}",
                b.name
            );
        }
        // Sanity floor: codex ships 55 variants. We should be at least within a
        // few of that, accounting for platform gates dropping a handful.
        let visible: Vec<_> = builtins().into_iter().filter(|b| b.visible).collect();
        assert!(
            visible.len() >= 50,
            "suspiciously few visible builtins: {}",
            visible.len()
        );
    }

    #[test]
    fn well_known_codex_commands_present() {
        let names: HashSet<&str> = builtins()
            .into_iter()
            .filter(|b| b.visible)
            .map(|b| b.name)
            .collect();
        for must in [
            "model", "compact", "init", "review", "skills", "plan", "status",
        ] {
            assert!(names.contains(must), "missing codex built-in /{must}");
        }
    }
}
