//! Spawn-time, ADDITIVE injection of the thread bus as an MCP server for each
//! tool. Never overrides a sub-repo's own config: claude/codex use file-less
//! launch flags; opencode deep-merges into the worktree opencode.json (which is
//! a throwaway worktree, not the canonical repo — architecture §2.1).

use std::path::Path;

/// Extra args to PREPEND to the tool's own args (global flags must precede any
/// subcommand, e.g. `codex -c k=v resume <id>`).
pub struct Injection {
    pub args: Vec<String>,
    /// Environment variables the spawn site must set on the tool's OWN child
    /// process. Three producers today:
    ///  - the codex computer-use injection: the per-session bearer must not
    ///    ride in `-c` argv (world-readable via process listings on Linux) —
    ///    the `-c` flag names only the VARIABLE, and the secret rides here
    ///    instead (a child's environment is readable only by its own uid);
    ///  - OpenCode's documented OPENCODE_CONFIG_CONTENT (per-session bus
    ///    injection, see `inject_opencode_session_bus`), so concurrent
    ///    sessions in one worktree never overwrite a shared opencode.json bus
    ///    URL;
    ///  - the OpenCode computer-use injection (see the `"opencode"` arm of
    ///    [`inject_computer`]), which carries its per-session bearer through
    ///    the SAME OPENCODE_CONFIG_CONTENT variable instead of the worktree's
    ///    `opencode.json` file.
    /// Every other injection leaves this empty.
    ///
    /// Because TWO producers can both emit an `OPENCODE_CONFIG_CONTENT` entry
    /// for one spawn (the session bus and the computer server), a spawn site
    /// that accumulates several injections' `env` lists MUST pass the result
    /// through [`coalesce_env`] before handing it to the child —
    /// `Command::envs` applies duplicate keys last-wins, which would silently
    /// drop the earlier server's config.
    pub env: Vec<(String, String)>,
}

/// The one environment variable two different injections can both target —
/// OpenCode's documented inline-config channel, read additively on top of
/// any file-based config.
const OPENCODE_CONFIG_CONTENT_VAR: &str = "OPENCODE_CONFIG_CONTENT";

/// Deep-merge `overlay` into `base`: objects merge key-by-key recursively,
/// anything else is replaced by the overlay's value. Only used by
/// [`coalesce_env`] on OPENCODE_CONFIG_CONTENT payloads, where the two
/// producers' configs are disjoint except for the shared `"mcp"` object that
/// must UNION (`weft_bus` + `weft_computer`), not clobber.
fn deep_merge_json(base: &mut serde_json::Value, overlay: serde_json::Value) {
    match (base, overlay) {
        (serde_json::Value::Object(base_map), serde_json::Value::Object(overlay_map)) => {
            for (key, value) in overlay_map {
                match base_map.get_mut(&key) {
                    Some(slot) => deep_merge_json(slot, value),
                    None => {
                        base_map.insert(key, value);
                    }
                }
            }
        }
        (slot, value) => *slot = value,
    }
}

/// Collapse duplicate `OPENCODE_CONFIG_CONTENT` entries in an accumulated
/// injection-env list into ONE deep-merged entry — see [`Injection`]'s `env`
/// doc for why every spawn site must call this.
/// Later entries merge ON TOP of earlier ones, preserving the same
/// precedence `Command::envs`' last-wins would give whole values. If any
/// entry is not a JSON object (nothing this module produces — defensive
/// only), merging is impossible and the list falls back to plain last-wins
/// for that variable. Every other variable passes through untouched, in
/// order.
pub fn coalesce_env(env: Vec<(String, String)>) -> Vec<(String, String)> {
    let (mut out, opencode): (Vec<_>, Vec<_>) =
        env.into_iter().partition(|(key, _)| key != OPENCODE_CONFIG_CONTENT_VAR);
    if opencode.len() <= 1 {
        out.extend(opencode);
        return out;
    }
    let mut parsed = Vec::with_capacity(opencode.len());
    for (_, value) in &opencode {
        let object = serde_json::from_str::<serde_json::Value>(value)
            .ok()
            .filter(serde_json::Value::is_object);
        match object {
            Some(object) => parsed.push(object),
            None => {
                // Unmergeable payload: keep plain env semantics (last wins).
                if let Some(last) = opencode.into_iter().next_back() {
                    out.push(last);
                }
                return out;
            }
        }
    }
    let mut iter = parsed.into_iter();
    let Some(mut merged) = iter.next() else {
        return out;
    };
    for overlay in iter {
        deep_merge_json(&mut merged, overlay);
    }
    out.push((OPENCODE_CONFIG_CONTENT_VAR.to_string(), merged.to_string()));
    out
}

impl Injection {
    /// The empty injection — no args, no env. The overwhelmingly common
    /// fallback shape in this module.
    fn none() -> Injection {
        Injection { args: vec![], env: vec![] }
    }

    /// An args-only injection (no env) — every producer that carries no env
    /// entry (most of this module).
    fn args_only(args: Vec<String>) -> Injection {
        Injection { args, env: vec![] }
    }
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

/// `wt`: the caller's own worktree id, when it
/// could resolve one — appended as `?wt=<id>` so `bus::computer_srv::
/// handle_computer` can pin screenshots/audit to THIS EXACT worktree instead
/// of falling back to "the first worktree of this direction" on a multi-repo
/// direction (see `bus::computer_srv::session_root`'s own doc for the
/// closed-set validation this query param goes through server-side). `None`
/// omits the query param entirely — an absent `wt` behaves EXACTLY like the
/// pre-existing URL shape, so every caller that can't resolve a worktree id
/// (the lead lane, which has no worktree at all; an ACP branch that hasn't
/// wired one through yet) is unaffected.
///
/// `&key=<token>`: EVERY `weft_computer` URL now
/// also carries this session-scoped bearer — `bus::computer_srv::
/// computer_session_token(thread, dir, wt)`, an HMAC of the path's own
/// identity AND the exact worktree it carries,
/// under a process-lifetime secret that never leaves memory (see that
/// function's own doc for the full rationale). This is the ONLY bus MCP URL
/// that gets one: `mcp_url`/`planner_url`/`curator_url`/`global_url` above are
/// all unauthenticated by design (see `bus::server`'s own top-of-file doc
/// comment on that tradeoff) — `/computer` alone can capture the screen and
/// inject input, so it alone needs a caller-side credential on top of the
/// path-derived identity every OTHER bus endpoint relies on. Appended with
/// `&` when `?wt=` is already present, `?` otherwise — still exactly ONE
/// query string, never two separately-prefixed ones.
fn computer_url(base: &str, thread: i32, dir: &str, wt: Option<i32>) -> String {
    // the bearer is minted for the EXACT `wt` this
    // URL embeds (see `computer_srv::computer_token_mac`), so a worker that
    // later swaps its own `?wt=` to a sibling's id presents a token that no
    // longer matches and is rejected server-side. Current-generation render —
    // the INJECTION paths never call this; each carries the token its own
    // atomic rotate-and-mint produced (see `computer_url_with_key`).
    let key = crate::bus::computer_srv::computer_session_token(thread, dir, wt);
    computer_url_with_key(base, thread, dir, wt, &key)
}

/// [`computer_url`] with an explicit, already-minted bearer — the shape the
/// injection paths use so the URL carries EXACTLY the token their own atomic
/// rotate-and-mint produced, never a separately re-read one: two overlapping
/// injections that each rotated and THEN re-read would both render the
/// LATEST generation, keeping the stale child's bearer alive alongside the
/// replacement's.
fn computer_url_with_key(base: &str, thread: i32, dir: &str, wt: Option<i32>, key: &str) -> String {
    match wt {
        Some(id) => format!("{base}/computer/{thread}/{dir}/mcp?wt={id}&key={key}"),
        None => format!("{base}/computer/{thread}/{dir}/mcp?key={key}"),
    }
}

/// HTTP MCP servers Weft should pass on ACP `session/new|resume` for this
/// engine role. Workers get `weft_bus` with their exact persisted session id;
/// lead also gets planner when `dir` is the lead lane; concierge/global callers
/// pass `include_global`.
///
/// `computer_wt`: the worker's own worktree id,
/// forwarded into [`computer_url`] when `include_computer` is set — see that
/// function's own doc. Ignored (harmlessly) when `include_computer` is
/// `false`. `None` for the lead lane (a lead has no worktree at all) and for
/// any worker caller that couldn't resolve one — both fall back to the
/// pre-existing unpinned URL shape.
pub fn acp_mcp_servers(
    base: &str,
    thread: i32,
    dir: &str,
    session_id: Option<i32>,
    include_bus: bool,
    include_planner: bool,
    include_global: bool,
    include_curator: bool,
    include_computer: bool,
    computer_wt: Option<i32>,
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
    if include_computer {
        // An ACP session establishment replaces this identity's previous
        // child — the ATOMIC rotate-and-mint invalidates every bearer issued
        // to the predecessor and pins THIS injection's URL to its own bump
        // (see `computer_srv::rotate_and_mint_computer_session_token`), same
        // as `inject_computer` does for the non-ACP engines.
        let key =
            crate::bus::computer_srv::rotate_and_mint_computer_session_token(thread, dir, computer_wt);
        out.push(crate::acp::McpServerSpec {
            name: "weft_computer".into(),
            url: computer_url_with_key(base, thread, dir, computer_wt, &key),
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
        return Injection::none();
    }
    if tool == "opencode" {
        return inject_opencode_ask_plugin(base, thread, dir, session_id, cwd);
    }
    if tool != "claude" && tool != "codex" {
        return Injection::none();
    }
    let url = ask_url(base, thread, dir, session_id, tool);
    if tool == "codex" {
        let route = cwd.join(".weft-codex-ask-url");
        if std::fs::write(&route, &url).is_err() {
            return Injection::none();
        }
        crate::git::git_exclude(cwd, ".weft-codex-ask-url");
        return Injection::none();
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
        return Injection::none();
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
                return Injection::none();
            }
            crate::git::git_exclude(cwd, &settings_name);
            Injection::args_only(vec!["--settings".into(), settings.to_string_lossy().to_string()])
        }
        // Codex now warns loudly when --dangerously-bypass-hook-trust is used.
        // Do not inject Weft's PreToolUse hook through that bypass path; Codex's
        // own sandbox/approval mode remains authoritative for exec sessions.
        "codex" => Injection::none(),
        _ => Injection::none(),
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
        return Injection::none();
    }
    let template = include_str!("weft-ask-plugin.js");
    let body = template.replace("__URL__", &url);
    let _ = std::fs::write(plugins.join("weft-ask.js"), body);
    crate::git::git_exclude(cwd, ".opencode/plugins/weft-ask.js");
    Injection::none()
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
    opencode_env_config_injection("weft_bus", url)
}

/// Register one HTTP MCP `server` for an OpenCode session via the
/// OPENCODE_CONFIG_CONTENT environment channel — the shared builder behind
/// [`inject_opencode_session_bus`] and the `"opencode"` arm of
/// [`inject_computer`]. Layers on top of any
/// OPENCODE_CONFIG_CONTENT already present in WEFT's OWN process
/// environment (a user-provided base config is preserved, not clobbered);
/// when several injections for ONE spawn each produce an entry, the spawn
/// site's [`coalesce_env`] pass deep-merges them into one.
fn opencode_env_config_injection(server: &str, url: &str) -> Injection {
    let mut root = std::env::var(OPENCODE_CONFIG_CONTENT_VAR)
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
            server.to_string(),
            serde_json::json!({ "type": "remote", "url": url, "enabled": true }),
        );
    }
    Injection {
        args: vec![],
        env: vec![(OPENCODE_CONFIG_CONTENT_VAR.to_string(), root.to_string())],
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

/// Build the computer-use MCP injection for a session, per
/// thread/direction. Unlike every
/// OTHER injected server (`weft_bus`/`weft_planner`/`weft_curator`/
/// `weft_global`, all unauthenticated by design — see their own URL
/// builders' doc comments), this URL embeds a per-session bearer
/// (`computer_url`'s own `&key=`), so this does NOT go through the shared,
/// generic [`inject_mcp`] the others use — that helper writes claude's
/// config INSIDE the worktree and opencode's merge into the worktree's own
/// (possibly git-tracked) `opencode.json`, neither of which this endpoint's
/// token can safely land in. See [`inject_computer_claude`]'s own doc for
/// Claude's dedicated, token-safe file path; OpenCode's arm below routes the
/// token through the OPENCODE_CONFIG_CONTENT environment channel instead of
/// ANY file (see that arm's own comment). Codex needs neither: its own
/// injection is already a bare `-c` CLI flag with no file at all (unchanged,
/// same as every other codex injection in this module).
///
/// Callers MUST gate this on `crate::computer::enabled(db)` themselves — this
/// function injects unconditionally. as of this
/// round every production call site injects UNCONDITIONALLY instead
/// (concierge/curator excluded) — the setting is enforced server-side, on
/// every call, by `bus::computer_srv::run_action`'s own `computer::enabled`
/// gate; this function's own behavior (inject regardless) hasn't changed,
/// only who calls it.
///
/// `wt`: the calling worker's own worktree id —
/// see [`computer_url`]'s doc. Every worker call site can resolve this (its
/// own materialized worktree row is already in scope where it calls this
/// function); the lead call site always passes `None` (a lead has no
/// worktree — it runs out of its own scratch cwd).
///
/// No `cwd` parameter — dropped deliberately, not incidentally: no arm of
/// this function touches the session's checkout at all (claude's token file
/// lives under Weft's own state dir, codex and opencode ride argv/env), and
/// the signature enforces that a future arm can't quietly reintroduce a
/// worktree write.
pub fn inject_computer(base: &str, thread: i32, dir: &str, tool: &str, wt: Option<i32>) -> Injection {
    if crate::acp::backend_for(tool).is_some() {
        // MCP is supplied on session/new|resume, not via launch flags/files —
        // same rule `inject_mcp` applies for every other server.
        return Injection::none();
    }
    // This injection belongs to a NEW child for `(thread, dir, wt)` — a
    // spawn, a rerun, a resume under a new persisted session, or an engine
    // switch. The ATOMIC rotate-and-mint renders the replacement's token
    // under the generation its own bump produced, which invalidates every
    // token issued to the child it replaces (a bare 401 from then on) and —
    // because bump and render share one critical section — can never hand
    // two overlapping injections the same latest-generation bearer (see
    // `computer_srv::rotate_and_mint_computer_session_token`).
    let key = crate::bus::computer_srv::rotate_and_mint_computer_session_token(thread, dir, wt);
    let url = computer_url_with_key(base, thread, dir, wt, &key);
    match tool {
        "claude" => inject_computer_claude(thread, dir, wt, &url),
        // codex is the ONE tool
        // whose injection rides argv (`-c` flags), and argv is world-readable
        // through process listings on Linux — so the bearer must NOT be part
        // of the URL here the way it is for every file-carried config above.
        // Instead: the URL goes in WITHOUT `key=`, a second `-c` names the
        // ENVIRONMENT VARIABLE codex should read the bearer from
        // (`bearer_token_env_var` — codex's own streamable-HTTP MCP auth
        // field; codex sends it as `Authorization: Bearer <value>`, which
        // `bus::computer_srv::handle_computer` accepts as a full equivalent
        // of `?key=`), and the token itself travels via `Injection::env`,
        // which the spawn site sets on the codex child process ONLY — a
        // child's environment is readable by its own uid alone (the accepted
        // same-uid residual), never via `ps` from another account. The
        // `?wt=` pin stays in the URL: a worktree id is not a secret.
        "codex" => {
            let no_key_url = match wt {
                Some(id) => format!("{base}/computer/{thread}/{dir}/mcp?wt={id}"),
                None => format!("{base}/computer/{thread}/{dir}/mcp"),
            };
            Injection {
                args: vec![
                    "-c".into(),
                    format!("mcp_servers.weft_computer.url={no_key_url}"),
                    "-c".into(),
                    format!("mcp_servers.weft_computer.bearer_token_env_var={COMPUTER_TOKEN_ENV_VAR}"),
                ],
                // The SAME token this injection's atomic rotate-and-mint
                // produced above — never a separate re-read, which could
                // render a LATER overlapping injection's generation.
                env: vec![(COMPUTER_TOKEN_ENV_VAR.to_string(), key.clone())],
            }
        }
        // The bearer-carrying URL rides the OPENCODE_CONFIG_CONTENT
        // environment channel — the SAME mechanism the per-session bus
        // already uses — and never touches `opencode.json` at all. A file
        // merge would leave the token in an UNIGNORED worktree file whenever
        // the cwd is a LINKED worktree (`merge_opencode_config` rightly
        // skips `git_exclude` there — a linked worktree's exclude file is
        // the CANONICAL repo's), where any broad `git add` sweep would
        // commit — and potentially publish — a live credential. The env
        // channel closes that entirely (nothing on disk to sweep), and with
        // it the tracked-`opencode.json` hazard — so OpenCode sessions whose
        // tracked status cannot be proven (a lead's non-repo scratch cwd, a
        // checkout where git can't run) get the computer tool instead of a
        // silent withhold. Same accepted same-uid residual as every
        // env-carried secret (`Injection::env`'s codex bearer note).
        "opencode" => opencode_env_config_injection("weft_computer", &url),
        _ => Injection::none(),
    }
}

/// The environment variable name the codex computer-use injection routes the
/// per-session bearer through — see the codex arm of
/// [`inject_computer`]. `#[doc(hidden)] pub` for the same cross-crate-test
/// reason as `computer_srv::computer_session_token`.
#[doc(hidden)]
pub const COMPUTER_TOKEN_ENV_VAR: &str = "WEFT_COMPUTER_MCP_TOKEN";

/// Write `bytes` to `path` as an ATOMICALLY-created, owner-only file:
/// a bare `std::fs::write`
/// under the common `022` umask creates the file `0644` FIRST, and only
/// narrows it to `0600` a moment later via a SEPARATE `set_permissions`
/// call — any other local account that can reach the path in that gap (the
/// exact shared/traversable-machine threat model [`inject_computer_claude`]'s
/// own doc already targets) can read a bearer token straight off disk before
/// the chmod ever lands. The one token-bearing config write in this module
/// ([`inject_computer_claude`]'s Claude `.mcp.json`) goes through this
/// instead of write-then-chmod (OpenCode's token never touches disk at all —
/// see [`opencode_env_config_injection`]).
///
/// `#[cfg(unix)]`: writes to a fresh temp SIBLING opened with `create_new`
/// (O_EXCL) + `O_NOFOLLOW` + mode `0o600` in ONE syscall — never observably
/// `0644`, not even for an instant — then, only after the FULL contents are
/// written and flushed, atomically `rename`s that temp over `path`. This
/// mirrors `computer::screenshot_window`'s own owner-only write and
/// `lead_chat::engine`'s `write_attachment_no_follow` for the "never readable
/// at any wider mode, ever" guarantee, and adds crash/failure atomicity on
/// top.
///
/// the previous version unlinked `path` FIRST and then
/// `create_new`+wrote in place, so a write that failed partway (full disk,
/// quota, I/O error) left the caller's ORIGINAL config permanently destroyed
/// — catastrophic for `merge_opencode_config`, which merges the USER's
/// existing `opencode.json` and writes it back. The temp-then-rename order
/// fixes that: `path` is only ever replaced by a temp that already holds the
/// complete bytes, so any failure before the rename leaves the original
/// untouched, and the rename itself is atomic (a reader sees either the old
/// complete file or the new complete file, never a truncated one). The temp
/// name carries the pid so two processes can't collide on it; a stale temp
/// from a crashed run is removed best-effort first. `rename` replaces the
/// directory entry at `path` (never writes THROUGH a symlink sitting there),
/// so the no-follow guarantee holds for the final path too.
///
/// Windows gets an owner-only
/// path too now — [`set_owner_only_windows`] creates the temp file, stamps it
/// with a PROTECTED DACL that grants ONLY the current user's SID (breaking
/// ACL inheritance from the — possibly world-traversable — checkout directory)
/// BEFORE the bearer bytes are written, then atomically renames it into place.
/// It is fail-CLOSED: if any ACL step fails it writes nothing and returns
/// `false`, so the caller injects nothing rather than leaving the credential
/// under an inherited, other-account-readable ACL. Any other non-unix target
/// keeps the pre-existing plain `write` (no owner-only concept this crate can
/// portably act on there). Returns whether the write actually landed, so
/// callers keep their existing best-effort `Injection::none()` fallback on
/// failure.
fn write_owner_only_atomic(path: &Path, bytes: &[u8]) -> bool {
    #[cfg(unix)]
    {
        use std::io::Write as _;
        use std::os::unix::fs::OpenOptionsExt;
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            return false;
        };
        // a PID-only temp name is
        // NOT unique WITHIN one process. Two concurrent constructors injecting
        // the same session's config would derive the identical temp path, and
        // the best-effort `remove_file` below would let the second writer unlink
        // the first writer's still-open temp — after which the first writer's
        // `rename` could publish the second writer's still-buffered file,
        // yielding a partial config. A monotonic per-write counter makes every
        // temp name distinct, so each writer only ever touches its OWN temp.
        let seq = {
            use std::sync::atomic::{AtomicU64, Ordering};
            static TMP_SEQ: AtomicU64 = AtomicU64::new(0);
            TMP_SEQ.fetch_add(1, Ordering::Relaxed)
        };
        let tmp = path.with_file_name(format!(".{name}.{}.{seq}.weft-tmp", std::process::id()));
        let _ = std::fs::remove_file(&tmp); // best-effort: clear a stale temp from a crashed run
        let mut opt = std::fs::OpenOptions::new();
        opt.write(true).create_new(true).mode(0o600).custom_flags(libc::O_NOFOLLOW);
        let Ok(file) = opt.open(&tmp) else { return false };
        let mut w = std::io::BufWriter::new(file);
        if w.write_all(bytes).is_err() || w.flush().is_err() {
            drop(w);
            let _ = std::fs::remove_file(&tmp);
            return false;
        }
        drop(w);
        // Atomic replace ONLY after the temp holds the complete, flushed bytes:
        // any earlier failure above left `path` untouched.
        if std::fs::rename(&tmp, path).is_err() {
            let _ = std::fs::remove_file(&tmp);
            return false;
        }
        true
    }
    #[cfg(windows)]
    {
        set_owner_only_windows(path, bytes)
    }
    #[cfg(not(any(unix, windows)))]
    {
        std::fs::write(path, bytes).is_ok()
    }
}

/// Windows owner-only atomic write
/// — the `#[cfg(windows)]` counterpart of the unix
/// `create_new(0o600)` path in [`write_owner_only_atomic`]. Writes to a
/// pid-stamped temp beside `path`, CREATED with an owner-only PROTECTED DACL
/// supplied on the creation call itself (see [`create_file_owner_only`] —
/// never created first and tightened afterward), writes+flushes, then
/// atomically replaces `path`. Fail-CLOSED at every step: any failure
/// removes the temp and returns `false` (the caller then injects nothing) so
/// a bearer token is NEVER on disk under the directory's inherited ACL — not
/// even for the instant a create-then-tighten sequence would leave open on a
/// shared/traversable Windows checkout.
#[cfg(windows)]
fn set_owner_only_windows(path: &Path, bytes: &[u8]) -> bool {
    use std::io::Write as _;
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    // distinct temp name per write
    // — a PID-only name collides between two concurrent same-process writers for
    // one destination (see the unix branch's own note for the unlink/rename
    // race this closes).
    let seq = {
        use std::sync::atomic::{AtomicU64, Ordering};
        static TMP_SEQ: AtomicU64 = AtomicU64::new(0);
        TMP_SEQ.fetch_add(1, Ordering::Relaxed)
    };
    let tmp = path.with_file_name(format!(".{name}.{}.{seq}.weft-tmp", std::process::id()));
    let _ = std::fs::remove_file(&tmp); // best-effort: clear a stale temp from a crashed run
    // The owner-only DACL rides the CREATION call itself — there is no
    // instant in which the temp exists under the directory's inherited ACL
    // for another account to open and hold; see `create_file_owner_only`.
    let Some(file) = create_file_owner_only(&tmp, OwnerOnlyCreate::CreateNew) else {
        return false;
    };
    let mut w = std::io::BufWriter::new(file);
    if w.write_all(bytes).is_err() || w.flush().is_err() {
        drop(w);
        let _ = std::fs::remove_file(&tmp);
        return false;
    }
    drop(w);
    // Replace `path` with the owner-locked temp only after it holds the
    // complete, flushed bytes.
    // `std::fs::rename` FAILS on Windows when the destination already exists —
    // and OpenCode's generic MCP injection (and Claude reinjection) can write a
    // config at this exact path FIRST, so a plain rename would fail on the very
    // first computer-use injection and silently leave the tool unconfigured.
    // `MoveFileExW` with `MOVEFILE_REPLACE_EXISTING` atomically replaces the
    // destination while preserving the temp's owner-only ACL. Fail-closed on
    // error (remove the temp, write nothing).
    if !windows_replace_existing(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return false;
    }
    true
}

/// Atomically replace `to` with `from` on Windows via `MoveFileExW`
/// (`MOVEFILE_REPLACE_EXISTING`), preserving `from`'s
/// ACL. Unlike `std::fs::rename`, this succeeds
/// when `to` already exists (the common case: a prior generic-MCP or reinject
/// write). Returns whether the replace succeeded.
#[cfg(windows)]
fn windows_replace_existing(from: &Path, to: &Path) -> bool {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_REPLACE_EXISTING};
    let wide = |p: &Path| -> Vec<u16> {
        p.as_os_str().encode_wide().chain(std::iter::once(0)).collect()
    };
    let from_w = wide(from);
    let to_w = wide(to);
    // SAFETY: both buffers are null-terminated and live for the whole call.
    unsafe {
        MoveFileExW(PCWSTR(from_w.as_ptr()), PCWSTR(to_w.as_ptr()), MOVEFILE_REPLACE_EXISTING).is_ok()
    }
}

/// The current process user's raw `TOKEN_USER` buffer — the shared SID
/// source for [`owner_only_dacl`] (which grants to this SID) and
/// [`file_dacl_is_owner_only`] (which compares against it). The SID inside
/// points INTO the returned Vec, so the Vec must stay alive for as long as
/// the SID pointer is used. `None` on ANY failure.
#[cfg(windows)]
fn current_user_token_buf() -> Option<Vec<u8>> {
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::Security::{GetTokenInformation, TokenUser, TOKEN_QUERY};
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    // SAFETY: the token handle is closed on every path; the two-call idiom
    // first sizes the TOKEN_USER buffer, then fills it.
    unsafe {
        let mut token = HANDLE(core::ptr::null_mut());
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
            return None;
        }
        let mut needed: u32 = 0;
        let _ = GetTokenInformation(token, TokenUser, None, 0, &mut needed);
        if needed == 0 {
            let _ = CloseHandle(token);
            return None;
        }
        let mut buf = vec![0u8; needed as usize];
        let filled = GetTokenInformation(
            token,
            TokenUser,
            Some(buf.as_mut_ptr() as *mut core::ffi::c_void),
            needed,
            &mut needed,
        )
        .is_ok();
        let _ = CloseHandle(token);
        if !filled {
            return None;
        }
        Some(buf)
    }
}

/// Build a DACL granting full control to ONLY the current process user's
/// SID — the shared core of [`create_file_owner_only`]. The returned ACL is
/// self-contained (`SetEntriesInAclW` copies the SID into it) and MUST be
/// released with `LocalFree` by the caller. `None` on ANY failure.
#[cfg(windows)]
unsafe fn owner_only_dacl() -> Option<*mut windows::Win32::Security::ACL> {
    use windows::core::PWSTR;
    use windows::Win32::Security::Authorization::{
        SetEntriesInAclW, EXPLICIT_ACCESS_W, NO_MULTIPLE_TRUSTEE, SET_ACCESS, TRUSTEE_IS_SID,
        TRUSTEE_IS_USER, TRUSTEE_W,
    };
    use windows::Win32::Security::{ACE_FLAGS, ACL, PSID, TOKEN_USER};
    use windows::Win32::Storage::FileSystem::FILE_ALL_ACCESS;

    // The SID points INTO `buf`, which must stay alive until after
    // `SetEntriesInAclW` copies it into the new ACL below.
    let buf = current_user_token_buf()?;
    let token_user = &*(buf.as_ptr() as *const TOKEN_USER);
    let sid: PSID = token_user.User.Sid;
    if sid.0.is_null() {
        return None;
    }

    let ea = EXPLICIT_ACCESS_W {
        grfAccessPermissions: FILE_ALL_ACCESS.0,
        grfAccessMode: SET_ACCESS,
        grfInheritance: ACE_FLAGS(0), // NO_INHERITANCE
        Trustee: TRUSTEE_W {
            pMultipleTrustee: core::ptr::null_mut(),
            MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_USER,
            // For TRUSTEE_IS_SID, `ptstrName` is reinterpreted as the SID
            // pointer (documented Win32 idiom).
            ptstrName: PWSTR(sid.0 as *mut u16),
        },
    };

    let mut new_dacl: *mut ACL = core::ptr::null_mut();
    if SetEntriesInAclW(Some(core::slice::from_ref(&ea)), None, &mut new_dacl).0 != 0
        || new_dacl.is_null()
    {
        return None;
    }
    Some(new_dacl)
}

/// How [`create_file_owner_only`] opens the file — the two shapes the three
/// owner-only writers need.
#[cfg(windows)]
pub(crate) enum OwnerOnlyCreate {
    /// Fail if the path already exists — the secret-config temp and the
    /// screenshot writers, mirroring unix `create_new`.
    CreateNew,
    /// Append, creating if absent — the audit log. `OPEN_ALWAYS` only stamps
    /// the creation DACL when it actually CREATES: an already-existing file
    /// keeps whatever ACL it has, which is why that caller validates the
    /// opened handle with [`file_dacl_is_owner_only`] and sets a permissive
    /// pre-existing file aside instead of appending through it.
    AppendOrCreate,
}

/// Create `path` with an owner-only DACL supplied ON THE CREATION CALL
/// ITSELF, via `SECURITY_ATTRIBUTES` on `CreateFileW` — the Windows
/// counterpart of the unix single-syscall `create_new + mode(0o600)`.
/// Creating first and tightening afterward leaves an instant in which the
/// file exists under the destination directory's inherited (possibly
/// other-account-readable) ACL — another account monitoring the directory
/// can open the predictable new entry in that instant and RETAIN its read
/// handle across the later tightening, reading every secret byte written
/// afterward despite the tightening "succeeding". An explicit DACL in the
/// creation security descriptor is applied verbatim (inherited ACEs from
/// the parent are not merged in), so no such instant exists. Returns `None`
/// on any failure — every caller fails closed.
///
/// `pub(crate)` because it is the ONE Windows owner-only-file primitive
/// shared across every place unix uses a `0o600` create: the secret-config
/// writer here, plus the screenshot save (`computer::screenshot_resolved`)
/// and the audit log (`bus::computer_srv::open_audit_file_for_append`).
#[cfg(windows)]
pub(crate) fn create_file_owner_only(path: &Path, mode: OwnerOnlyCreate) -> Option<std::fs::File> {
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::FromRawHandle;
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{GENERIC_WRITE, LocalFree, HLOCAL};
    use windows::Win32::Security::{
        InitializeSecurityDescriptor, SetSecurityDescriptorControl, SetSecurityDescriptorDacl,
        ACL, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, SECURITY_DESCRIPTOR, SE_DACL_PROTECTED,
    };
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, CREATE_NEW, FILE_APPEND_DATA, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_MODE,
        FILE_SHARE_READ, OPEN_ALWAYS, READ_CONTROL,
    };
    use windows::Win32::System::SystemServices::SECURITY_DESCRIPTOR_REVISION;

    // SAFETY: every pointer below is a stack local (or the `LocalFree`-owned
    // ACL) kept alive for the duration of the call that reads it; the ACL is
    // released on every return path after `CreateFileW` (which copies what it
    // needs into the file's own security descriptor). Any Win32 failure
    // short-circuits to `None`.
    unsafe {
        let dacl = owner_only_dacl()?;
        let free_dacl = || {
            let _ = LocalFree(Some(HLOCAL(dacl as *mut core::ffi::c_void)));
        };
        let mut sd = SECURITY_DESCRIPTOR::default();
        let psd = PSECURITY_DESCRIPTOR(&mut sd as *mut SECURITY_DESCRIPTOR as *mut core::ffi::c_void);
        // SE_DACL_PROTECTED: an explicit creation DACL alone does not stop
        // INHERITABLE ACEs from a permissive parent directory being merged
        // into the new file's security — the protected bit is what severs
        // inheritance, so the file's DACL is EXACTLY the owner-only one and
        // nothing more, under precisely the permissive-parent scenario this
        // primitive exists for.
        if InitializeSecurityDescriptor(psd, SECURITY_DESCRIPTOR_REVISION).is_err()
            || SetSecurityDescriptorDacl(psd, true, Some(dacl as *const ACL), false).is_err()
            || SetSecurityDescriptorControl(psd, SE_DACL_PROTECTED, SE_DACL_PROTECTED).is_err()
        {
            free_dacl();
            return None;
        }
        let sa = SECURITY_ATTRIBUTES {
            nLength: core::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: psd.0,
            bInheritHandle: false.into(),
        };
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
        // AppendOrCreate also requests READ_CONTROL: its one caller
        // immediately validates the opened handle's DACL
        // (`file_dacl_is_owner_only` → `GetSecurityInfo`), and querying a
        // handle's security REQUIRES that right on the handle itself —
        // append-only access would make the validation fail unconditionally,
        // and with it (fail-closed) every audit append. Reading one's own
        // security descriptor grants nothing write-ward; the handle still
        // cannot modify the ACL (that would need WRITE_DAC).
        let (access, share, disposition) = match mode {
            OwnerOnlyCreate::CreateNew => (GENERIC_WRITE.0, FILE_SHARE_MODE(0), CREATE_NEW),
            OwnerOnlyCreate::AppendOrCreate => {
                (FILE_APPEND_DATA.0 | READ_CONTROL.0, FILE_SHARE_READ, OPEN_ALWAYS)
            }
        };
        let created = CreateFileW(
            PCWSTR(wide.as_ptr()),
            access,
            share,
            Some(&sa),
            disposition,
            FILE_ATTRIBUTE_NORMAL,
            None,
        );
        free_dacl();
        match created {
            Ok(handle) => Some(std::fs::File::from_raw_handle(handle.0 as _)),
            Err(_) => None,
        }
    }
}

/// Whether `file`'s LIVE DACL grants access to ONLY the current process
/// user — the read-side complement of [`create_file_owner_only`], for the
/// one opener shape whose security attributes can silently NOT apply:
/// `OPEN_ALWAYS` only stamps the creation DACL when it actually CREATES,
/// so an already-existing file opened through it keeps whatever ACL it
/// had. A caller that must never write secrets/metadata through a
/// permissive pre-existing file (the audit log —
/// `bus::computer_srv::open_audit_file_for_append`) validates the opened
/// handle with this and reacts to a mismatch itself.
///
/// `Some(true)` — the DACL is exactly the owner-only shape this module
/// stamps: at least one ACE, and EVERY ACE a plain `ACCESS_ALLOWED` entry
/// for this process user's own SID. `Some(false)` — anything else: a NULL
/// DACL (unrestricted access for everyone), an EMPTY DACL (access for no
/// one — not this primitive's shape either), any deny/audit/object ACE
/// kind, or any ACE for a foreign SID. `None` — the query machinery itself
/// failed, so no judgment is possible (callers treat that as "not proven
/// owner-only" and fail closed).
#[cfg(windows)]
pub(crate) fn file_dacl_is_owner_only(file: &std::fs::File) -> Option<bool> {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::{LocalFree, ERROR_SUCCESS, HANDLE, HLOCAL};
    use windows::Win32::Security::Authorization::{GetSecurityInfo, SE_FILE_OBJECT};
    use windows::Win32::Security::{
        EqualSid, GetAce, ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, DACL_SECURITY_INFORMATION,
        PSECURITY_DESCRIPTOR, PSID, TOKEN_USER,
    };
    use windows::Win32::System::SystemServices::ACCESS_ALLOWED_ACE_TYPE;

    // `my_sid` points INTO `buf` — keep it alive across every use below.
    let buf = current_user_token_buf()?;
    // SAFETY: `buf` is a filled TOKEN_USER; the security descriptor returned
    // by `GetSecurityInfo` (which owns the ACL it hands back) is released
    // with `LocalFree` on every path after a successful query; every ACE
    // pointer is read only while the descriptor is alive.
    unsafe {
        let token_user = &*(buf.as_ptr() as *const TOKEN_USER);
        let my_sid: PSID = token_user.User.Sid;
        if my_sid.0.is_null() {
            return None;
        }

        let handle = HANDLE(file.as_raw_handle() as _);
        let mut dacl: *mut ACL = core::ptr::null_mut();
        let mut psd = PSECURITY_DESCRIPTOR::default();
        let got = GetSecurityInfo(
            handle,
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            None,
            None,
            Some(&mut dacl),
            None,
            Some(&mut psd),
        );
        if got != ERROR_SUCCESS {
            return None;
        }
        let free_sd = || {
            let _ = LocalFree(Some(HLOCAL(psd.0)));
        };
        if dacl.is_null() {
            free_sd();
            return Some(false);
        }
        let count = u32::from((*dacl).AceCount);
        if count == 0 {
            free_sd();
            return Some(false);
        }
        for i in 0..count {
            let mut pace: *mut core::ffi::c_void = core::ptr::null_mut();
            if GetAce(dacl, i, &mut pace).is_err() || pace.is_null() {
                free_sd();
                return None;
            }
            let header = &*(pace as *const ACE_HEADER);
            if u32::from(header.AceType) != ACCESS_ALLOWED_ACE_TYPE {
                free_sd();
                return Some(false);
            }
            // For ACCESS_ALLOWED_ACE the SID is stored INLINE starting at
            // `SidStart` (documented Win32 layout).
            let ace = &*(pace as *const ACCESS_ALLOWED_ACE);
            let ace_sid = PSID(core::ptr::addr_of!(ace.SidStart) as *mut core::ffi::c_void);
            // `EqualSid` maps to `Err` both for "not equal" and for an
            // invalid SID — either way this ACE is not proven to be ours.
            if EqualSid(my_sid, ace_sid).is_err() {
                free_sd();
                return Some(false);
            }
        }
        free_sd();
        Some(true)
    }
}

/// Best-effort move of `path` to a `<name>.insecure` sibling so a fresh,
/// owner-only file can take the path — the reaction
/// [`file_dacl_is_owner_only`]'s audit-log caller applies to a pre-existing
/// PERMISSIVE file: its bytes were already exposed under the old ACL (moving
/// preserves them for the human to inspect rather than destroying evidence),
/// but nothing new may ever be appended through it. `MOVEFILE_REPLACE_
/// EXISTING` keeps the newest quarantined copy when this fires more than
/// once. Returns whether the move succeeded — a caller must fail closed
/// (write nothing) on `false`, since the permissive file still occupies the
/// path.
#[cfg(windows)]
pub(crate) fn set_aside_insecure(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    let aside = path.with_file_name(format!("{name}.insecure"));
    windows_replace_existing(path, &aside)
}

/// Claude's computer-use MCP config.
/// `inject_mcp`'s generic claude branch writes
/// `.weft-<stem>.mcp.json` INSIDE the worktree (`cwd`), relying on
/// `git::git_exclude` to keep it out of `git status`/commits — fine for
/// `weft_bus`/`weft_planner`/`weft_curator`/`weft_global`, none of which
/// carry a secret, but this URL embeds a per-session bearer token
/// (`computer_url`'s own `&key=`). `git_exclude` only ever hides a path from
/// git's OWN status/diff view; it does nothing about filesystem
/// PERMISSIONS, and it can never protect against a human's own `git add -A`
/// sweeping the whole worktree (CLAUDE.md: stage explicit paths only — but
/// this file existing at all inside a repo-visible path is itself the
/// exposure this closes, not just the commit). On a shared or traversable
/// checkout, a DEFAULT-umask file sitting inside the worktree is readable by
/// any other account that can reach the path at all.
///
/// This writes the config to a Weft-managed, OUT-OF-REPO location instead —
/// under `paths::weft_home()`, never inside `cwd` — created ATOMICALLY
/// owner-only on unix (see
/// [`write_owner_only_atomic`]'s own doc for why write-then-chmod left a
/// readable window this closes) so only this user's own account can read
/// the bearer even on a shared machine, and named per `(thread, dir)` so
/// concurrent sessions each get their own file. There is no repo path here
/// for the token to ever land in a commit, so — unlike the claude branch of
/// `inject_mcp` — this never calls `git::git_exclude` at all (nothing to
/// exclude: canonical repos must never see Weft's own bookkeeping, and now
/// there is none to see). Best-effort: an unwritable `weft_home`/config dir,
/// or a failed atomic write, falls back to no injection (`Injection { args:
/// vec![] }`) rather than erroring the whole session, matching
/// `inject_mcp`'s own best-effort contract.
fn inject_computer_claude(thread: i32, dir: &str, wt: Option<i32>, url: &str) -> Injection {
    let Ok(home) = crate::paths::weft_home() else {
        return Injection::none();
    };
    let mcp_dir = home.join("computer-mcp");
    if std::fs::create_dir_all(&mcp_dir).is_err() {
        return Injection::none();
    }
    // the config filename includes
    // the worktree `wt`, with a DISTINCT representation for the absent/lead
    // case. Two Claude workers of one multi-repo direction share a single
    // `(thread, dir)` but differ by `wt`; naming the file by `(thread, dir)`
    // alone made the SECOND worker's injection atomically overwrite the first's
    // — and since spawning doesn't guarantee the first child has already parsed
    // `--mcp-config`, that worker could start with its sibling's URL (and thus
    // its `wt`), routing screenshots/audit into the wrong worktree namespace.
    let wt_suffix = match wt {
        Some(id) => format!("-wt{id}"),
        None => String::new(),
    };
    // ... and a LAUNCH-unique sequence on top: two overlapping launches for
    // the SAME identity (a rerun/rebuild racing the engine it replaces) must
    // never share a file either — with one stable path, the second launch's
    // write lands before either child is guaranteed to have parsed the file,
    // so the OLD child could read the REPLACEMENT's current-generation bearer
    // and stay authorized alongside it, defeating the rotation. A per-launch
    // filename means each child can only ever read the exact token its own
    // launch minted (already dead the moment a newer launch rotates).
    let launch = {
        use std::sync::atomic::{AtomicU64, Ordering};
        static LAUNCH_SEQ: AtomicU64 = AtomicU64::new(1);
        LAUNCH_SEQ.fetch_add(1, Ordering::Relaxed)
    };
    let ident = format!("{thread}-{}{wt_suffix}", sanitize_filename_component(dir));
    let file = mcp_dir.join(format!("{ident}-L{launch}.mcp.json"));
    let json = serde_json::json!({
        "mcpServers": { "weft_computer": { "type": "http", "url": url } }
    });
    let bytes = serde_json::to_vec_pretty(&json).unwrap_or_default();
    if !write_owner_only_atomic(&file, &bytes) {
        return Injection::none();
    }
    // Best-effort prune of THIS identity's OTHER launches (and the legacy
    // stable-name file): their bearers are already dead, and without pruning
    // the directory grows one file per launch forever. Never touches another
    // identity's files (the exact-structure match below), and a failure here
    // only leaves a stale file behind — the fresh injection above stands.
    //
    // AGE-GATED: only files quiet for longer than any plausible
    // spawn-to-config-parse window are removed. An OVERLAPPING sibling
    // launch's config is necessarily fresh — deleting it before its child
    // parses `--mcp-config` would strip that child's computer server
    // entirely, and removal order isn't launch order (an earlier-sequence
    // launch finishing its write last could otherwise even delete the
    // NEWEST config). A file still sitting here past the window means its
    // launch parsed it long ago (a started child reads the file within
    // moments of spawn) or never came up at all — and the bearer inside is
    // rotated-dead either way, so removing it reclaims only disk, never a
    // live child's configuration.
    const PRUNE_MIN_AGE: std::time::Duration = std::time::Duration::from_secs(600);
    if let Ok(entries) = std::fs::read_dir(&mcp_dir) {
        let legacy = format!("{ident}.mcp.json");
        let launch_prefix = format!("{ident}-L");
        let ours = format!("{ident}-L{launch}.mcp.json");
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if name == ours {
                continue;
            }
            let is_legacy = name == legacy;
            let is_other_launch = name
                .strip_prefix(&launch_prefix)
                .and_then(|rest| rest.strip_suffix(".mcp.json"))
                .is_some_and(|seq| !seq.is_empty() && seq.bytes().all(|b| b.is_ascii_digit()));
            if !(is_legacy || is_other_launch) {
                continue;
            }
            // An unreadable mtime keeps the file — prune is best-effort and
            // must never guess a file old.
            let old_enough = entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.elapsed().ok())
                .is_some_and(|age| age >= PRUNE_MIN_AGE);
            if old_enough {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
    Injection::args_only(vec!["--mcp-config".into(), file.to_string_lossy().to_string()])
}

/// Keep only characters safe as a bare filename component — everything else
/// (a `dir` should always be a plain numeric direction id or "lead", but this
/// stays defensive rather than trusting that) becomes `_`. Used by
/// [`inject_computer_claude`] to name its per-`(thread, dir)` config file.
fn sanitize_filename_component(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect()
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
        return Injection::none();
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
            Injection::args_only(vec!["--mcp-config".into(), cfg.to_string_lossy().to_string()])
        }
        "codex" => Injection::args_only(vec![
            "-c".into(),
            format!("mcp_servers.{server}.url={url}"),
        ]),
        "opencode" => {
            // None of `weft_bus`/`weft_planner`/`weft_curator`/`weft_global`
            // carry a bearer token — see `merge_opencode_config`'s doc.
            merge_opencode_config(cwd, server, url);
            Injection::none()
        }
        _ => Injection::none(),
    }
}

/// Deep-merge `mcp.<server> = {type:remote, url, enabled:true}` into the cwd's
/// opencode.json, preserving any existing config the sub-repo shipped.
///
/// Reached only via `inject_mcp`'s generic `"opencode"` branch — none of
/// `weft_bus`/`weft_planner`/`weft_curator`/`weft_global` carry a secret, so
/// the plain (non-atomic, non-owner-only) write is fine, and tightening the
/// file anyway would needlessly restrict a config the sub-repo may expect to
/// read/write with its own tooling. The one server whose URL embeds a bearer
/// token (`weft_computer`) never writes ANY file — it rides the
/// OPENCODE_CONFIG_CONTENT environment channel instead (see
/// [`opencode_env_config_injection`]), which is what keeps this merge
/// no-secrets-only.
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
    let bytes = serde_json::to_vec_pretty(&root).unwrap_or_default();
    let _ = std::fs::write(&path, &bytes);
    // Best-effort: only hides opencode.json from git when the sub-repo does NOT
    // track it. If the repo ships a tracked opencode.json, the merge still shows
    // as a modification — an accepted limitation of the worktree-local merge.
    //
    // but NEVER from a LINKED
    // worktree (`.git` is a gitfile, not a directory). A linked worktree's
    // `git rev-parse --git-path info/exclude` resolves to the CANONICAL
    // repository's shared `.git/info/exclude` (worktrees share one gitdir's
    // `info/` directory — the exact mechanism `paths::computer_output_root`'s
    // own doc records for the identical `.weft/` leak), so excluding here
    // would permanently hide `opencode.json` in the user's REAL checkout and
    // every sibling worktree — a hard violation of "never write cross-repo
    // wiring into canonical repositories" (CLAUDE.md). The cost of skipping is
    // only cosmetic: `opencode.json` shows as untracked dirty state inside the
    // Weft-managed worktree, which is exactly the "worktree-local ignored
    // files" tradeoff that constraint's own menu sanctions.
    if !cwd.join(".git").is_file() {
        crate::git::git_exclude(cwd, "opencode.json");
    }
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

    /// Claude's computer-use config now lives
    /// OUT OF the worktree entirely, under a Weft-managed `weft_home()`
    /// subdirectory, narrowed to `0600` on unix — see
    /// `inject_computer_claude`'s own doc for why. Needs an ISOLATED
    /// `WEFT_HOME` (this test writes into it) — see `paths::ENV_LOCK`'s own
    /// doc for why every WEFT_HOME-touching test must hold it.
    #[test]
    fn computer_claude_writes_its_own_config() {
        let _env = crate::paths::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let weft_home =
            std::env::temp_dir().join(format!("weft-inj-comp-home-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&weft_home);
        std::env::set_var("WEFT_HOME", weft_home.to_str().unwrap());

        let dir = std::env::temp_dir().join(format!("weft-inj-comp-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let inj = inject_computer("http://127.0.0.1:9", 601, "10", "claude", None);
        assert_eq!(inj.args[0], "--mcp-config");
        let cfg_path = std::path::PathBuf::from(&inj.args[1]);
        assert!(
            !cfg_path.starts_with(&dir),
            "the computer MCP config must live OUTSIDE the worktree entirely, got {cfg_path:?}"
        );
        assert!(
            !dir.join(".weft-computer.mcp.json").exists(),
            "must never write anything computer-related inside cwd at all"
        );
        let cfg = std::fs::read_to_string(&cfg_path).unwrap();
        assert!(cfg.contains("weft_computer") && cfg.contains("/computer/601/10/mcp"));
        // the injected URL still carries the
        // EXACT per-session bearer `computer_session_token` would mint for
        // this same (thread, dir) — recomputed AFTER the inject, which
        // rotated this identity's token generation.
        assert!(
            cfg.contains(&format!("key={}", crate::bus::computer_srv::computer_session_token(601, "10", None))),
            "{cfg}"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&cfg_path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "the bearer-token-bearing config must be 0600, got {mode:o}");
        }

        std::env::remove_var("WEFT_HOME");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&weft_home);
    }

    /// A SECOND launch for the SAME `(thread, dir, wt)` must get its OWN
    /// launch-unique config path — never rewrite the earlier launch's file in
    /// place, where the old child (not guaranteed to have parsed
    /// `--mcp-config` yet) could read the replacement's fresh bearer. Pruning
    /// of this identity's other files is AGE-GATED: a FRESH sibling config
    /// (an overlapping launch whose child may not have parsed it yet) must
    /// survive, while files older than the parse window — a backdated prior
    /// launch, the legacy stable-name file — are removed. The new file still
    /// lands via [`write_owner_only_atomic`]: exactly `0600` on unix, no
    /// `.weft-tmp` sibling left behind. A DIFFERENT `wt` (or the absent-wt
    /// lead case) is a DIFFERENT identity whose files the prune must never
    /// touch, fresh or stale.
    #[test]
    fn computer_claude_relaunch_gets_a_fresh_path_and_prunes_stale_configs() {
        let _env = crate::paths::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let weft_home =
            std::env::temp_dir().join(format!("weft-inj-comp-reinject-home-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&weft_home);
        std::env::set_var("WEFT_HOME", weft_home.to_str().unwrap());

        let dir = std::env::temp_dir().join(format!("weft-inj-comp-reinject-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);

        // Backdate a file far past the prune window, standing in for "its
        // launch is long gone".
        let backdate = |p: &std::path::Path| {
            let f = std::fs::OpenOptions::new().write(true).open(p).unwrap();
            let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
            f.set_times(std::fs::FileTimes::new().set_modified(old)).unwrap();
        };

        // Plant a LEGACY stable-name file for this identity (the pre-launch-
        // suffix naming, backdated: genuinely stale) and a STALE config
        // belonging to a DIFFERENT identity: the next injection's prune must
        // remove the former and never touch the latter, age notwithstanding.
        let mcp_dir = weft_home.join("computer-mcp");
        std::fs::create_dir_all(&mcp_dir).unwrap();
        let legacy = mcp_dir.join("5-50-wt7.mcp.json");
        std::fs::write(&legacy, b"stale-legacy").unwrap();
        backdate(&legacy);
        let foreign = mcp_dir.join("99-77-L1.mcp.json");
        std::fs::write(&foreign, b"another-identity").unwrap();
        backdate(&foreign);

        let first = inject_computer("http://127.0.0.1:9", 5, "50", "claude", Some(7));
        let first_path = std::path::PathBuf::from(&first.args[1]);
        assert!(first_path.exists(), "the first injection must write the config");
        assert!(!legacy.exists(), "the stale legacy stable-name file must be pruned on injection");
        assert!(foreign.exists(), "another identity's config must never be pruned, even stale");

        // A second launch for the SAME (thread, dir, wt) with a DIFFERENT
        // base URL — a rerun racing the engine it replaces. It must get its
        // OWN file, and the first launch's FRESH file must survive: that
        // child may not have parsed `--mcp-config` yet, and deleting the
        // file would strip its computer server entirely.
        let second = inject_computer("http://127.0.0.1:8", 5, "50", "claude", Some(7));
        let second_path = std::path::PathBuf::from(&second.args[1]);
        assert_ne!(
            second_path, first_path,
            "each launch must get its own config path, never rewrite the prior launch's in place"
        );
        assert!(
            first_path.exists(),
            "a FRESH sibling launch's config must never be pruned — its child may not have \
             parsed it yet"
        );
        let cfg = std::fs::read_to_string(&second_path).unwrap();
        assert!(cfg.contains("127.0.0.1:8"), "the new launch's content must be its own: {cfg}");

        // Once the first launch's file is genuinely old, the next injection
        // prunes it — and only it (the second launch's file stays fresh).
        backdate(&first_path);
        let third = inject_computer("http://127.0.0.1:7", 5, "50", "claude", Some(7));
        let third_path = std::path::PathBuf::from(&third.args[1]);
        assert!(!first_path.exists(), "a stale prior launch's config must be pruned");
        assert!(second_path.exists(), "a still-fresh sibling config must survive the prune");
        assert!(third_path.exists());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&second_path).unwrap().permissions().mode() & 0o777;
            assert_eq!(
                mode, 0o600,
                "every launch's config must be exactly 0600, got {mode:o}"
            );
        }

        // The atomic rename consumes the temp on success — no
        // `.weft-tmp` sibling may linger next to the real config.
        let leftover: Vec<_> = std::fs::read_dir(&mcp_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains("weft-tmp"))
            .collect();
        assert!(leftover.is_empty(), "no temp file may be left behind: {leftover:?}");

        // A DIFFERENT wt for the SAME (thread, dir) — and the absent-wt lead
        // case — are DIFFERENT identities: their filenames carry their own
        // identity prefix and their injections must not prune this one's file.
        let other_wt = inject_computer("http://127.0.0.1:9", 5, "50", "claude", Some(8));
        let other_name = std::path::PathBuf::from(&other_wt.args[1]);
        let other_name = other_name.file_name().unwrap().to_string_lossy().to_string();
        assert!(
            other_name.starts_with("5-50-wt8-L"),
            "a different wt must be a different identity, got {other_name}"
        );
        let absent_wt = inject_computer("http://127.0.0.1:9", 5, "50", "claude", None);
        let absent_name = std::path::PathBuf::from(&absent_wt.args[1]);
        let absent_name = absent_name.file_name().unwrap().to_string_lossy().to_string();
        assert!(
            absent_name.starts_with("5-50-L"),
            "the absent-wt identity must differ from any explicit wt's, got {absent_name}"
        );
        assert!(
            second_path.exists(),
            "another identity's injection must never prune this identity's config"
        );
        assert!(foreign.exists(), "the planted foreign config must still be untouched");

        std::env::remove_var("WEFT_HOME");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&weft_home);
    }

    /// `write_owner_only_atomic` must replace an
    /// existing file's contents WHOLESALE (never append/partial) at exactly
    /// 0600, and leave no temp sibling — the temp-then-rename path. (The
    /// data-preservation-on-failure guarantee — the original survives a failed
    /// write — is structural to writing the temp first and only renaming after
    /// a complete flush; it isn't unit-forceable without injecting a write
    /// fault, so this pins the observable success-path invariants.)
    #[cfg(unix)]
    #[test]
    fn write_owner_only_atomic_replaces_existing_content_owner_only_no_temp() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("weft-woa-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("opencode.json");

        // A pre-existing, wider-mode file with OLD content.
        std::fs::write(&path, b"OLD-LONGER-CONTENT-that-must-be-fully-replaced").unwrap();

        assert!(write_owner_only_atomic(&path, b"new"));
        assert_eq!(std::fs::read(&path).unwrap(), b"new", "content must be replaced wholesale");
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600,
            "the replaced file must be exactly 0600"
        );
        let leftover: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains("weft-tmp"))
            .collect();
        assert!(leftover.is_empty(), "no temp file may be left behind: {leftover:?}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// codex's argv carries NO
    /// bearer — the URL is key-less, a second `-c` names the env VARIABLE, and
    /// the token itself rides `Injection::env` for the spawn site to set on
    /// the codex child alone.
    #[test]
    fn computer_codex_uses_config_override_with_the_bearer_in_env_not_argv() {
        // Captured AFTER the inject (which rotates this identity's token
        // generation) — the env token must match the CURRENT render. A
        // thread id no other test injects for, so parallel tests can't
        // rotate it mid-assertion.
        let inj = inject_computer("http://127.0.0.1:9", 611, "10", "codex", None);
        let token = crate::bus::computer_srv::computer_session_token(611, "10", None);
        assert_eq!(
            inj.args,
            vec![
                "-c".to_string(),
                "mcp_servers.weft_computer.url=http://127.0.0.1:9/computer/611/10/mcp".to_string(),
                "-c".to_string(),
                format!("mcp_servers.weft_computer.bearer_token_env_var={COMPUTER_TOKEN_ENV_VAR}"),
            ]
        );
        assert_eq!(inj.env, vec![(COMPUTER_TOKEN_ENV_VAR.to_string(), token.clone())]);
        assert!(
            inj.args.iter().all(|a| !a.contains(&token)),
            "the bearer must never appear in any argv element: {:?}",
            inj.args
        );
    }

    /// a resolved `wt` appends `?wt=<id>` to the
    /// injected URL, for both the claude file-based injection and codex's
    /// config-override flag. Needs an ISOLATED `WEFT_HOME` for the claude half
    ///  — see
    /// `paths::ENV_LOCK`'s own doc.
    #[test]
    fn computer_wt_appends_the_query_param_for_claude_and_codex() {
        let _env = crate::paths::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let weft_home =
            std::env::temp_dir().join(format!("weft-inj-comp-wt-home-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&weft_home);
        std::env::set_var("WEFT_HOME", weft_home.to_str().unwrap());

        let dir = std::env::temp_dir().join(format!("weft-inj-comp-wt-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let inj = inject_computer("http://127.0.0.1:9", 612, "10", "claude", Some(42));
        // Recomputed AFTER the inject — each injection rotates the identity's
        // token generation, so only the current render matches.
        let key = crate::bus::computer_srv::computer_session_token(612, "10", Some(42));
        assert_eq!(inj.args[0], "--mcp-config");
        let cfg_path = std::path::PathBuf::from(&inj.args[1]);
        assert!(!cfg_path.starts_with(&dir), "must live outside the worktree, got {cfg_path:?}");
        let cfg = std::fs::read_to_string(&cfg_path).unwrap();
        assert!(cfg.contains("/computer/612/10/mcp?wt=42"), "{cfg}");
        assert!(cfg.contains(&format!("&key={key}")), "{cfg}");

        std::env::remove_var("WEFT_HOME");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&weft_home);

        // codex: the `?wt=` pin stays in the argv URL
        // (a worktree id is not a secret) but the bearer rides env, minted for
        // this EXACT `wt`.
        let inj = inject_computer("http://127.0.0.1:9", 612, "10", "codex", Some(42));
        // The codex inject rotated the generation again — recompute.
        let key = crate::bus::computer_srv::computer_session_token(612, "10", Some(42));
        assert_eq!(
            inj.args,
            vec![
                "-c".to_string(),
                "mcp_servers.weft_computer.url=http://127.0.0.1:9/computer/612/10/mcp?wt=42".to_string(),
                "-c".to_string(),
                format!("mcp_servers.weft_computer.bearer_token_env_var={COMPUTER_TOKEN_ENV_VAR}"),
            ]
        );
        assert_eq!(inj.env, vec![(COMPUTER_TOKEN_ENV_VAR.to_string(), key.clone())]);
        assert!(inj.args.iter().all(|a| !a.contains(&key)), "{:?}", inj.args);
    }

    // —— computer_url mints a per-session `&key=` ——

    /// `computer_url` itself, directly: the token it appends is EXACTLY
    /// `computer_session_token(thread, dir, wt)` for that same path's own
    /// identity — never a different/derived value — and it's attached
    /// correctly whether or not `?wt=` is already present (`&key=` vs
    /// `?key=`, never two separately-prefixed query strings).
    #[test]
    fn computer_url_appends_exactly_the_session_token_for_this_thread_dir() {
        let no_wt = computer_url("http://127.0.0.1:9", 3, "30", None);
        assert_eq!(
            no_wt,
            format!(
                "http://127.0.0.1:9/computer/3/30/mcp?key={}",
                crate::bus::computer_srv::computer_session_token(3, "30", None)
            )
        );

        let with_wt = computer_url("http://127.0.0.1:9", 3, "30", Some(9));
        assert_eq!(
            with_wt,
            format!(
                "http://127.0.0.1:9/computer/3/30/mcp?wt=9&key={}",
                crate::bus::computer_srv::computer_session_token(3, "30", Some(9))
            )
        );

        // A DIFFERENT (thread, dir) mints a DIFFERENT token — this isn't a
        // constant/global secret slapped on every URL.
        let other = computer_url("http://127.0.0.1:9", 4, "40", None);
        assert_ne!(
            no_wt.split("key=").nth(1),
            other.split("key=").nth(1),
            "two different (thread, dir) pairs must never share the same token"
        );
    }

    #[test]
    fn acp_mcp_servers_include_computer_toggles_weft_computer() {
        let with_computer = acp_mcp_servers(
            "http://127.0.0.1:9",
            631,
            "10",
            None,
            true,
            false,
            false,
            false,
            true,
            None,
        );
        // Captured AFTER the call — the computer arm rotates this identity's
        // token generation before minting, so only the current render matches.
        let key = crate::bus::computer_srv::computer_session_token(631, "10", None);
        assert!(with_computer.iter().any(|s| s.name == "weft_computer"
            && s.url == format!("http://127.0.0.1:9/computer/631/10/mcp?key={key}")));

        let without_computer = acp_mcp_servers(
            "http://127.0.0.1:9",
            631,
            "10",
            None,
            true,
            false,
            false,
            false,
            false,
            None,
        );
        assert!(!without_computer.iter().any(|s| s.name == "weft_computer"));
    }

    /// `computer_wt` forwards into the injected
    /// `weft_computer` URL's `?wt=` query param for an ACP worker.
    #[test]
    fn acp_mcp_servers_computer_wt_pins_the_worktree_query_param() {
        let with_wt = acp_mcp_servers(
            "http://127.0.0.1:9",
            632,
            "10",
            None,
            true,
            false,
            false,
            false,
            true,
            Some(7),
        );
        // Captured AFTER the call — the computer arm rotates the identity's
        // token generation before minting.
        let key = crate::bus::computer_srv::computer_session_token(632, "10", Some(7));
        assert!(with_wt.iter().any(|s| s.name == "weft_computer"
            && s.url == format!("http://127.0.0.1:9/computer/632/10/mcp?wt=7&key={key}")));
    }

    /// Re-injecting for the SAME `(thread, dir, wt)` — a rerun, a resume
    /// under a new persisted session, or an engine switch — must invalidate
    /// the bearer minted for the child it replaces: only the LATEST
    /// injection's token matches the current render, so a process that kept
    /// its old config gets a bare 401 at the endpoint's entry gate.
    #[test]
    fn each_computer_injection_invalidates_the_previous_sessions_bearer() {
        let first = inject_computer("http://127.0.0.1:9", 641, "10", "codex", Some(3));
        let first_token = first.env[0].1.clone();
        assert_eq!(
            first_token,
            crate::bus::computer_srv::computer_session_token(641, "10", Some(3)),
            "the freshly-injected token must be the current render"
        );

        let second = inject_computer("http://127.0.0.1:9", 641, "10", "codex", Some(3));
        let second_token = second.env[0].1.clone();
        assert_ne!(first_token, second_token, "a re-injection must mint a DIFFERENT bearer");
        let current = crate::bus::computer_srv::computer_session_token(641, "10", Some(3));
        assert_eq!(second_token, current, "the replacement's token is the current render");
        assert_ne!(first_token, current, "the replaced session's token no longer recomputes");

        // A sibling worker (same thread/dir, different wt) is untouched by
        // the rotation — its own identity has its own generation.
        let sibling = inject_computer("http://127.0.0.1:9", 641, "10", "codex", Some(4));
        assert_eq!(
            sibling.env[0].1,
            crate::bus::computer_srv::computer_session_token(641, "10", Some(4)),
        );
        assert_eq!(
            crate::bus::computer_srv::computer_session_token(641, "10", Some(3)),
            current,
            "rotating the sibling must not disturb this identity's generation"
        );
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

    // —— the OpenCode computer bearer rides the OPENCODE_CONFIG_CONTENT env
    // channel, never a file ——

    /// The base property: the computer injection for OpenCode produces ONLY
    /// an env-carried inline config (bearer URL included) and touches no
    /// file at all — a plain directory (not even a git repo, the case the
    /// old file merge WITHHELD as tracked-indeterminate) now both stays
    /// clean on disk AND actually gets the tool.
    #[test]
    fn computer_opencode_rides_the_env_channel_and_writes_no_file() {
        let dir = std::env::temp_dir().join(format!("weft-inj-comp-oc-env-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let inj = inject_computer("http://127.0.0.1:9", 621, "10", "opencode", None);
        assert!(inj.args.is_empty(), "opencode has no launch-flag injection");
        assert_eq!(inj.env.len(), 1);
        assert_eq!(inj.env[0].0, "OPENCODE_CONFIG_CONTENT");
        let inline: serde_json::Value = serde_json::from_str(&inj.env[0].1).unwrap();
        let url = inline["mcp"]["weft_computer"]["url"].as_str().unwrap();
        assert!(url.contains("/computer/621/10/mcp"), "{url}");
        assert!(
            url.contains(&format!(
                "key={}",
                crate::bus::computer_srv::computer_session_token(621, "10", None)
            )),
            "the per-session bearer must ride the env-carried URL: {url}"
        );
        assert!(
            !dir.join("opencode.json").exists(),
            "the computer injection must write NO opencode.json — the bearer never touches disk"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A sub-repo that ships (and committed) its own `opencode.json`: the
    /// file stays byte-for-byte untouched — no token can ever land in the
    /// repo's history — while the env channel still delivers the tool (a
    /// file merge would have to WITHHOLD it here; the env channel doesn't).
    #[test]
    fn computer_opencode_leaves_a_tracked_opencode_json_untouched_and_still_injects() {
        use std::process::Command;
        let root = std::env::temp_dir().join(format!("weft-inj-comp-oc-tracked-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let sh = |dir: &Path, args: &[&str]| {
            assert!(Command::new(args[0])
                .args(&args[1..])
                .current_dir(dir)
                .status()
                .unwrap()
                .success());
        };
        sh(&root, &["git", "init", "-q"]);
        sh(&root, &["git", "config", "user.email", "t@t.t"]);
        sh(&root, &["git", "config", "user.name", "t"]);
        let shipped = r#"{"mcp":{"repo_own":{"type":"local","command":["x"]}}}"#;
        std::fs::write(root.join("opencode.json"), shipped).unwrap();
        sh(&root, &["git", "add", "-A"]);
        sh(&root, &["git", "commit", "-q", "-m", "init"]);

        let token = crate::bus::computer_srv::computer_session_token(1, "10", None);
        let inj = inject_computer("http://127.0.0.1:9", 1, "10", "opencode", None);
        let cfg = std::fs::read_to_string(root.join("opencode.json")).unwrap();
        assert_eq!(cfg, shipped, "a tracked opencode.json must stay byte-for-byte untouched");
        assert!(!cfg.contains(&token));
        let inline: serde_json::Value = serde_json::from_str(&inj.env[0].1).unwrap();
        assert!(
            inline["mcp"]["weft_computer"].is_object(),
            "the tool is delivered via env despite the tracked file: {inline}"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// The hazard the env channel exists for: in a LINKED worktree (`.git`
    /// is a gitfile), `git_exclude` is rightly skipped (the exclude file is
    /// the canonical repo's), so a file-carried bearer would sit UNIGNORED —
    /// one broad `git add` away from a committed, pushable credential. The
    /// env channel must leave the worktree's `git status` completely clean.
    #[test]
    fn computer_opencode_writes_nothing_into_a_linked_worktree() {
        use std::process::Command;
        let root = std::env::temp_dir().join(format!("weft-inj-comp-oc-linked-{}", std::process::id()));
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
        sh(&repo, &["git", "worktree", "add", "-q", wt.to_str().unwrap()]);

        let inj = inject_computer("http://127.0.0.1:9", 5, "50", "opencode", Some(7));
        assert!(
            !wt.join("opencode.json").exists(),
            "no opencode.json may be created in a linked worktree — it would be UNIGNORED there"
        );
        let status = Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(&wt)
            .output()
            .unwrap();
        let s = String::from_utf8_lossy(&status.stdout);
        assert!(s.trim().is_empty(), "the worktree must stay clean — nothing sweepable: {s}");
        let inline: serde_json::Value = serde_json::from_str(&inj.env[0].1).unwrap();
        let url = inline["mcp"]["weft_computer"]["url"].as_str().unwrap();
        assert!(url.contains("wt=7") && url.contains("key="), "{url}");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// An opencode WORKER's session bus and computer server BOTH produce an
    /// OPENCODE_CONFIG_CONTENT entry — `coalesce_env` must deep-merge them
    /// into one entry carrying BOTH mcp servers (a raw `Command::envs` pass
    /// would keep only the later one, silently dropping the bus), while
    /// unrelated variables pass through.
    #[test]
    fn coalesce_env_deep_merges_the_bus_and_computer_opencode_entries() {
        let dir = std::env::temp_dir().join(format!("weft-inj-coalesce-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let bus = inject("http://127.0.0.1:9", 7, "19", Some(63), "opencode", &dir);
        let comp = inject_computer("http://127.0.0.1:9", 7, "19", "opencode", Some(4));
        let mut env = vec![("OTHER_VAR".to_string(), "kept".to_string())];
        env.extend(bus.env);
        env.extend(comp.env);

        let coalesced = coalesce_env(env);
        let opencode: Vec<_> =
            coalesced.iter().filter(|(k, _)| k == "OPENCODE_CONFIG_CONTENT").collect();
        assert_eq!(opencode.len(), 1, "exactly one merged entry: {coalesced:?}");
        let inline: serde_json::Value = serde_json::from_str(&opencode[0].1).unwrap();
        assert_eq!(
            inline["mcp"]["weft_bus"]["url"],
            "http://127.0.0.1:9/bus/7/19/mcp?session_id=63",
            "the bus half must survive the merge: {inline}"
        );
        let comp_url = inline["mcp"]["weft_computer"]["url"].as_str().unwrap();
        assert!(comp_url.contains("/computer/7/19/mcp?wt=4&key="), "{comp_url}");
        assert!(
            coalesced.iter().any(|(k, v)| k == "OTHER_VAR" && v == "kept"),
            "unrelated variables pass through untouched: {coalesced:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `coalesce_env` edge shapes: a single entry passes through verbatim,
    /// and an unmergeable (non-object) payload falls back to plain last-wins
    /// rather than corrupting either value.
    #[test]
    fn coalesce_env_passes_singletons_and_falls_back_to_last_wins_when_unmergeable() {
        let single = coalesce_env(vec![(
            "OPENCODE_CONFIG_CONTENT".to_string(),
            r#"{"mcp":{}}"#.to_string(),
        )]);
        assert_eq!(single.len(), 1);
        assert_eq!(single[0].1, r#"{"mcp":{}}"#, "a singleton must pass through byte-identical");

        let fallback = coalesce_env(vec![
            ("OPENCODE_CONFIG_CONTENT".to_string(), "not-json".to_string()),
            ("OPENCODE_CONFIG_CONTENT".to_string(), r#"{"mcp":{"a":{}}}"#.to_string()),
        ]);
        let opencode: Vec<_> =
            fallback.iter().filter(|(k, _)| k == "OPENCODE_CONFIG_CONTENT").collect();
        assert_eq!(opencode.len(), 1);
        assert_eq!(
            opencode[0].1, r#"{"mcp":{"a":{}}}"#,
            "unmergeable input keeps plain env last-wins semantics"
        );
    }
}
