//! Spawn-time, ADDITIVE injection of the thread bus as an MCP server for each
//! tool. Never overrides a sub-repo's own config: claude/codex use file-less
//! launch flags; opencode deep-merges into the worktree opencode.json (which is
//! a throwaway worktree, not the canonical repo — architecture §2.1).

use std::path::Path;

/// Extra args to PREPEND to the tool's own args (global flags must precede any
/// subcommand, e.g. `codex -c k=v resume <id>`).
pub struct Injection {
    pub args: Vec<String>,
    /// Per-engine environment overrides. Used for OpenCode's documented
    /// OPENCODE_CONFIG_CONTENT so concurrent sessions in one worktree never
    /// overwrite a shared opencode.json bus URL.
    pub env: Vec<(String, String)>,
}

fn mcp_url(base: &str, thread: i32, dir: &str, session_id: Option<i32>) -> String {
    let url = format!("{base}/bus/{thread}/{dir}/mcp");
    match session_id {
        Some(session_id) => format!("{url}?session_id={session_id}"),
        None => url,
    }
}

fn planner_url(base: &str, thread: i32) -> String {
    format!("{base}/planner/{thread}/mcp")
}

fn curator_url(base: &str, thread: i32) -> String {
    format!("{base}/curator/{thread}/mcp")
}

fn global_url(base: &str) -> String {
    format!("{base}/global/mcp")
}

fn ask_url(
    base: &str,
    thread: i32,
    dir: &str,
    session_id: Option<i32>,
    tool: &str,
) -> String {
    let url = format!("{base}/ask/{thread}/{dir}?tool={tool}");
    match session_id {
        Some(session_id) => format!("{url}&session_id={session_id}"),
        None => url,
    }
}

/// HTTP MCP servers Weft should pass on ACP `session/new|resume` for this
/// engine role. Workers get `weft_bus` with their exact persisted session id;
/// lead also gets planner when `dir` is the lead lane; concierge/global callers
/// pass `include_global`.
pub fn acp_mcp_servers(
    base: &str,
    thread: i32,
    dir: &str,
    session_id: Option<i32>,
    include_bus: bool,
    include_planner: bool,
    include_global: bool,
    include_curator: bool,
) -> Vec<crate::acp::McpServerSpec> {
    let mut out = Vec::new();
    // Concierge is global-only (no per-thread bus) — same as inject_global path.
    if include_bus {
        out.push(crate::acp::McpServerSpec {
            name: "weft_bus".into(),
            url: mcp_url(base, thread, dir, session_id),
        });
    }
    if include_planner {
        out.push(crate::acp::McpServerSpec {
            name: "weft_planner".into(),
            url: planner_url(base, thread),
        });
    }
    if include_curator {
        out.push(crate::acp::McpServerSpec {
            name: "weft_curator".into(),
            url: curator_url(base, thread),
        });
    }
    if include_global {
        out.push(crate::acp::McpServerSpec {
            name: "weft_global".into(),
            url: global_url(base),
        });
    }
    out
}

/// The shared, FAIL-CLOSED tail of both bash ask-hook scripts — claude's
/// per-session `.weft-ask-hook[-<session>].sh` (below) and codex's global helper
/// (`codex.rs::ensure_codex_hook_in`, which splices it in at
/// `__DECIDE_OR_DENY__`). Expects `$resp` = curl's stdout and `$rc` = curl's
/// exit status, prints exactly one PreToolUse decision, and always exits 0.
///
/// Why it can't just print `$resp` and exit: "exit 0 with no output" means
/// *continue* to both consumers — Codex documents it as success, and claude
/// falls back to its own permission flow, which the user's own allowlist can
/// auto-approve — so a weft that isn't answering (app quit or crashed, port
/// moved, curl missing) would ALLOW every tool call of an unsupervised agent.
/// Instead, only a body carrying one of weft's two actual verdicts is passed
/// through; anything else emits the same explicit deny shape a human's Deny
/// answer produces (`bus/server.rs::hook_decision`, which #139 made the
/// timeout/cancel fallback for the same reason). Exit status stays 0 in every
/// branch on purpose: a NON-zero exit is reported as a hook error and the tool
/// call continues, i.e. it would be fail-open all over again.
///
/// Passthrough requires ALL of: curl reporting a successful transfer (`$rc` 0 —
/// both scripts pass `-f`, so an HTTP error status is a failure too), a body that
/// still looks structurally whole (`{`…`}`), and one of the exact
/// `"permissionDecision":"<verdict>"` pairs `hook_decision`'s compact JSON emits.
/// The `$rc` gate is what catches a truncated answer: the verdict pair sits
/// BEFORE `permissionDecisionReason` in that JSON, so a transfer interrupted
/// after the verdict (weft crashing mid-body → curl exit 18) leaves a body that
/// matches the pair but is invalid JSON, which the consumer would parse-fail and
/// then CONTINUE on. This is a structural check, not a JSON parse — the scripts
/// deliberately depend on nothing but bash and curl — so the exact-pair match and
/// the `$rc` gate carry the weight together. Both passthrough tests fail loudly
/// if that serialization ever changes.
///
/// The two reasons are BILINGUAL in one string rather than locale-selected. The
/// frontend `src/i18n/*.ts` tables are unreachable from here (they are TS modules
/// bundled into the webview), and codex's copy of this tail lives in ONE global
/// `~/.weft/weft-codex-hook.sh` shared by every session and locale — written by
/// `ensure_codex_hook()`, which has no session and no locale to key off. Asking
/// weft for the localized text is self-defeating too: this text exists precisely
/// for the case where weft can't be reached. So both languages ship inline, the
/// same "embed the pair in Rust" shape `im::outbound::t` already uses for
/// human-facing Rust text.
///
/// Authored flush-left with NO trailing newline; codex's copy is re-indented by
/// its own writer.
pub(crate) const HOOK_DECIDE_OR_DENY: &str = r#"if [ "$rc" -eq 0 ]; then
  case "$resp" in
    '{'*'}')
      case "$resp" in
        *'"permissionDecision":"allow"'*|*'"permissionDecision":"deny"'*)
          printf '%s' "$resp"
          exit 0
          ;;
      esac
      ;;
  esac
fi
if [ "$rc" -eq 0 ]; then
  reason="weft's permission bridge answered without a usable decision, so this tool call was not reviewed by a human and is denied. / weft 的授权桥没有返回可用的决定，这次工具调用未经人工确认，已拒绝。"
elif [ "$rc" -eq 22 ]; then
  reason="weft's permission bridge answered with an error status, so this tool call was not reviewed by a human and is denied. / weft 的授权桥返回了错误状态，这次工具调用未经人工确认，已拒绝。"
else
  reason="weft could not be reached (or the answer was cut off), so nobody can approve this tool call - denied by default. Start weft again (or check that its local ask bridge is up), then retry. / 无法连接 weft（或回答被中断），没人能批准这次工具调用，已按默认拒绝。请重新启动 weft（或确认本地授权桥仍在监听）后重试。"
fi
printf '{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":"%s"}}' "$reason"
exit 0"#;

/// Install the Ask Bridge for one exact persisted session. Worker URLs carry
/// `session_id`; lead-family callers pass `None` explicitly. Claude's worker
/// script/settings filenames are session-specific so a later session sharing
/// the worktree cannot rewrite the URL an already-running process launched
/// with. Codex writes a worktree route file consumed by Weft's stable global
/// hook; OpenCode bridges via its server plugin. ACP tools use
/// `session/request_permission` instead. Best-effort: empty injection on write
/// failure.
pub fn inject_ask_hook(
    base: &str,
    thread: i32,
    dir: &str,
    session_id: Option<i32>,
    tool: &str,
    cwd: &Path,
) -> Injection {
    if crate::acp::backend_for(tool).is_some() {
        return Injection {
            args: vec![],
            env: vec![],
        };
    }
    if tool == "opencode" {
        return inject_opencode_ask_plugin(base, thread, dir, session_id, cwd);
    }
    if tool != "claude" && tool != "codex" {
        return Injection {
            args: vec![],
            env: vec![],
        };
    }
    let url = ask_url(base, thread, dir, session_id, tool);
    if tool == "codex" {
        let route = cwd.join(".weft-codex-ask-url");
        if std::fs::write(&route, &url).is_err() {
            return Injection {
                args: vec![],
                env: vec![],
            };
        }
        crate::git::git_exclude(cwd, ".weft-codex-ask-url");
        return Injection {
            args: vec![],
            env: vec![],
        };
    }
    let suffix = session_id
        .map(|session_id| format!("-{session_id}"))
        .unwrap_or_default();
    let script_name = format!(".weft-ask-hook{suffix}.sh");
    let script = cwd.join(&script_name);
    // Reads the PreToolUse JSON on stdin, asks weft, echoes weft's decision JSON —
    // or, when weft doesn't answer with one, an explicit deny (fail-closed; see
    // HOOK_DECIDE_OR_DENY). -m matches the server's ASK_WAIT: hold the call until
    // the human answers in Needs-you rather than timing out into a fallback.
    let body = format!(
        "#!/usr/bin/env bash\n\
         resp=$(curl -sf -m 3600 -X POST '{url}' -H 'Content-Type: application/json' --data-binary @- 2>/dev/null)\n\
         rc=$?\n\
         {HOOK_DECIDE_OR_DENY}\n"
    );
    if std::fs::write(&script, body).is_err() {
        return Injection {
            args: vec![],
            env: vec![],
        };
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755));
    }
    crate::git::git_exclude(cwd, &script_name);

    match tool {
        "claude" => {
            let settings_name = format!(".weft-ask{suffix}.settings.json");
            let settings = cwd.join(&settings_name);
            // The matcher STAYS a wildcard on purpose. Narrowing it to exclude
            // safe tools would look like the fix for the over-asking storm, but
            // a matcher is a positive filter: a name it doesn't match is never
            // seen by the hook at all, so it runs UNGATED. Every tool name the
            // pattern-author didn't anticipate would then be ungated by
            // default. The narrowing lives in `bus::builtin_allow` instead,
            // where an unrecognized name falls through to the human.
            let json = serde_json::json!({
                "hooks": { "PreToolUse": [
                    { "matcher": "*", "hooks": [
                        { "type": "command",
                          "command": format!("bash {}", script.to_string_lossy()),
                          "timeout": 3650 }
                    ] }
                ] }
            });
            if std::fs::write(
                &settings,
                serde_json::to_vec_pretty(&json).unwrap_or_default(),
            )
            .is_err()
            {
                return Injection {
                    args: vec![],
                    env: vec![],
                };
            }
            crate::git::git_exclude(cwd, &settings_name);
            Injection {
                args: vec!["--settings".into(), settings.to_string_lossy().to_string()],
                env: vec![],
            }
        }
        // Codex now warns loudly when --dangerously-bypass-hook-trust is used.
        // Do not inject Weft's PreToolUse hook through that bypass path; Codex's
        // own sandbox/approval mode remains authoritative for exec sessions.
        "codex" => Injection {
            args: vec![],
            env: vec![],
        },
        _ => Injection {
            args: vec![],
            env: vec![],
        },
    }
}

/// OpenCode has no PreToolUse hook; its analog is a local plugin's
/// `tool.execute.before`, which is async and throws to deny. Drop a plugin in
/// the worktree's `.opencode/plugins/` that POSTs each tool action to weft's
/// /ask endpoint and throws unless weft explicitly allows — same Ask Bridge,
/// same endpoint, same allow/deny contract as claude/codex. Auto-loaded (no
/// launch flag). The plugin source lives in `weft-ask-plugin.js` (see its
/// header for the fail-closed contract) so the node test can run that exact
/// file; only `__URL__` is substituted here.
fn inject_opencode_ask_plugin(
    base: &str,
    thread: i32,
    dir: &str,
    session_id: Option<i32>,
    cwd: &Path,
) -> Injection {
    let url = ask_url(base, thread, dir, session_id, "opencode");
    let plugins = cwd.join(".opencode").join("plugins");
    if std::fs::create_dir_all(&plugins).is_err() {
        return Injection {
            args: vec![],
            env: vec![],
        };
    }
    let template = include_str!("weft-ask-plugin.js");
    let body = template.replace("__URL__", &url);
    let _ = std::fs::write(plugins.join("weft-ask.js"), body);
    crate::git::git_exclude(cwd, ".opencode/plugins/weft-ask.js");
    Injection {
        args: vec![],
        env: vec![],
    }
}

/// Build the thread-bus injection. `cwd` is the worktree (used for the claude
/// temp config and the opencode merge). `dir` is the direction id as a string;
/// workers must pass their exact session id and lead-family engines pass None.
pub fn inject(
    base: &str,
    thread: i32,
    dir: &str,
    session_id: Option<i32>,
    tool: &str,
    cwd: &Path,
) -> Injection {
    if tool == "opencode" && session_id.is_some() {
        return inject_opencode_session_bus(&mcp_url(base, thread, dir, session_id));
    }
    let stem = session_id
        .map(|session_id| format!("bus-{session_id}"))
        .unwrap_or_else(|| "bus".to_string());
    inject_mcp(
        "weft_bus",
        &stem,
        &mcp_url(base, thread, dir, session_id),
        tool,
        cwd,
    )
}

fn inject_opencode_session_bus(url: &str) -> Injection {
    let mut root = std::env::var("OPENCODE_CONFIG_CONTENT")
        .ok()
        .and_then(|content| serde_json::from_str::<serde_json::Value>(&content).ok())
        .filter(serde_json::Value::is_object)
        .unwrap_or_else(|| serde_json::json!({}));
    let Some(root_obj) = root.as_object_mut() else {
        return Injection {
            args: vec![],
            env: vec![],
        };
    };
    let mcp = root_obj
        .entry("mcp".to_string())
        .or_insert_with(|| serde_json::json!({}));
    if !mcp.is_object() {
        *mcp = serde_json::json!({});
    }
    if let Some(mcp_obj) = mcp.as_object_mut() {
        mcp_obj.insert(
            "weft_bus".to_string(),
            serde_json::json!({ "type": "remote", "url": url, "enabled": true }),
        );
    }
    Injection {
        args: vec![],
        env: vec![("OPENCODE_CONFIG_CONTENT".to_string(), root.to_string())],
    }
}

/// Build the planner-MCP injection for a lead session (read-only planning).
/// Same additive mechanism as the bus, a different server keyed to the thread.
pub fn inject_planner(base: &str, thread: i32, tool: &str, cwd: &Path) -> Injection {
    inject_mcp(
        "weft_planner",
        "planner",
        &planner_url(base, thread),
        tool,
        cwd,
    )
}

/// The curator chat's MCP (get_repo_map + calibrate_edges), per curator thread.
pub fn inject_curator(base: &str, thread: i32, tool: &str, cwd: &Path) -> Injection {
    inject_mcp(
        "weft_curator",
        "curator",
        &curator_url(base, thread),
        tool,
        cwd,
    )
}

/// Build the global-MCP injection for the Concierge engine (M3-2). Not
/// per-thread — the URL has no thread/dir in path; identity is "the global
/// helper running in IM single-chat". Same additive shape as planner.
pub fn inject_global(base: &str, tool: &str, cwd: &Path) -> Injection {
    inject_mcp("weft_global", "global", &global_url(base), tool, cwd)
}

/// Additively register one HTTP MCP `server` at `url` for `tool`, never
/// overriding the sub-repo's own config. `stem` names the claude temp config
/// file (`.weft-<stem>.mcp.json`).
fn inject_mcp(server: &str, stem: &str, url: &str, tool: &str, cwd: &Path) -> Injection {
    if crate::acp::backend_for(tool).is_some() {
        // MCP is supplied on session/new|resume, not via launch flags/files.
        return Injection {
            args: vec![],
            env: vec![],
        };
    }
    match tool {
        "claude" => {
            // ephemeral --mcp-config file inside the cwd. It's an injected,
            // untracked file, so we add it to git exclude (see git_exclude) to
            // keep it out of `git status` / diffs / commits.
            let file = format!(".weft-{stem}.mcp.json");
            let cfg = cwd.join(&file);
            let json = serde_json::json!({
                "mcpServers": { server: { "type": "http", "url": url } }
            });
            let _ = std::fs::write(&cfg, serde_json::to_vec_pretty(&json).unwrap_or_default());
            crate::git::git_exclude(cwd, &file);
            Injection {
                args: vec!["--mcp-config".into(), cfg.to_string_lossy().to_string()],
                env: vec![],
            }
        }
        "codex" => Injection {
            args: vec!["-c".into(), format!("mcp_servers.{server}.url={url}")],
            env: vec![],
        },
        "opencode" => {
            merge_opencode_config(cwd, server, url);
            Injection {
                args: vec![],
                env: vec![],
            }
        }
        _ => Injection {
            args: vec![],
            env: vec![],
        },
    }
}

/// Deep-merge `mcp.<server> = {type:remote, url, enabled:true}` into the cwd's
/// opencode.json, preserving any existing config the sub-repo shipped.
fn merge_opencode_config(cwd: &Path, server: &str, url: &str) {
    let path = cwd.join("opencode.json");
    let mut root: serde_json::Value = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    if !root.is_object() {
        root = serde_json::json!({});
    }
    // root is guaranteed an object here; guard instead of unwrap so a panic is
    // impossible even if the invariant ever changes.
    let Some(obj) = root.as_object_mut() else {
        return;
    };
    obj.entry("$schema".to_string())
        .or_insert_with(|| serde_json::json!("https://opencode.ai/config.json"));
    let mcp = obj
        .entry("mcp".to_string())
        .or_insert_with(|| serde_json::json!({}));
    if let Some(mcp_obj) = mcp.as_object_mut() {
        mcp_obj.insert(
            server.to_string(),
            serde_json::json!({ "type": "remote", "url": url, "enabled": true }),
        );
    }
    let _ = std::fs::write(&path, serde_json::to_vec_pretty(&root).unwrap_or_default());
    // Best-effort: only hides opencode.json from git when the sub-repo does NOT
    // track it. If the repo ships a tracked opencode.json, the merge still shows
    // as a modification — an accepted limitation of the worktree-local merge.
    crate::git::git_exclude(cwd, "opencode.json");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_writes_mcp_config_and_flags() {
        let dir = std::env::temp_dir().join(format!("weft-inj-claude-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let inj = inject("http://127.0.0.1:9", 1, "10", Some(41), "claude", &dir);
        assert_eq!(inj.args[0], "--mcp-config");
        let cfg = std::fs::read_to_string(dir.join(".weft-bus-41.mcp.json")).unwrap();
        assert!(cfg.contains("weft_bus") && cfg.contains("/bus/1/10/mcp?session_id=41"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn codex_uses_config_override() {
        let inj = inject(
            "http://127.0.0.1:9",
            2,
            "30",
            Some(52),
            "codex",
            Path::new("/tmp"),
        );
        assert_eq!(
            inj.args,
            vec![
                "-c".to_string(),
                "mcp_servers.weft_bus.url=http://127.0.0.1:9/bus/2/30/mcp?session_id=52"
                    .to_string()
            ]
        );
    }

    #[test]
    fn claude_worker_sessions_keep_separate_bus_configs() {
        let dir = std::env::temp_dir().join(format!("weft-inj-sessions-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let first = inject("http://127.0.0.1:9", 2, "30", Some(51), "claude", &dir);
        let second = inject("http://127.0.0.1:9", 2, "30", Some(52), "claude", &dir);

        assert_ne!(first.args[1], second.args[1]);
        let first_config = std::fs::read_to_string(&first.args[1]).unwrap();
        let second_config = std::fs::read_to_string(&second.args[1]).unwrap();
        assert!(first_config.contains("session_id=51"));
        assert!(second_config.contains("session_id=52"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn planner_claude_writes_its_own_config() {
        let dir = std::env::temp_dir().join(format!("weft-inj-plan-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let inj = inject_planner("http://127.0.0.1:9", 7, "claude", &dir);
        assert_eq!(inj.args[0], "--mcp-config");
        let cfg = std::fs::read_to_string(dir.join(".weft-planner.mcp.json")).unwrap();
        assert!(cfg.contains("weft_planner") && cfg.contains("/planner/7/mcp"));
        // the bus config is a SEPARATE file — planner doesn't clobber it
        assert_ne!(
            inj.args[1],
            dir.join(".weft-bus.mcp.json").to_string_lossy()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn claude_ask_hook_wires_pretooluse_settings() {
        let dir = std::env::temp_dir().join(format!("weft-askh-c-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let inj = inject_ask_hook(
            "http://127.0.0.1:9",
            1,
            "10",
            Some(41),
            "claude",
            &dir,
        );
        assert_eq!(inj.args[0], "--settings");
        let script = std::fs::read_to_string(dir.join(".weft-ask-hook-41.sh")).unwrap();
        assert!(script.contains("/ask/1/10?tool=claude&session_id=41"));
        let settings: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(dir.join(".weft-ask-41.settings.json")).unwrap(),
        )
        .unwrap();
        assert!(settings["hooks"]["PreToolUse"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap()
            .contains(".weft-ask-hook-41.sh"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn worker_ask_hook_bakes_the_exact_session_into_its_url() {
        let dir = std::env::temp_dir().join(format!(
            "weft-askh-worker-session-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let _ = inject_ask_hook(
            "http://127.0.0.1:9",
            1,
            "10",
            Some(41),
            "claude",
            &dir,
        );
        let script = std::fs::read_to_string(dir.join(".weft-ask-hook-41.sh")).unwrap();
        assert!(script.contains("/ask/1/10?tool=claude&session_id=41"));
        let settings = std::fs::read_to_string(dir.join(".weft-ask-41.settings.json")).unwrap();
        assert!(settings.contains(".weft-ask-hook-41.sh"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn lead_ask_hook_url_explicitly_has_no_worker_session() {
        let dir = std::env::temp_dir().join(format!("weft-askh-lead-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let _ = inject_ask_hook(
            "http://127.0.0.1:9",
            1,
            crate::bus::LEAD,
            None,
            "claude",
            &dir,
        );
        let script = std::fs::read_to_string(dir.join(".weft-ask-hook.sh")).unwrap();
        assert!(script.contains("/ask/1/lead?tool=claude"));
        assert!(!script.contains("session_id="));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn codex_ask_hook_writes_worktree_route_without_launch_bypass() {
        let dir = std::env::temp_dir().join(format!("weft-askh-x-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let inj = inject_ask_hook(
            "http://127.0.0.1:9",
            2,
            "30",
            Some(52),
            "codex",
            &dir,
        );
        assert!(
            inj.args.is_empty(),
            "global trusted hook needs no launch args"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join(".weft-codex-ask-url")).unwrap(),
            "http://127.0.0.1:9/ask/2/30?tool=codex&session_id=52"
        );
        assert!(!dir.join(".weft-ask-hook.sh").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn opencode_ask_plugin_written_and_excluded() {
        let dir = std::env::temp_dir().join(format!("weft-inj-oask-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let inj = inject_ask_hook(
            "http://127.0.0.1:9",
            1,
            "10",
            Some(41),
            "opencode",
            &dir,
        );
        assert!(
            inj.args.is_empty(),
            "opencode plugin auto-loads, no launch flag"
        );
        let plugin = std::fs::read_to_string(dir.join(".opencode/plugins/weft-ask.js")).unwrap();
        assert!(plugin.contains("tool.execute.before"));
        assert!(plugin.contains("/ask/1/10?tool=opencode&session_id=41"));
        assert!(plugin.contains("Denied in weft"));
        // The URL placeholder must be fully substituted — an unsubstituted
        // template would POST to a literal "__URL__" and (now) deny everything.
        assert!(
            !plugin.contains("__URL__"),
            "placeholder must be substituted:\n{plugin}"
        );
        // Fail-closed shape: an unreachable bridge is a named verdict with a
        // message, not a swallowed exception. The BEHAVIOR of each verdict is
        // covered by tests/frontend/opencodeAskPlugin.test.ts, which runs this
        // exact file; these asserts only keep the wiring from silently reverting.
        assert!(
            plugin.contains("unreachable") && plugin.contains("undecided"),
            "plugin must name the non-allow verdicts:\n{plugin}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    const HOOK_PAYLOAD: &str = r#"{"tool_name":"Bash","tool_input":{"command":"rm -rf /"}}"#;

    /// Both hook tests below are bounded by the shared runner, which KILLS the
    /// script at the deadline (see `hook_test_support::run_hook_script`).
    #[cfg(unix)]
    const HOOK_LIMIT: std::time::Duration = std::time::Duration::from_secs(60);

    /// The Ask Bridge is the ONLY thing that surfaces a tool call to the human,
    /// so a weft that can't be reached (app quit/crashed, port moved) must DENY,
    /// not fall through. Before this, the script printed nothing and exited 0,
    /// which claude treats as "no decision → normal permission flow" (a user
    /// allowlist can auto-approve) — an unsupervised agent ran unreviewed.
    #[tokio::test]
    #[cfg(unix)]
    async fn claude_ask_hook_denies_when_weft_is_unreachable() {
        use crate::hook_test_support::{closed_port, decision_of, run_hook_script};
        let dir = std::env::temp_dir().join(format!("weft-askh-down-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let base = format!("http://127.0.0.1:{}", closed_port());
        let inj = inject_ask_hook(&base, 1, crate::bus::LEAD, None, "claude", &dir);
        assert_eq!(inj.args[0], "--settings");

        let (stdout, code) = run_hook_script(
            &dir.join(".weft-ask-hook.sh"),
            &dir,
            HOOK_PAYLOAD,
            HOOK_LIMIT,
        )
        .await;
        let out = decision_of(&stdout, code);
        assert_eq!(
            out["permissionDecision"], "deny",
            "unreachable weft must fail CLOSED, not fall through: {out}"
        );
        assert!(
            out["permissionDecisionReason"]
                .as_str()
                .unwrap_or_default()
                .contains("could not be reached"),
            "the reason must tell the human weft is down: {out}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Review round 2 (P1): a decision-shaped body arriving with an ERROR status
    /// must not be forwarded. `-f` makes that a failed transfer, so the hook denies
    /// instead of handing the engine a body it would ignore (→ continue).
    #[tokio::test]
    #[cfg(unix)]
    async fn claude_ask_hook_denies_a_decision_shaped_error_response() {
        use crate::hook_test_support::{
            decision_body, decision_of, run_hook_script, serve_raw_once,
        };
        let body = decision_body("allow", false);
        let len = body.len();
        let base = serve_raw_once("HTTP/1.1 500 Internal Server Error", body, len).await;

        let dir = std::env::temp_dir().join(format!("weft-askh-500-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let _ = inject_ask_hook(&base, 1, crate::bus::LEAD, None, "claude", &dir);

        let (stdout, code) = run_hook_script(
            &dir.join(".weft-ask-hook.sh"),
            &dir,
            HOOK_PAYLOAD,
            HOOK_LIMIT,
        )
        .await;
        let out = decision_of(&stdout, code);
        assert_eq!(
            out["permissionDecision"], "deny",
            "an allow carried by a 5xx must not pass through: {out}"
        );
        assert!(
            out["permissionDecisionReason"]
                .as_str()
                .unwrap_or_default()
                .contains("error status"),
            "the reason must name the error status: {out}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Review round 2 (P1): the verdict pair sits BEFORE the reason field, so a
    /// transfer cut right after it leaves a body that matches the gate but is
    /// invalid JSON — which the engine would parse-fail and then CONTINUE on. The
    /// `$rc` gate (curl exit 18) is what has to catch this.
    #[tokio::test]
    #[cfg(unix)]
    async fn claude_ask_hook_denies_an_answer_cut_off_after_the_verdict() {
        use crate::hook_test_support::{
            decision_body, decision_of, run_hook_script, serve_raw_once,
        };
        let cut = decision_body("allow", true);
        // The fixture must defeat every gate EXCEPT `$rc`, or this test would pass
        // for the wrong reason.
        assert!(
            cut.contains("\"permissionDecision\":\"allow\"")
                && cut.starts_with('{')
                && cut.ends_with('}'),
            "fixture must slip past the shape and verdict gates: {cut}"
        );
        // Promise more than we send → curl reports a partial transfer.
        let base = serve_raw_once("HTTP/1.1 200 OK", cut, 4096).await;

        let dir = std::env::temp_dir().join(format!("weft-askh-cut-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let _ = inject_ask_hook(&base, 1, crate::bus::LEAD, None, "claude", &dir);

        let (stdout, code) = run_hook_script(
            &dir.join(".weft-ask-hook.sh"),
            &dir,
            HOOK_PAYLOAD,
            HOOK_LIMIT,
        )
        .await;
        let out = decision_of(&stdout, code);
        assert_eq!(
            out["permissionDecision"], "deny",
            "a truncated allow must not pass through: {out}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The other side of the same seam: when weft IS reachable, the human's real
    /// answer must reach the engine unchanged. Without this, "deny everything"
    /// would pass the test above.
    #[tokio::test]
    #[cfg(unix)]
    async fn claude_ask_hook_passes_a_real_weft_decision_through() {
        use crate::ask::{Answer, AskRegistry};
        use crate::hook_test_support::{answer_first_ask, decision_of, run_hook_script};
        let asks = AskRegistry::new();
        let db = crate::store::Db::connect("sqlite::memory:").await.unwrap();
        let workspace = crate::store::repo::create_workspace(&db, "hook pass through")
            .await
            .unwrap();
        let thread = crate::store::repo::create_thread(
            &db,
            workspace.id,
            "hook pass through",
            "feature",
            "claude",
        )
        .await
        .unwrap();
        let (base, _h) =
            crate::bus::server::serve(crate::bus::BusRegistry::new(), db, asks.clone())
                .await
                .unwrap();

        let dir = std::env::temp_dir().join(format!("weft-askh-live-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let _ = inject_ask_hook(
            &base,
            thread.id,
            crate::bus::LEAD,
            None,
            "claude",
            &dir,
        );

        // One task, two concurrent futures: the hook runs while the "human"
        // answers. No detached task to outlive the test, and the runner kills the
        // script if it somehow never exits.
        let script = dir.join(".weft-ask-hook.sh");
        let ((stdout, code), ()) = tokio::join!(
            run_hook_script(&script, &dir, HOOK_PAYLOAD, HOOK_LIMIT),
            answer_first_ask(&asks, Answer::Allow),
        );
        let out = decision_of(&stdout, code);
        assert_eq!(
            out["permissionDecision"], "allow",
            "a human's Allow must pass through untouched: {out}"
        );
        assert_eq!(out["permissionDecisionReason"], "Approved in weft");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn planner_codex_override_targets_planner_server() {
        let inj = inject_planner("http://127.0.0.1:9", 3, "codex", Path::new("/tmp"));
        assert_eq!(
            inj.args,
            vec![
                "-c".to_string(),
                "mcp_servers.weft_planner.url=http://127.0.0.1:9/planner/3/mcp".to_string()
            ]
        );
    }

    #[test]
    fn opencode_merges_preserving_existing() {
        let dir = std::env::temp_dir().join(format!("weft-inj-oc-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        // sub-repo already ships an opencode.json with its own mcp server
        std::fs::write(
            dir.join("opencode.json"),
            r#"{"mcp":{"repo_own":{"type":"local","command":["x"]}}}"#,
        )
        .unwrap();
        let inj = inject("http://127.0.0.1:9", 1, "lead", None, "opencode", &dir);
        assert!(inj.args.is_empty());
        let merged: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("opencode.json")).unwrap())
                .unwrap();
        // both the repo's server AND weft_bus must be present
        assert!(
            merged["mcp"]["repo_own"].is_object(),
            "repo's own server preserved"
        );
        assert_eq!(merged["mcp"]["weft_bus"]["type"], "remote");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn opencode_worker_uses_session_scoped_inline_config() {
        let dir = std::env::temp_dir().join(format!("weft-inj-oc-session-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("opencode.json"),
            r#"{"mcp":{"repo_own":{"type":"local","command":["x"]}}}"#,
        )
        .unwrap();

        let inj = inject("http://127.0.0.1:9", 7, "19", Some(63), "opencode", &dir);

        assert!(inj.args.is_empty());
        assert_eq!(inj.env.len(), 1);
        assert_eq!(inj.env[0].0, "OPENCODE_CONFIG_CONTENT");
        let inline: serde_json::Value = serde_json::from_str(&inj.env[0].1).unwrap();
        assert_eq!(
            inline["mcp"]["weft_bus"]["url"],
            "http://127.0.0.1:9/bus/7/19/mcp?session_id=63"
        );
        let project = std::fs::read_to_string(dir.join("opencode.json")).unwrap();
        assert!(project.contains("repo_own"));
        assert!(!project.contains("weft_bus"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn injected_file_is_git_excluded() {
        use std::process::Command;
        let root = std::env::temp_dir().join(format!("weft-inj-git-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let repo = root.join("repo");
        let wt = root.join("wt");
        std::fs::create_dir_all(&repo).unwrap();
        let sh = |dir: &Path, args: &[&str]| {
            assert!(Command::new(args[0])
                .args(&args[1..])
                .current_dir(dir)
                .status()
                .unwrap()
                .success());
        };
        sh(&repo, &["git", "init", "-q"]);
        sh(&repo, &["git", "config", "user.email", "t@t.t"]);
        sh(&repo, &["git", "config", "user.name", "t"]);
        std::fs::write(repo.join("README.md"), "x\n").unwrap();
        sh(&repo, &["git", "add", "-A"]);
        sh(&repo, &["git", "commit", "-q", "-m", "init"]);
        sh(
            &repo,
            &["git", "worktree", "add", "-q", wt.to_str().unwrap()],
        );

        let _ = inject("http://127.0.0.1:9", 1, "1", Some(74), "claude", &wt);
        assert!(wt.join(".weft-bus-74.mcp.json").exists(), "file written");
        let status = Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(&wt)
            .output()
            .unwrap();
        let s = String::from_utf8_lossy(&status.stdout);
        assert!(
            !s.contains(".weft-bus-74.mcp.json"),
            "injected file must be git-excluded, got: {s}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
