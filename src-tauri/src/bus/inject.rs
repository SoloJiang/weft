//! Spawn-time, ADDITIVE injection of the thread bus as an MCP server for each
//! tool. Never overrides a sub-repo's own config: claude/codex use file-less
//! launch flags; opencode deep-merges into the worktree opencode.json (which is
//! a throwaway worktree, not the canonical repo — architecture §2.1).

use std::path::Path;

/// Extra args to PREPEND to the tool's own args (global flags must precede any
/// subcommand, e.g. `codex -c k=v resume <id>`).
pub struct Injection {
    pub args: Vec<String>,
}

fn mcp_url(base: &str, thread: i32, dir: &str) -> String {
    format!("{base}/bus/{thread}/{dir}/mcp")
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

fn ask_url(base: &str, thread: i32, dir: &str, tool: &str) -> String {
    format!("{base}/ask/{thread}/{dir}?tool={tool}")
}

/// `wt` (issue #160 round-2 P2 §5): the caller's own worktree id, when it
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
/// `&key=<token>` (issue #160 round-11 P1 #A): EVERY `weft_computer` URL now
/// also carries this session-scoped bearer — `bus::computer_srv::
/// computer_session_token(thread, dir)`, an HMAC of the path's own identity
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
    let key = crate::bus::computer_srv::computer_session_token(thread, dir);
    match wt {
        Some(id) => format!("{base}/computer/{thread}/{dir}/mcp?wt={id}&key={key}"),
        None => format!("{base}/computer/{thread}/{dir}/mcp?key={key}"),
    }
}

/// HTTP MCP servers Weft should pass on ACP `session/new|resume` for this
/// engine role. Workers get `weft_bus`; lead also gets planner when `dir` is
/// the lead lane; concierge/global callers pass `include_global`.
///
/// `computer_wt` (issue #160 round-2 P2 §5): the worker's own worktree id,
/// forwarded into [`computer_url`] when `include_computer` is set — see that
/// function's own doc. Ignored (harmlessly) when `include_computer` is
/// `false`. `None` for the lead lane (a lead has no worktree at all) and for
/// any worker caller that couldn't resolve one — both fall back to the
/// pre-existing unpinned URL shape.
pub fn acp_mcp_servers(
    base: &str,
    thread: i32,
    dir: &str,
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
            url: mcp_url(base, thread, dir),
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
        out.push(crate::acp::McpServerSpec {
            name: "weft_computer".into(),
            url: computer_url(base, thread, dir, computer_wt),
        });
    }
    out
}

/// The shared, FAIL-CLOSED tail of both bash ask-hook scripts — claude's
/// per-worktree `.weft-ask-hook.sh` (below) and codex's global helper
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

/// Install the Ask Bridge for a session. Claude gets a worktree-local
/// PreToolUse settings file; Codex writes only a worktree route file consumed
/// by Weft's stable global hook in `~/.codex/config.toml`; OpenCode bridges via
/// its server `/event` plugin. ACP tools (omp) use `session/request_permission`
/// instead — no worktree files. Best-effort: empty args if files can't be written.
pub fn inject_ask_hook(base: &str, thread: i32, dir: &str, tool: &str, cwd: &Path) -> Injection {
    if crate::acp::backend_for(tool).is_some() {
        return Injection { args: vec![] };
    }
    if tool == "opencode" {
        return inject_opencode_ask_plugin(base, thread, dir, cwd);
    }
    if tool != "claude" && tool != "codex" {
        return Injection { args: vec![] };
    }
    let url = ask_url(base, thread, dir, tool);
    if tool == "codex" {
        let route = cwd.join(".weft-codex-ask-url");
        if std::fs::write(&route, &url).is_err() {
            return Injection { args: vec![] };
        }
        crate::git::git_exclude(cwd, ".weft-codex-ask-url");
        return Injection { args: vec![] };
    }
    let script = cwd.join(".weft-ask-hook.sh");
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
        return Injection { args: vec![] };
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755));
    }
    crate::git::git_exclude(cwd, ".weft-ask-hook.sh");

    match tool {
        "claude" => {
            let settings = cwd.join(".weft-ask.settings.json");
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
                return Injection { args: vec![] };
            }
            crate::git::git_exclude(cwd, ".weft-ask.settings.json");
            Injection {
                args: vec!["--settings".into(), settings.to_string_lossy().to_string()],
            }
        }
        // Codex now warns loudly when --dangerously-bypass-hook-trust is used.
        // Do not inject Weft's PreToolUse hook through that bypass path; Codex's
        // own sandbox/approval mode remains authoritative for exec sessions.
        "codex" => Injection { args: vec![] },
        _ => Injection { args: vec![] },
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
fn inject_opencode_ask_plugin(base: &str, thread: i32, dir: &str, cwd: &Path) -> Injection {
    let url = ask_url(base, thread, dir, "opencode");
    let plugins = cwd.join(".opencode").join("plugins");
    if std::fs::create_dir_all(&plugins).is_err() {
        return Injection { args: vec![] };
    }
    let template = include_str!("weft-ask-plugin.js");
    let body = template.replace("__URL__", &url);
    let _ = std::fs::write(plugins.join("weft-ask.js"), body);
    crate::git::git_exclude(cwd, ".opencode/plugins/weft-ask.js");
    Injection { args: vec![] }
}

/// Build the thread-bus injection. `cwd` is the worktree (used for the claude
/// temp config and the opencode merge). `dir` is the direction id as a string.
pub fn inject(base: &str, thread: i32, dir: &str, tool: &str, cwd: &Path) -> Injection {
    inject_mcp("weft_bus", "bus", &mcp_url(base, thread, dir), tool, cwd)
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

/// Build the computer-use MCP injection (issue #160) for a session, per
/// thread/direction. Round-12 P1 #6 (Codex round-11 finding): unlike every
/// OTHER injected server (`weft_bus`/`weft_planner`/`weft_curator`/
/// `weft_global`, all unauthenticated by design — see their own URL
/// builders' doc comments), this URL embeds a per-session bearer
/// (`computer_url`'s own `&key=`), so this does NOT go through the shared,
/// generic [`inject_mcp`] the others use — that helper writes claude's
/// config INSIDE the worktree and opencode's merge into the worktree's own
/// (possibly git-tracked) `opencode.json`, neither of which this endpoint's
/// token can safely land in. See [`inject_computer_claude`] and
/// [`inject_computer_opencode`]'s own doc comments for each tool's
/// dedicated, token-safe path. Codex needs neither: its own injection is
/// already a bare `-c` CLI flag with no file at all (unchanged, same as
/// every other codex injection in this module).
///
/// Callers MUST gate this on `crate::computer::enabled(db)` themselves — this
/// function injects unconditionally. issue #160 round-12 P2 #7: as of this
/// round every production call site injects UNCONDITIONALLY instead
/// (concierge/curator excluded) — the setting is enforced server-side, on
/// every call, by `bus::computer_srv::run_action`'s own `computer::enabled`
/// gate; this function's own behavior (inject regardless) hasn't changed,
/// only who calls it.
///
/// `wt` (issue #160 round-2 P2 §5): the calling worker's own worktree id —
/// see [`computer_url`]'s doc. Every worker call site can resolve this (its
/// own materialized worktree row is already in scope where it calls this
/// function); the lead call site always passes `None` (a lead has no
/// worktree — it runs out of its own scratch cwd).
pub fn inject_computer(base: &str, thread: i32, dir: &str, tool: &str, cwd: &Path, wt: Option<i32>) -> Injection {
    if crate::acp::backend_for(tool).is_some() {
        // MCP is supplied on session/new|resume, not via launch flags/files —
        // same rule `inject_mcp` applies for every other server.
        return Injection { args: vec![] };
    }
    let url = computer_url(base, thread, dir, wt);
    match tool {
        "claude" => inject_computer_claude(thread, dir, &url),
        "codex" => Injection {
            args: vec!["-c".into(), format!("mcp_servers.weft_computer.url={url}")],
        },
        "opencode" => {
            inject_computer_opencode(cwd, &url);
            Injection { args: vec![] }
        }
        _ => Injection { args: vec![] },
    }
}

/// Write `bytes` to `path` as an ATOMICALLY-created, owner-only file (issue
/// #160 round-12 P1 #D, Codex round-11 finding): a bare `std::fs::write`
/// under the common `022` umask creates the file `0644` FIRST, and only
/// narrows it to `0600` a moment later via a SEPARATE `set_permissions`
/// call — any other local account that can reach the path in that gap (the
/// exact shared/traversable-machine threat model [`inject_computer_claude`]'s
/// own doc already targets) can read a bearer token straight off disk before
/// the chmod ever lands. Both token-bearing config writes in this module
/// ([`inject_computer_claude`]'s Claude `.mcp.json`, and
/// [`inject_computer_opencode`]'s merged `opencode.json`) go through this
/// instead of write-then-chmod.
///
/// `#[cfg(unix)]`: opens with `create_new` (O_EXCL) + `O_NOFOLLOW` + mode
/// `0o600` in ONE syscall — never observably `0644`, not even for an
/// instant — mirroring `computer::screenshot_window`'s own owner-only write
/// and `lead_chat::engine`'s `write_attachment_no_follow`, the two other
/// places this codebase already needed the identical "never readable at any
/// wider mode, ever" guarantee.
///
/// Re-injection (the SAME path written again — a resumed/rerun session, or
/// `merge_opencode_config` re-merging on a later spawn) is the ordinary
/// case, not an edge case: an existing file at `path` is removed FIRST
/// (best-effort; `remove_file` unlinks the directory entry itself, it never
/// follows a symlink there), then `create_new` runs fresh — so a stale
/// leftover from a previous session never blocks this one with
/// `AlreadyExists`, and a symlink planted at this predictable path in the
/// gap between the `remove_file` and the `create_new` is refused by
/// `O_NOFOLLOW`/`create_new` itself rather than followed.
///
/// Non-unix keeps the pre-existing plain `write` (no owner-only concept
/// there this crate can portably act on) — matches every other
/// `#[cfg(unix)]` split in this codebase. Returns whether the write actually
/// landed, so callers keep their existing best-effort
/// `Injection { args: vec![] }` fallback on failure.
fn write_owner_only_atomic(path: &Path, bytes: &[u8]) -> bool {
    #[cfg(unix)]
    {
        use std::io::Write as _;
        use std::os::unix::fs::OpenOptionsExt;
        let _ = std::fs::remove_file(path);
        let mut opt = std::fs::OpenOptions::new();
        opt.write(true).create_new(true).mode(0o600).custom_flags(libc::O_NOFOLLOW);
        let Ok(file) = opt.open(path) else { return false };
        let mut w = std::io::BufWriter::new(file);
        if w.write_all(bytes).is_err() {
            return false;
        }
        w.flush().is_ok()
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, bytes).is_ok()
    }
}

/// Claude's computer-use MCP config, issue #160 round-12 P1 #6 (Codex
/// round-11 finding): `inject_mcp`'s generic claude branch writes
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
/// owner-only on unix (issue #160 round-12 P1 #D — see
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
fn inject_computer_claude(thread: i32, dir: &str, url: &str) -> Injection {
    let Ok(home) = crate::paths::weft_home() else {
        return Injection { args: vec![] };
    };
    let mcp_dir = home.join("computer-mcp");
    if std::fs::create_dir_all(&mcp_dir).is_err() {
        return Injection { args: vec![] };
    }
    let file = mcp_dir.join(format!(
        "{thread}-{}.mcp.json",
        sanitize_filename_component(dir)
    ));
    let json = serde_json::json!({
        "mcpServers": { "weft_computer": { "type": "http", "url": url } }
    });
    let bytes = serde_json::to_vec_pretty(&json).unwrap_or_default();
    if !write_owner_only_atomic(&file, &bytes) {
        return Injection { args: vec![] };
    }
    Injection {
        args: vec!["--mcp-config".into(), file.to_string_lossy().to_string()],
    }
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

/// Whether `cwd`'s `opencode.json` is a git-TRACKED file — issue #160
/// round-12 P1 #6 (Codex round-11 finding). `merge_opencode_config`'s own
/// `git_exclude` call only ever hides an UNTRACKED file from `git status`;
/// it can never un-track a file the sub-repo already committed. Merging the
/// computer server's session-scoped bearer token into an ALREADY-TRACKED
/// `opencode.json` would persist Weft's own credential wiring straight into
/// the user's repo history the next time they commit — a hard CLAUDE.md
/// violation ("never write cross-repo wiring into canonical repositories"),
/// not merely a same-machine readability concern the way an untracked file
/// with lax permissions is. `git ls-files --error-unmatch` is the standard
/// "is this path tracked" check; any error (not a git repo at all, the
/// binary is missing, some other git failure) reads as "not tracked" — the
/// SAME fail-open-to-"attempt the merge" default `merge_opencode_config`
/// already has for every other failure mode on this path (best-effort
/// throughout this module), rather than a NEW way for this one check to
/// refuse computer-use injection outright.
fn opencode_json_is_tracked(cwd: &Path) -> bool {
    std::process::Command::new("git")
        .args(["ls-files", "--error-unmatch", "opencode.json"])
        .current_dir(cwd)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// OpenCode's computer-use MCP merge, issue #160 round-12 P1 #6 (Codex
/// round-11 finding): a SEPARATE path from `inject_mcp`'s generic
/// `merge_opencode_config` (used by `weft_bus`/`weft_planner`/
/// `weft_curator`/`weft_global`, none of which carry a secret) precisely
/// because `url` here embeds a per-session bearer token. Two protections
/// the generic merge doesn't need:
///
/// 1. NEVER merges into an ALREADY-TRACKED `opencode.json`
///    ([`opencode_json_is_tracked`]) — the computer-use tool simply isn't
///    offered to this OpenCode worker in that one case, rather than risking
///    Weft's own credential wiring landing in the user's next commit. This
///    is the ONE tool this round is willing to withhold to hold the "never
///    write cross-repo wiring into a canonical repo" line; every other MCP
///    server keeps `merge_opencode_config`'s pre-existing accepted
///    limitation (it still merges into a tracked file — harmless, since none
///    of them carry a secret).
/// 2. The merged file is written ATOMICALLY owner-only on unix (issue #160
///    round-12 P1 #D — see [`write_owner_only_atomic`]'s own doc) — the
///    bearer token must not be left world/group-readable on a shared or
///    traversable checkout, mirroring the SAME protection
///    [`inject_computer_claude`] applies to Claude's own config. Before
///    round-12, this narrowed the file to `0600` via a SEPARATE
///    `set_permissions` call right after `merge_opencode_config`'s own
///    plain `std::fs::write` landed — a default-umask-readable window
///    between the two, same shape as [`inject_computer_claude`]'s own
///    pre-round-12 gap.
///
/// KNOWN, ACCEPTED residual (documented here, and in the round's own report):
/// same-uid processes can still read this file/env regardless (existing §9
/// residual) — what this closes is the token landing in a TRACKED repo file,
/// not same-machine, same-account visibility.
fn inject_computer_opencode(cwd: &Path, url: &str) {
    if opencode_json_is_tracked(cwd) {
        return;
    }
    merge_opencode_config(cwd, "weft_computer", url, true);
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
        return Injection { args: vec![] };
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
            }
        }
        "codex" => Injection {
            args: vec!["-c".into(), format!("mcp_servers.{server}.url={url}")],
        },
        "opencode" => {
            // `secret: false` — none of `weft_bus`/`weft_planner`/
            // `weft_curator`/`weft_global` carry a bearer token, so the
            // plain (non-atomic, non-owner-only) write is unchanged from
            // before round-12 P1 #D. Only `inject_computer_opencode`'s
            // `weft_computer` merge passes `true`.
            merge_opencode_config(cwd, server, url, false);
            Injection { args: vec![] }
        }
        _ => Injection { args: vec![] },
    }
}

/// Deep-merge `mcp.<server> = {type:remote, url, enabled:true}` into the cwd's
/// opencode.json, preserving any existing config the sub-repo shipped.
///
/// `secret` (issue #160 round-12 P1 #D): when `true` ([`inject_computer_opencode`]'s
/// ONE caller — `url` embeds a per-session bearer token), the merged file is
/// written via [`write_owner_only_atomic`] instead of a plain `std::fs::write`
/// — see that function's own doc for the write-then-chmod window this
/// closes. `false` (every other caller, via `inject_mcp`'s generic
/// `"opencode"` branch) keeps the pre-existing plain write: none of
/// `weft_bus`/`weft_planner`/`weft_curator`/`weft_global` carry a secret, so
/// there is nothing here for owner-only atomicity to protect, and forcing it
/// anyway would needlessly tighten a config file the sub-repo may expect to
/// read/write with its own tooling.
fn merge_opencode_config(cwd: &Path, server: &str, url: &str, secret: bool) {
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
    if secret {
        let _ = write_owner_only_atomic(&path, &bytes);
    } else {
        let _ = std::fs::write(&path, &bytes);
    }
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
        let inj = inject("http://127.0.0.1:9", 1, "10", "claude", &dir);
        assert_eq!(inj.args[0], "--mcp-config");
        let cfg = std::fs::read_to_string(dir.join(".weft-bus.mcp.json")).unwrap();
        assert!(cfg.contains("weft_bus") && cfg.contains("/bus/1/10/mcp"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn codex_uses_config_override() {
        let inj = inject("http://127.0.0.1:9", 2, "30", "codex", Path::new("/tmp"));
        assert_eq!(
            inj.args,
            vec![
                "-c".to_string(),
                "mcp_servers.weft_bus.url=http://127.0.0.1:9/bus/2/30/mcp".to_string()
            ]
        );
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

    /// issue #160 round-12 P1 #6: Claude's computer-use config now lives
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
        let inj = inject_computer("http://127.0.0.1:9", 1, "10", "claude", &dir, None);
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
        assert!(cfg.contains("weft_computer") && cfg.contains("/computer/1/10/mcp"));
        // issue #160 round-11 P1 #A: the injected URL still carries the
        // EXACT per-session bearer `computer_session_token` would mint for
        // this same (thread, dir).
        assert!(
            cfg.contains(&format!("key={}", crate::bus::computer_srv::computer_session_token(1, "10"))),
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

    /// issue #160 round-12 P1 #D: re-injection (the SAME `(thread, dir)`
    /// writing this SAME path again — a resumed/rerun session) must land
    /// via [`write_owner_only_atomic`]'s remove-then-`create_new` path, not
    /// silently fall back to a wider-mode write because the file already
    /// exists. Runs the injection TWICE for the identical `(thread, dir)`
    /// and asserts the SECOND write is still exactly `0600` and the content
    /// reflects the newer URL — never a stale leftover from the first write,
    /// and never briefly `0644` in between.
    #[test]
    fn computer_claude_config_reinjection_stays_owner_only_and_atomic() {
        let _env = crate::paths::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let weft_home =
            std::env::temp_dir().join(format!("weft-inj-comp-reinject-home-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&weft_home);
        std::env::set_var("WEFT_HOME", weft_home.to_str().unwrap());

        let dir = std::env::temp_dir().join(format!("weft-inj-comp-reinject-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);

        let first = inject_computer("http://127.0.0.1:9", 5, "50", "claude", &dir, None);
        let cfg_path = std::path::PathBuf::from(&first.args[1]);
        assert!(cfg_path.exists(), "the first injection must write the config");

        // A second injection for the SAME (thread, dir) — standing in for a
        // resumed/rerun session hitting the SAME predictable path.
        let second = inject_computer("http://127.0.0.1:9", 5, "50", "claude", &dir, Some(7));
        assert_eq!(
            std::path::PathBuf::from(&second.args[1]),
            cfg_path,
            "re-injection for the same (thread, dir) must reuse the same predictable path"
        );
        let cfg = std::fs::read_to_string(&cfg_path).unwrap();
        assert!(cfg.contains("wt=7"), "the SECOND write's content must win: {cfg}");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&cfg_path).unwrap().permissions().mode() & 0o777;
            assert_eq!(
                mode, 0o600,
                "re-injection must remain exactly 0600, never a wider mode surviving from a stale \
                 create, got {mode:o}"
            );
        }

        std::env::remove_var("WEFT_HOME");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&weft_home);
    }

    #[test]
    fn computer_codex_uses_config_override() {
        let inj = inject_computer("http://127.0.0.1:9", 1, "10", "codex", Path::new("/tmp"), None);
        assert_eq!(
            inj.args,
            vec![
                "-c".to_string(),
                format!(
                    "mcp_servers.weft_computer.url=http://127.0.0.1:9/computer/1/10/mcp?key={}",
                    crate::bus::computer_srv::computer_session_token(1, "10")
                ),
            ]
        );
    }

    /// issue #160 round-2 P2 §5: a resolved `wt` appends `?wt=<id>` to the
    /// injected URL, for both the claude file-based injection and codex's
    /// config-override flag. Needs an ISOLATED `WEFT_HOME` for the claude half
    /// (round-12 P1 #6 moved that config out of the worktree) — see
    /// `paths::ENV_LOCK`'s own doc.
    #[test]
    fn computer_wt_appends_the_query_param_for_claude_and_codex() {
        let _env = crate::paths::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let weft_home =
            std::env::temp_dir().join(format!("weft-inj-comp-wt-home-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&weft_home);
        std::env::set_var("WEFT_HOME", weft_home.to_str().unwrap());

        let key = crate::bus::computer_srv::computer_session_token(1, "10");
        let dir = std::env::temp_dir().join(format!("weft-inj-comp-wt-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let inj = inject_computer("http://127.0.0.1:9", 1, "10", "claude", &dir, Some(42));
        assert_eq!(inj.args[0], "--mcp-config");
        let cfg_path = std::path::PathBuf::from(&inj.args[1]);
        assert!(!cfg_path.starts_with(&dir), "must live outside the worktree, got {cfg_path:?}");
        let cfg = std::fs::read_to_string(&cfg_path).unwrap();
        assert!(cfg.contains("/computer/1/10/mcp?wt=42"), "{cfg}");
        assert!(cfg.contains(&format!("&key={key}")), "{cfg}");

        std::env::remove_var("WEFT_HOME");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&weft_home);

        let inj = inject_computer("http://127.0.0.1:9", 1, "10", "codex", Path::new("/tmp"), Some(42));
        assert_eq!(
            inj.args,
            vec![
                "-c".to_string(),
                format!("mcp_servers.weft_computer.url=http://127.0.0.1:9/computer/1/10/mcp?wt=42&key={key}"),
            ]
        );
    }

    // —— issue #160 round-11 P1 #A: computer_url mints a per-session `&key=` ——

    /// `computer_url` itself, directly: the token it appends is EXACTLY
    /// `computer_session_token(thread, dir)` for that same path's own
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
                crate::bus::computer_srv::computer_session_token(3, "30")
            )
        );

        let with_wt = computer_url("http://127.0.0.1:9", 3, "30", Some(9));
        assert_eq!(
            with_wt,
            format!(
                "http://127.0.0.1:9/computer/3/30/mcp?wt=9&key={}",
                crate::bus::computer_srv::computer_session_token(3, "30")
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
        let key = crate::bus::computer_srv::computer_session_token(1, "10");
        let with_computer = acp_mcp_servers(
            "http://127.0.0.1:9",
            1,
            "10",
            true,
            false,
            false,
            false,
            true,
            None,
        );
        assert!(with_computer.iter().any(|s| s.name == "weft_computer"
            && s.url == format!("http://127.0.0.1:9/computer/1/10/mcp?key={key}")));

        let without_computer = acp_mcp_servers(
            "http://127.0.0.1:9",
            1,
            "10",
            true,
            false,
            false,
            false,
            false,
            None,
        );
        assert!(!without_computer.iter().any(|s| s.name == "weft_computer"));
    }

    /// issue #160 round-2 P2 §5: `computer_wt` forwards into the injected
    /// `weft_computer` URL's `?wt=` query param for an ACP worker.
    #[test]
    fn acp_mcp_servers_computer_wt_pins_the_worktree_query_param() {
        let key = crate::bus::computer_srv::computer_session_token(1, "10");
        let with_wt = acp_mcp_servers(
            "http://127.0.0.1:9",
            1,
            "10",
            true,
            false,
            false,
            false,
            true,
            Some(7),
        );
        assert!(with_wt.iter().any(|s| s.name == "weft_computer"
            && s.url == format!("http://127.0.0.1:9/computer/1/10/mcp?wt=7&key={key}")));
    }

    #[test]
    fn claude_ask_hook_wires_pretooluse_settings() {
        let dir = std::env::temp_dir().join(format!("weft-askh-c-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let inj = inject_ask_hook("http://127.0.0.1:9", 1, "10", "claude", &dir);
        assert_eq!(inj.args[0], "--settings");
        let script = std::fs::read_to_string(dir.join(".weft-ask-hook.sh")).unwrap();
        assert!(script.contains("/ask/1/10?tool=claude"));
        let settings: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(dir.join(".weft-ask.settings.json")).unwrap(),
        )
        .unwrap();
        assert!(settings["hooks"]["PreToolUse"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap()
            .contains(".weft-ask-hook.sh"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn codex_ask_hook_writes_worktree_route_without_launch_bypass() {
        let dir = std::env::temp_dir().join(format!("weft-askh-x-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let inj = inject_ask_hook("http://127.0.0.1:9", 2, "30", "codex", &dir);
        assert!(
            inj.args.is_empty(),
            "global trusted hook needs no launch args"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join(".weft-codex-ask-url")).unwrap(),
            "http://127.0.0.1:9/ask/2/30?tool=codex"
        );
        assert!(!dir.join(".weft-ask-hook.sh").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn opencode_ask_plugin_written_and_excluded() {
        let dir = std::env::temp_dir().join(format!("weft-inj-oask-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let inj = inject_ask_hook("http://127.0.0.1:9", 1, "10", "opencode", &dir);
        assert!(
            inj.args.is_empty(),
            "opencode plugin auto-loads, no launch flag"
        );
        let plugin = std::fs::read_to_string(dir.join(".opencode/plugins/weft-ask.js")).unwrap();
        assert!(plugin.contains("tool.execute.before"));
        assert!(plugin.contains("/ask/1/10?tool=opencode"));
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
        let inj = inject_ask_hook(&base, 1, "10", "claude", &dir);
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
        use crate::hook_test_support::{decision_body, decision_of, run_hook_script, serve_raw_once};
        let body = decision_body("allow", false);
        let len = body.len();
        let base = serve_raw_once("HTTP/1.1 500 Internal Server Error", body, len).await;

        let dir = std::env::temp_dir().join(format!("weft-askh-500-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let _ = inject_ask_hook(&base, 1, "10", "claude", &dir);

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
        use crate::hook_test_support::{decision_body, decision_of, run_hook_script, serve_raw_once};
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
        let _ = inject_ask_hook(&base, 1, "10", "claude", &dir);

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
        let (base, _h) =
            crate::bus::server::serve(crate::bus::BusRegistry::new(), db, asks.clone())
                .await
                .unwrap();

        let dir = std::env::temp_dir().join(format!("weft-askh-live-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let _ = inject_ask_hook(&base, 1, "10", "claude", &dir);

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
        let inj = inject("http://127.0.0.1:9", 1, "10", "opencode", &dir);
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

        let _ = inject("http://127.0.0.1:9", 1, "1", "claude", &wt);
        assert!(wt.join(".weft-bus.mcp.json").exists(), "file written");
        let status = Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(&wt)
            .output()
            .unwrap();
        let s = String::from_utf8_lossy(&status.stdout);
        assert!(
            !s.contains(".weft-bus.mcp.json"),
            "injected file must be git-excluded, got: {s}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    // —— issue #160 round-12 P1 #6: computer-use token never lands in a tracked opencode.json ——

    /// The untracked/common case: no pre-existing `opencode.json` at all (or
    /// one the sub-repo never committed) — the computer server's merge
    /// proceeds exactly like `inject_mcp`'s generic merge does for
    /// `weft_bus`, PLUS narrows the file to `0600` on unix (the token-bearing
    /// difference `inject_computer_opencode` adds on top).
    #[test]
    fn computer_opencode_merges_and_narrows_permissions_when_untracked() {
        let dir = std::env::temp_dir().join(format!("weft-inj-comp-oc-untracked-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let inj = inject_computer("http://127.0.0.1:9", 1, "10", "opencode", &dir, None);
        assert!(inj.args.is_empty(), "opencode has no launch-flag injection");
        let cfg_path = dir.join("opencode.json");
        let cfg = std::fs::read_to_string(&cfg_path).unwrap();
        assert!(cfg.contains("weft_computer") && cfg.contains("/computer/1/10/mcp"), "{cfg}");
        assert!(
            cfg.contains(&format!("key={}", crate::bus::computer_srv::computer_session_token(1, "10"))),
            "{cfg}"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&cfg_path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "the bearer-token-bearing opencode.json must be 0600, got {mode:o}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// issue #160 round-12 P1 #D: re-merging the SAME `opencode.json` again
    /// (a second spawn/resume for the same worktree) must go through
    /// `write_owner_only_atomic`'s remove-then-`create_new` path and land at
    /// exactly `0600` again — never fall back to a wider mode because the
    /// file already exists — and the merge itself must still preserve
    /// whatever the FIRST merge already wrote (the deep-merge semantics
    /// `merge_opencode_config` reads-then-rewrites are unaffected by which
    /// write path lands the bytes).
    #[test]
    fn computer_opencode_reinjection_stays_owner_only_and_preserves_the_merge() {
        let dir = std::env::temp_dir().join(format!("weft-inj-comp-oc-reinject-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let cfg_path = dir.join("opencode.json");

        let _ = inject_computer("http://127.0.0.1:9", 2, "20", "opencode", &dir, None);
        assert!(cfg_path.exists(), "the first merge must write the config");

        // A second merge, standing in for a resumed/rerun session's own
        // spawn-time injection landing on the SAME worktree.
        let _ = inject_computer("http://127.0.0.1:9", 2, "20", "opencode", &dir, Some(9));
        let cfg = std::fs::read_to_string(&cfg_path).unwrap();
        assert!(cfg.contains("wt=9"), "the SECOND merge's URL must win: {cfg}");
        assert!(cfg.contains("weft_computer"), "{cfg}");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&cfg_path).unwrap().permissions().mode() & 0o777;
            assert_eq!(
                mode, 0o600,
                "re-merging must remain exactly 0600, never a wider mode surviving from a stale \
                 create, got {mode:o}"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The protected case: the sub-repo ships (and has COMMITTED) its own
    /// `opencode.json` — merging the computer server's per-session bearer
    /// token into it would persist Weft's own credential wiring straight into
    /// the user's repo history the next time they commit. This must refuse
    /// the merge outright (the tool simply isn't offered to this worker),
    /// never write the token into that tracked file. `weft_bus`'s OWN merge
    /// (via `inject`, unaffected by this round) is exercised alongside to
    /// confirm this restriction is scoped to the computer server alone — the
    /// other servers keep their pre-existing "still merges into a tracked
    /// file, accepted limitation" behavior since they carry no secret.
    #[test]
    fn computer_opencode_never_merges_into_an_already_tracked_opencode_json() {
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
        std::fs::write(
            root.join("opencode.json"),
            r#"{"mcp":{"repo_own":{"type":"local","command":["x"]}}}"#,
        )
        .unwrap();
        sh(&root, &["git", "add", "-A"]);
        sh(&root, &["git", "commit", "-q", "-m", "init"]);

        let token = crate::bus::computer_srv::computer_session_token(1, "10");
        let inj = inject_computer("http://127.0.0.1:9", 1, "10", "opencode", &root, None);
        assert!(inj.args.is_empty());
        let cfg = std::fs::read_to_string(root.join("opencode.json")).unwrap();
        assert!(
            !cfg.contains("weft_computer") && !cfg.contains(&token),
            "a TRACKED opencode.json must never gain the computer server or its token: {cfg}"
        );
        // The repo's own pre-existing content is untouched.
        assert!(cfg.contains("repo_own"));

        // Sanity: `weft_bus` (no secret) still merges into this SAME tracked
        // file — confirming the refusal above is specific to the computer
        // server, not a blanket "never touch a tracked opencode.json" that
        // would also silently break the bus.
        let _ = inject("http://127.0.0.1:9", 1, "10", "opencode", &root);
        let cfg_after_bus = std::fs::read_to_string(root.join("opencode.json")).unwrap();
        assert!(cfg_after_bus.contains("weft_bus"), "{cfg_after_bus}");

        let _ = std::fs::remove_dir_all(&root);
    }
}
